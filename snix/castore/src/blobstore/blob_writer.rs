use std::{pin::pin, task::Poll};

use futures::TryStreamExt;
use pin_project_lite::pin_project;
use tokio::{
    io::{SimplexStream, WriteHalf},
    task::JoinHandle,
};
use tokio_util::io::InspectReader;

use crate::{
    B3Digest,
    blobstore::{BlobMeta, ChunkMeta},
    chunkstore::{Chunk, ChunkStore},
};

pin_project! {
/// Writes bytes to a [ChunkStore], returning the [BlobMeta] and [B3Digest] on close.
pub struct BlobWriter{
    // This is where we write received bytes into during a poll_write.
    // The background task keeps the chunker and takes care of uploading.
    writer: WriteHalf<SimplexStream>,

    // Represents the completion of the upload task, which can either fail if we
    // couldn't upload a chunk, or succeed (if it reaches EOF, which happens
    // when we close our writer).
    // In that case, we return the [BlobMeta] and [B3Digest] of the blob payload.
    upload_task: JoinHandle<std::io::Result<(BlobMeta, B3Digest)>>,
}
}

impl BlobWriter {
    /// Constructs a new [BlobWriter] writing to the given [ChunkStore].
    pub fn new(
        chunk_store: impl ChunkStore + 'static,
        avg_chunk_size: u32,
        upload_concurrency: usize,
    ) -> Self {
        let (reader, writer) = tokio::io::simplex(1024 * 1024 * 10);

        let upload_task = tokio::task::spawn(async move {
            let mut blob_hasher = blake3::Hasher::new();
            let reader = InspectReader::new(reader, |b| {
                blob_hasher.update(b);
            });

            let mut chunker = fastcdc::v2020::AsyncStreamCDC::new(
                reader,
                avg_chunk_size / 2,
                avg_chunk_size,
                avg_chunk_size * 2,
            );

            let chunk_metas: Vec<ChunkMeta> = chunker
                .as_stream()
                .map_ok(|chunk| {
                    let chunk = Chunk::from(chunk.data);
                    let chunk_len = chunk.len();
                    // we only want to move a &ChunkStore.
                    let chunk_store = &chunk_store;
                    async move {
                        let chunk_digest = chunk_store.put(chunk).await.map_err(|_| {
                            std::io::Error::other("Failed to upload chunk".to_owned())
                        })?;

                        // on an upload error, this will cause rx to be dropped, which we will see when trying to enqueue work.
                        Ok(ChunkMeta::new(chunk_digest, chunk_len as u64))
                    }
                })
                .try_buffered(upload_concurrency)
                .try_collect()
                .await?;

            Ok((
                BlobMeta::from_iter(chunk_metas),
                blob_hasher.finalize().into(),
            ))
        });

        BlobWriter {
            writer,
            upload_task,
        }
    }

    /// Get the resulting [BlobMeta] and [B3Digest] once all data has been written.
    ///
    /// This will consume self, drop the internal writer and wait for the
    /// background task to finish uploading.
    /// It is the responsibility of the caller to persist to the BlobStore.
    pub async fn close(self) -> std::io::Result<(BlobMeta, B3Digest)> {
        let Self {
            mut writer,
            upload_task,
        } = self;

        // shutdown the writer, to ensure the EOF gets propagated.
        use tokio::io::AsyncWriteExt;
        writer.shutdown().await?;

        // drop the writer, wait for the results to come back.
        drop(writer);

        upload_task.await.expect("poisoned")
    }
}

impl tokio::io::AsyncWrite for BlobWriter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        // // Writes to the writer, which maintains its own internal buffer.
        pin!(self.project().writer).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        pin!(self.project().writer).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        pin!(self.project().writer).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod test {
    use tokio::io::AsyncWriteExt;

    use super::BlobWriter;
    use crate::blobstore::{BlobMeta, ChunkMeta};
    use crate::{
        B3Digest,
        chunkstore::{self, ChunkStore},
    };
    use std::time::Duration;

    #[tokio::test]
    async fn test_write_scenario() {
        let mut test_builder = tokio_test::io::Builder::new();
        test_builder.read("test".as_bytes());
        test_builder.wait(Duration::from_nanos(1));
        test_builder.read("123".as_bytes());

        let exp_digest: B3Digest = blake3::hash(b"test123").into();

        let chunk_store = chunkstore::memory::MemoryChunkStore::default();
        let mut blob_writer = BlobWriter::new(chunk_store.clone(), 4096, 10);

        let written = tokio::io::copy(&mut test_builder.build(), &mut blob_writer)
            .await
            .expect("should write");
        assert_eq!(7, written);

        blob_writer.flush().await.expect("should flush");

        let (blob_meta, blob_digest) = blob_writer.close().await.expect("should close");
        assert_eq!(7, blob_meta.blob_len(), "BlobMeta blob len must match");
        assert_eq!(exp_digest, blob_digest, "blob digest must match",);

        let exp_blob_meta = BlobMeta::from_iter([ChunkMeta::new(exp_digest, 7)]);
        assert_eq!(exp_blob_meta, blob_meta, "expected blob meta to be correct");

        assert_eq!(
            &b"test123",
            &chunk_store
                .get(&exp_digest)
                .await
                .expect("ChunkStore to not fail")
                .expect("to have the chunk")
                .as_ref(),
            "the returned data to be correct"
        )
    }

    #[tokio::test]
    async fn test_write_empty() {
        let mut chunk_store = chunkstore::MockChunkStore::new();
        // put shall never be called
        chunk_store.expect_put().never();

        let mut blob_writer = BlobWriter::new(chunk_store, 4096, 10);
        let written = tokio::io::copy(&mut tokio::io::empty(), &mut blob_writer)
            .await
            .expect("copy to succeed");
        assert_eq!(0, written, "expected 0 bytes to be written");

        let (blob_meta, blob_digest) = blob_writer.close().await.expect("should close");

        let exp_digest: B3Digest = blake3::hash(b"").into();
        assert_eq!(exp_digest, blob_digest, "blob digest must match",);

        let exp_blob_meta = BlobMeta::from_iter([]);
        assert_eq!(exp_blob_meta, blob_meta, "expected blob meta to be correct");
    }
}
