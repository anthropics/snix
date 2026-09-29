use std::{
    io::{self, Cursor, SeekFrom},
    task::Poll,
};

use bytes::Buf;
use futures::{StreamExt, ready, stream::BoxStream};
use pin_project_lite::pin_project;
use tokio::io::AsyncBufRead;

use crate::{
    blobstore::BlobMeta,
    chunkstore::{Chunk, ChunkStore},
};

pin_project! {
/// A reader for one Blob.
///
/// Supports single-chunked blobs as well as blobs composed of multiple chunks.
///
/// In the latter case, it takes an existing [BlobMeta] with a specific chunking
/// provided upfront.
///
/// Any fallback to other chunkings, needs to be happen elsewhere.
#[project = BlobReaderProj]
    pub enum BlobReader<'a, CS> {
        /// Blob is composed of a single chunk.
        /// This allows skipping asking for a [BlobMeta].
        /// For very small blobs the callsite might ask a [ChunkStore] directly.
        SingleChunk{
            #[pin] cursor: Cursor<Chunk>
        },
        /// The blob is composed of multiple chunks,
        /// and we need to query a [ChunkStore] to read data.
        ChunkedBlob {
            // Chunking information for this blob
            blob_meta: &'a BlobMeta,

            // A [ChunkStore] to read chunks from
            chunk_store: &'a CS,

            // The configured fetch concurrency
            fetch_concurrency: usize,

            // The position of the reader in the entire blob
            pos: u64,

            // The current chunk.
            current_chunk: Cursor<Vec<u8>>,

            // A stream providing the remaining bytes from pos + current_chunk.remaining() till the end.
            #[pin] stream: BoxStream<'a, std::io::Result<Cursor<Vec<u8>>>>,
        },
    }
}

impl<'a, CS> BlobReader<'a, CS>
where
    CS: ChunkStore,
{
    /// Initialize a [BlobReader] with the [Chunk] of a single-chunked blob
    pub fn from_single_chunk(chunk: Chunk) -> Self {
        Self::SingleChunk {
            cursor: Cursor::new(chunk),
        }
    }

    /// Initialize a [BlobReader] with a fixed [BlobMeta],
    /// a reference to a [ChunkStore] and a fetch concurrency.
    pub fn from_blob_meta(
        blob_meta: &'a BlobMeta,
        chunk_store: &'a CS,
        fetch_concurrency: usize,
    ) -> BlobReader<'a, CS> {
        let stream = blob_meta
            .bytes_stream_for_offset(0, fetch_concurrency, chunk_store)
            .boxed();

        Self::ChunkedBlob {
            blob_meta,
            chunk_store,
            fetch_concurrency,
            pos: 0,
            current_chunk: Cursor::new(vec![]),
            stream,
        }
    }

    /// The total length of the blob.
    /// Length of the single chunk or, for chunked blobs, the length as
    /// specified in the [BlobMeta].
    fn blob_len(&self) -> u64 {
        match self {
            BlobReader::SingleChunk { cursor } => cursor.get_ref().as_ref().len() as u64,
            BlobReader::ChunkedBlob { blob_meta, .. } => blob_meta.blob_len(),
        }
    }

    /// Get the current position, from the start of the blob.
    fn position(&self) -> u64 {
        match self {
            BlobReader::SingleChunk { cursor } => cursor.position(),
            BlobReader::ChunkedBlob { pos, .. } => *pos,
        }
    }
}

