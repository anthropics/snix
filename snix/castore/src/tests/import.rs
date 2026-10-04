use crate::Node;
use crate::blob_engine::BlobEngine;
use crate::fixtures::*;
use crate::import::fs::ingest_path;
use crate::utils::{gen_test_blob_engine, gen_test_directory_service};

use tempfile::TempDir;

#[cfg(target_family = "unix")]
#[tokio::test]
async fn symlink() {
    let blob_engine = gen_test_blob_engine();
    let directory_service = gen_test_directory_service();

    let tmpdir = TempDir::new().unwrap();

    std::fs::create_dir_all(&tmpdir).unwrap();
    std::os::unix::fs::symlink(
        "/nix/store/somewhereelse",
        tmpdir.path().join("doesntmatter"),
    )
    .unwrap();

    let root_node = ingest_path::<_, _, _, &[u8]>(
        blob_engine,
        directory_service,
        tmpdir.path().join("doesntmatter"),
        None,
    )
    .await
    .expect("must succeed");

    assert_eq!(
        Node::Symlink {
            target: "/nix/store/somewhereelse".try_into().unwrap()
        },
        root_node,
    )
}

#[tokio::test]
async fn single_file() {
    let blob_engine = gen_test_blob_engine();
    let directory_service = gen_test_directory_service();

    let tmpdir = TempDir::new().unwrap();

    std::fs::write(tmpdir.path().join("root"), HELLOWORLD_BLOB_CONTENTS).unwrap();

    let root_node = ingest_path::<_, _, _, &[u8]>(
        blob_engine.clone(),
        directory_service,
        tmpdir.path().join("root"),
        None,
    )
    .await
    .expect("must succeed");

    assert_eq!(
        Node::File {
            digest: *HELLOWORLD_BLOB_DIGEST,
            size: HELLOWORLD_BLOB_CONTENTS.len() as u64,
            executable: false,
        },
        root_node,
    );

    // ensure the blob has been uploaded
    assert!(blob_engine.has(&HELLOWORLD_BLOB_DIGEST).await.unwrap());
}

#[cfg(target_family = "unix")]
#[tokio::test]
async fn complicated() {
    use crate::directoryservice::DirectoryService;

    let blob_engine = gen_test_blob_engine();
    let directory_service = gen_test_directory_service();

    let tmpdir = TempDir::new().unwrap();

    // File ``.keep`
    std::fs::write(tmpdir.path().join(".keep"), vec![]).unwrap();
    // Symlink `aa`
    std::os::unix::fs::symlink("/nix/store/somewhereelse", tmpdir.path().join("aa")).unwrap();
    // Directory `keep`
    std::fs::create_dir(tmpdir.path().join("keep")).unwrap();
    // File ``keep/.keep`
    std::fs::write(tmpdir.path().join("keep").join(".keep"), vec![]).unwrap();

    let root_node =
        ingest_path::<_, _, _, &[u8]>(blob_engine.clone(), &directory_service, tmpdir.path(), None)
            .await
            .expect("must succeed");

    // ensure root_node matched expectations
    assert_eq!(
        Node::Directory {
            digest: DIRECTORY_COMPLICATED.digest(),
            size: DIRECTORY_COMPLICATED.size(),
        },
        root_node,
    );

    // ensure DIRECTORY_WITH_KEEP and DIRECTORY_COMPLICATED have been uploaded
    assert!(
        directory_service
            .get(&DIRECTORY_WITH_KEEP.digest())
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        directory_service
            .get(&DIRECTORY_COMPLICATED.digest())
            .await
            .unwrap()
            .is_some()
    );

    // ensure EMPTY_BLOB_CONTENTS has been uploaded
    assert!(blob_engine.has(&EMPTY_BLOB_DIGEST).await.unwrap());
}
