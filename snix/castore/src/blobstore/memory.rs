//! In-memory implementation, using a `HashMap`.

use hashbrown::HashMap;
use parking_lot::RwLock;
use std::sync::Arc;
use tonic::async_trait;
use tracing::instrument;

use crate::{
    B3Digest,
    blobstore::{BlobMeta, BlobStore},
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
