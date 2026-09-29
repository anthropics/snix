use crate::B3Digest;

mod reader;

/// Describes information to assemble a Blob.
///
/// Currently stores only a list of non-empty chunk digests and offsets,
/// but might gain bao support and other fields in the future.
///
/// (See <https://snix.dev/docs/components/castore/blobstore-chunking-verified-streaming>)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobMeta {
    chunk_metas: Vec<ChunkMeta>,
}

impl BlobMeta {
    /// Returns the length of the blob.
    pub fn blob_len(&self) -> u64 {
        self.chunk_metas.last().map(|cm| cm.end).unwrap_or_default()
    }

    /// Constructs from an iterator of digests and chunk sizes.
    ///
    /// NOTE: The empty chunk is skipped silently, as there's no point fetching it.
    pub fn from_digests_and_sizes<T: IntoIterator<Item = (B3Digest, u64)>>(iter: T) -> Self {
        let mut blob_len: u64 = 0;
        let chunk_metas = iter
            .into_iter()
            .filter(|(_, chunk_size)| *chunk_size != 0)
            .map(|(chunk_digest, chunk_size)| {
                blob_len += chunk_size;
                ChunkMeta {
                    digest: chunk_digest,
                    end: blob_len,
                }
            })
            .collect();

        Self { chunk_metas }
    }

    /// For a given starting offset, returns an iterator over all chunk digests
    /// in this range, as well as the number of bytes to skip inside the first chunk.
    fn chunk_digests_from(&self, offset: u64) -> (u64, impl ExactSizeIterator<Item = &B3Digest>) {
        assert!(
            offset <= self.blob_len(),
            "illegal to seek past end of blob"
        );

        let partition_point = self.chunk_metas.partition_point(|e| e.end <= offset);

        // get the start
        let start_offset = if partition_point == 0 {
            0
        } else {
            self.chunk_metas[partition_point - 1].end
        };

        (
            offset - start_offset,
            self.chunk_metas[partition_point..]
                .iter()
                .map(|ChunkMeta { digest, .. }| digest),
        )
    }
}

/// Records information about a chunk part of a [BlobMeta]
#[derive(Clone, Debug, PartialEq, Eq)]
struct ChunkMeta {
    /// Digest of that specific chunk
    digest: B3Digest,
    /// Total length in the blob so far, with this chunk included.
    end: u64,
}

#[cfg(test)]
mod test {
    use std::sync::LazyLock;

    use crate::B3Digest;

    use super::BlobMeta;
    static CHUNK_1: &[u8] = b"ab";
    static CHUNK_2: &[u8] = b"c";
    static CHUNK_1_DIGEST: LazyLock<B3Digest> = LazyLock::new(|| blake3::hash(CHUNK_1).into());
    static CHUNK_2_DIGEST: LazyLock<B3Digest> = LazyLock::new(|| blake3::hash(CHUNK_2).into());

    static BLOB_META: LazyLock<BlobMeta> = LazyLock::new(|| {
        BlobMeta::from_digests_and_sizes([
            (*CHUNK_1_DIGEST, CHUNK_1.len() as u64),
            (*CHUNK_2_DIGEST, CHUNK_2.len() as u64),
            (*CHUNK_1_DIGEST, CHUNK_1.len() as u64),
        ])
    });

    #[rstest::rstest]
    #[case::start(0, &*CHUNK_1_DIGEST, 0)]
    #[case::one(1, &*CHUNK_1_DIGEST, 1)]
    #[case::two(2, &*CHUNK_2_DIGEST, 0)]
    #[case::three(3, &*CHUNK_1_DIGEST, 0)]
    #[case::four(4, &*CHUNK_1_DIGEST, 1)]
    fn chunk_digests_from(
        #[case] offset: u64,
        #[case] exp_first_chunk_digest: &B3Digest,
        #[case] exp_skip_in_chunk: u64,
    ) {
        let (skip_in_chunk, iter) = BLOB_META.chunk_digests_from(offset);
        assert_eq!(exp_skip_in_chunk, skip_in_chunk, "skip_in_chunk");

        let mut c = iter.into_iter().peekable();
        let a = *c.peek().expect("it to have some");
        assert_eq!(exp_first_chunk_digest, a, "first digest to match");
    }

    #[test]
    fn chunk_digests_from_end() {
        let (skip_in_chunk, iter) = BLOB_META.chunk_digests_from(BLOB_META.blob_len());
        assert_eq!(0, skip_in_chunk, "skip_in_chunk should be zero");
        assert_eq!(0, iter.into_iter().count(), "iter should be empty");
    }

    #[test]
    #[should_panic]
    fn chunk_digests_past_end() {
        let _ = BLOB_META.chunk_digests_from(BLOB_META.blob_len() + 1);
    }

    #[test]
    fn chunk_digests_empty() {
        let blob_meta = BlobMeta::from_digests_and_sizes([]);
        let (skip_in_chunk, iter) = blob_meta.chunk_digests_from(0);
        assert_eq!(0, skip_in_chunk, "skip_in_chunk should be zero");
        assert_eq!(0, iter.into_iter().count(), "iter should be empty");
    }
}
