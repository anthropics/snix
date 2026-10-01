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
    chunkstore::{Chunk, ChunkStore, EMPTY_CHUNK},
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
// pin_project_lite doesnt't seem to support docstrings fully
#[allow(missing_docs)]
#[project = BlobReaderProj]
    pub enum BlobReader<'a, CS> {
        /// Blob is composed of a single chunk.
        /// This allows skipping asking for a [BlobMeta].
        /// For very small blobs the callsite might ask a [ChunkStore] directly.
        SingleChunk {
            // A cursor over the single chunk.
            #[pin] cursor: Cursor<Chunk>,
        },
        /// The blob is composed of multiple chunks,
        /// and we need to query a [ChunkStore] to read data.
        ChunkedBlob {
            // Chunking information for this blob
            blob_meta: BlobMeta,

            // A [ChunkStore] to read chunks from
            chunk_store: &'a CS,

            // The configured fetch concurrency
            fetch_concurrency: usize,

            // The position of the reader in the entire blob
            pos: u64,

            // The current chunk.
            current_chunk: Cursor<Chunk>,

            // A stream providing the remaining bytes from pos + current_chunk.remaining() till the end.
            #[pin] stream: BoxStream<'a, std::io::Result<Cursor<Chunk>>>,
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
        blob_meta: BlobMeta,
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
            current_chunk: Cursor::new(EMPTY_CHUNK),
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

                let b = &current_chunk.get_ref().as_ref()[current_chunk.position() as usize..];
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

                Poll::Ready(Ok(&current_chunk.get_ref().as_ref()[(p as usize)..]))
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
                // if the new position is still covered by our current buffer, we can simply update our position in there.
                // else, empty current_chunk, and construct a new stream from the new position.
                if !seek_in_current_chunk(*pos, new_pos, current_chunk)? {
                    *current_chunk = Cursor::new(EMPTY_CHUNK);
                    stream.set(
                        blob_meta
                            .bytes_stream_for_offset(new_pos, *fetch_concurrency, *chunk_store)
                            .boxed(),
                    );
                }

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

// FUTUREWORK: move the trait into BlobStore once BlobService is gone
impl<'a, CS> crate::blobservice::BlobReader for BlobReader<'a, CS> where CS: ChunkStore + 'a {}

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

/// Seeks the cursor to the new blob position if contained in the chunk.
///
/// Returns true if the new position is contained in the current chunk.
/// It is permissible to seek to the end of the chunk, (with no more bytes remaining)
fn seek_in_current_chunk(
    cur_blob_pos: u64,
    new_blob_pos: u64,
    current_chunk: &mut Cursor<impl AsRef<[u8]>>,
) -> io::Result<bool> {
    // Determine current_chunks blob offsets
    let chunk_len = current_chunk.get_ref().as_ref().len();
    let chunk_start_offset = cur_blob_pos - current_chunk.position();
    let chunk_end_pos =
        chunk_start_offset
            .checked_add(chunk_len as u64)
            .ok_or(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "chunk_end > u64::MAX",
            ))?;

    if (chunk_start_offset..=chunk_end_pos).contains(&new_blob_pos) {
        let new_chunk_pos = new_blob_pos - chunk_start_offset;
        debug_assert!(
            new_chunk_pos <= chunk_len as u64,
            "Snix bug: tried seeking past chunk boundary"
        );
        current_chunk.set_position(new_chunk_pos);
        Ok(true)
    } else {
        Ok(false)
    }
}

#[cfg(test)]
mod test {
    use mockall::predicate;
    use std::{
        io::{Cursor, SeekFrom},
        sync::LazyLock,
    };
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    use crate::{
        blobstore::BlobMeta,
        chunkstore::{Chunk, ChunkStore, MockChunkStore, memory::MemoryChunkStore},
    };

