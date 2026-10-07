#![deny(missing_docs)]
//! [BlobMeta] storage, used to assemble blobs from more granular Chunks.
use auto_impl::auto_impl;
use tonic::async_trait;

mod blob_meta;
mod blob_reader;
mod blob_writer;
pub mod grpc;
pub mod memory;
pub mod object_store;

use crate::{
    B3Digest,
    composition::{Registry, ServiceBuilder},
};
pub use blob_meta::BlobMeta;
pub use blob_reader::BlobReader;
pub use blob_writer::BlobWriter;

/// Retrieves and stores [BlobMeta] about Blobs.
/// Blobs can consist of multiple Chunks.
/// Blobs are keyed by the BLAKE3 digest of their entire data.
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

/// Error returned from all BlobStores.
///
/// Usually boxes an inner, more backend-specific error.
#[derive(thiserror::Error, Debug)]
#[error(transparent)]
pub struct Error(#[from] Box<dyn std::error::Error + Send + Sync + 'static>);

/// Registers the builtin [BlobStore]s with the registry.
pub(crate) fn register_blob_stores(reg: &mut Registry) {
    reg.register::<Box<dyn ServiceBuilder<Output = dyn BlobStore>>, grpc::GRPCBlobStoreConfig>(
        "grpc",
    );
    reg.register::<Box<dyn ServiceBuilder<Output = dyn BlobStore>>, memory::MemoryBlobStoreConfig>(
        "memory",
    );
    reg.register::<Box<dyn ServiceBuilder<Output = dyn BlobStore>>, object_store::ObjectStoreBlobStoreConfig>("objectstore");
}
