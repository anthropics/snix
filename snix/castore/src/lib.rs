#![cfg_attr(docsrs, feature(doc_cfg))]

mod digests;
mod errors;

pub mod blob_engine;
pub mod blobservice;
pub mod blobstore;
pub mod chunkstore;
pub mod combinators;
pub mod composition;
pub mod directoryservice;
pub mod fixtures;
pub(crate) mod object_store;
pub mod refscan;
pub mod utils;

#[cfg(feature = "fs")]
pub mod fs;

mod nodes;
pub use nodes::*;

mod path;
pub use path::{Path, PathBuf, PathComponent, PathComponentError};

pub mod import;
pub mod proto;
pub mod tonic;

// Used as user agent in various HTTP Clients
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

pub use digests::B3Digest;
pub use errors::{DirectoryError, ValidateNodeError};

#[cfg(test)]
mod tests;
