use super::Chunk;
use crate::{B3Digest, chunkstore::ChunkStore};
use data_encoding::HEXLOWER;
use object_store::{ObjectStoreExt, PutPayload, path::Path};
use std::{io::Cursor, sync::Arc};
use tokio::io;
use tonic::async_trait;
use tracing::{Level, instrument, warn};

pub struct ObjectStoreChunkStore {
    instance_name: String,
    object_store: Arc<dyn object_store::ObjectStore>,
    base_path: Path,
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
                let chunk_contents = zstd::stream::decode_all(Cursor::new(compressed_bytes))
                    .map_err(Error::ChunkDecompress)?;

                let actual_digest = blake3::hash(&chunk_contents);
                if actual_digest.as_bytes() != digest.as_ref() {
                    Err(Error::IncorrectDigest {
                        expected: digest.to_owned(),
                        actual: actual_digest.into(),
                    })?
                }

                Ok(Some(chunk_contents.into()))
            }
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(err) => Err(super::Error(Box::new(err)))?,
        }
    }

    #[instrument(skip_all, err, ret(Display), fields(instance_name=%self.instance_name))]
    async fn put(&self, chunk: Chunk) -> Result<B3Digest, super::Error> {
        let digest: B3Digest = blake3::hash(chunk.as_ref()).into();
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
