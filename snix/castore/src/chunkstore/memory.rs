//! In-memory implementation, using a `HashMap`.

use hashbrown::HashMap;
use parking_lot::RwLock;
use std::sync::Arc;
use tonic::async_trait;
use tracing::instrument;

use crate::{
    B3Digest,
    chunkstore::{Chunk, ChunkStore},
    composition::{CompositionContext, ServiceBuilder},
};

/// In-memory [ChunkStore] implementation.
///
/// Uses a `HashMap<B3Digest, Chunk>` behind an `Arc<RwLock<_>>` to allow cloning and concurrent access.
#[derive(Default, Clone)]
pub struct MemoryChunkStore {
    instance_name: String,
    db: Arc<RwLock<HashMap<B3Digest, Chunk>>>,
}

impl MemoryChunkStore {
    /// Constructs a new in-memory [ChunkStore] with the given `instance_name`.
    pub fn new(instance_name: String) -> Self {
        Self {
            instance_name,
            db: Default::default(),
        }
    }
}

#[async_trait]
impl ChunkStore for MemoryChunkStore {
    #[instrument(skip_all, err, fields(chunk.digest=%digest, instance_name=%self.instance_name))]
    async fn get(&self, digest: &B3Digest) -> Result<Option<Chunk>, super::Error> {
        Ok(self.db.read().get(digest).cloned())
    }

    #[instrument(skip_all, err, fields(chunk.digest=%digest, instance_name=%self.instance_name))]
    async fn has(&self, digest: &B3Digest) -> Result<bool, super::Error> {
        Ok(self.db.read().contains_key(digest))
    }

    #[instrument(skip_all, err, fields(instance_name=%self.instance_name))]
    async fn put(&self, chunk: Chunk) -> Result<B3Digest, super::Error> {
        let digest: B3Digest = blake3::hash(chunk.as_ref()).into();
        self.db.write().insert(digest, chunk);
        Ok(digest)
    }
}

/// Config for [MemoryChunkStore].
#[derive(serde::Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct MemoryChunkStoreConfig {}

impl TryFrom<url::Url> for MemoryChunkStoreConfig {
    type Error = Box<dyn std::error::Error + Send + Sync>;
    fn try_from(url: url::Url) -> Result<Self, Self::Error> {
        // memory doesn't support authority or path in the URL.
        if url.has_authority() || !url.path().is_empty() {
            return Err("invalid url".into());
        }
        Ok(MemoryChunkStoreConfig {})
    }
}

#[async_trait]
impl ServiceBuilder for MemoryChunkStoreConfig {
    type Output = dyn ChunkStore;
    async fn build<'a>(
        &'a self,
        instance_name: &str,
        _context: &CompositionContext,
    ) -> Result<Arc<Self::Output>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Arc::new(MemoryChunkStore::new(instance_name.to_string())))
    }
}
