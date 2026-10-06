//! Construction of chunks by appending one or more [bytes::Bytes]

use super::Chunk;

/// Allows assembling a [Chunk] from one or more [bytes::Bytes]
/// If the Chunk is constructed from just one [bytes::Bytes], we reuse it zero-copy.
#[derive(Default)]
pub enum ChunkBuilder {
    #[default]
    Empty,
    Single(bytes::Bytes),
    Multiple(Vec<u8>),
}

impl ChunkBuilder {
    /// Adds a bytes::Bytes
    pub fn append_bytes(&mut self, b: bytes::Bytes) {
        match std::mem::take(self) {
            ChunkBuilder::Empty => *self = ChunkBuilder::Single(b),
            ChunkBuilder::Single(old_buf) => {
                *self = ChunkBuilder::Multiple({
                    let mut v: Vec<u8> = old_buf.into();
                    v.extend(b);
                    v
                })
            }
            ChunkBuilder::Multiple(mut v) => {
                *self = ChunkBuilder::Multiple({
                    v.extend(b);
                    v
                })
            }
        }
    }

    /// Returns a [Chunk] with all the data appended so far.
    pub fn build(self) -> Chunk {
        match self {
            ChunkBuilder::Empty => Chunk::from_static(&[]),
            ChunkBuilder::Single(b) => Chunk::from(b),
            ChunkBuilder::Multiple(v) => Chunk::from(v),
        }
    }
}
