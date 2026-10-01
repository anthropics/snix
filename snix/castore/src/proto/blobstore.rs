use crate::{
    B3Digest,
    proto::{BsStatBlobRequest, GetBlobRequest},
};

impl BsStatBlobRequest {
    pub fn with_digest(digest: B3Digest) -> Self {
        Self {
            digest: digest.into(),
        }
    }
}

impl GetBlobRequest {
    pub fn with_digest(digest: B3Digest) -> Self {
        Self {
            digest: digest.into(),
        }
    }
}
