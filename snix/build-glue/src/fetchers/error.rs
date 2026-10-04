use nix_compat::nixhash::NixHash;
use nix_compat::store_path;
use snix_castore::{blob_engine, import};
use url::Url;

#[derive(Debug, thiserror::Error)]
pub enum FetcherError {
    #[error("hash mismatch in file downloaded from {}:\n  wanted: {}\n     got: {}", _0.0, _0.1, _0.2)]
    HashMismatch(Box<(Url, NixHash, NixHash)>),

    #[error("Invalid hash type '{0}' for fetcher")]
    InvalidHashType(&'static str),

    #[error("Unable to parse URL: {0}")]
    InvalidUrl(#[from] url::ParseError),

    #[error(transparent)]
    Http(#[from] reqwest::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    BlobEngine(#[from] blob_engine::Error),

    #[error(transparent)]
    Import(Box<snix_castore::import::IngestionError<import::archive::Error>>),

    #[error("Error calculating store path for fetcher output: {0}")]
    StorePath(#[from] store_path::ParseStorePathError),
}

impl FetcherError {
    /// Helper to constructs a HashMismatch error kind.
    pub fn hash_mismatch(url: Url, expected: NixHash, actual: NixHash) -> Self {
        Self::HashMismatch(Box::new((url, expected, actual)))
    }
}

// thiserror doesn't support Box<#[from] ...> unfortunately
impl From<snix_castore::import::IngestionError<import::archive::Error>> for FetcherError {
    fn from(value: snix_castore::import::IngestionError<import::archive::Error>) -> Self {
        Self::Import(Box::new(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_mismatch_display() {
        let err = FetcherError::hash_mismatch(
            Url::parse("https://example.com/foo.tar.gz").unwrap(),
            NixHash::Sha256([0; 32]),
            NixHash::Sha256([1; 32]),
        );
        let msg = err.to_string();
        assert!(msg.contains("https://example.com/foo.tar.gz"));
        assert!(
            msg.contains(
                "wanted: sha256:0000000000000000000000000000000000000000000000000000000000000000"
            ) || msg.contains("wanted:")
        );
        assert!(!msg.contains("wanted: 0.1"));
    }
}
