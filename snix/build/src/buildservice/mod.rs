use auto_impl::auto_impl;
use futures::stream::BoxStream;

pub mod build_request;
pub use crate::buildservice::build_request::*;
mod dummy;
mod from_addr;
mod grpc;

#[cfg(target_os = "linux")]
mod oci;

#[cfg(target_os = "linux")]
mod bwrap;

pub mod streaming;

pub use dummy::DummyBuildService;
pub use from_addr::from_addr;

pub enum BuildUpdate {
    ProducedOutput {
        /// The contents of this output
        node: snix_castore::Node,

        /// Specifies which output is being sent, indexing into [BuildRequest::outputs].
        idx: u64,

        /// Indexes into the found [BuildRequest::refscan_needles] in this output.
        refscan_needles: std::collections::BTreeSet<u64>,
    },

    // The build produced output on stdout.
    ProducedStdout(Vec<u8>),

    // The build produced output on stderr.
    ProducedStderr(Vec<u8>),

    // The build failed.
    BuildFailure(BuildFailure),
}

impl From<BuildFailure> for BuildUpdate {
    fn from(value: BuildFailure) -> Self {
        Self::BuildFailure(value)
    }
}

#[derive(thiserror::Error, Debug)]
pub enum BuildFailure {
    #[error("nonzero exit code")]
    NonzeroExitCode,
    #[error("not all outputs produced, missing: {}", .output)]
    MissingOutput { output: String },
    #[error("other error: {}", .message)]
    Other { message: String },
}

impl From<std::io::Error> for BuildFailure {
    fn from(err: std::io::Error) -> Self {
        Self::Other {
            message: err.to_string(),
        }
    }
}

impl From<anyhow::Error> for BuildFailure {
    fn from(err: anyhow::Error) -> Self {
        Self::Other {
            message: err.to_string(),
        }
    }
}

impl From<tonic::Status> for BuildFailure {
    fn from(value: tonic::Status) -> Self {
        Self::Other {
            message: format!("Tonic status: {value}"),
        }
    }
}

impl From<tokio::sync::AcquireError> for BuildFailure {
    fn from(err: tokio::sync::AcquireError) -> Self {
        Self::Other {
            message: format!("failed to acquire semaphore: {err}"),
        }
    }
}

impl<E: std::fmt::Display> From<snix_castore::import::IngestionError<E>> for BuildFailure {
    fn from(err: snix_castore::import::IngestionError<E>) -> Self {
        Self::Other {
            message: format!("Unable to ingest output: {err}"),
        }
    }
}

#[auto_impl(&, &mut, Arc, Box)]
pub trait BuildService: Send + Sync {
    // TODO: document
    fn do_build(&self, request: BuildRequest) -> BoxStream<'_, BuildUpdate>;
}
