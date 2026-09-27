use auto_impl::auto_impl;
use tonic::async_trait;

use crate::B3Digest;

mod chunk;
pub use chunk::Chunk;

#[cfg_attr(test, mockall::automock)]
#[async_trait]
#[auto_impl(&, &mut, Arc, Box)]
pub trait ChunkStore: Send + Sync {
    async fn get(&self, digest: &B3Digest) -> Result<Option<Chunk>, Error>;
    async fn has(&self, digest: &B3Digest) -> Result<bool, Error>;
    async fn put(&self, chunk: Chunk) -> Result<B3Digest, Error>;
}

#[derive(thiserror::Error, Debug)]
#[error(transparent)]
pub struct Error(Box<dyn std::error::Error + Send + Sync + 'static>);
