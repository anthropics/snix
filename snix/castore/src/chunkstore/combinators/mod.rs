//! Combinators that compose multiple [ChunkStore] implementations.
//!
//! [ChunkStore]: super::ChunkStore

mod cache;
mod priority;
mod race;

pub use cache::{Cache, CacheConfig};
pub use priority::{Priority, PriorityConfig};
pub use race::{Race, RaceConfig};
