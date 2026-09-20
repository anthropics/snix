use std::path::PathBuf;

use bstr::BStr;
use futures::{StreamExt, stream::FuturesOrdered};
use snix_castore::{
    blobservice::BlobService,
    directoryservice::DirectoryService,
    fs::fuse::FuseDaemon,
    import::fs::ingest_path,
    refscan::{ReferencePattern, ReferenceScanner},
};
use tonic::async_trait;
use tracing::{Span, debug, info, instrument, warn};
use uuid::Uuid;

use super::BuildService;
use crate::{
    buildservice::{BuildConstraints, BuildOutput, BuildRequest, BuildResult},
    bwrap::Bwrap,
    sandbox::{SandboxSpec, event::SandboxEvent},
};
const SANDBOX_SHELL: &str = env!("SNIX_BUILD_SANDBOX_SHELL");

pub struct BubblewrapBuildService<BS, DS> {
    /// Root path in which all builds run
    workdir: PathBuf,

    /// Handle to a [BlobService], used by filesystems spawned during builds.
    blob_service: BS,
    /// Handle to a [DirectoryService], used by filesystems spawned during builds.
    directory_service: DS,

    // semaphore to track number of concurrently running builds.
    // this is necessary, as otherwise we very quickly run out of open file handles.
    concurrent_builds: tokio::sync::Semaphore,
}
impl<BS, DS> BubblewrapBuildService<BS, DS> {
    pub fn new(workdir: PathBuf, blob_service: BS, directory_service: DS) -> Self {
        // We map root inside the container to the uid/gid this is running at,
        // and allocate one for uid 1000 into the container from the range we
        // got in /etc/sub{u,g}id.
        // FUTUREWORK: use different uids?
        Self {
            workdir,
            blob_service,
            directory_service,
            concurrent_builds: tokio::sync::Semaphore::new(2),
        }
    }
}

#[async_trait]
impl<BS, DS> BuildService for BubblewrapBuildService<BS, DS>
where
    BS: BlobService + Clone + 'static,
    DS: DirectoryService + Clone + 'static,
{
    #[instrument(skip_all, err, fields(build.name=tracing::field::Empty))]
    async fn do_build(&self, request: BuildRequest) -> std::io::Result<BuildResult> {
        let _permit = self.concurrent_builds.acquire().await.unwrap();

        let build_name = Uuid::new_v4();
        let sandbox_path = self.workdir.join(build_name.to_string());

        let span = Span::current();
        span.record("build.name", build_name.to_string());

        let blob_service = self.blob_service.clone();
        let directory_service = self.directory_service.clone();

        info!("Starting bwrap build");

        let spec = SandboxSpec::builder()
            .host_workdir(sandbox_path)
            .sandbox_workdir(request.working_dir)
            .scratches(request.scratch_paths)
            .command(request.command_args)
            .env_vars(request.environment_vars)
            .additional_files(request.additional_files)
            .with_inputs(request.inputs_dir, move |path| {
                let root_nodes = Box::new(request.inputs.clone());
                let fs = snix_castore::fs::SnixStoreFs::new(
                    blob_service.clone(),
                    directory_service.clone(),
                    root_nodes,
                    snix_castore::fs::FSSettings {
                        list_root: true,
                        uid_gid_override: None,
                        show_xattr: false,
                    },
                    tokio::runtime::Handle::current(),
                );
                // FUTUREWORK: make fuse daemon threads configurable?
                FuseDaemon::new(fs, path, 4, false)
            })
            .allow_network(
                request
                    .constraints
                    .contains(&BuildConstraints::NetworkAccess),
            )
            .provide_shell(
                request
                    .constraints
                    .contains(&BuildConstraints::ProvideBinSh)
                    .then_some(SANDBOX_SHELL.into()),
            )
            .build();

        // TODO(#132): stream results to the client once new streaming RPCs are in place.
        let (mut stream, finder) = Bwrap::initialize(spec)?.run()?;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_code = None;

        while let Some(event) = stream.next().await {
            match event {
                SandboxEvent::Stdout(chunk) => stdout.extend(chunk),
                SandboxEvent::Stderr(chunk) => stderr.extend(chunk),
                SandboxEvent::ExitCode(code) => exit_code = Some(code),
            }
        }

        let exit_code = exit_code.unwrap_or(1);
        if exit_code != 0 {
            let stdout = BStr::new(&stdout);
            let stderr = BStr::new(&stderr);

            warn!(stdout=%stdout, stderr=%stderr, exit_code=%exit_code, "build failed");

            return Err(std::io::Error::other("nonzero exit code".to_string()));
        }

        let output_paths = request
            .outputs
            .iter()
            .map(|o| finder.find_path(o))
            .collect::<FuturesOrdered<_>>()
            .filter_map(|e| async { e })
            .collect::<Vec<_>>()
            .await;

        if output_paths.len() != request.outputs.len() {
            warn!("Not all outputs produced");
            return Err(std::io::Error::other(
                "Not all outputs produced".to_string(),
            ));
        }

        let patterns = ReferencePattern::new(request.refscan_needles);

        let outputs = futures::future::try_join_all(output_paths.into_iter().enumerate().map(
            |(i, host_output_path)| {
                let output_path = &request.outputs[i];
                debug!(host.path=?host_output_path, output.path=?output_path, "ingesting path");
                let patterns = patterns.clone();
                async move {
                    let scanner = ReferenceScanner::new(patterns);
                    Ok::<_, std::io::Error>(BuildOutput {
                        node: ingest_path(
                            &self.blob_service,
                            &self.directory_service,
                            host_output_path,
                            Some(&scanner),
                        )
                        .await
                        .map_err(|e| {
                            std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                format!("Unable to ingest output: {e}"),
                            )
                        })?,

                        output_needles: scanner
                            .matches()
                            .into_iter()
                            .enumerate()
                            .filter(|(_, val)| *val)
                            .map(|(idx, _)| idx as u64)
                            .collect(),
                    })
                }
            },
        ))
        .await?;
        Ok(BuildResult { outputs })
    }
}
