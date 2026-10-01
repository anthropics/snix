#![deny(missing_docs)]
//! [Chunk] storage.

use auto_impl::auto_impl;
use tonic::async_trait;

pub mod memory;
pub mod object_store;

use crate::B3Digest;

mod chunk;
pub use chunk::Chunk;

/// The chunk with zero bytes of payload.
pub const EMPTY_CHUNK: Chunk = Chunk::from_static(&[]);

/// Allows reading (or uploading) content-addressed chunks of raw data.
///
/// BLAKE3 is used as a hashing function for the data. Uploading a blob will
/// return the BLAKE3 digest of it, and that's the identifier used to Read/Stat
/// them too.
///
/// Chunks are considered small enough to not require any further chunking.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
#[auto_impl(&, &mut, Arc, Box)]
pub trait ChunkStore: Send + Sync {
    /// Retrieves a [Chunk] by its [B3Digest], if it exists.
    async fn get(&self, digest: &B3Digest) -> Result<Option<Chunk>, Error>;
    /// Check if a Chunk with this [B3Digest] exists.
    async fn has(&self, digest: &B3Digest) -> Result<bool, Error>;
    /// Upload a Chunk, returns its [B3Digest].
    async fn put(&self, chunk: Chunk) -> Result<B3Digest, Error>;
}

/// Error returned from all ChunkStores.
///
/// Usually boxes an inner, more backend-specific error.
#[derive(thiserror::Error, Debug)]
#[error(transparent)]
pub struct Error(Box<dyn std::error::Error + Send + Sync + 'static>);
