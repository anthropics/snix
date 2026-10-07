use std::sync::Arc;

use futures::{StreamExt, stream::BoxStream};
use nix_compat::nixbase32;
use snix_castore::composition::{CompositionContext, ServiceBuilder};
use tonic::async_trait;
use tracing::instrument;

use crate::{
    nar::NarCalculationService,
    path_info::PathInfo,
    pathinfoservice::{self, PathInfoService},
};

/// Tries requests on multiple stores sequentially, returning the first positive or erroneous answer.
///
/// A negative response is returned if all backends return them.
/// Write requests are not implemented.
pub struct Priority<PS> {
    instance_name: String,
    // NOTE: Arc<dyn PS> implements PS too, so you can put different service types in here.
    services: Vec<PS>,
}

impl<DS> Priority<DS> {
    /// Construct from an iterator of services.
    pub fn new<I: IntoIterator<Item = DS>>(instance_name: String, iter: I) -> Priority<DS> {
        Self {
            instance_name,
            services: Vec::from_iter(iter),
        }
    }
}

#[async_trait]
impl<PS> PathInfoService for Priority<PS>
where
    PS: PathInfoService,
{
    #[instrument(skip_all, err, fields(path_info.digest = nixbase32::encode(&digest), instance_name = %self.instance_name))]
    async fn get(&self, digest: [u8; 20]) -> Result<Option<PathInfo>, pathinfoservice::Error> {
        // traverse the list of services. If any service has it, return from there.
        // Errors cause the combinator to bail out early.
        for (idx, service) in self.services.iter().enumerate() {
            if let Some(directory) = service
                .get(digest)
                .await
                .map_err(|err| Error::Backend(idx, err))?
            {
                return Ok(Some(directory));
            }
        }

        Ok(None)
    }

    #[instrument(skip_all, err, fields(path_info.digest = nixbase32::encode(&digest), instance_name = %self.instance_name))]
    async fn has(&self, digest: [u8; 20]) -> Result<bool, pathinfoservice::Error> {
        // traverse the list of services. If any service has it, return true.
        // Errors cause the combinator to bail out early.
        for (idx, service) in self.services.iter().enumerate() {
            if service
                .has(digest)
                .await
                .map_err(|err| Error::Backend(idx, err))?
            {
                return Ok(true);
            }
        }

        Ok(false)
    }

    async fn put(&self, _path_info: PathInfo) -> Result<PathInfo, pathinfoservice::Error> {
        return Err(Error::Unimplemented.into());
    }

    fn list(&self) -> BoxStream<'static, Result<PathInfo, pathinfoservice::Error>> {
        futures::stream::once(async { Err(Box::new(Error::Unimplemented))? }).boxed()
    }

    fn nar_calculation_service(&self) -> Option<Arc<dyn NarCalculationService>> {
        // We can't possibly know which one has all contents to calculate.
        None
    }
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("wrong arguments: {0}")]
    WrongConfig(&'static str),

    #[error("error from service with index {0}")]
    Backend(usize, #[source] pathinfoservice::Error),

    #[error("unimplemented")]
    Unimplemented,
}

#[derive(serde::Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct PriorityConfig {
    services: Vec<String>,
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
    type Output = dyn PathInfoService;
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

        Ok(Arc::new(Priority::new(instance_name.to_string(), services)))
    }
}
