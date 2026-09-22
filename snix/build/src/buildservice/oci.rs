use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Stdio,
};

use anyhow::Context;
use futures::stream::BoxStream;
use snix_castore::{
    blobservice::BlobService, directoryservice::DirectoryService, fs::fuse::FuseDaemon,
};
use tokio::process::{Child, Command};
use tonic::async_trait;
use tracing::{Span, debug, instrument};
use uuid::Uuid;

use super::{BuildFailure, BuildResult, BuildService, BuildUpdate};
use crate::sandbox::event::{SandboxEvent, stream_process};
use crate::{buildservice::BuildRequest, oci::OciOutputs};
use crate::{
    buildservice::streaming::run_build_streaming,
    oci::{make_bundle, make_spec},
};

const SANDBOX_SHELL: &str = env!("SNIX_BUILD_SANDBOX_SHELL");
const MAX_CONCURRENT_BUILDS: usize = 2; // TODO: make configurable

pub struct OCIBuildService<BS, DS> {
    /// Root path in which all bundles are created in
    bundle_root: PathBuf,

    /// Handle to a [BlobService], used by filesystems spawned during builds.
    blob_service: BS,
    /// Handle to a [DirectoryService], used by filesystems spawned during builds.
    directory_service: DS,

    // semaphore to track number of concurrently running builds.
    // this is necessary, as otherwise we very quickly run out of open file handles.
    concurrent_builds: tokio::sync::Semaphore,
}

impl<BS, DS> OCIBuildService<BS, DS> {
    pub fn new(bundle_root: PathBuf, blob_service: BS, directory_service: DS) -> Self {
        // We map root inside the container to the uid/gid this is running at,
        // and allocate one for uid 1000 into the container from the range we
        // got in /etc/sub{u,g}id.
        // FUTUREWORK: use different uids?
        Self {
            bundle_root,
            blob_service,
            directory_service,
            concurrent_builds: tokio::sync::Semaphore::new(MAX_CONCURRENT_BUILDS),
        }
    }
}

impl<BS, DS> OCIBuildService<BS, DS>
where
    BS: BlobService + Clone + 'static,
    DS: DirectoryService + Clone + 'static,
{
    /// Assembles an OCI container environment and prepares its execution
    ///
    /// Returns a stream which emits events as the build progresses and a SandboxOutputs handle
    /// which can be used by callers to access build output paths relative to the logical root of
    /// the sandbox. SandboxOutputs.find_paths are only guaranteed to succeed after the build has
    /// finished, in other words the stream has ended.
    async fn spawn_bundle_process(
        &self,
        request: &BuildRequest,
        bundle_path: &Path,
        build_name: &str,
    ) -> Result<(BoxStream<'static, SandboxEvent>, OciOutputs), BuildFailure> {
        let outputs = OciOutputs::new(bundle_path, &request.scratch_paths)?;
        let mut runtime_spec =
            make_spec(request, true, SANDBOX_SHELL).context("failed to create spec")?;

        let linux = runtime_spec.linux().clone().unwrap();
        runtime_spec.set_linux(Some(linux));

        make_bundle(request, &runtime_spec, bundle_path).context("failed to produce bundle")?;

        let blob_service = self.blob_service.clone();
        let directory_service = self.directory_service.clone();
        let dest = bundle_path.join("inputs");
        let root_nodes = Box::new(request.inputs.clone());

        let fuse_daemon = tokio::task::spawn_blocking(move || {
            let fs = snix_castore::fs::SnixStoreFs::new(
                blob_service,
                directory_service,
                root_nodes,
                snix_castore::fs::FSSettings {
                    list_root: true,
                    uid_gid_override: None,
                    show_xattr: false,
                },
                tokio::runtime::Handle::current(),
            );
            FuseDaemon::new(fs, dest, 4, true).context("failed to start fuse daemon")
        })
        .await
        .map_err(|e| BuildFailure::Other {
            message: e.to_string(),
        })??;

        debug!(bundle.path=?bundle_path, "about to spawn bundle");

        let child = spawn_bundle(bundle_path, build_name)?;
        let stream = stream_process(child, fuse_daemon)?;
        Ok((stream, outputs))
    }
}

#[async_trait]
impl<BS, DS> BuildService for OCIBuildService<BS, DS>
where
    BS: BlobService + Clone + 'static,
    DS: DirectoryService + Clone + 'static,
{
    #[instrument(skip_all, fields(build.name=tracing::field::Empty))]
    fn do_build_streaming(&self, request: BuildRequest) -> BoxStream<'_, BuildUpdate> {
        let span = Span::current();
        let build_name = Uuid::new_v4();
        let bundle_path = self.bundle_root.join(build_name.to_string());
        span.record("build.name", build_name.to_string());

        run_build_streaming(
            &self.concurrent_builds,
            self.blob_service.clone(),
            self.directory_service.clone(),
            request.outputs.clone(),
            request.refscan_needles.clone(),
            move || async move {
                self.spawn_bundle_process(&request, &bundle_path, &build_name.to_string())
                    .await
            },
        )
    }

    #[instrument(skip_all, err, fields(build.name=tracing::field::Empty))]
    async fn do_build(&self, request: BuildRequest) -> std::io::Result<BuildResult> {
        let stream = self.do_build_streaming(request);
        BuildResult::try_from_build_updates(stream)
            .await
            .map_err(std::io::Error::other)
    }
}

/// Spawns runc with the bundle at bundle_path.
/// On success, returns the child.
#[instrument(err)]
fn spawn_bundle(
    bundle_path: impl AsRef<OsStr> + std::fmt::Debug,
    bundle_name: &str,
) -> std::io::Result<Child> {
    let mut command = Command::new("runc");

    command
        .args(&[
            "run".into(),
            "--bundle".into(),
            bundle_path.as_ref().to_os_string(),
            bundle_name.into(),
        ])
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);

    command.spawn()
}
