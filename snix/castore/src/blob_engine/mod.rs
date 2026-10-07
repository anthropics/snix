use auto_impl::auto_impl;
use tonic::async_trait;

mod blobservice;
mod chunking;
pub mod concurrent_uploads;

pub use blobservice::BlobServiceEngine;
pub use chunking::ChunkingBlobEngine;

use crate::B3Digest;

/// Trait describing an interface to open blobs for reading and writing.
#[cfg_attr(any(test, feature = "mocks"), mockall::automock)]
#[async_trait]
#[auto_impl(&, &mut, Arc, Box)]
pub trait BlobEngine: Send + Sync {
    /// Check if the blob with this digest exists.
    async fn has(&self, digest: &B3Digest) -> Result<bool, Error>;

    /// Request a blob from the store for reading.
    /// An optional size_hint can be specified, allowing some implementations to optimize.
    async fn open_read(
        &self,
        digest: &B3Digest,
        size_hint: Option<u64>,
    ) -> Result<Option<Box<dyn crate::blobservice::BlobReader + 'static>>, Error>;

    /// Allows writing a new blob.
    /// Returns a [crate::blobservice::BlobWriter], which
    /// implements [tokio::io::AsyncWrite] and has a `close` method to finalize
    /// the blob and get its digest.
    async fn open_write<'a>(&'a self) -> Box<dyn crate::blobservice::BlobWriter + 'a>;
}

#[derive(thiserror::Error, Debug)]
#[error(transparent)]
pub struct Error(#[from] Box<dyn std::error::Error + Send + Sync + 'static>);
