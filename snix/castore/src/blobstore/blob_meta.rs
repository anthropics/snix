use crate::B3Digest;

/// Describes information to assemble a Blob.
///
/// Currently stores only a list of [ChunkMeta],
/// but might gain bao support and other fields in the future.
///
/// (See https://snix.dev/docs/components/castore/blobstore-chunking-verified-streaming)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobMeta(Vec<ChunkMeta>);

impl BlobMeta {
    /// Returns the length of the blob, by summing up the sizes for each individual chunk.
    pub fn blob_len(&self) -> u64 {
        self.0.iter().map(|chunk| chunk.size).sum()
    }
}

impl FromIterator<ChunkMeta> for BlobMeta {
    fn from_iter<T: IntoIterator<Item = ChunkMeta>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// Records information about a chunk part of a [BlobMeta]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkMeta {
    /// Digest of that specific chunk
    digest: B3Digest,
    /// Length of that chunk, in bytes.
    size: u64,
}

impl ChunkMeta {
    pub fn new(digest: B3Digest, size: u64) -> Self {
        Self { digest, size }
    }

    /// Returns the digest of that specific chunk.
    pub fn digest(&self) -> &B3Digest {
        &self.digest
    }

    /// Returns the length of the chunks data / payload, in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }
}
