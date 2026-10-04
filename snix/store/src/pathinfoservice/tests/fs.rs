//! Contains a testcase for castore/fs, using a PathInfoService as RootNodesProvider.

use snix_castore::{
    fs::{FSSettings, SnixStoreFs, fuse::FuseDaemon},
    utils::{gen_test_blob_engine, gen_test_directory_service},
};
use tempfile::TempDir;

use crate::pathinfoservice::{RedbPathInfoService, RedbPathInfoServiceConfig, RootNodesWrapper};

/// Regression test for #206.
/// Call opendir(), which calls `list()` or the `RootNodesProvider`, which some
/// implementations expect to happen in the context of a tokio runtime.
#[test]
fn list_async_pathinfoservice() {
    // https://plume.benboeckel.net/~/JustAnotherBlog/skipping-tests-in-rust
    if !std::path::Path::new("/dev/fuse").exists() {
        eprintln!("skipping test");
        return;
    }

    // Construct blob and directory services
    let blob_engine = gen_test_blob_engine();
    let directory_service = gen_test_directory_service();

    // Manually start a tokio runtime, we want most of this test to not be in
    // the context of the tokio runtime.
    let tokio_runtime = tokio::runtime::Runtime::new().expect("runtime to start");

    // Construct a redb pathinfo service, using a filesystem underneath.
    let tmpdir = TempDir::new().unwrap();
    let tmpdir_pathinfoservice = TempDir::new().unwrap();

    let p_mountpoint = tmpdir.path();

    let path_info_service = {
        let url = format!(
            "redb:{}",
            tmpdir_pathinfoservice
                .path()
                .join("pathinfos.redb")
                .to_string_lossy()
        )
        .parse::<url::Url>()
        .expect("URL to parse");

        tokio_runtime
            .block_on(tokio_runtime.spawn(async move {
                RedbPathInfoService::new(
                    "root".to_string(),
                    RedbPathInfoServiceConfig::try_from(url).expect("url to parse to config"),
                )
                .await
            }))
            .expect("task to finish")
            .expect("PathInfoService to be created")
    };

    let fs = SnixStoreFs::new(
        blob_engine,
        directory_service,
        RootNodesWrapper::from(path_info_service),
        FSSettings {
            list_root: true,
            ..Default::default()
        },
        tokio_runtime.handle().clone(),
    );

    let _fuse_daemon = tokio_runtime
        .block_on({
            let handle = tokio_runtime.handle();
            let p = p_mountpoint.to_path_buf();
            async move {
                handle
                    .spawn_blocking(move || FuseDaemon::new(fs, p, 4, false))
                    .await
            }
        })
        .expect("task to finish")
        .expect("mount to succeed");

    let it = std::fs::read_dir(p_mountpoint).expect("read_dir to succeed");
    let elems = it.collect::<Vec<_>>();

    // This should be an empty directory, and we got all the way here without panicking.
    assert_eq!(elems.len(), 0);
}
