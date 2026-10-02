//! Object-store backend, also supporting the local filesystem.

use std::sync::Arc;

use data_encoding::HEXLOWER;
use object_store::{ObjectStoreExt, PutPayload, path::Path};
use prost::Message;
use tonic::async_trait;
use tracing::{Level, instrument};

use crate::{
    B3Digest,
    blobstore::{BlobMeta, BlobStore},
    chunkstore::{ChunkStore, object_store::ObjectStoreChunkStore},
    proto,
};

/// Stores BlobMeta in any backend supported by the [object_store] crate.
///
/// Each [BlobMeta] is stored as a [proto::BlobMeta] at
/// `{base_path}/blob/b3/{HEXLOWER(digest[0..2])}/{HEXLOWER(digest)}`.
///
/// If old BlobMeta are present, it might also access
/// `{base_path}/chunk/b3/{HEXLOWER(digest[0..2])}/{HEXLOWER(digest)}`.
pub struct ObjectStoreBlobStore {
    instance_name: String,
    object_store: Arc<dyn object_store::ObjectStore>,
    base_path: Path,
}

#[instrument(level=Level::TRACE, skip_all,fields(base_path=%base_path,blob.digest=%digest),ret(Display))]
fn derive_blob_path(base_path: &Path, digest: &B3Digest) -> Path {
    base_path
        .clone()
        .join("blobs")
        .join("b3")
        .join(HEXLOWER.encode(&digest[..2]))
        .join(HEXLOWER.encode(&digest[..]))
}

#[async_trait]
impl BlobStore for ObjectStoreBlobStore {
    #[instrument(skip_all, err, fields(blob.digest=%digest, instance_name=%self.instance_name))]
    async fn get(&self, digest: &B3Digest) -> Result<Option<BlobMeta>, super::Error> {
        if digest == &B3Digest::EMPTY {
            return Ok(Some(BlobMeta::from_digests_and_sizes([])));
        }

        let p = derive_blob_path(&self.base_path, digest);
        match self.object_store.get(&p).await {
            Ok(get_result) => {
                let bytes = get_result
                    .bytes()
                    .await
                    .map_err(|err| Error::object_store("reading payload", err))?;

                // parse proto
                let blob_meta_proto =
                    proto::BlobMeta::decode(bytes).map_err(Error::DecodeBlobMetaProto)?;

                let blob_meta = match BlobMeta::try_from(blob_meta_proto) {
                    Ok(blob_meta) => blob_meta,
                    Err(super::blob_meta::DecodeError::NoChunks) => {
                        // synthesize BlobMeta for single-chunk blob
                        self.gen_single_chunk_blob_meta(digest).await?

                        // FUTUREWORK: write fixed BlobMeta, configurable?
                    }
                    Err(err) => Err(Error::DecodeBlobMeta(err))?,
                };

                Ok(Some(blob_meta))
            }
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(err) => Err(Error::object_store("getting blob", err))?,
        }
    }

    #[instrument(skip_all, err, fields(blob.digest=%digest, instance_name=%self.instance_name))]
    async fn has(&self, digest: &B3Digest) -> Result<bool, super::Error> {
        let p = derive_blob_path(&self.base_path, digest);
        match self.object_store.head(&p).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(err) => Err(Error::object_store("heading blob", err))?,
        }
    }

    #[instrument(skip_all, err, fields(blob.digest=%digest, instance_name=%self.instance_name))]
    async fn put(&self, digest: &B3Digest, blob_meta: BlobMeta) -> Result<(), super::Error> {
        let p = derive_blob_path(&self.base_path, digest);

        let serialized = proto::BlobMeta::from(blob_meta).encode_to_vec();
        self.object_store
            .put(&p, PutPayload::from_bytes(serialized.into()))
            .await
            .map_err(|err| Error::object_store("putting blob", err))?;

        Ok(())
    }
}

