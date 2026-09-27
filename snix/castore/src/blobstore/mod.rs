use auto_impl::auto_impl;
use tonic::async_trait;

mod blob_meta;

use crate::B3Digest;
pub use blob_meta::BlobMeta;

#[cfg_attr(test, mockall::automock)]
#[async_trait]
#[auto_impl(&, &mut, Arc, Box)]
pub trait BlobStore: Send + Sync {
    /// Retrieves a [BlobMeta] for a given blob digest.
    async fn get(&self, digest: &B3Digest) -> Result<Option<BlobMeta>, Error>;

    /// Checks if a [BlobMeta] for a given blob exists.
    async fn has(&self, digest: &B3Digest) -> Result<bool, Error>;

    /// Persists a [BlobMeta] for the given blob digest.
    async fn put(&self, digest: &B3Digest, blob_meta: BlobMeta) -> Result<(), Error>;
}

#[derive(thiserror::Error, Debug)]
#[error(transparent)]
pub struct Error(#[from] Box<dyn std::error::Error>);
