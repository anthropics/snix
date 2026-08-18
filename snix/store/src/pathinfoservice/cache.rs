use std::sync::Arc;

use futures::stream::BoxStream;
use nix_compat::nixbase32;
use snix_castore::composition::{CompositionContext, ServiceBuilder};
use tonic::async_trait;
use tracing::{debug, instrument, warn};

use crate::pathinfoservice;

use super::{PathInfo, PathInfoService};

/// Asks near first, if not found, asks far.
/// If found in there, returns it, and *inserts* it into
/// near.
/// There is no negative cache.
/// Inserts and listings are not implemented for now.
pub struct Cache<PS1, PS2> {
    instance_name: String,
    near: PS1,
    far: PS2,
}

impl<PS1, PS2> Cache<PS1, PS2> {
    pub fn new(instance_name: String, near: PS1, far: PS2) -> Self {
        Self {
            instance_name,
            near,
            far,
        }
    }
}

#[async_trait]
impl<PS1, PS2> PathInfoService for Cache<PS1, PS2>
where
    PS1: PathInfoService,
    PS2: PathInfoService,
{
    #[instrument(level = "trace", skip_all, fields(path_info.digest = nixbase32::encode(&digest), instance_name = %self.instance_name))]
    async fn get(&self, digest: [u8; 20]) -> Result<Option<PathInfo>, pathinfoservice::Error> {
        match self.near.get(digest).await.map_err(Error::NearGet)? {
            Some(path_info) => {
                debug!("serving from cache");
                Ok(Some(path_info))
            }
            None => {
                debug!("not found in near, asking remote…");
                match self.far.get(digest).await.map_err(Error::FarGet)? {
                    None => Ok(None),
                    Some(path_info) => {
                        debug!("found in remote, adding to cache");
                        self.near
                            .put(path_info.clone())
                            .await
                            .map_err(Error::NearPut)?;
                        Ok(Some(path_info))
                    }
                }
            }
        }
    }

    #[instrument(level = "trace", skip_all, fields(path_info.digest = nixbase32::encode(&digest), instance_name = %self.instance_name))]
    async fn has(&self, digest: [u8; 20]) -> Result<bool, pathinfoservice::Error> {
        // FUTUREWORK: queue background tasks if ! self.near.has && self.far.has ? (configurable)
        Ok(self.near.has(digest).await.map_err(Error::NearGet)?
            || self.far.has(digest).await.map_err(Error::FarGet)?)
    }

    #[instrument(level = "trace", skip_all, err, fields(path_info.digest = nixbase32::encode(_path_info.store_path.digest()), instance_name = %self.instance_name))]
    async fn put(&self, _path_info: PathInfo) -> Result<PathInfo, pathinfoservice::Error> {
        Err(Error::Unsupported)?
    }

    fn list(&self) -> BoxStream<'static, Result<PathInfo, pathinfoservice::Error>> {
        warn!("listing unsupported");
        Box::pin(tokio_stream::once(Err(Error::Unsupported.into())))
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheConfig {
    pub near: String,
    pub far: String,
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("instantiating from a url is not supported")]
    URLNotSupported,

    #[error("getting from near: {0}")]
    NearGet(#[source] pathinfoservice::Error),
    #[error("putting into near: {0}")]
    NearPut(#[source] pathinfoservice::Error),
    #[error("getting from far: {0}")]
    FarGet(#[source] pathinfoservice::Error),

    #[error("unsupported")]
    Unsupported,
}

impl TryFrom<url::Url> for CacheConfig {
    type Error = Box<dyn std::error::Error + Send + Sync>;
    fn try_from(_url: url::Url) -> Result<Self, Self::Error> {
        Err(Error::URLNotSupported)?
    }
}

#[async_trait]
impl ServiceBuilder for CacheConfig {
    type Output = dyn PathInfoService;
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

#[cfg(test)]
mod test {
    use std::num::NonZeroUsize;

    use crate::{
        fixtures::PATH_INFO,
        pathinfoservice::{LruPathInfoService, PathInfoService},
        utils::gen_test_pathinfo_service,
    };

    /// Helper function setting up an instance of a Cache PathInfoService.
    async fn create_pathinfoservice() -> super::Cache<LruPathInfoService, impl PathInfoService> {
        // Create an instance of a "far" PathInfoService.
        let far = gen_test_pathinfo_service();

        // … and an instance of a "near" PathInfoService.
        let near = LruPathInfoService::with_capacity("near".into(), NonZeroUsize::new(1).unwrap());

        // create a Pathinfoservice combining the two and return it.
        super::Cache::new("root".into(), near, far)
    }

    /// Getting from the far backend is gonna insert it into the near one.
    #[tokio::test]
    async fn test_populate_cache() {
        let svc = create_pathinfoservice().await;

        // query the PathInfo, things should not be there.
        assert!(
            svc.get(*PATH_INFO.store_path.digest())
                .await
                .unwrap()
                .is_none()
        );

        // insert it into the far one.
        svc.far.put(PATH_INFO.clone()).await.unwrap();

        // now try getting it again, it should succeed.
        assert_eq!(
            Some(PATH_INFO.clone()),
            svc.get(*PATH_INFO.store_path.digest()).await.unwrap()
        );

        // peek near, it should now be there.
        assert_eq!(
            Some(PATH_INFO.clone()),
            svc.near.get(*PATH_INFO.store_path.digest()).await.unwrap()
        );
    }
}
