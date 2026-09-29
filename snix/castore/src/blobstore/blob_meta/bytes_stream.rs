use futures::StreamExt;
use std::io::{self, Cursor};

use crate::{B3Digest, blobstore::BlobMeta, chunkstore::ChunkStore};

impl BlobMeta {
    /// Returns a stream of bytes of actual blob bytes.
    /// Queries the passed ChunkStore, with configurable concurrency.
    /// A offset to start seeking from can be specified.
    /// Chunks before this are then skipped.
    pub fn bytes_stream_for_offset<'a>(
        &'a self,
        offset: u64,
        fetch_concurrency: usize,
        chunk_store: &'a (impl ChunkStore + 'a),
    ) -> impl futures::Stream<Item = std::io::Result<Cursor<Vec<u8>>>> + 'a {
        // Get all remaining chunk digests, and the bytes to skip from the first chunk
        let (skip_first, iter) = self.chunk_digests_from(offset);

        // produce a stream of byte chunks
        tokio_stream::iter(iter.zip(std::iter::once(skip_first).chain(std::iter::repeat(0))))
            .map(move |(chunk_digest, skip_in_chunk)| {
                chunk_digest_to_buf(chunk_digest, skip_in_chunk, chunk_store)
            })
            .buffered(fetch_concurrency)
    }
}

/// For a given chunk digest, offset to skip and ChunkStore, return a impl Buf.
async fn chunk_digest_to_buf(
    chunk_digest: &B3Digest,
    skip_in_chunk: u64,
    chunk_store: &impl ChunkStore,
) -> io::Result<Cursor<Vec<u8>>> {
    let chunk = chunk_store
        .get(chunk_digest)
        .await
        .map_err(io::Error::other)?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("chunk {0} not found", chunk_digest),
            )
        })?
        .into_vec();

    let chunk_len = chunk.len();
    let mut cursor = Cursor::new(chunk);

    // skip the first few bytes if we're told to.
    if skip_in_chunk > 0 {
        debug_assert!(
            chunk_len as u64 > skip_in_chunk,
            "Snix bug: chunk size is smaller than bytes to skip"
        );
        cursor.set_position(skip_in_chunk);
    }

    Ok::<_, io::Error>(cursor)
}

#[cfg(test)]
mod test {
    use std::{io::Cursor, sync::LazyLock};

    use futures::TryStreamExt;
    use mockall::predicate;

    use crate::{
        B3Digest,
        blobstore::BlobMeta,
        chunkstore::{Chunk, MockChunkStore},
    };

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

    async fn collect_chunks(
        s: impl futures::Stream<Item = std::io::Result<Cursor<Vec<u8>>>>,
    ) -> Vec<Vec<u8>> {
        s.map_ok(|c| c.get_ref()[c.position() as usize..].to_vec())
            .try_collect()
            .await
            .expect("to not fail")
    }

    /// Sets offset to the beginning of the second chunk, ensures the first chunk is not fetched.
    #[tokio::test]
    async fn chunked_get_skip() {
        let mut chunk_service = MockChunkStore::new();
        chunk_service
            .expect_get()
            .with(predicate::eq(*CHUNK_2_DIGEST))
            .return_once(|_| Ok(Some(CHUNK_2.to_owned())));

        let chunks =
            collect_chunks((*BLOB_1_META).bytes_stream_for_offset(2, 10, &chunk_service)).await;

        assert_eq!(vec![b"c".to_vec()], chunks);
    }

    /// Skip the first byte in the first chunk
    #[tokio::test]
    async fn chunked_skip_one_byte() {
        let mut chunk_service = MockChunkStore::new();
        chunk_service
            .expect_get()
            .returning(|digest| {
                if *digest == *CHUNK_1_DIGEST {
                    Ok(Some(CHUNK_1.to_owned()))
                } else if *digest == *CHUNK_2_DIGEST {
                    Ok(Some(CHUNK_2.to_owned()))
                } else {
                    panic!("called with unexpected digest")
                }
            })
            .times(2);

        let chunks =
            collect_chunks((*BLOB_1_META).bytes_stream_for_offset(1, 10, &chunk_service)).await;
        assert_eq!(vec![b"b".to_vec(), b"c".to_vec()], chunks, "data to match");
    }

    /// Skip to the end
    #[tokio::test]
    async fn chunked_skip_end() {
        let mut chunk_service = MockChunkStore::new();
        chunk_service.expect_get().never();

        let chunks =
            collect_chunks((*BLOB_1_META).bytes_stream_for_offset(3, 10, &chunk_service)).await;
        assert_eq!(Vec::<Vec<u8>>::new(), chunks, "data to match");
    }

    #[tokio::test]
    async fn read_empty() {
        let mut chunk_service = MockChunkStore::new();
        chunk_service.expect_get().never();

        let blob_meta = BlobMeta::from_digests_and_sizes([]);
        let chunks = collect_chunks(blob_meta.bytes_stream_for_offset(0, 10, &chunk_service)).await;

        assert_eq!(Vec::<Vec<u8>>::new(), chunks, "data to match");
    }
}
