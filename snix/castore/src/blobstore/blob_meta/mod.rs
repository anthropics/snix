use crate::B3Digest;

mod reader;

/// Describes information to assemble a Blob.
///
/// Currently stores only a list of [ChunkMeta],
/// but might gain bao support and other fields in the future.
///
/// (See <https://snix.dev/docs/components/castore/blobstore-chunking-verified-streaming>)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobMeta(Vec<ChunkMeta>);

impl BlobMeta {
    /// Returns the length of the blob, by summing up the sizes for each individual chunk.
    pub fn blob_len(&self) -> u64 {
        self.0.iter().map(|e| e.size).sum()
    }

    /// Constructs from an iterator of digests and chunk sizes
    pub fn from_digests_and_sizes<T: IntoIterator<Item = (B3Digest, u64)>>(iter: T) -> Self {
        Self(
            iter.into_iter()
                .map(|(digest, size)| ChunkMeta { digest, size })
                .collect(),
        )
    }
}

/// Records information about a chunk part of a [BlobMeta]
#[derive(Clone, Debug, PartialEq, Eq)]
struct ChunkMeta {
    /// Digest of that specific chunk
    digest: B3Digest,
    /// Length of that chunk, in bytes.
    size: u64,
}