impl ObjectStoreBlobStore {
    /// Create [BlobMeta] for a single chunk.
    ///
    /// In an earlier format, when [BlobMeta] described a single chunk,
    /// the version serialized to disk was an empty file.
    /// As we cannot immediately return a [BlobMeta] from that,
    /// as we don't know its size.
    ///
    /// This fetches the chunk, assuming an ObjectStoreChunkStore at the same
    /// backend and base path, and returns a [BlobMeta].
    ///
    /// If the chunk cannot be fetched from the chunk store, it returns None.
    async fn gen_single_chunk_blob_meta(&self, digest: &B3Digest) -> Result<BlobMeta, Error> {
        // retrieve the single chunk, by creating a [ObjectStoreChunkStore] on the fly.
        let chunk_store = ObjectStoreChunkStore::from_parts(
            format!("{}-chunkstore", self.instance_name),
            self.object_store.clone(),
            self.base_path.clone(),
        );

        let chunk = chunk_store
            .get(digest)
            .await
            .map_err(Error::SingleChunkGetFailure)?
            .ok_or(Error::SingleChunkNotFound)?;

        Ok(BlobMeta::from_digests_and_sizes([(
            *digest,
            chunk.len() as u64,
        )]))
    }
}

/// Error returned from [ObjectStoreChunkStore].
#[derive(thiserror::Error, Debug)]
enum Error {
    #[error("decoding BlobMeta proto: {0}")]
    DecodeBlobMetaProto(#[from] prost::DecodeError),

    #[error("decoding BlobMeta: {0}")]
    DecodeBlobMeta(#[from] crate::blobstore::blob_meta::DecodeError),

    #[error("{0} in object_store: {1})")]
    ObjectStore(&'static str, object_store::Error),

    #[error("unable to fetch single chunk: {0}")]
    SingleChunkGetFailure(crate::chunkstore::Error),

    #[error("unable to find single chunk")]
    SingleChunkNotFound,
}

impl Error {
    /// Wrap an [object_store::Error] with some context
    fn object_store(message: &'static str, err: object_store::Error) -> Self {
        Self::ObjectStore(message, err)
    }
}

impl From<Error> for super::Error {
    fn from(value: Error) -> Self {
        super::Error(Box::new(value))
    }
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use object_store::{ObjectStoreExt, PutPayload};

    use super::ObjectStoreBlobStore;
    use crate::{
        blobstore::{BlobMeta, BlobStore},
        chunkstore::{self, Chunk, ChunkStore},
    };

    #[tokio::test]
    async fn single_chunk_blobs() {
        let chunk = Chunk::from_static(b"test");
        let exp_blob_meta =
            BlobMeta::from_digests_and_sizes([(chunk.digest(), chunk.len() as u64)]);

        let os = Arc::new(object_store::memory::InMemory::new());
        let base_path: object_store::path::Path = "/".into();
        let instance_name = "uut".to_string();

        let chunk_store = chunkstore::object_store::ObjectStoreChunkStore::from_parts(
            instance_name.clone(),
            os.clone(),
            base_path.clone(),
        );

        let blob_store = ObjectStoreBlobStore {
            instance_name,
            object_store: os.clone(),
            base_path: base_path.clone(),
        };

        // Store the Chunk only, and persist the legacy 0-byte marker file where the BlobMeta should be.
        chunk_store
            .put(chunk.clone())
            .await
            .expect("putting chunk to succeed");
        let blob_path = super::derive_blob_path(&base_path, &chunk.digest());
        os.put(&blob_path, PutPayload::from_static(b""))
            .await
            .expect("put to succeed");

        // Try to retrieve a BlobMeta, it should be synthesized.
        let blob_meta = blob_store
            .get(&chunk.digest())
            .await
            .expect("to succeed")
            .expect("to be some");
        assert_eq!(exp_blob_meta, blob_meta, "expect BlobMeta to be correct");
    }
}
