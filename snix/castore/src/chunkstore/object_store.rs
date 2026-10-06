//! Object-store backend, also supporting the local filesystem.

use super::Chunk;
use crate::{
    B3Digest,
    chunkstore::ChunkStore,
    composition::{CompositionContext, ServiceBuilder},
};
use data_encoding::HEXLOWER;
use object_store::{ObjectStoreExt, PutPayload, path::Path};
use std::collections::HashMap;
use std::{io::Cursor, sync::Arc};
use tokio::io;
use tonic::async_trait;
use tracing::{Level, instrument, warn};

/// Stores Chunks in any backend supported by the [object_store] crate.
///
/// Each [Chunk] is stored as a zstd-compressed object
/// at `{base_path}/chunk/b3/{HEXLOWER(digest[0..2])}/{HEXLOWER(digest)}`.
pub struct ObjectStoreChunkStore {
    instance_name: String,
    object_store: Arc<dyn object_store::ObjectStore>,
    base_path: Path,
}

impl ObjectStoreChunkStore {
    /// Constructs a new [ObjectStoreChunkStore] from already existing
    /// `instance_name`, `object_store` and `base_name`.
    pub fn from_parts(
        instance_name: String,
        object_store: Arc<dyn object_store::ObjectStore>,
        base_path: Path,
    ) -> Self {
        Self {
            instance_name,
            object_store,
            base_path,
        }
    }
}

#[instrument(level=Level::TRACE, skip_all,fields(base_path=%base_path,chunk.digest=%digest))]
fn derive_chunk_path(base_path: &Path, digest: &B3Digest) -> Path {
    base_path
        .clone()
        .join("chunk")
        .join("b3")
        .join(HEXLOWER.encode(&digest[..2]))
        .join(HEXLOWER.encode(&digest[..]))
}

#[async_trait]
impl ChunkStore for ObjectStoreChunkStore {
    #[instrument(skip_all, err, fields(chunk.digest=%digest, instance_name=%self.instance_name))]
    async fn has(&self, digest: &B3Digest) -> Result<bool, super::Error> {
        let p = derive_chunk_path(&self.base_path, digest);
        match self.object_store.head(&p).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(err) => Err(Error::from(err))?,
        }
    }

    #[instrument(skip_all, err, fields(chunk.digest=%digest, instance_name=%self.instance_name))]
    async fn get(&self, digest: &B3Digest) -> Result<Option<Chunk>, super::Error> {
        let p = derive_chunk_path(&self.base_path, digest);
        match self.object_store.get(&p).await {
            Ok(get_result) => {
                let compressed_bytes = get_result.bytes().await.map_err(|err| {
                    warn!(%err,"failed to read payload from object_store");
                    Error::ObjectStore(err)
                })?;
                // FUTUREWORK: use zstd::bulk to prevent decompression bombs
                let chunk = Chunk::from(
                    zstd::stream::decode_all(Cursor::new(compressed_bytes))
                        .map_err(Error::ChunkDecompress)?,
                );

                let actual_digest = chunk.digest();
                if &actual_digest != digest {
                    Err(Error::IncorrectDigest {
                        expected: digest.to_owned(),
                        actual: actual_digest,
                    })?
                }

                Ok(Some(chunk))
            }
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(err) => Err(super::Error(Box::new(err)))?,
        }
    }

    #[instrument(skip_all, err, ret(Display), fields(instance_name=%self.instance_name))]
    async fn put(&self, chunk: Chunk) -> Result<B3Digest, super::Error> {
        let digest: B3Digest = chunk.digest();
        let p = derive_chunk_path(&self.base_path, &digest);
        let compressed_chunk =
            zstd::stream::encode_all(Cursor::new(chunk.as_ref()), zstd::DEFAULT_COMPRESSION_LEVEL)
                .map_err(Error::ChunkCompress)?;

        self.object_store
            .put(&p, PutPayload::from_bytes(compressed_chunk.into()))
            .await
            .map_err(Error::ObjectStore)?;

        Ok(digest)
    }
}

/// Configuration for an [ObjectStoreChunkStore].
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectStoreChunkStoreConfig {
    object_store_url: url::Url,
    #[serde(default)]
    object_store_options: HashMap<String, String>,
}

impl TryFrom<url::Url> for ObjectStoreChunkStoreConfig {
    type Error = Box<dyn std::error::Error + Send + Sync>;
    fn try_from(url: url::Url) -> Result<Self, Self::Error> {
        Ok(ObjectStoreChunkStoreConfig {
            object_store_url: crate::object_store::trim_objectstore_prefix(&url)?,
            object_store_options: url
                .query_pairs()
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        })
    }
}

#[async_trait]
impl ServiceBuilder for ObjectStoreChunkStoreConfig {
    type Output = dyn ChunkStore;
    async fn build<'a>(
        &'a self,
        instance_name: &str,
        _context: &CompositionContext,
    ) -> Result<Arc<Self::Output>, Box<dyn std::error::Error + Send + Sync>> {
        let (object_store, path) = crate::object_store::setup_object_store(
            &self.object_store_url,
            &self.object_store_options,
        )
        .await?;

        Ok(Arc::new(ObjectStoreChunkStore::from_parts(
            instance_name.to_string(),
            Arc::new(object_store),
            path,
        )))
    }
}

/// Error returned from [ObjectStoreChunkStore].
#[derive(thiserror::Error, Debug)]
enum Error {
    #[error("reading bytes from object_store")]
    ObjectStore(#[from] object_store::Error),
    #[error("decompressing zstd chunk")]
    ChunkDecompress(io::Error),
    #[error("compressing zstd chunk")]
    ChunkCompress(io::Error),
    #[error("unexpected digest, expected: {expected}, got: {actual}")]
    IncorrectDigest {
        expected: B3Digest,
        actual: B3Digest,
    },
}

impl From<Error> for super::Error {
    fn from(value: Error) -> Self {
        super::Error(Box::new(value))
    }
}
