use std::sync::Arc;

use crate::{B3Digest, proto};

mod bytes_stream;

/// Describes information to assemble a Blob.
///
/// Currently stores only a list of non-empty chunk digests and offsets,
/// but might gain bao support and other fields in the future.
///
/// (See <https://snix.dev/docs/components/castore/blobstore-chunking-verified-streaming>)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobMeta {
    chunk_metas: Arc<[ChunkMeta]>,
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

    /// For a given offset into the blob, returns the index of the first chunk
    /// holding the requested bytes, as well as the number of bytes to skip inside the first chunk.
    fn chunk_idx_from(&self, offset: u64) -> (usize, u64) {
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

        (partition_point, offset - start_offset)
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

impl From<BlobMeta> for proto::BlobMeta {
    fn from(value: BlobMeta) -> Self {
        let mut prev_end = 0;

        Self {
            chunks: value
                .chunk_metas
                .iter()
                .map(|ChunkMeta { digest, end }| {
                    let size = end - prev_end;
                    prev_end = *end;
                    proto::blob_meta::ChunkMeta {
                        digest: (*digest).into(),
                        size,
                    }
                })
                .collect(),
        }
    }
}

/// Constructs by parsing a [proto::BlobMeta] message.
///
/// This will reject a [proto::BlobMeta] message without any chunks.
/// This is to prevent accidentially passing a `StatBlobResponse` message which
/// historically omitted chunks in the single-chunk case.
///
/// It will also reject chunks with a size of zero
///
/// To explicitly create a [BlobMeta] for the empty blob,
/// use [BlobMeta::from_digests_and_sizes] with an empty iterator instead.
impl TryFrom<proto::BlobMeta> for BlobMeta {
    type Error = DecodeError;

    fn try_from(value: proto::BlobMeta) -> Result<Self, Self::Error> {
        if value.chunks.is_empty() {
            return Err(DecodeError::NoChunks);
        }

        // We iterate until a possible error.
        // If there's no error we can return the complete BlobMeta.
        let mut err = None;
        let blob_meta =
            Self::from_digests_and_sizes(value.chunks.into_iter().enumerate().map_while(
                |(idx, proto::blob_meta::ChunkMeta { size, digest })| {
                    if size == 0 {
                        err = Some(DecodeError::EmptyChunk(idx));
                        return None;
                    }
                    match B3Digest::try_from(digest) {
                        Ok(digest) => Some((digest, size)),
                        Err(_) => {
                            err = Some(DecodeError::InvalidDigest(idx));
                            // stop iteration
                            None
                        }
                    }
                },
            ));

        match err {
            Some(err) => Err(err),
            None => Ok(blob_meta),
        }
    }
}

#[derive(thiserror::Error, Debug, PartialEq, Eq)]
pub enum DecodeError {
    #[error("refusing to contruct with empty chunk at idx {0}")]
    EmptyChunk(usize),
    #[error("refusing to construct with zero chunks")]
    NoChunks,
    #[error("invalid digest len at idx {0}")]
    InvalidDigest(usize),
}

#[cfg(test)]
mod test {
    use std::sync::LazyLock;

    use crate::{chunkstore::Chunk, proto};

    use super::{BlobMeta, DecodeError};

    const CHUNK_1: Chunk = Chunk::from_static(b"ab");
    const CHUNK_2: Chunk = Chunk::from_static(b"c");

    static BLOB_META: LazyLock<BlobMeta> = LazyLock::new(|| {
        BlobMeta::from_digests_and_sizes([
            (CHUNK_1.digest(), CHUNK_1.len() as u64),
            (CHUNK_2.digest(), CHUNK_2.len() as u64),
            (CHUNK_1.digest(), CHUNK_1.len() as u64),
        ])
    });

    #[rstest::rstest]
    #[case::start(0, 0, 0)]
    #[case::one(1, 0, 1)]
    #[case::two(2, 1, 0)]
    #[case::three(3, 2, 0)]
    #[case::four(4, 2, 1)]
    fn chunk_idx_from(
        #[case] offset: u64,
        #[case] exp_start_idx: usize,
        #[case] exp_skip_in_chunk: u64,
    ) {
        let (start_idx, skip_in_chunk) = BLOB_META.chunk_idx_from(offset);
        assert_eq!(exp_skip_in_chunk, skip_in_chunk, "skip_in_chunk to match");
        assert_eq!(exp_start_idx, start_idx, "start_idx to match");
    }

    #[test]
    /// Calling chunk_idx_from for a offset equal to the blob len should return a start_idx equal to the chunk_meta len.
    /// (So the range (start_idx..chunk_meta.len()) is empty)
    fn chunk_idx_from_end() {
        let (start_idx, skip_in_chunk) = BLOB_META.chunk_idx_from(BLOB_META.blob_len());
        assert_eq!(
            BLOB_META.chunk_metas.len(),
            start_idx,
            "chunk_idx should be 3"
        );
        assert_eq!(0, skip_in_chunk, "skip_in_chunk should be zero");
    }

    #[test]
    #[should_panic]
    /// Calling chunk_idx_from for a offset past the blob len should panic
    fn chunk_digests_past_end() {
        let _ = BLOB_META.chunk_idx_from(BLOB_META.blob_len() + 1);
    }

    #[test]
    fn chunk_digests_empty() {
        let blob_meta = BlobMeta::from_digests_and_sizes([]);
        let (chunk_idx, skip_in_chunk) = blob_meta.chunk_idx_from(0);
        assert_eq!(0, skip_in_chunk, "skip_in_chunk should be zero");
        assert_eq!(0, chunk_idx, "chunk_idx should be 0");
    }

    #[test]
    /// Decoding a BlobMeta without any chunks should fail
    fn decode_blob_meta_empty() {
        let blob_meta = proto::BlobMeta { chunks: vec![] };
        let err = BlobMeta::try_from(blob_meta).expect_err("should fail");
        assert_eq!(DecodeError::NoChunks, err);
    }

    #[test]
    /// Decoding a BlobMeta with a wrong digest len should fail
    fn decode_blob_meta_wrong_digest() {
        let blob_meta = proto::BlobMeta {
            chunks: vec![proto::blob_meta::ChunkMeta {
                digest: "abcd".into(),
                size: 42,
            }],
        };

        let err = BlobMeta::try_from(blob_meta).expect_err("should fail");
        assert_eq!(DecodeError::InvalidDigest(0), err);
    }

    #[test]
    /// Decoding a BlobMeta with an empty chunk should fail
    fn decode_blob_meta_empty_chunk() {
        let blob_meta = proto::BlobMeta {
            chunks: vec![proto::blob_meta::ChunkMeta {
                digest: Chunk::from_static(b"").digest().into(),
                size: 0,
            }],
        };

        let err = BlobMeta::try_from(blob_meta).expect_err("should fail");
        assert_eq!(DecodeError::EmptyChunk(0), err);
    }
}