    use super::{BlobReader, seek_in_current_chunk};
    use proptest::prelude::*;
    proptest! {
        #[test]
        fn seek_correctly_seeks(
            // chunk data
            chunk_data in prop::collection::vec(any::<u8>(), 0..256),
            // position inside the chunk
            chunk_pos in 0..=256usize,
            // the chunk must not necessarily be at the start
            chunk_start_offset in 0..256u64,
            // where we want to seek to
            seek_relative in -128..128i64,
        ) {
            // clamp chunk_pos to not fall outside chunk_data.
            let chunk_pos = chunk_data.len().min(chunk_pos) as u64;

            let cur_blob_pos = chunk_start_offset.checked_add(chunk_pos).expect("to not overflow");

            let chunk_end_pos = chunk_start_offset.checked_add(chunk_data.len() as u64).expect("to not overflow");
            let mut current_chunk = {
                let mut c = Cursor::new(chunk_data);
                c.set_position(chunk_pos);
                c
            };

            // bail out early if we're trying to seek to a negative position
            let new_blob_pos = match cur_blob_pos.checked_add_signed(seek_relative) {
                None => return Ok(()),
                Some(v) => v,
            };
            let success = seek_in_current_chunk(cur_blob_pos, new_blob_pos, &mut current_chunk).expect("to not error");

            let target_is_inside_chunk = new_blob_pos >= chunk_start_offset && new_blob_pos <= chunk_end_pos;
            if target_is_inside_chunk {
                assert!(success, "seek should have been successful");
                let new_chunk_pos = chunk_pos.checked_add_signed(seek_relative).expect("to not over/underflow");
                assert_eq!(new_chunk_pos, current_chunk.position(), "seek should be to the new position");
            } else {
                assert!(!success, "seek should be unsuccessful, from {cur_blob_pos} to {new_blob_pos}, {chunk_start_offset}..={chunk_end_pos}");
                assert_eq!(chunk_pos, current_chunk.position(), "no seek should have happened")
            }
        }
    }

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

    const CHUNK_1: Chunk = Chunk::from_static(b"ab");
    const CHUNK_2: Chunk = Chunk::from_static(b"c");
    static BLOB_1_META: LazyLock<BlobMeta> = LazyLock::new(|| {
        BlobMeta::from_digests_and_sizes([
            (CHUNK_1.digest(), CHUNK_1.len() as u64),
            (CHUNK_2.digest(), CHUNK_2.len() as u64),
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

        let mut rd = BlobReader::from_blob_meta(BLOB_1_META.to_owned(), &chunk_store, 10);
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

        // seek to the front
        let pos = rd.seek(SeekFrom::Start(0)).await.expect("seek to succeed");
        assert_eq!(0, pos, "position to be correct");
    }

    #[tokio::test]
    async fn test_seek_same_chunk() {
        // This chunk store will only ever respond with CHUNK_1 once.
        let mut chunk_store = MockChunkStore::new();
        chunk_store
            .expect_get()
            .with(predicate::eq(CHUNK_1.digest()))
            .return_once(|_| Ok(Some(CHUNK_1.to_owned())));

        // construct BlobReader
        let mut rd = BlobReader::from_blob_meta(
            BLOB_1_META.to_owned(),
            &chunk_store,
            // we explicitly set the concurrency to 1, so BLOB2 will only get fetched if would poll the stream a second time
            // (which we don't).
            1,
        );

        let first = rd.read_u8().await.expect("to read first byte");
        assert_eq!(b'a', first, "expect first byte to match");

        // seek backwards to start, read again
        rd.seek(SeekFrom::Start(0)).await.expect("seek to succeed");
        let first = rd.read_u8().await.expect("to read first byte");
        assert_eq!(b'a', first, "expect first byte to match");

        // seek to the end of this chunk
        rd.seek(SeekFrom::Start(2)).await.expect("seek to succeed");
        // we now don't read, so we won't poll the stream and cause the chunkservice to panic
        // seek back to the middle of the first chunk
        rd.seek(SeekFrom::Start(1)).await.expect("seek to succeed");
        // and read a bit more
        let second = rd.read_u8().await.expect("to read second byte");
        assert_eq!(b'b', second, "expect second byte to match");
    }
}
