use crate::{
    B3Digest,
    proto::{GetChunkRequest, PutChunkResponse, StatChunkRequest},
};

impl StatChunkRequest {
    pub fn with_digest(digest: B3Digest) -> Self {
        Self {
            digest: digest.into(),
        }
    }
}

impl GetChunkRequest {
    pub fn with_digest(digest: B3Digest) -> Self {
        Self {
            digest: digest.into(),
        }
    }
}

impl PutChunkResponse {
    pub fn with_digest(digest: B3Digest) -> Self {
        Self {
            digest: digest.into(),
        }
    }
}
