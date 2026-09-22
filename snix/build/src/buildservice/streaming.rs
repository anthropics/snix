use std::{future::Future, path::PathBuf};

use futures::{StreamExt, stream::BoxStream};
use snix_castore::{
    blobservice::BlobService,
    directoryservice::DirectoryService,
    import::fs::ingest_path,
    refscan::{ReferencePattern, ReferenceScanner},
};
use tracing::{debug, warn};
use tracing_futures::Instrument;

use super::{BuildFailure, BuildUpdate};
use crate::sandbox::{SandboxOutputs, event::SandboxEvent};

/// Executes a sandboxed build and streams its progress and outputs.
pub(crate) fn run_build_streaming<'a, BS, DS, R, SpawnFut>(
    semaphore: &'a tokio::sync::Semaphore,
    blob_service: BS,
    directory_service: DS,
    expected_outputs: Vec<PathBuf>,
    refscan_needles: Vec<String>,
    spawn: impl FnOnce() -> SpawnFut + Send + 'a,
) -> BoxStream<'a, BuildUpdate>
where
    BS: BlobService + Clone + 'a,
    DS: DirectoryService + Clone + 'a,
    R: SandboxOutputs + Send + Sync + 'static,
    SpawnFut:
        Future<Output = Result<(BoxStream<'static, SandboxEvent>, R), BuildFailure>> + Send + 'a,
{
    let stream = async_stream::try_stream! {
        let _permit = semaphore.acquire().await?;

        let (mut event_stream, output_resolver) = spawn().await?;

        let mut exit_code = None;
        while let Some(event) = event_stream.next().await {
            match event {
                SandboxEvent::Stdout(chunk) => yield BuildUpdate::ProducedStdout(chunk),
                SandboxEvent::Stderr(chunk) => yield BuildUpdate::ProducedStderr(chunk),
                SandboxEvent::ExitCode(code) => exit_code = Some(code),
            }
        }

        let exit_code = exit_code.unwrap_or(1);
        if exit_code != 0 {
            warn!(exit_code=%exit_code, "build failed");
            Err(BuildFailure::NonzeroExitCode)?;
        }

        let host_output_paths = futures::future::try_join_all(expected_outputs.into_iter().map(|o| {
            let output_resolver = &output_resolver;
            async move {
                output_resolver.find_path(&o).await.ok_or(BuildFailure::MissingOutputs)
            }
        })).await?;

        let patterns = ReferencePattern::new(refscan_needles);
        for (idx, host_output_path) in host_output_paths.into_iter().enumerate() {
            debug!(host.path=?host_output_path, idx, "ingesting path");
            let scanner = ReferenceScanner::new(patterns.clone());
            let node = ingest_path(
                &blob_service,
                &directory_service,
                host_output_path,
                Some(&scanner),
            )
            .await?;

            let refscan_needles = scanner
                .matches()
                .into_iter()
                .enumerate()
                .filter(|(_, val)| *val)
                .map(|(idx, _)| idx as u64)
                .collect();

            yield BuildUpdate::ProducedOutput {
                node,
                idx: idx as u64,
                refscan_needles,
            };
        }
    };

    stream
        .map(|res| match res {
            Ok(update) => update,
            Err(failure) => BuildUpdate::BuildFailure(failure),
        })
        .instrument(tracing::Span::current())
        .boxed()
}
