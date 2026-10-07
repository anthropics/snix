use std::sync::Arc;

use tonic::async_trait;
use tracing::{instrument, trace};

use crate::{
    B3Digest,
    blobstore::{self, BlobMeta, BlobStore},
    composition::{CompositionContext, ServiceBuilder},
};

/// Asks near first, if not found, asks far.
/// If found in there, returns it, and *inserts* it into
/// near.
/// There is no negative cache.
pub struct Cache<CN, CF> {
    instance_name: String,
    near: CN,
    far: CF,
}

impl<BN, BF> Cache<BN, BF> {
    /// Constructs a new [Cache] from the passed `instance_name`, `near` and `far` [BlobStore]s.
    pub fn new(instance_name: String, near: BN, far: BF) -> Self {
        Self {
            instance_name,
            near,
            far,
        }
    }
}

#[async_trait]
impl<BN, BF> BlobStore for Cache<BN, BF>
where
    BN: BlobStore,
    BF: BlobStore,
{
    #[instrument(skip(self, digest), fields(blob.digest=%digest, instance_name=%self.instance_name))]
    async fn get(&self, digest: &B3Digest) -> Result<Option<BlobMeta>, blobstore::Error> {
        if let Some(blob) = self.near.get(digest).await.map_err(Error::NearGet)? {
            trace!("serving from cache");
            return Ok(Some(blob));
        }

        trace!("not found in near, asking remote…");

        match self.far.get(digest).await.map_err(Error::FarGet)? {
            // blob_meta doesn't exist on the far side either, nothing we can do.
            None => Ok(None),
            Some(blob_meta) => {
                // blob_meta is present on the far side, insert it into near before returning.
                self.near
                    .put(digest, blob_meta.clone())
                    .await
                    .map_err(Error::NearPut)?;

                Ok(Some(blob_meta))
            }
        }
    }

    #[instrument(skip_all, fields(Blob.digest=%digest, instance_name=%self.instance_name))]
    async fn has(&self, digest: &B3Digest) -> Result<bool, blobstore::Error> {
        Ok(self.near.has(digest).await.map_err(Error::NearGet)?
            || self.far.has(digest).await.map_err(Error::FarGet)?)
    }

    /// Persists a [BlobMeta] for the given blob digest.
    #[instrument(skip_all, fields(instance_name=%self.instance_name))]
    async fn put(&self, digest: &B3Digest, blob_meta: BlobMeta) -> Result<(), blobstore::Error> {
        Ok(self
            .near
            .put(digest, blob_meta)
            .await
            .map_err(Error::NearPut)?)
    }
}