impl<CS> tokio::io::AsyncRead for BlobReader<'_, CS>
where
    CS: ChunkStore,
{
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.project() {
            BlobReaderProj::SingleChunk { cursor } => cursor.poll_read(cx, buf),
            BlobReaderProj::ChunkedBlob {
                pos,
                current_chunk,
                mut stream,
                ..
            } => {
                // If the buffer we write to is full, we can't do anything
                if buf.remaining() == 0 {
                    return Poll::Ready(Ok(()));
                }

                // If we have no more bytes to read in current_chunk, poll for the next one
                if current_chunk.remaining() == 0 {
                    if let Some(res) = ready!(stream.poll_next_unpin(cx)) {
                        *current_chunk = res?;
                        debug_assert!(
                            current_chunk.remaining() > 0,
                            "fetched chunk should have some data"
                        );
                    } else {
                        // end of stream, EOF
                        return Poll::Ready(Ok(()));
                    }
                }

                let b = &current_chunk.get_ref()[current_chunk.position() as usize..];
                let to_fill = std::cmp::min(buf.remaining(), b.len());
                let dst = buf.initialize_unfilled_to(to_fill);
                dst.copy_from_slice(&b[..to_fill]);

                buf.advance(to_fill);
                current_chunk.advance(to_fill);

                *pos = pos.checked_add(to_fill as u64).ok_or(std::io::Error::new(
                    std::io::ErrorKind::OutOfMemory,
                    "position > u64::MAX bytes",
                ))?;

                Poll::Ready(Ok(()))
            }
        }
    }
}

impl<CS> AsyncBufRead for BlobReader<'_, CS>
where
    CS: ChunkStore,
{
    fn poll_fill_buf(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<io::Result<&[u8]>> {
        match self.project() {
            BlobReaderProj::SingleChunk { cursor } => cursor.poll_fill_buf(cx),
            BlobReaderProj::ChunkedBlob {
                current_chunk,
                mut stream,
                ..
            } => {
                if current_chunk.remaining() == 0 {
                    if let Some(res) = ready!(stream.poll_next_unpin(cx)) {
                        *current_chunk = res?;
                        debug_assert!(
                            current_chunk.remaining() > 0,
                            "fetched chunk should have some data"
                        );
                    } else {
                        // end of stream, EOF
                        return Poll::Ready(Ok(&[]));
                    }
                }
                let p = current_chunk.position();

                Poll::Ready(Ok(&current_chunk.get_ref()[(p as usize)..]))
            }
        }
    }

    fn consume(self: std::pin::Pin<&mut Self>, amt: usize) {
        match self.project() {
            BlobReaderProj::SingleChunk { cursor } => cursor.consume(amt),
            BlobReaderProj::ChunkedBlob {
                current_chunk, pos, ..
            } => {
                current_chunk.advance(amt);
                *pos = pos
                    .checked_add(amt as u64)
                    .expect("consume would increase pos > u64::MAX bytes");
            }
        }
    }
}
impl<CS> tokio::io::AsyncSeek for BlobReader<'_, CS>
where
    CS: ChunkStore,
{
    fn start_seek(self: std::pin::Pin<&mut Self>, position: SeekFrom) -> io::Result<()> {
        // calculate the new position
        let new_pos = calc_position(self.position(), self.blob_len(), position)?;
        if new_pos == self.position() {
            return Ok(());
        }

        match self.project() {
            BlobReaderProj::SingleChunk { cursor } => cursor.start_seek(position),
            BlobReaderProj::ChunkedBlob {
                blob_meta,
                chunk_store,
                fetch_concurrency,
                pos,
                current_chunk,
                mut stream,
            } => {
                // Empty current_chunk, and construct a new stream from the new position.
                *current_chunk = Cursor::new(vec![]);
                stream.set(
                    blob_meta
                        .bytes_stream_for_offset(new_pos, *fetch_concurrency, *chunk_store)
                        .boxed(),
                );
                *pos = new_pos;

                Ok(())
            }
        }
    }

    fn poll_complete(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> Poll<io::Result<u64>> {
        Poll::Ready(Ok(self.position()))
    }
}

/// For a given blob length and current position, returns the position that seek_from would seek to.
fn calc_position(cur_pos: u64, blob_len: u64, seek_from: SeekFrom) -> std::io::Result<u64> {
    let new_pos = match seek_from {
        SeekFrom::Start(p) => p,
        SeekFrom::End(p) => blob_len.checked_sub_signed(p).ok_or(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "tried to seek before beginning of blob",
        ))?,
        SeekFrom::Current(p) => cur_pos.checked_add_signed(p).ok_or(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "tried to seek way past end of blob",
        ))?,
    };

    if new_pos > blob_len {
        Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "tried to seek past end of blob",
        ))
    } else {
        Ok(new_pos)
    }
}

#[cfg(test)]
mod test {
    use std::{io::SeekFrom, sync::LazyLock};
    use tokio::io::AsyncSeekExt;

