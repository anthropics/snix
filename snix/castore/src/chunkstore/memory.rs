use std::sync::Arc;

use hashbrown::HashMap;
use parking_lot::RwLock;
use tonic::async_trait;

use crate::{
    B3Digest,
    chunkstore::{Chunk, ChunkStore},
};

/// In-memory [ChunkStore] implementation.
#[derive(Default, Clone)]
pub struct MemoryChunkStore {
    db: Arc<RwLock<HashMap<B3Digest, Chunk>>>,
}

#[async_trait]
impl ChunkStore for MemoryChunkStore {
    async fn get(&self, digest: &B3Digest) -> Result<Option<Chunk>, super::Error> {
        Ok(self.db.read().get(digest).cloned())
    }
    async fn has(&self, digest: &B3Digest) -> Result<bool, super::Error> {
        Ok(self.db.read().contains_key(digest))
    }
    async fn put(&self, chunk: Chunk) -> Result<B3Digest, super::Error> {
        let digest: B3Digest = blake3::hash(chunk.as_ref()).into();
        self.db.write().insert(digest, chunk);
        Ok(digest)
    }
}
