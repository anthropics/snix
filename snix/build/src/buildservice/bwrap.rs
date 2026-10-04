use std::path::{Path, PathBuf};

use futures::stream::BoxStream;
use snix_castore::{
    blob_engine::BlobServiceEngine, blobservice::BlobService, directoryservice::DirectoryService,
    fs::fuse::FuseDaemon,
};
use tracing::{Span, info, instrument};
use uuid::Uuid;

use super::BuildService;
use crate::{
    buildservice::{BuildConstraints, BuildRequest, BuildUpdate, streaming::run_build_streaming},
    bwrap::Bwrap,
    sandbox::SandboxSpec,
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

impl<BS, DS> BubblewrapBuildService<BS, DS>
where
    BS: BlobService + Clone + 'static,
    DS: DirectoryService + Clone + 'static,
{
    fn make_spec(&self, request: BuildRequest, sandbox_path: &Path) -> SandboxSpec {
        let blob_service = self.blob_service.clone();
        let directory_service = self.directory_service.clone();

        SandboxSpec::builder()
            .host_workdir(sandbox_path.to_path_buf())
            .sandbox_workdir(request.working_dir)
            .scratches(request.scratch_paths)
            .command(request.command_args)
            .env_vars(request.environment_vars)
            .additional_files(request.additional_files)
            .with_inputs(request.inputs_dir, move |path| {
                let root_nodes = Box::new(request.inputs.clone());
                let fs = snix_castore::fs::SnixStoreFs::new(
                    BlobServiceEngine(blob_service.clone()),
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
            .build()
    }
}

impl<BS, DS> BuildService for BubblewrapBuildService<BS, DS>
where
    BS: BlobService + Clone + 'static,
    DS: DirectoryService + Clone + 'static,
{
    #[instrument(skip_all, fields(build.name=tracing::field::Empty))]
    fn do_build(&self, request: BuildRequest) -> BoxStream<'_, BuildUpdate> {
        let span = Span::current();
        let build_name = Uuid::new_v4();
        let sandbox_path = self.workdir.join(build_name.to_string());
        span.record("build.name", build_name.to_string());

        let outputs = request.outputs.clone();
        let needles = request.refscan_needles.clone();

        let spec = self.make_spec(request, &sandbox_path);

        run_build_streaming(
            &self.concurrent_builds,
            self.blob_service.clone(),
            self.directory_service.clone(),
            outputs,
            needles,
            move || async move {
                info!("Starting bwrap build");

                let (stream, finder) = Bwrap::initialize(spec)?.run()?;
                Ok((stream, finder))
            },
        )
    }
}
