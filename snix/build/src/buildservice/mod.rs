use auto_impl::auto_impl;
use futures::{StreamExt, stream::BoxStream};
use tonic::async_trait;

pub mod build_request;
pub use crate::buildservice::build_request::*;
mod dummy;
mod from_addr;
mod grpc;

#[cfg(target_os = "linux")]
mod oci;

#[cfg(target_os = "linux")]
mod bwrap;

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

#[derive(thiserror::Error, Debug)]
pub enum BuildFailure {
    #[error("nonzero exit code")]
    NonzeroExitCode,
    #[error("not all outputs produced")]
    MissingOutputs,
    #[error("other error: {}", .message)]
    Other { message: String },
}

#[async_trait]
#[auto_impl(&, &mut, Arc, Box)]
pub trait BuildService: Send + Sync {
    /// TODO: document
    async fn do_build(&self, request: BuildRequest) -> std::io::Result<BuildResult>;

    // TODO: call this do_build and drop default impl once everything is streaming
    fn do_build_streaming(&self, request: BuildRequest) -> BoxStream<'_, BuildUpdate> {
        async_stream::stream! {
            let rq_num_outputs = request.outputs.len();

            // NOTE: The unary do_build can't send stdout/stderr at all currently.

            match self.do_build(request).await {
                Ok(build_result) => {
                    if build_result.outputs.len() != rq_num_outputs {
                        yield BuildUpdate::BuildFailure(BuildFailure::MissingOutputs)
                    }

                    for (
                        idx,
                        BuildOutput {
                            node,
                            output_needles,
                        },
                    ) in build_result.outputs.into_iter().enumerate()
                    {
                        yield BuildUpdate::ProducedOutput {
                            node,
                            idx: idx as u64,
                            refscan_needles: output_needles,
                        };
                    }
                }
                Err(err) => {
                    yield BuildUpdate::BuildFailure(BuildFailure::Other {
                        message: err.to_string(),
                    });
                }
            }
        }
        .boxed()
    }
}
