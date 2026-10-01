use bytes::Bytes;

/// Newtype for bytes in a Chunk returned from `ChunkStore`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk(Bytes);

impl Chunk {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub const fn from_static(bytes: &'static [u8]) -> Self {
        Self(Bytes::from_static(bytes))
    }

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
