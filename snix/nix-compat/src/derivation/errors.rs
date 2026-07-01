//! Contains [DerivationError], exported as [crate::derivation::DerivationError]
use crate::store_path::{self, StorePath};
use thiserror::Error;

/// Errors that can occur during the validation of Derivation structs.
#[derive(Debug, Error, PartialEq)]
#[allow(missing_docs)]
pub enum DerivationError {
    #[error("unable to parse derivation name {0}: {1}")]
    InvalidDerivationName(String, #[source] store_path::ParseStorePathError),

    // outputs
    #[error("{0}")]
    InvalidOutputs(
        #[from]
        #[source]
        super::outputs::OutputsError,
    ),
    // input derivation
    #[error("unable to parse input derivation path {0}: {1}")]
    InvalidInputDerivationPath(String, #[source] store_path::ParseStorePathError),
    #[error("unable to lookup input derivation {0}")]
    MissingInputDerivation(StorePath),
    #[error("input derivation {0} doesn't end with .drv")]
    InvalidInputDerivationPrefix(String),
    #[error("input derivation {0} output names are empty")]
    EmptyInputDerivationOutputNames(String),
    #[error("input derivation {0} output name {1} is invalid")]
    InvalidInputDerivationOutputName(String, String),

    // input sources
    #[error("unable to parse input sources path {0}: {1}")]
    InvalidInputSourcesPath(String, #[source] store_path::ParseStorePathError),

    // platform
    #[error("invalid platform field: {0}")]
    InvalidPlatform(String),

    // builder
    #[error("invalid builder field: {0}")]
    InvalidCommand(String),

    // environment
    #[error("invalid environment key {0}")]
    InvalidEnvironmentKey(String),
}
