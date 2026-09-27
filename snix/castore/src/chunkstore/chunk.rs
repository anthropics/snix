/// Newtype for bytes in a Chunk returned from `ChunkStore`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk(Vec<u8>);

impl Chunk {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

impl AsRef<[u8]> for Chunk {
    fn as_ref(&self) -> &[u8] {
        self.0.as_slice()
    }
}

impl From<&[u8]> for Chunk {
    fn from(value: &[u8]) -> Self {
        Self(value.to_vec())
    }
}

impl From<Vec<u8>> for Chunk {
    fn from(value: Vec<u8>) -> Self {
        Self(value)
    }
}

impl From<bytes::Bytes> for Chunk {
    fn from(value: bytes::Bytes) -> Self {
        Self(value.to_vec())
    }
}

impl From<Chunk> for Vec<u8> {
    fn from(value: Chunk) -> Self {
        value.0
    }
}

impl From<Chunk> for bytes::Bytes {
    fn from(value: Chunk) -> Self {
        Self::from_owner(value.0)
    }
}