/// Error returned from [Cache].
#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("wrong arguments: {0}")]
    WrongConfig(&'static str),

    #[error("getting from near: {0}")]
    NearGet(#[source] blobstore::Error),
    #[error("putting into near: {0}")]
    NearPut(#[source] blobstore::Error),
    #[error("getting from far: {0}")]
    FarGet(#[source] blobstore::Error),
}

impl From<Error> for blobstore::Error {
    fn from(value: Error) -> Self {
        Self(Box::new(value))
    }
}

/// Configuration for a [Cache] [BlobStore].
#[derive(serde::Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct CacheConfig {
    near: String,
    far: String,
}

impl TryFrom<url::Url> for CacheConfig {
    type Error = Box<dyn std::error::Error + Send + Sync>;
    fn try_from(url: url::Url) -> Result<Self, Self::Error> {
        // cache doesn't support host or path in the URL.
        if url.has_authority() || !url.path().is_empty() {
            return Err(Error::WrongConfig("no authority or path allowed").into());
        }
        Ok(serde_qs::from_str(url.query().unwrap_or_default())?)
    }
}

#[async_trait]
impl ServiceBuilder for CacheConfig {
    type Output = dyn BlobStore;
    async fn build<'a>(
        &'a self,
        instance_name: &str,
        context: &CompositionContext,
    ) -> Result<Arc<Self::Output>, Box<dyn std::error::Error + Send + Sync>> {
        let (near, far) = futures::join!(
            context.resolve::<Self::Output>(&self.near),
            context.resolve::<Self::Output>(&self.far)
        );
        Ok(Arc::new(Cache {
            instance_name: instance_name.to_string(),
            near: near?,
            far: far?,
        }))
    }
}

#[cfg(test)]
mod test {
    use std::sync::LazyLock;

    use crate::{
        blobstore::{BlobMeta, BlobStore, MockBlobStore, memory::MemoryBlobStore},
        chunkstore::Chunk,
    };

    use super::Cache;

    const CHUNK: Chunk = Chunk::from_static(b"foo");
    static BLOB_META: LazyLock<BlobMeta> =
        LazyLock::new(|| BlobMeta::from_digests_and_sizes([(CHUNK.digest(), CHUNK.len() as u64)]));

    /// A BlobMeta present in near is served from there, far is not queried.
    #[tokio::test]
    async fn get_near_only() {
        let near = MemoryBlobStore::default();
        near.put(&CHUNK.digest(), BLOB_META.clone())
            .await
            .expect("to succeed");
        let mut far = MockBlobStore::new();
        far.expect_get().never();

        let uut = Cache::new("uut".to_string(), near, far);

        assert_eq!(
            Some(BLOB_META.clone()),
            uut.get(&CHUNK.digest()).await.expect("to succeed")
        );
    }

    /// A BlobMeta only present in far is returned, and inserted into near.
    #[tokio::test]
    async fn get_populates_near() {
        let near = MemoryBlobStore::default();
        let far_with_chunk = MemoryBlobStore::default();
        far_with_chunk
            .put(&CHUNK.digest(), BLOB_META.clone())
            .await
            .expect("to succeed");

        let uut = Cache::new("uut".to_string(), &near, far_with_chunk);

        // we know near is empty.
        assert!(
            !near.has(&CHUNK.digest()).await.expect("to succeed"),
            "near should be empty"
        );

        // query uut, which will populate near
        uut.get(&CHUNK.digest()).await.expect("to succeed");

        // now near should have the chunk.
        assert!(
            near.has(&CHUNK.digest()).await.expect("to succeed"),
            "near should be populated now"
        );
    }

    /// If neither has it, None is returned.
    #[tokio::test]
    async fn get_not_found() {
        let near = MemoryBlobStore::default();
        let far = MemoryBlobStore::default();

        let uut = Cache::new("uut".to_string(), near, far);

        assert_eq!(None, uut.get(&CHUNK.digest()).await.expect("to succeed"));
    }

    /// has() returns true if either near or far has the chunk.
    #[tokio::test]
    async fn has_near_or_far() {
        let cs_without = MemoryBlobStore::default();
        let cs_with = MemoryBlobStore::default();
        cs_with
            .put(&CHUNK.digest(), BLOB_META.clone())
            .await
            .expect("to succeed");

        let uut = Cache::new("uut".to_string(), &cs_without, &cs_with);
        assert!(uut.has(&CHUNK.digest()).await.expect("to succeed"));
        let uut = Cache::new("uut".to_string(), &cs_with, &cs_without);
        assert!(uut.has(&CHUNK.digest()).await.expect("to succeed"));
    }

    /// put() writes to near only.
    #[tokio::test]
    async fn put_writes_to_near() {
        let near = MemoryBlobStore::default();
        let far = MemoryBlobStore::default();

        let uut = Cache::new("uut".to_string(), near.clone(), far.clone());

        uut.put(&CHUNK.digest(), BLOB_META.clone())
            .await
            .expect("to succeed");

        assert!(near.has(&CHUNK.digest()).await.expect("to succeed"));
        assert!(!far.has(&CHUNK.digest()).await.expect("to succeed"));
    }
}
