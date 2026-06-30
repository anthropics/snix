use std::{
    collections::{BTreeMap, btree_map},
    fmt::Display,
    sync::Arc,
};

use futures::{StreamExt, TryStreamExt, stream::BoxStream};
use tonic::async_trait;
use tracing::instrument;

use crate::{
    B3Digest, Directory,
    composition::{CompositionContext, CompositionError, ServiceBuilder},
    directoryservice::{self, DirectoryPutter, DirectoryService, FailingPutter},
};

/// Holds references to many different directory services, each with an associated priority.
/// Read requests try services sequentially, sorted by their priority, ascending.
/// Any error in a service bubbles up.
/// Write requests are not implemented.
pub struct Priority<DS> {
    instance_name: String,
    /// The services, keyed by their priority.
    // NOTE: Arc<dyn DS> implements DS too, so you can put different service types in here.
    services: BTreeMap<Prio, DS>,
}

impl From<u64> for Prio {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Eq, PartialEq, PartialOrd, Ord, Debug)]
pub struct Prio(u64);

impl Display for Prio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl<DS> Priority<DS> {
    /// Construct from an iterator of priorities and services.
    /// Services with the same priority are converted to a race combinator.
    pub fn new<I: IntoIterator<Item = (Prio, DS)>>(instance_name: String, iter: I) -> Priority<DS> {
        let mut services = BTreeMap::new();

        for (prio, service) in iter {
            match services.entry(prio) {
                btree_map::Entry::Vacant(entry) => {
                    entry.insert(service);
                }
                btree_map::Entry::Occupied(_entry) => {
                    unimplemented!(
                        "already got another service at prio {prio}, race not implemented"
                    );
                }
            }
        }

        Self {
            instance_name,
            services,
        }
    }
}

#[async_trait]
impl<DS> DirectoryService for Priority<DS>
where
    DS: DirectoryService,
{
    #[instrument(skip(self, digest), fields(directory.digest = %digest, instance_name = %self.instance_name))]
    async fn get(&self, digest: &B3Digest) -> Result<Option<Directory>, directoryservice::Error> {
        // traverse the list of services in priority order. If any service has it, return from there.
        // Errors cause the combinator to bail out early.
        for (prio, service) in self.services.iter() {
            if let Some(directory) = service
                .get(digest)
                .await
                .map_err(|err| Error::Backend(*prio, err))?
            {
                return Ok(Some(directory));
            }
        }

        Ok(None)
    }

    #[instrument(skip_all, fields(directory.digest = %root_directory_digest, instance_name = %self.instance_name))]
    fn get_recursive(
        &self,
        root_directory_digest: &B3Digest,
    ) -> BoxStream<'_, Result<Directory, directoryservice::Error>> {
        let digest = *root_directory_digest;
        async_stream::try_stream! {
            for (prio, service) in self.services.iter() {
                let mut directories_stream = service.get_recursive(&digest);
                // Once a service said it has a closure (non-empty stream), we return everything from there, including errors.
                if let Some(directory) = directories_stream.try_next().await.map_err(|err| { Error::Backend(*prio, err)})? {
                    yield directory;

                    while let Some(directory) = directories_stream.try_next().await.map_err(|err| { Error::Backend(*prio, err)})? {
                        yield directory;
                    }
                    // we're done
                    return;
                }
                // try the next service in the list
            }
        }
        .boxed()
    }

    #[instrument(skip_all, fields(instance_name = %self.instance_name))]
    async fn put(&self, _directory: Directory) -> Result<B3Digest, directoryservice::Error> {
        Err(Error::Unimplemented.into())
    }

    #[instrument(skip_all)]
    fn put_multiple_start(&self) -> Box<dyn DirectoryPutter + '_> {
        Box::new(FailingPutter)
    }
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("wrong arguments: {0}")]
    WrongConfig(&'static str),

    #[error("error from service with prio {0}")]
    Backend(Prio, #[source] directoryservice::Error),

    #[error("puts are unimplemented")]
    Unimplemented,
}

impl From<Error> for directoryservice::Error {
    fn from(value: Error) -> Self {
        Self(Box::new(value))
    }
}

#[derive(serde::Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct PriorityConfig {
    services: BTreeMap<u64, String>,
}

