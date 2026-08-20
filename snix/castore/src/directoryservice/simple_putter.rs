use super::Directory;
use super::DirectoryPutter;
use super::DirectoryService;
use crate::B3Digest;
use crate::directoryservice::directory_graph::DirectoryGraphBuilder;
use crate::directoryservice::order_validator::{self, LeavesToRoot};
use tonic::async_trait;
use tracing::instrument;
use tracing::warn;

/// This is an implementation of DirectoryPutter that simply
/// inserts individual Directory messages one by one, on close, after
/// they successfully validated.
pub struct SimplePutter<'a, DS> {
    directory_service: &'a DS,

    builder: Option<DirectoryGraphBuilder<LeavesToRoot>>,
}

impl<'a, DS> SimplePutter<'a, DS>
where
    DS: DirectoryService,
{
    pub fn new(directory_service: &'a DS) -> Self {
        Self {
            directory_service,
            builder: Some(DirectoryGraphBuilder::<LeavesToRoot>::new()),
        }
    }
}

#[async_trait]
impl<DS: DirectoryService + 'static> DirectoryPutter for SimplePutter<'_, DS> {
    #[instrument(level = "trace", skip_all, fields(directory.digest=%directory.digest()), err)]
    async fn put(&mut self, directory: Directory) -> Result<(), super::Error> {
        let builder = self.builder.as_mut().ok_or_else(|| Error::AlreadyClosed)?;

        builder
            .try_insert(directory)
            .map_err(Error::OrderingError)?;

        Ok(())
    }

    #[instrument(level = "trace", skip_all, ret, err)]
    async fn close(&mut self) -> Result<B3Digest, super::Error> {
        let builder = self.builder.take().ok_or_else(|| Error::AlreadyClosed)?;

        // Retrieve the validated directories.
        let directory_graph = builder.build().map_err(Error::OrderingError)?;
        let root_digest = directory_graph.root().digest();

        for directory in directory_graph.drain_leaves_to_root() {
            let exp_digest = directory.digest();
            let actual_digest = self.directory_service.put(directory).await?;

            // ensure the digest the backend told us matches our expectations.
            if exp_digest != actual_digest {
                warn!(directory.digest_expected=%exp_digest, directory.digest_actual=%actual_digest, "unexpected digest");
                Err(Error::UnexpectedDigest {
                    expected: exp_digest,
                    actual: actual_digest,
                })?;
            }
        }

        Ok(root_digest)
    }
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("DirectoryGraphBuilder already closed")]
    AlreadyClosed,
    #[error("got unexpected digest from backend, expected {expected}, actual {actual}")]
    UnexpectedDigest {
        expected: B3Digest,
        actual: B3Digest,
    },
    #[error("failure during graph validation")]
    OrderingError(#[from] order_validator::OrderingError),
}

impl From<Error> for super::Error {
    fn from(value: Error) -> Self {
        Self(Box::new(value))
    }
}
