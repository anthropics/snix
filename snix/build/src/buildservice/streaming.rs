use std::future::Future;

use futures::{
    StreamExt,
    stream::{BoxStream, FuturesUnordered},
};
use snix_castore::{
    blob_engine::BlobServiceEngine,
    blobservice::BlobService,
    directoryservice::DirectoryService,
    import::{IngestionError, fs::ingest_path},
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
    expected_outputs: Vec<String>,
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
    async_stream::try_stream! {
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

        let patterns = ReferencePattern::new(refscan_needles);

        let mut tasks : FuturesUnordered<_> = (expected_outputs.into_iter().enumerate().map(|(idx, o)| {
            let blob_service = &blob_service;
            let directory_service = &directory_service;
            let output_resolver = &output_resolver;
            let scanner = ReferenceScanner::new(patterns.clone());
            async move {
                let host_output_path = output_resolver.find_path(&o).await.ok_or(BuildFailure::MissingOutput{output: o})?;
                ingest_host_output(idx, &host_output_path, scanner, blob_service, directory_service).await.map_err(BuildFailure::from)
            }.in_current_span()
        })).collect();

        while let Some(elem) = tasks.next().await {
            yield elem?;
        }
    }
    .map(|res| match res {
        Ok(update) => update,
        Err(failure) => BuildUpdate::BuildFailure(failure),
    })
    .in_current_span()
    .boxed()
}

/// Ingests the given host output path, while running reference scanning.
/// Returns either a [BuildUpdate::ProducedOutput], or a [BuildFailure]
#[tracing::instrument(skip_all, err, fields(host.path = ?host_output_path))]
async fn ingest_host_output<BS, DS>(
    idx: usize,
    host_output_path: &std::path::Path,
    scanner: ReferenceScanner<String>,
    blob_service: BS,
    directory_service: DS,
) -> Result<BuildUpdate, IngestionError<snix_castore::import::fs::Error>>
where
    BS: BlobService,
    DS: DirectoryService,
{
    debug!("ingesting path");
    let node = ingest_path(
        BlobServiceEngine(&blob_service),
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

    Ok(BuildUpdate::ProducedOutput {
        node,
        idx: idx as u64,
        refscan_needles,
    })
}
