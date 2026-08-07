use auto_impl::auto_impl;
use snix_castore::B3Digest;
use snix_castore::Node;
use snix_castore::directoryservice::OrderingError;
use tonic::async_trait;

mod import;
mod listing;
mod renderer;
pub mod seekable;

pub use import::{NarIngestionError, ingest_nar, ingest_nar_and_hash};
pub use listing::{Error as ListingError, produce_listing};
pub use renderer::SimpleRenderer;
pub use renderer::calculate_size_and_sha256;
pub use renderer::write_nar;

use crate::pathinfoservice;

#[cfg_attr(any(test, feature = "mocks"), mockall::automock)]
#[async_trait]
#[auto_impl(&, &mut, Arc, Box)]
pub trait NarCalculationService: Send + Sync {
    /// Return the nar size and nar sha256 digest for a given root node.
    /// This can be used to calculate NAR-based output paths.
    async fn calculate_nar(
        &self,
        root_node: &Node,
    ) -> Result<(u64, [u8; 32]), pathinfoservice::Error>;
}

/// Errors that can encounter while rendering NARs.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("failure talking to a backing directory service")]
    DirectoryService(#[source] snix_castore::directoryservice::Error),

    #[error("failure talking to a backing blob service")]
    BlobService(#[source] std::io::Error),

    #[error("unable to find directory {0}, referred from {1:?}")]
    DirectoryNotFound(B3Digest, bytes::Bytes),

    #[error("Invalid Ordering")]
    OrderingError(#[source] OrderingError),

    #[error("unable to find blob {0}, referred from {1:?}")]
    BlobNotFound(B3Digest, bytes::Bytes),

    #[error(
        "unexpected size in metadata for blob {0}, referred from {1:?} returned, expected {2}, got {3}"
    )]
    UnexpectedBlobMeta(B3Digest, bytes::Bytes, u32, u32),

    #[error("failure using the NAR writer: {0}")]
    NARWriterError(std::io::Error),
}