    use crate::{
        B3Digest,
        blobstore::BlobMeta,
        chunkstore::{Chunk, ChunkStore, memory::MemoryChunkStore},
    };

    use super::BlobReader;

    #[tokio::test]
    async fn single_chunk() {
        let mut rd = BlobReader::<MemoryChunkStore>::from_single_chunk(b"abc"[..].into());

        {
            let mut buf = Vec::new();

            let bytes_read = tokio::io::copy(&mut rd, &mut buf)
                .await
                .expect("to succeed");
            assert_eq!(3, bytes_read, "expect bytes_read to match");
            assert_eq!(b"abc"[..], buf[..], "expect data to match");
        }

        // seek to pos 1
        rd.seek(SeekFrom::Current(-2))
            .await
            .expect("seek to succeed");

        // should read "bc". we try the bufread part
        {
            let mut buf = Vec::new();
            let bytes_read = tokio::io::copy_buf(&mut rd, &mut buf)
                .await
                .expect("to succeed");
            assert_eq!(2, bytes_read, "expect bytes_read to match");
            assert_eq!(b"bc"[..], buf[..], "expect data to match");
        }
    }

    static CHUNK_1: LazyLock<Chunk> = LazyLock::new(|| b"ab".as_slice().into());
    static CHUNK_2: LazyLock<Chunk> = LazyLock::new(|| b"c".as_slice().into());
    static CHUNK_1_DIGEST: LazyLock<B3Digest> =
        LazyLock::new(|| blake3::hash(CHUNK_1.as_ref()).into());
    static CHUNK_2_DIGEST: LazyLock<B3Digest> =
        LazyLock::new(|| blake3::hash(CHUNK_2.as_ref()).into());
    static BLOB_1_META: LazyLock<BlobMeta> = LazyLock::new(|| {
        BlobMeta::from_digests_and_sizes([
            (*CHUNK_1_DIGEST, CHUNK_1.len() as u64),
            (*CHUNK_2_DIGEST, CHUNK_2.len() as u64),
        ])
    });

    #[tokio::test]
    async fn multiple_chunks() {
        let chunk_store = MemoryChunkStore::default();
        chunk_store
            .put(CHUNK_1.to_owned())
            .await
            .expect("to succeed");
        chunk_store
            .put(CHUNK_2.to_owned())
            .await
            .expect("to succeed");

        let mut rd = BlobReader::from_blob_meta(&BLOB_1_META, &chunk_store, 10);
        {
            let mut buf = Vec::new();
            tokio::io::copy(&mut rd, &mut buf)
                .await
                .expect("to succeed");
            assert_eq!(b"abc"[..].to_vec(), buf.as_slice(), "data to match");
        }

        let pos = rd.seek(SeekFrom::Start(1)).await.expect("seek to succeed");
        assert_eq!(1, pos, "position to be correct");

        {
            let mut buf = Vec::new();
            tokio::io::copy_buf(&mut rd, &mut buf)
                .await
                .expect("to succeed");
            assert_eq!(b"bc"[..].to_vec(), buf.as_slice(), "data to match");
        }

        // use AsyncBufRead a bit, then seek relatively. This ensures consume() updates the internal position tracking.
        {
            // seek to the front
            let pos = rd.seek(SeekFrom::Start(0)).await.expect("seek to succeed");
            assert_eq!(0, pos, "position to be correct");

            // use tokio::io::copy_buf to read to the end
            {
                let mut buf = Vec::new();
                tokio::io::copy_buf(&mut rd, &mut buf)
                    .await
                    .expect("to succeed");
                assert_eq!(b"abc"[..].to_vec(), buf.as_slice(), "data to match");
            }

            // seek relatively
            let new_pos = rd
                .seek(SeekFrom::Current(-1))
                .await
                .expect("seek to succeed");
            assert_eq!(2, new_pos, "expect position to be correct");

            // read to the end again
            {
                let mut buf = Vec::new();
                tokio::io::copy_buf(&mut rd, &mut buf)
                    .await
                    .expect("to succeed");
                assert_eq!(b"c"[..].to_vec(), buf.as_slice(), "data to match");
            }
        }
    }
}
