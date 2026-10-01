use futures::StreamExt;
use std::io::{self, Cursor};

use crate::{
    B3Digest,
    blobstore::BlobMeta,
    chunkstore::{Chunk, ChunkStore},
};

impl BlobMeta {
    /// Returns a stream of bytes of actual blob bytes.
    /// Queries the passed ChunkStore, with configurable concurrency.
    /// A offset to start seeking from can be specified.
    /// Chunks before this are then skipped.
    pub fn bytes_stream_for_offset<'cs>(
        &self,
        offset: u64,
        fetch_concurrency: usize,
        chunk_store: &'cs (impl ChunkStore + 'cs),
    ) -> impl futures::Stream<Item = std::io::Result<Cursor<Chunk>>> + 'cs {
        // Calculate the range of chunks we want to iterate over
        let (start_idx, skip_first) = self.chunk_idx_from(offset);
        let chunk_metas = self.chunk_metas.clone();

        // produce a stream of byte chunks
        tokio_stream::iter(start_idx..chunk_metas.len())
            .map(move |i| {
                let skip = if i == start_idx { skip_first } else { 0 };
                fetch_chunk_and_seek(chunk_metas[i].digest, skip, chunk_store)
            })
            .buffered(fetch_concurrency)
    }
}

/// Fetches a Chunk by its digest, wraps it in a Cursor and seeks to the specified position.
async fn fetch_chunk_and_seek(
    chunk_digest: B3Digest,
    chunk_position: u64,
    chunk_store: &impl ChunkStore,
) -> io::Result<Cursor<Chunk>> {
    let chunk = chunk_store
        .get(&chunk_digest)
        .await
        .map_err(io::Error::other)?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("chunk {0} not found", chunk_digest),
            )
        })?;

    let chunk_len = chunk.len();
    let mut cursor = Cursor::new(chunk);

    // Set the chunk position
    if chunk_position > 0 {
        debug_assert!(
            chunk_len as u64 > chunk_position,
            "Snix bug: chunk size is smaller than bytes to skip"
        );
        cursor.set_position(chunk_position);
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
        s: impl futures::Stream<Item = std::io::Result<Cursor<Chunk>>>,
    ) -> Vec<Vec<u8>> {
        s.map_ok(|c| c.get_ref().as_ref()[c.position() as usize..].to_vec())
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
