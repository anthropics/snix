use std::sync::Arc;

use futures::{StreamExt, TryFutureExt, TryStreamExt, stream::BoxStream};
use tonic::async_trait;
use tracing::instrument;

use crate::{
    B3Digest, Directory,
    composition::{CompositionContext, ServiceBuilder},
    directoryservice::{self, DirectoryPutter, DirectoryService, FailingPutter},
};

/// Holds references to multiple directory services.
/// Read requests try services in parallel.
/// The first positive response is returned (Ok(None) does only bubble up if all backends return this)
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
}

#[async_trait]
impl<DS> DirectoryService for Race<DS>
where
    DS: DirectoryService,
{
    #[instrument(skip(self, digest), fields(directory.digest = %digest, instance_name = %self.instance_name))]
    async fn get(&self, digest: &B3Digest) -> Result<Option<Directory>, directoryservice::Error> {
        // prepare requests to all backends, and annotate the backend_idx in the error case.
        let mut requests: Vec<_> = self
            .services
            .iter()
            .enumerate()
            .map(|(backend_idx, svc)| {
                svc.get(digest)
                    .map_err(move |err| Error::Backend(backend_idx, err))
            })
            .collect();

        while !requests.is_empty() {
            let (resp, _fut_idx, remaining) = futures::future::select_all(requests).await;

            match resp {
                // If this Ok(Some(_)), return, we're done
                Ok(Some(directory)) => return Ok(Some(directory)),
                // Skip over backends that reported they don't have it.
                Ok(None) => {}
                // Bubble up errors. We already mapped the backend_idx into the error.
                Err(err) => return Err(err)?,
            }

            requests = remaining;
        }

        // if we exhausted all backends, return Ok(None).
        return Ok(None);
    }

    #[instrument(skip_all, fields(directory.digest = %root_directory_digest, instance_name = %self.instance_name))]
    fn get_recursive(
        &self,
        root_directory_digest: &B3Digest,
    ) -> BoxStream<'_, Result<Directory, directoryservice::Error>> {
        let digest = *root_directory_digest;

        // Create a bunch of futures that return ready once they get the first element of the stream, or an EOF.
        let mut requests: Vec<_> = self
            .services
            .iter()
            .enumerate()
            .map(|(backend_idx, svc)| {
                Box::pin(async move {
                    let mut stream = svc
                        .get_recursive(&digest)
                        .map_err(move |err| Error::Backend(backend_idx, err));
                    if let Some(directory) = stream.try_next().await? {
                        Ok::<_, Error>(Some((directory, stream)))
                    } else {
                        Ok(None)
                    }
                })
            })
            .collect();

        async_stream::try_stream! {
            while !requests.is_empty() {
                let (resp, _fut_idx, remaining) = futures::future::select_all(requests).await;

                match resp {
                    // If this Ok(Some(_, _)), yield from that stream.
                    Ok(Some((directory, mut stream))) => {
                        yield directory;

                        while let Some(directory) = stream.try_next().await? {
                            yield directory
                        }
                    }
                    Ok(None) => {
                        // Skip over backends that reported they don't have it.
                    },
                    // Bubble up errors. We already mapped the backend_idx into the error.
                    Err(err) => Err(directoryservice::Error::from(err))?,
                }
                requests = remaining;
            }
            // if we exhausted all backends, this returns an empty stream
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

    #[error("error from service at index {0}")]
    Backend(usize, #[source] directoryservice::Error),

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
        assert_matches!(err, Error::Backend(0, _))
    }

    // FUTUREWORK: ideally we'd be constructing mocks that take longer than others / never return,
    // but that's not supported in automock: https://github.com/asomers/mockall/issues/189
    // So it's a bit tough to create test cases reliably.
}
