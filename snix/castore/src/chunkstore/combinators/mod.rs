//! Combinators that compose multiple [ChunkStore] implementations.
//!
//! [ChunkStore]: super::ChunkStore

mod cache;

pub use cache::{Cache, CacheConfig};