impl TryFrom<url::Url> for PriorityConfig {
    type Error = Box<dyn std::error::Error + Send + Sync>;
    fn try_from(url: url::Url) -> Result<Self, Self::Error> {
        if url.has_authority() || !url.path().is_empty() {
            return Err(Error::WrongConfig("no authority or path allowed").into());
        }
        Ok(serde_qs::from_str(url.query().unwrap_or_default())?)
    }
}

#[async_trait]
impl ServiceBuilder for PriorityConfig {
    type Output = dyn DirectoryService;
    async fn build<'a>(
        &'a self,
        instance_name: &str,
        context: &CompositionContext,
    ) -> Result<Arc<Self::Output>, Box<dyn std::error::Error + Send + Sync>> {
        let services = futures::future::try_join_all(self.services.iter().map(
            |(prio, instance_ref)| async move {
                Ok::<_, CompositionError>((
                    Prio::from(*prio),
                    context.resolve::<Self::Output>(instance_ref).await?,
                ))
            },
        ))
        .await?;

        Ok(Arc::new(Priority::new(instance_name.to_string(), services)))
    }
}

#[cfg(test)]
mod test {
    use mockall::{Sequence, predicate};
    use pretty_assertions::{assert_eq, assert_matches};

    use super::*;
    use crate::{
        directoryservice::{MockDirectoryService, failing_putter},
        fixtures::{DIRECTORY_A, DIRECTORY_B, DIRECTORY_WITH_KEEP},
    };

    /// If first has something, last is never tried.
    #[tokio::test]
    async fn get_first_gets_tried_only() {
        let mut first = MockDirectoryService::new();
        let mut last = MockDirectoryService::new();

        first
            .expect_get()
            .with(predicate::eq(DIRECTORY_WITH_KEEP.digest()))
            .once()
            .returning(|_| Ok(Some(DIRECTORY_WITH_KEEP.clone())));

        last.expect_get().never();

        let uut = Priority::new("uut".to_string(), [(0.into(), first), (1.into(), last)]);

        assert_eq!(
            Some(DIRECTORY_WITH_KEEP.clone()),
            uut.get(&DIRECTORY_WITH_KEEP.digest())
                .await
                .expect("to succeed")
        )
    }

    /// If first doesn't have it, we try last.
    #[tokio::test]
    async fn get_first_then_last() {
        let mut first = MockDirectoryService::new();
        let mut last = MockDirectoryService::new();
        let mut seq = Sequence::new();

        first
            .expect_get()
            .with(predicate::eq(DIRECTORY_WITH_KEEP.digest()))
            .once()
            .in_sequence(&mut seq)
            .returning(|_| Ok(None));

        last.expect_get()
            .with(predicate::eq(DIRECTORY_WITH_KEEP.digest()))
            .once()
            .in_sequence(&mut seq)
            .returning(|_| Ok(Some(DIRECTORY_WITH_KEEP.clone())));

        let uut = Priority::new("uut".to_string(), [(0.into(), first), (1.into(), last)]);

        assert_eq!(
            Some(DIRECTORY_WITH_KEEP.clone()),
            uut.get(&DIRECTORY_WITH_KEEP.digest())
                .await
                .expect("to succeed")
        )
    }

    /// If none of the two have it, we return None.
    #[tokio::test]
    async fn get_first_then_last_not_found() {
        let mut first = MockDirectoryService::new();
        let mut last = MockDirectoryService::new();
        let mut seq = Sequence::new();

        first
            .expect_get()
            .with(predicate::eq(DIRECTORY_WITH_KEEP.digest()))
            .once()
            .in_sequence(&mut seq)
            .returning(|_| Ok(None));

        last.expect_get()
            .with(predicate::eq(DIRECTORY_WITH_KEEP.digest()))
            .once()
            .in_sequence(&mut seq)
            .returning(|_| Ok(None));

        let uut = Priority::new("uut".to_string(), [(0.into(), first), (1.into(), last)]);

        assert_eq!(
            None,
            uut.get(&DIRECTORY_WITH_KEEP.digest())
                .await
                .expect("to succeed")
        )
    }

    /// Errors are bubbled up from the first backend emitting the error,
    /// and the error identifies the backend that emitted the error.
    #[tokio::test]
    async fn get_bubble_up_error_first() {
        let mut first = MockDirectoryService::new();
        let mut last = MockDirectoryService::new();

        first
            .expect_get()
            .with(predicate::eq(DIRECTORY_WITH_KEEP.digest()))
            .once()
            .returning(|_| Err(directoryservice::Error("oh no".into())));

        last.expect_get().never();

        let uut = Priority::new("uut".to_string(), [(0.into(), first), (1.into(), last)]);

        let err = uut
            .get(&DIRECTORY_WITH_KEEP.digest())
            .await
            .expect_err("must fail")
            .0;

        let err = err.downcast_ref::<Error>().unwrap();
        assert_matches!(err, Error::Backend(Prio(0), _));
    }

