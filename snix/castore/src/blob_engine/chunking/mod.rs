use tonic::async_trait;

use crate::{
    B3Digest,
    blob_engine::BlobEngine,
    blobstore::{self, BlobReader, BlobStore, BlobWriter},
    chunkstore::{self, ChunkStore, memory::MemoryChunkStore},
};

mod writer;
use writer::BlobWriterWrapper;

/// A BlobEngine that will first query [BlobStore]
/// to retrieve more granular chunking,
/// then use the [ChunkStore] to render the blob.
pub struct ChunkingBlobEngine<BS, CS> {
    blob_store: BS,
    chunk_store: CS,
    fetch_concurrency: usize,
    upload_concurrency: usize,
    avg_chunk_size: u32,
}

#[async_trait]
impl<BS, CS> BlobEngine for ChunkingBlobEngine<BS, CS>
where
    BS: BlobStore,
    CS: ChunkStore + Clone + 'static,
{
    #[tracing::instrument(skip_all, fields(blob_digest=%digest))]
    async fn has(&self, digest: &B3Digest) -> Result<bool, super::Error> {
        Ok(self
            .blob_store
            .has(digest)
            .await
            .map_err(Error::Blobstore)?)
    }

    #[tracing::instrument(skip_all, fields(blob_digest=%digest, blob.size_hint=size_hint))]
    async fn open_read<'a>(
        &'a self,
        digest: &B3Digest,
        size_hint: Option<u64>,
    ) -> Result<Option<Box<dyn crate::blobservice::BlobReader + 'a>>, super::Error> {
        // If the size_hint suggests it's smaller than min_size,
        // check chunk_store first before falling back to blob_store.
        // We still need to check there as chunking parameters might have changed.
        if let Some(size_hint) = size_hint
            && size_hint < (self.avg_chunk_size as u64) / 2
            && let Some(chunk) = self
                .chunk_store
                .get(digest)
                .await
                .map_err(Error::Chunkstore)?
        {
            return Ok(Some(
                Box::new(BlobReader::<MemoryChunkStore>::from_single_chunk(chunk))
                    as Box<dyn crate::blobservice::BlobReader>,
            ));
        }
        Ok(self
            .blob_store
            .get(digest)
            .await
            .map_err(Error::Blobstore)?
            .map(|blob_meta| {
                Box::new(BlobReader::from_blob_meta(
                    blob_meta,
                    &self.chunk_store,
                    self.fetch_concurrency,
                )) as Box<dyn crate::blobservice::BlobReader>
            }))
    }

    async fn open_write<'a>(&'a self) -> Box<dyn crate::blobservice::BlobWriter + 'a> {
        let blob_writer = BlobWriter::new(
            self.chunk_store.clone(),
            self.avg_chunk_size,
            self.upload_concurrency,
        );

        Box::new(BlobWriterWrapper::new(blob_writer, &self.blob_store))
            as Box<dyn crate::blobservice::BlobWriter>
    }
}

#[derive(thiserror::Error, Debug)]
enum Error {
    #[error("querying blobstore: {0}")]
    Blobstore(blobstore::Error),
    #[error("querying chunkstore: {0}")]
    Chunkstore(chunkstore::Error),
}

impl From<Error> for super::Error {
    fn from(value: Error) -> Self {
        Self(value.into())
    }
}
