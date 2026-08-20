use std::sync::Arc;

use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream::BoxStream;
use tonic::async_trait;
use tracing::{instrument, trace};

use crate::composition::{CompositionContext, ServiceBuilder};
use crate::directoryservice::DirectoryServiceGraphExt;
use crate::directoryservice::directory_graph::DirectoryGraphBuilder;
use crate::directoryservice::order_validator::RootToLeaves;
use crate::directoryservice::{self, DirectoryPutter, DirectoryService, SimplePutter};
use crate::{B3Digest, Directory};

/// Asks near first, if not found, asks far.
/// If found in there, returns it, and *inserts* it into
/// near.
/// Specifically, it always obtains the entire directory closure from far and inserts it into near,
/// which is useful when far does not support accessing intermediate directories (but near does).
/// There is no negative cache.
/// Inserts and listings are not implemented for now.
pub struct Cache<DN, DF> {
    instance_name: String,
    near: DN,
    far: DF,
}

impl<DN, DF> Cache<DN, DF> {
    pub fn new(instance_name: String, near: DN, far: DF) -> Self {
        Self {
            instance_name,
            near,
            far,
        }
    }
}

#[async_trait]
impl<DN, DF> DirectoryService for Cache<DN, DF>
where
    DN: DirectoryService + Clone + 'static,
    DF: DirectoryService + Clone + 'static,
{
    #[instrument(skip(self, digest), fields(directory.digest = %digest, instance_name = %self.instance_name))]
    async fn get(&self, digest: &B3Digest) -> Result<Option<Directory>, directoryservice::Error> {
        // check near
        if let Some(directory) = self.near.get(digest).await.map_err(Error::NearGet)? {
            trace!("serving from cache");
            return Ok(Some(directory));
        }

        trace!("not found in near, asking remote…");
        // We always ask recursive, and populate the children to support far not allowing non-root access
        // We currently wait for all children to be received before returning
        // the requested directory, so subsequent children requests don't fail when these
        // stores are used.
        let directory_graph = self
            .far
            .get_directory_graph(digest)
            .await
            .map_err(Error::FarGet)?;

        // If far had the directory, put into near.
        if let Some(directory_graph) = directory_graph {
            let root = directory_graph.root().to_owned();

            // Drain into near
            let digest_near = self
                .near
                .put_directory_graph(directory_graph)
                .await
                .map_err(Error::NearPut)?;

            if digest_near != *digest {
                Err(Error::InsertingGraphMismatch {
                    digest_expected: *digest,
                    digest_actual: digest_near,
                })?;
            }

            Ok(Some(root))
        } else {
            Ok(None)
        }
    }

    #[instrument(skip_all, fields(instance_name = %self.instance_name))]
    async fn put(&self, _directory: Directory) -> Result<B3Digest, directoryservice::Error> {
        Err(Error::Unimplemented.into())
    }

    #[instrument(skip_all, fields(directory.digest = %root_directory_digest, instance_name = %self.instance_name))]
    fn get_recursive(
        &self,
        root_directory_digest: &B3Digest,
    ) -> BoxStream<'_, Result<Directory, directoryservice::Error>> {
        let near = &self.near;
        let far = &self.far;
        let digest = *root_directory_digest;

        async_stream::try_stream! {
            let mut directories = near.get_recursive(&digest);

            if let Some(first) = directories.try_next().await.map_err(Error::NearGet)? {
                trace!("serving from cache");
                yield first;

                while let Some(dir) = directories.try_next().await.map_err(Error::NearGet)? {
                    yield dir;
                }
                return;
            }

            trace!("not found in 'near', asking 'far'");

            let mut directories = std::pin::pin!(far.get_recursive(&digest).peekable());
            if directories.as_mut().peek().await.is_none() {
                trace!("not found in 'far' either");
                return;
            }

            let mut builder = DirectoryGraphBuilder::<RootToLeaves>::new(digest);

            // Return to the client, while inserting to the graph builder.
            while let Some(directory) = directories.try_next().await.map_err(Error::FarGet)? {
                builder.try_insert(directory.clone())?;
                yield directory;
            }

            let directory_graph = builder.build()?;

            // Drain into near
            let digest_near = near.put_directory_graph(directory_graph).await.map_err(Error::NearPut)?;
            if digest_near != digest {
                Err(Error::InsertingGraphMismatch {
                    digest_expected: digest,
                    digest_actual: digest_near,
                })?;
            }
        }
        .boxed()
    }

    #[instrument(skip_all)]
    fn put_multiple_start(&self) -> Box<dyn DirectoryPutter + '_> {
        Box::new(SimplePutter::new(self))
    }
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("wrong arguments: {0}")]
    WrongConfig(&'static str),
    #[error("serde-qs error: {0}")]
    SerdeQS(#[from] serde_qs::Error),

    #[error("getting from near: {0}")]
    NearGet(#[source] directoryservice::Error),
    #[error("putting into near: {0}")]
    NearPut(#[source] directoryservice::Error),
    #[error("getting from far: {0}")]
    FarGet(#[source] directoryservice::Error),
    #[error(
        "inserting closure with root {digest_expected} into near returned different digest ({digest_actual})"
    )]
    InsertingGraphMismatch {
        digest_expected: B3Digest,
        digest_actual: B3Digest,
    },

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
pub struct CacheConfig {
    near: String,
    far: String,
}

impl TryFrom<url::Url> for CacheConfig {
    type Error = Box<dyn std::error::Error + Send + Sync>;
    fn try_from(url: url::Url) -> Result<Self, Self::Error> {
        // cache doesn't support host or path in the URL.
        if url.has_authority() || !url.path().is_empty() {
            return Err(Error::WrongConfig("no authority or path allowed").into());
        }
        Ok(serde_qs::from_str(url.query().unwrap_or_default())?)
    }
}

#[async_trait]
impl ServiceBuilder for CacheConfig {
    type Output = dyn DirectoryService;
    async fn build<'a>(
        &'a self,
        instance_name: &str,
        context: &CompositionContext,
    ) -> Result<Arc<Self::Output>, Box<dyn std::error::Error + Send + Sync>> {
        let (near, far) = futures::join!(
            context.resolve::<Self::Output>(&self.near),
            context.resolve::<Self::Output>(&self.far)
        );
        Ok(Arc::new(Cache {
            instance_name: instance_name.to_string(),
            near: near?,
            far: far?,
        }))
    }
}
