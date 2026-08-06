use crate::fixtures::{
    CASTORE_NODE_COMPLICATED, CASTORE_NODE_HELLOWORLD, CASTORE_NODE_SYMLINK, CASTORE_NODE_TOO_BIG,
    CASTORE_NODE_TOO_SMALL,
};
use crate::fixtures::{NAR_CONTENTS_COMPLICATED, NAR_CONTENTS_HELLOWORLD, NAR_CONTENTS_SYMLINK};
use crate::nar::seekable::Reader;
use futures::StreamExt;
use mockall::predicate;
use snix_castore::Node;
use snix_castore::blobservice::MockBlobService;
use snix_castore::directoryservice::{self, MockDirectoryService};
use snix_castore::fixtures::{
    DIRECTORY_COMPLICATED, DIRECTORY_WITH_KEEP, HELLOWORLD_BLOB_CONTENTS, HELLOWORLD_BLOB_DIGEST,
};
use std::io::{self, Cursor, ErrorKind};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

#[rstest::rstest]
#[case::symlink(&*CASTORE_NODE_SYMLINK, &NAR_CONTENTS_SYMLINK,
    |bs: &mut MockBlobService|{bs.expect_open_read().never();},
    |ds: &mut MockDirectoryService| {ds.expect_get().never(); ds.expect_get_recursive().never();})]
#[case::helloworld(&*CASTORE_NODE_HELLOWORLD, &NAR_CONTENTS_HELLOWORLD,
    |bs: &mut MockBlobService|{bs.expect_open_read().once().with(predicate::eq(&*HELLOWORLD_BLOB_DIGEST)).returning(|_| {
        Ok(Some(Box::new(Cursor::new(HELLOWORLD_BLOB_CONTENTS))))
    });},
    |ds: &mut MockDirectoryService| {ds.expect_get().never(); ds.expect_get_recursive().never();})]
#[case::complicated(&*CASTORE_NODE_COMPLICATED, &NAR_CONTENTS_COMPLICATED,
    // the closure only refers to the empty blob, which we don't send requests for.
    |bs: &mut MockBlobService|{bs.expect_open_read().never();},
    |ds: &mut MockDirectoryService| {ds.expect_get_recursive().once().with(predicate::eq(DIRECTORY_COMPLICATED.digest())).returning(|_| {
        futures::stream::iter([
            &*DIRECTORY_COMPLICATED, &*DIRECTORY_WITH_KEEP
        ].map(|e| {Ok::<_, directoryservice::Error>(e.to_owned())})).boxed()
    });})]
#[tokio::test]
async fn read(
    #[case] root_node: &Node,
    #[case] expected_nar: &[u8],
    #[case] blobservice_fn: impl Fn(&mut MockBlobService),
    #[case] directoryservice_fn: impl Fn(&mut MockDirectoryService),
) {
    for case in 0..1 {
        // setup services and reader
        let mut blob_service = MockBlobService::new();
        blobservice_fn(&mut blob_service);
        let mut directory_service = MockDirectoryService::new();
        directoryservice_fn(&mut directory_service);
        let mut reader = Reader::new(root_node, &blob_service, directory_service)
            .await
            .expect("constructing reader to succeed");

        match case {
            // AsyncRead
            0 => {
                let mut buf = vec![];
                tokio::io::copy(&mut reader, &mut buf)
                    .await
                    .expect("copy_buf to succeed");
                assert_eq!(&expected_nar, &buf, "expect NAR to match");
            }
            // AsyncBufRead
            1 => {
                let mut buf = vec![];
                tokio::io::copy_buf(&mut reader, &mut buf)
                    .await
                    .expect("copy_buf to succeed");

                assert_eq!(&expected_nar, &buf, "expect NAR to match");
            }
            n => unreachable!("unhandled test iteration: {n}"),
        }
    }
}

#[tokio::test]
/// Renders a NAR with a root node signalling a size larger than what the blob actually is, ensuring we fail.
async fn detect_too_big() {
    let mut blob_service = MockBlobService::new();
    blob_service
        .expect_open_read()
        .once()
        .with(predicate::eq(&*HELLOWORLD_BLOB_DIGEST))
        .returning(|_| Ok(Some(Box::new(Cursor::new(HELLOWORLD_BLOB_CONTENTS)))));

    let mut reader = Reader::new(
        &CASTORE_NODE_TOO_BIG,
        &blob_service,
        MockDirectoryService::new(),
    )
    .await
    .expect("constructing reader to succeed");

    let err = tokio::io::copy(&mut reader, &mut tokio::io::sink())
        .await
        .expect_err("expect reading to fail");
    assert_eq!(
        io::ErrorKind::InvalidData,
        err.kind(),
        "expected to get EOF"
    )
}

#[tokio::test]
/// Renders a NAR with a root node signalling a size smaller than what the blob actually is, ensuring we fail.
async fn detect_too_small() {
    let mut blob_service = MockBlobService::new();
    blob_service
        .expect_open_read()
        .once()
        .with(predicate::eq(&*HELLOWORLD_BLOB_DIGEST))
        .returning(|_| Ok(Some(Box::new(Cursor::new(HELLOWORLD_BLOB_CONTENTS)))));

    let mut reader = Reader::new(
        &CASTORE_NODE_TOO_SMALL,
        &blob_service,
        MockDirectoryService::new(),
    )
    .await
    .expect("constructing reader to succeed");

    let err = tokio::io::copy(&mut reader, &mut tokio::io::sink())
        .await
        .expect_err("expect reading to fail");
    assert_eq!(
        io::ErrorKind::InvalidData,
        err.kind(),
        "expected to get invalid data"
    )
}

#[tokio::test]
/// Make sure it fails if a referred blob doesn't exist.
async fn single_file_missing_blob() {
    let mut blob_service = MockBlobService::new();
    blob_service
        .expect_open_read()
        .once()
        .return_once(|_| Ok(None));

    let mut reader = Reader::new(
        &CASTORE_NODE_HELLOWORLD,
        &blob_service,
        MockDirectoryService::new(),
    )
    .await
    .expect("constructing reader to succeed");

    let err = tokio::io::copy(&mut reader, &mut tokio::io::sink())
        .await
        .expect_err("copy to fail");
    assert_eq!(ErrorKind::NotFound, err.kind());
}

#[tokio::test]
async fn seek() {
    let mut blob_service = MockBlobService::new();
    blob_service
        .expect_open_read()
        .with(predicate::eq(&*HELLOWORLD_BLOB_DIGEST))
        .returning(|_| Ok(Some(Box::new(Cursor::new(HELLOWORLD_BLOB_CONTENTS)))));

    let mut reader = Reader::new(
        &crate::fixtures::CASTORE_NODE_HELLOWORLD,
        &blob_service,
        &MockDirectoryService::new(),
    )
    .await
    .expect("must succeed");

    let mut buf = [0u8; 10];

    // FUTUREWORK: This could probably be a proptest…
    for position in [
        io::SeekFrom::Start(0x65), // Just before the file contents
        io::SeekFrom::Start(0x68), // Seek back the file contents
        io::SeekFrom::Start(0x70), // Just before the end of the file contents
    ] {
        let n = reader.seek(position).await.expect("seek to succeed") as usize;
        reader.read_exact(&mut buf).await.expect("read_exact");
        assert_eq!(
            crate::fixtures::NAR_CONTENTS_HELLOWORLD[n..n + 10],
            buf,
            "expect slice to match"
        );
    }
}
