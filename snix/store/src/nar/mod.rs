use snix_castore::B3Digest;
use snix_castore::directoryservice::order_validator;

mod import;
mod listing;
mod narcalculationservice;
mod renderer;

pub use import::{NarIngestionError, ingest_nar, ingest_nar_and_hash};
pub use listing::{Error as ListingError, produce_listing};
pub use narcalculationservice::{NarCalculationService, Renderer};
pub use renderer::{Reader, write_nar, write_nar_simple};

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
    OrderingError(#[source] order_validator::OrderingError),

    #[error("unable to find blob {0}, referred from {1:?}")]
    BlobNotFound(B3Digest, bytes::Bytes),

    #[error(
        "unexpected size in metadata for blob {0}, referred from {1:?} returned, expected {2}, got {3}"
    )]
    UnexpectedBlobMeta(B3Digest, bytes::Bytes, u32, u32),

    #[error("failure using the NAR writer: {0}")]
    NARWriterError(std::io::Error),
}
