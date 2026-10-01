//! Provides [Chunk], a wrapper around bytes and returned by `ChunkStore`.

use crate::B3Digest;
use bytes::Bytes;

/// A Chunk returned from `ChunkStore`.
///
/// It's a newtype over its payload, stored as [Bytes], to allow sharing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk(Bytes);

impl Chunk {
    /// Returns the length of the payload
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns true if the payload is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the BLAKE3 digest of the payload.
    pub fn digest(&self) -> B3Digest {
        B3Digest::from(blake3::hash(self.0.as_ref()))
    }

    /// Constructs a new Chunk from a static byte slice as payload.
    pub const fn from_static(bytes: &'static [u8]) -> Self {
        Self(Bytes::from_static(bytes))
    }

    /// Copies the Chunk payload into a new `Vec<[u8]>`.
    pub fn to_vec(self) -> Vec<u8> {
        self.0.to_vec()
    }
}

impl AsRef<[u8]> for Chunk {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl From<&[u8]> for Chunk {
    fn from(value: &[u8]) -> Self {
        Self(Bytes::copy_from_slice(value))
    }
}

impl From<Vec<u8>> for Chunk {
    fn from(value: Vec<u8>) -> Self {
        Self(Bytes::from_owner(value))
    }
}

impl From<bytes::Bytes> for Chunk {
    fn from(value: bytes::Bytes) -> Self {
        Self(value)
    }
}

impl From<Chunk> for Vec<u8> {
    fn from(value: Chunk) -> Self {
        value.0.to_vec()
    }
}

impl From<Chunk> for bytes::Bytes {
    fn from(value: Chunk) -> Self {
        value.0
    }
}
