//! Combinators that compose multiple [BlobStore] implementations.
//!
//! [BlobStore]: super::BlobStore

mod cache;

pub use cache::{Cache, CacheConfig};
