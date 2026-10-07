//! In-memory implementation, using a `HashMap`.

use hashbrown::HashMap;
use parking_lot::RwLock;
use std::sync::Arc;
use tonic::async_trait;
use tracing::instrument;

use crate::{
    B3Digest,
    blobstore::{BlobMeta, BlobStore},
    composition::{CompositionContext, ServiceBuilder},
};

/// In-memory [BlobStore] implementation.
///
/// Uses a `HashMap<B3Digest, BlobMeta>` behind an `Arc<RwLock<_>>` to allow cloning and concurrent access.
#[derive(Default, Clone)]
pub struct MemoryBlobStore {
    instance_name: String,
    db: Arc<RwLock<HashMap<B3Digest, BlobMeta>>>,
}

#[async_trait]
impl BlobStore for MemoryBlobStore {
    #[instrument(skip_all, err, fields(blob.digest=%digest, instance_name=%self.instance_name))]
    async fn get(&self, digest: &B3Digest) -> Result<Option<BlobMeta>, super::Error> {
        Ok(self.db.read().get(digest).cloned())
    }

    #[instrument(skip_all, err, fields(blob.digest=%digest, instance_name=%self.instance_name))]
    async fn has(&self, digest: &B3Digest) -> Result<bool, super::Error> {
        Ok(self.db.read().contains_key(digest))
    }
    #[instrument(skip_all, err, fields(blob.digest=%digest, instance_name=%self.instance_name))]
    async fn put(&self, digest: &B3Digest, blob_meta: BlobMeta) -> Result<(), super::Error> {
        match self.db.write().entry(*digest) {
            hashbrown::hash_map::Entry::Occupied(_) => {}
            hashbrown::hash_map::Entry::Vacant(vacant_entry) => {
                vacant_entry.insert_entry(blob_meta);
            }
        }

        Ok(())
    }
}

/// Config for [MemoryBlobStore].
#[derive(serde::Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct MemoryBlobStoreConfig {}

impl TryFrom<url::Url> for MemoryBlobStoreConfig {
    type Error = Box<dyn std::error::Error + Send + Sync>;
    fn try_from(url: url::Url) -> Result<Self, Self::Error> {
        // memory doesn't support authority or path in the URL.
        if url.has_authority() || !url.path().is_empty() {
            return Err("invalid url".into());
        }
        Ok(MemoryBlobStoreConfig {})
    }
}

#[async_trait]
impl ServiceBuilder for MemoryBlobStoreConfig {
    type Output = dyn BlobStore;
    async fn build<'a>(
        &'a self,
        instance_name: &str,
        _context: &CompositionContext,
    ) -> Result<Arc<Self::Output>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Arc::new(MemoryBlobStore {
            instance_name: instance_name.to_string(),
            db: Default::default(),
        }))
    }
}