    /// If the first backend responds to get_recursive, we return from there.
    #[tokio::test]
    async fn get_recursive_first() {
        let mut first = MockDirectoryService::new();
        let mut last = MockDirectoryService::new();

        first
            .expect_get_recursive()
            .with(predicate::eq(DIRECTORY_B.digest()))
            .once()
            .returning(|_| {
                futures::stream::iter([Ok(DIRECTORY_B.clone()), Ok(DIRECTORY_A.clone())]).boxed()
            });
        last.expect_get_recursive().never();

        let uut = Priority::new("uut".to_string(), [(0.into(), first), (1.into(), last)]);

        let directories = uut
            .get_recursive(&DIRECTORY_B.digest())
            .try_collect::<Vec<_>>()
            .await
            .expect("to succeed");

        assert_eq!(vec![DIRECTORY_B.clone(), DIRECTORY_A.clone()], directories);
    }

    /// If the first one doesn't have a directory closure, return from the next.
    #[tokio::test]
    async fn get_recursive_second() {
        let mut first = MockDirectoryService::new();
        let mut last = MockDirectoryService::new();
        let mut seq = Sequence::new();

        first
            .expect_get_recursive()
            .with(predicate::eq(DIRECTORY_B.digest()))
            .once()
            .in_sequence(&mut seq)
            .returning(|_| futures::stream::empty().boxed());

        last.expect_get_recursive()
            .with(predicate::eq(DIRECTORY_B.digest()))
            .once()
            .in_sequence(&mut seq)
            .returning(|_| {
                futures::stream::iter([Ok(DIRECTORY_B.clone()), Ok(DIRECTORY_A.clone())]).boxed()
            });

        let uut = Priority::new("uut".to_string(), [(0.into(), first), (1.into(), last)]);

        let directories = uut
            .get_recursive(&DIRECTORY_B.digest())
            .try_collect::<Vec<_>>()
            .await
            .expect("to succeed");

        assert_eq!(vec![DIRECTORY_B.clone(), DIRECTORY_A.clone()], directories);
    }

    /// Propagate errors from get_recursive
    #[tokio::test]
    async fn get_recursive_error_first() {
        let mut first = MockDirectoryService::new();
        let mut last = MockDirectoryService::new();

        first
            .expect_get_recursive()
            .with(predicate::eq(DIRECTORY_B.digest()))
            .once()
            .returning(|_| {
                futures::stream::iter([Err(directoryservice::Error("oh no".into()))]).boxed()
            });

        last.expect_get_recursive().never();

        let uut = Priority::new("uut".to_string(), [(0.into(), first), (1.into(), last)]);

        let err = uut
            .get_recursive(&DIRECTORY_B.digest())
            .try_collect::<Vec<_>>()
            .await
            .expect_err("to fail")
            .0;

        let err = err.downcast_ref::<Error>().unwrap();
        assert_matches!(err, Error::Backend(Prio(0), _));
    }

    /// put is unsupported, and not sent to the backend
    #[tokio::test]
    async fn put_unsupported() {
        let mut first = MockDirectoryService::new();
        first.expect_put().never();

        let uut = Priority::new("uut".to_string(), [(0.into(), first)]);

        let err = uut
            .put(DIRECTORY_WITH_KEEP.clone())
            .await
            .expect_err("must fail")
            .0;

        let err = err.downcast_ref::<Error>().unwrap();
        assert_matches!(err, Error::Unimplemented);
    }

    /// put_recursive is unsupported, and not sent to the backend
    #[tokio::test]
    async fn put_recursive_unsupported() {
        let mut first = MockDirectoryService::new();
        first.expect_put().never();

        let uut = Priority::new("uut".to_string(), [(0.into(), first)]);

        let mut handle = uut.put_multiple_start();
        let err = handle
            .put(DIRECTORY_WITH_KEEP.clone())
            .await
            .expect_err("must fail")
            .0;

        let err = err.downcast_ref::<failing_putter::Error>().unwrap();
        assert_matches!(err, failing_putter::Error::Unimplemented);
    }
}
