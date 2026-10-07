use std::sync::Arc;

use futures::{StreamExt, TryStreamExt, stream::BoxStream};
use tonic::async_trait;
use tracing::instrument;

use crate::{
    B3Digest, Directory, combinators,
    composition::{CompositionContext, ServiceBuilder},
    directoryservice::{self, DirectoryPutter, DirectoryService, FailingPutter},
};

/// Fans out requests to multiple stores in parallel, returning the first positive or erroneous answer.
///
/// A negative response is returned if all backends return them.
/// Write requests are not implemented.
pub struct Race<DS> {
    instance_name: String,
    services: Vec<DS>,
}

impl<DS> Race<DS> {
    /// Construct from an iterator of services.
    pub fn new<I: IntoIterator<Item = DS>>(instance_name: String, iter: I) -> Race<DS> {
        Self {
            instance_name,
            services: Vec::from_iter(iter),
        }
    }

    /// Add another sevice to the list.
    pub fn add(&mut self, svc: DS) {
        self.services.push(svc);
    }
}

#[async_trait]
impl<DS> DirectoryService for Race<DS>
where
    DS: DirectoryService,
{
    #[instrument(skip(self, digest), fields(directory.digest = %digest, instance_name = %self.instance_name))]
    async fn get(&self, digest: &B3Digest) -> Result<Option<Directory>, directoryservice::Error> {
        Ok(combinators::race::race_unary(&self.services, |svc| async {
            // Skip over `Ok(None)` by returning None,
            // but keep the Option<Directory> in the returned Ok() value.
            Some(svc.get(digest).await.transpose()?.map(Some))
        })
        .await
        .map_err(Error::Racing)?)
    }

    #[instrument(skip_all, fields(directory.digest = %root_directory_digest, instance_name = %self.instance_name))]
    fn get_recursive(
        &self,
        root_directory_digest: &B3Digest,
    ) -> BoxStream<'_, Result<Directory, directoryservice::Error>> {
        let digest = *root_directory_digest;
        combinators::race::race_stream(&self.services, move |svc| async move {
            let mut stream = svc.get_recursive(&digest).peekable();

            // Skip over backends that reported they don't have it.
            if std::pin::Pin::new(&mut stream).peek().await.is_none() {
                None
            } else {
                Some(stream)
            }
        })
        .map_err(Error::Racing)
        .err_into()
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

    #[error("error from racing")]
    Racing(#[source] combinators::race::Error<directoryservice::Error>),

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
pub struct RaceConfig {
    services: Vec<String>,
}

impl TryFrom<url::Url> for RaceConfig {
    type Error = Box<dyn std::error::Error + Send + Sync>;
    fn try_from(url: url::Url) -> Result<Self, Self::Error> {
        if url.has_authority() || !url.path().is_empty() {
            return Err(Error::WrongConfig("no authority or path allowed").into());
        }
        Ok(serde_qs::from_str(url.query().unwrap_or_default())?)
    }
}

#[async_trait]
impl ServiceBuilder for RaceConfig {
    type Output = dyn DirectoryService;
    async fn build<'a>(
        &'a self,
        instance_name: &str,
        context: &CompositionContext,
    ) -> Result<Arc<Self::Output>, Box<dyn std::error::Error + Send + Sync>> {
        let services =
            futures::future::try_join_all(self.services.iter().map(|instance_ref| async move {
                context.resolve::<Self::Output>(instance_ref).await
            }))
            .await?;

        Ok(Arc::new(Race::new(instance_name.to_string(), services)))
    }
}

#[cfg(test)]
mod test {
    use mockall::predicate;
    use pretty_assertions::assert_matches;

    use crate::{
        combinators,
        directoryservice::{self, DirectoryService, MockDirectoryService},
        fixtures::DIRECTORY_WITH_KEEP,
    };

    use super::{Error, Race};

    /// backends are tried exhaustively if all report None.
    #[tokio::test]
    async fn get_tries_exhaustively_on_none() {
        let first = {
            let mut svc = MockDirectoryService::new();
            svc.expect_get()
                .with(predicate::eq(DIRECTORY_WITH_KEEP.digest()))
                .once()
                .returning(|_| Ok(None));
            svc
        };
        let second = {
            let mut svc = MockDirectoryService::new();
            svc.expect_get()
                .with(predicate::eq(DIRECTORY_WITH_KEEP.digest()))
                .once()
                .returning(|_| Ok(None));
            svc
        };

        let uut = Race::new("uut".to_string(), [first, second]);

        assert!(
            uut.get(&DIRECTORY_WITH_KEEP.digest())
                .await
                .expect("to succeed")
                .is_none()
        )
    }

    /// if one has it and one does not, we return the positive result.
    #[tokio::test]
    async fn get_returns_positive() {
        let first = {
            let mut svc = MockDirectoryService::new();
            svc.expect_get()
                .with(predicate::eq(DIRECTORY_WITH_KEEP.digest()))
                .once()
                .returning(|_| Ok(Some(DIRECTORY_WITH_KEEP.clone())));
            svc
        };

        let second = {
            let mut svc = MockDirectoryService::new();
            svc.expect_get()
                .with(predicate::eq(DIRECTORY_WITH_KEEP.digest()))
                // We cannot be certain this is called at all, so no `once()` here.
                .returning(|_| Ok(None));
            svc
        };

        let uut = Race::new("uut".to_string(), [first, second]);

        assert_eq!(
            Some(DIRECTORY_WITH_KEEP.clone()),
            uut.get(&DIRECTORY_WITH_KEEP.digest())
                .await
                .expect("to succeed")
        )
    }

    /// Errors are bubbled up, and the error contains the correct service index.
    #[tokio::test]
    async fn get_return_error() {
        let first = {
            let mut svc = MockDirectoryService::new();
            svc.expect_get()
                .with(predicate::eq(DIRECTORY_WITH_KEEP.digest()))
                .once()
                .returning(|_| Err(directoryservice::Error("".into())));
            svc
        };

        // Ideally this one would be just slower than `first`.
        let second = {
            let mut svc = MockDirectoryService::new();
            svc.expect_get()
                .with(predicate::eq(DIRECTORY_WITH_KEEP.digest()))
                // We cannot be certain this is called at all, so no `once()` here.
                .returning(|_| Ok(None));
            svc
        };

        let uut = Race::new("uut".to_string(), [first, second]);

        let err = uut
            .get(&DIRECTORY_WITH_KEEP.digest())
            .await
            .expect_err("to fail")
            .0;
        let err = err.downcast_ref::<Error>().unwrap();
        assert_matches!(err, Error::Racing(combinators::race::Error::Backend(0, _)))
    }

    // FUTUREWORK: ideally we'd be constructing mocks that take longer than others / never return,
    // but that's not supported in automock: https://github.com/asomers/mockall/issues/189
    // So it's a bit tough to create test cases reliably.
}
