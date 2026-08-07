mod cache;
mod from_addr;
mod grpc;
mod lru;
mod nix_http;
mod redb;
mod signing_wrapper;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use auto_impl::auto_impl;
use futures::stream::BoxStream;
use snix_castore::composition::{Registry, ServiceBuilder};
use tonic::async_trait;

use crate::nar::NarCalculationService;
pub use crate::path_info::PathInfo;

pub use self::cache::{Cache as CachePathInfoService, CacheConfig as CachePathInfoServiceConfig};
pub use self::from_addr::from_addr;
pub use self::grpc::{GRPCPathInfoService, GRPCPathInfoServiceConfig};
pub use self::lru::{LruPathInfoService, LruPathInfoServiceConfig};
pub use self::nix_http::{NixHTTPPathInfoService, NixHTTPPathInfoServiceConfig};
pub use self::redb::{RedbPathInfoService, RedbPathInfoServiceConfig};
pub use self::signing_wrapper::{KeyFileSigningPathInfoServiceConfig, SigningPathInfoService};

#[cfg(feature = "cloud")]
mod bigtable;
#[cfg(feature = "cloud")]
pub use self::bigtable::{BigtableParameters, BigtablePathInfoService};

#[cfg(feature = "fs")]
mod fs;
#[cfg(feature = "fs")]
pub use self::fs::RootNodesWrapper;

pub type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

/// The base trait all PathInfo services need to implement.
#[cfg_attr(any(test, feature = "mocks"), mockall::automock)]
#[async_trait]
#[auto_impl(&, &mut, Arc, Box)]
pub trait PathInfoService: Send + Sync {
    /// Retrieve a PathInfo by the output digest.
    async fn get(&self, digest: [u8; 20]) -> Result<Option<PathInfo>, Error>;

    /// Check if a PathInfo exists.
    /// Has a naïve default impl, but store implementations may decide to
    /// implement their own.
    async fn has(&self, digest: [u8; 20]) -> Result<bool, Error> {
        Ok(self.get(digest).await?.is_some())
    }

    /// Store a PathInfo.
    async fn put(&self, path_info: PathInfo) -> Result<PathInfo, Error>;

    /// Iterate over all PathInfo objects in the store.
    /// Implementations can decide to disallow listing.
    ///
    /// This returns a pinned, boxed stream. The pinning allows for it to be polled easily,
    /// and the box allows different underlying stream implementations to be returned since
    /// Rust doesn't support this as a generic in traits yet. This is the same thing that
    /// [async_trait] generates, but for streams instead of futures.
    ///
    /// Even though this function is not async, underlying implementations are
    /// assumed to be nonblocking on IO, so they MUST use spawn_blocking when
    /// doing IO.
    /// Implementations can assume to be invoked in the context of a tokio runtime.
    fn list(&self) -> BoxStream<'static, Result<PathInfo, Error>>;

    /// Returns a (more) suitable NarCalculationService.
    /// This can be used to offload NAR calculation to the remote side.
    fn nar_calculation_service(&self) -> Option<Arc<dyn NarCalculationService>> {
        None
    }
}

/// Registers the builtin PathInfoService implementations with the registry
pub(crate) fn register_pathinfo_services(reg: &mut Registry) {
    reg.register::<Box<dyn ServiceBuilder<Output = dyn PathInfoService>>, CachePathInfoServiceConfig>("cache");
    reg.register::<Box<dyn ServiceBuilder<Output = dyn PathInfoService>>, GRPCPathInfoServiceConfig>("grpc");
    reg.register::<Box<dyn ServiceBuilder<Output = dyn PathInfoService>>, KeyFileSigningPathInfoServiceConfig>("keyfile-signing");
    reg.register::<Box<dyn ServiceBuilder<Output = dyn PathInfoService>>, LruPathInfoServiceConfig>("lru");
    reg.register::<Box<dyn ServiceBuilder<Output = dyn PathInfoService>>, NixHTTPPathInfoServiceConfig>("nix");
    reg.register::<Box<dyn ServiceBuilder<Output = dyn PathInfoService>>, RedbPathInfoServiceConfig>("redb");
    #[cfg(feature = "cloud")]
    {
        reg.register::<Box<dyn ServiceBuilder<Output = dyn PathInfoService>>, BigtableParameters>(
            "bigtable",
        );
    }
}
