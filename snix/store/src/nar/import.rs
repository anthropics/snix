use nix_compat::{
    nar::reader::r#async as nar_reader,
    nixhash::{CAHash, HashAlgo, NixHash, NixHashDigester, Sha256Digester, copy_hashed},
};
use snix_castore::{
    Node, PathBuf,
    blob_engine::{self, BlobEngine, concurrent_uploads},
    directoryservice::DirectoryService,
    import::{IngestionEntry, IngestionError, ingest_entries},
};
use tokio::{
    io::{AsyncBufRead, AsyncRead},
    sync::mpsc,
    try_join,
};
use tokio_util::io::InspectReader;

/// Represents errors that can happen during nar ingestion.
#[derive(Debug, thiserror::Error)]
pub enum NarIngestionError {
    #[error("{0}")]
    IngestionError(#[from] IngestionError<Error>),

    #[error("Hash mismatch, expected: {expected}, got: {actual}.")]
    HashMismatch { expected: NixHash, actual: NixHash },

    #[error("Expected the nar to contain a single file.")]
    TypeMismatch,

    #[error("Ingestion failed: {0}")]
    Io(#[from] std::io::Error),

    #[error("Calling open_read() on BlobEngine failed: {0}")]
    BlobEngine(#[from] blob_engine::Error),
}

/// Ingests the contents from a [AsyncRead] providing NAR into the snix store,
/// interacting with a [BlobEngine] and [DirectoryService].
/// Returns the castore root node, as well as the sha256 and size of the NAR
/// contents ingested.
pub async fn ingest_nar_and_hash<R, BE, DS>(
    blob_engine: BE,
    directory_service: DS,
    r: &mut R,
    expected_cahash: &Option<CAHash>,
) -> Result<(Node, [u8; 32], u64), NarIngestionError>
where
    R: AsyncRead + Unpin + Send,
    BE: BlobEngine + Clone + 'static,
    DS: DirectoryService,
{
    let mut nar_hash = Sha256Digester::new();
    let mut nar_size = 0;

    // Assemble NarHash and NarSize as we read bytes.
    let mut r = tokio_util::io::InspectReader::new(r, |b| {
        nar_size += b.len() as u64;
        nar_hash.update(b);
    });

    match expected_cahash {
        Some(CAHash::Nar(expected_hash)) => {
            let (root_node, actual_hash, nar_hash) = if expected_hash.algo() == HashAlgo::Sha256 {
                // If this is the required CAHash, we're already computing excatly this in `nar_hash` above.
                let mut r = tokio::io::BufReader::new(&mut r);

                let root_node = ingest_nar(blob_engine, directory_service, &mut r).await?;
                let nar_hash = nar_hash.finalize();
                (root_node, NixHash::from(nar_hash), nar_hash)
            } else {
                // For the other algos, wrap the reader with another digester.
                let mut digester = NixHashDigester::new(expected_hash.algo());
                let mut r =
                    tokio::io::BufReader::new(InspectReader::new(r, |data| digester.update(data)));

                let root_node = ingest_nar(blob_engine, directory_service, &mut r).await?;
                (root_node, digester.finalize(), nar_hash.finalize())
            };

            if actual_hash != *expected_hash {
                return Err(NarIngestionError::HashMismatch {
                    expected: expected_hash.clone(),
                    actual: actual_hash,
                });
            }
            Ok((root_node, nar_hash.into(), nar_size))
        }
        Some(CAHash::Flat(expected_hash)) => {
            // ingest as NAR
            let mut r = tokio::io::BufReader::new(&mut r);
            let root_node = ingest_nar(blob_engine.clone(), directory_service, &mut r).await?;

            // The resulting root node must be Node::File, else CAHash::Flat is not applicable
            if let Node::File { digest, size, .. } = &root_node {
                if let Some(mut blob_reader) = blob_engine.open_read(digest, Some(*size)).await? {
                    let (_, actual_hash) = copy_hashed(
                        &mut blob_reader,
                        &mut tokio::io::sink(),
                        expected_hash.algo(),
                    )
                    .await?;

                    if actual_hash != *expected_hash {
                        return Err(NarIngestionError::HashMismatch {
                            expected: expected_hash.clone(),
                            actual: actual_hash,
                        });
                    }
                    Ok((root_node, nar_hash.finalize().into(), nar_size))
                } else {
                    Err(NarIngestionError::Io(std::io::Error::other(
                        "Ingested data not found",
                    )))
                }
            } else {
                Err(NarIngestionError::TypeMismatch)
            }
        }
        // We either got CAHash::Text, or no CAHash at all, so we just don't do any additional
        // hash calculation/validation.
        // FUTUREWORK: We should figure out what to do with CAHash::Text, according to nix-cpp
        // they don't handle it either:
        // https://github.com/NixOS/nix/blob/3e9cc78eb5e5c4f1e762e201856273809fd92e71/src/libstore/local-store.cc#L1099-L1133
        _ => {
            let mut r = tokio::io::BufReader::new(&mut r);
            let root_node = ingest_nar(blob_engine, directory_service, &mut r).await?;
            Ok((root_node, nar_hash.finalize().into(), nar_size))
        }
    }
}

/// Ingests the contents from a [AsyncRead] providing NAR into the snix store,
/// interacting with a [BlobEngine] and [DirectoryService].
/// It returns the castore root node or an error.
pub async fn ingest_nar<R, BE, DS>(
    blob_engine: BE,
    directory_service: DS,
    r: &mut R,
) -> Result<Node, IngestionError<Error>>
where
    R: AsyncBufRead + Unpin + Send,
    BE: BlobEngine + Clone + 'static,
    DS: DirectoryService,
{
    // open the NAR for reading.
    // The NAR reader emits nodes in DFS preorder.
    let root_node = nar_reader::open(r).await.map_err(Error::IO)?;

    let (tx, rx) = mpsc::channel(1);
    let rx = tokio_stream::wrappers::ReceiverStream::new(rx);

    let produce = async move {
        let mut blob_uploader = concurrent_uploads::ConcurrentBlobUploader::new(blob_engine);

        let res = produce_nar_inner(
            &mut blob_uploader,
            root_node,
            "root".parse().unwrap(), // HACK: the root node sent to ingest_entries may not be ROOT.
            tx.clone(),
        )
        .await;

        if let Err(err) = blob_uploader.join().await {
            tx.send(Err(err.into()))
                .await
                .map_err(|e| Error::IO(std::io::Error::new(std::io::ErrorKind::BrokenPipe, e)))?;
        }

        tx.send(res)
            .await
            .map_err(|e| Error::IO(std::io::Error::new(std::io::ErrorKind::BrokenPipe, e)))?;

        Ok(())
    };

    let consume = ingest_entries(directory_service, rx);

    let (_, node) = try_join!(produce, consume)?;

    Ok(node)
}

async fn produce_nar_inner<BE>(
    blob_uploader: &mut concurrent_uploads::ConcurrentBlobUploader<BE>,
    node: nar_reader::Node<'_, '_>,
    path: PathBuf,
    tx: mpsc::Sender<Result<IngestionEntry, Error>>,
) -> Result<IngestionEntry, Error>
where
    BE: BlobEngine + Clone + 'static,
{
    Ok(match node {
        nar_reader::Node::Symlink { target } => IngestionEntry::Symlink { path, target },
        nar_reader::Node::File {
            executable,
            mut reader,
        } => {
            let size = reader.len();
            let digest = blob_uploader.upload(&path, size, &mut reader).await?;

            IngestionEntry::Regular {
                path,
                size,
                executable,
                digest,
            }
        }
        nar_reader::Node::Directory(mut dir_reader) => {
            while let Some(entry) = dir_reader.next().await? {
                let mut path = path.clone();

                // valid NAR names are valid castore names
                path.try_push(entry.name)
                    .expect("Snix bug: failed to join name");

                let entry = Box::pin(produce_nar_inner(
                    blob_uploader,
                    entry.node,
                    path,
                    tx.clone(),
                ))
                .await?;

                tx.send(Ok(entry)).await.map_err(|e| {
                    Error::IO(std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))
                })?;
            }

            IngestionEntry::Dir { path }
        }
    })
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    IO(#[from] std::io::Error),

    #[error(transparent)]
    BlobUpload(#[from] concurrent_uploads::Error),
}

#[cfg(test)]
mod test {
    use crate::fixtures::{
        NAR_CONTENTS_COMPLICATED, NAR_CONTENTS_HELLOWORLD, NAR_CONTENTS_SYMLINK,
    };
    use crate::nar::{NarIngestionError, ingest_nar, ingest_nar_and_hash};
    use std::io::Cursor;
    use std::sync::Arc;

    use hex_literal::hex;
    use mockall::predicate;
    use nix_compat::nixhash::{CAHash, NixHash};
    use rstest::*;
    use snix_castore::Node;
    use snix_castore::blobservice::{MockBlobService, TestBlobWriter};
    use snix_castore::directoryservice::{MockDirectoryPutter, MockDirectoryService};
    use snix_castore::fixtures::{
        DIRECTORY_COMPLICATED, DIRECTORY_WITH_KEEP, EMPTY_BLOB_DIGEST, HELLOWORLD_BLOB_CONTENTS,
        HELLOWORLD_BLOB_DIGEST,
    };
    use snix_castore::utils::gen_test_blob_service;

    #[tokio::test]
    async fn single_symlink() {
        let root_node = ingest_nar(
            Arc::new(MockBlobService::new()),
            MockDirectoryService::new(),
            &mut Cursor::new(&NAR_CONTENTS_SYMLINK),
        )
        .await
        .expect("must parse");

        assert_eq!(
            Node::Symlink {
                target: "/nix/store/somewhereelse".try_into().unwrap()
            },
            root_node
        );
    }

    #[tokio::test]
    async fn single_file() {
        let mut blob_service = MockBlobService::new();
        let mut seq = mockall::Sequence::new();
        blob_service
            .expect_has()
            .once()
            .with(predicate::eq(&*HELLOWORLD_BLOB_DIGEST))
            .return_once(|_| Ok(false))
            .in_sequence(&mut seq);

        blob_service
            .expect_open_write()
            .once()
            .return_once(|| Box::new(TestBlobWriter::new()))
            .in_sequence(&mut seq);

        let root_node = ingest_nar(
            Arc::new(blob_service),
            MockDirectoryService::new(),
            &mut Cursor::new(&NAR_CONTENTS_HELLOWORLD),
        )
        .await
        .expect("must parse");

        assert_eq!(
            Node::File {
                digest: *HELLOWORLD_BLOB_DIGEST,
                size: HELLOWORLD_BLOB_CONTENTS.len() as u64,
                executable: false,
            },
            root_node
        );
    }

    #[tokio::test]
    async fn complicated() {
        let mut blob_service = MockBlobService::new();
        let mut seq = mockall::Sequence::new();
        blob_service
            .expect_has()
            .once()
            .with(predicate::eq(&*EMPTY_BLOB_DIGEST))
            .return_once(|_| Ok(false))
            .in_sequence(&mut seq);
        blob_service
            .expect_open_write()
            .once()
            .return_once(|| Box::new(TestBlobWriter::new()))
            .in_sequence(&mut seq);
        blob_service
            .expect_has()
            .once()
            .with(predicate::eq(&*EMPTY_BLOB_DIGEST))
            .return_once(|_| Ok(true))
            .in_sequence(&mut seq);
        let mut directory_service = MockDirectoryService::new();
        directory_service
            .expect_put_multiple_start()
            .once()
            .return_once(|| {
                let mut directory_putter = MockDirectoryPutter::new();
                let mut seq = mockall::Sequence::new();
                directory_putter
                    .expect_put()
                    .once()
                    .with(predicate::eq(&*DIRECTORY_WITH_KEEP))
                    .returning(|_| Ok(()))
                    .in_sequence(&mut seq);
                directory_putter
                    .expect_put()
                    .once()
                    .with(predicate::eq(&*DIRECTORY_COMPLICATED))
                    .returning(|_| Ok(()))
                    .in_sequence(&mut seq);
                directory_putter
                    .expect_close()
                    .once()
                    .returning(|| Ok(DIRECTORY_COMPLICATED.digest()))
                    .in_sequence(&mut seq);
                Box::new(directory_putter)
            });

        let root_node = ingest_nar(
            Arc::new(blob_service),
            directory_service,
            &mut Cursor::new(&NAR_CONTENTS_COMPLICATED),
        )
        .await
        .expect("must parse");

        assert_eq!(
            Node::Directory {
                digest: DIRECTORY_COMPLICATED.digest(),
                size: DIRECTORY_COMPLICATED.size()
            },
            root_node,
        );
    }

    #[rstest]
    #[case::nar_sha256(Some(CAHash::Nar(NixHash::Sha256(hex!("fbd52279a8df024c9fd5718de4103bf5e760dc7f2cf49044ee7dea87ab16911a")))), NAR_CONTENTS_COMPLICATED.as_slice())]
    #[case::nar_sha512(Some(CAHash::Nar(NixHash::Sha512(Box::new(hex!("ff5d43941411f35f09211f8596b426ee6e4dd3af1639e0ed2273cbe44b818fc4a59e3af02a057c5b18fbfcf435497de5f1994206c137f469b3df674966a922f0"))))), NAR_CONTENTS_COMPLICATED.as_slice())]
    #[case::flat_md5(Some(CAHash::Flat(NixHash::Md5(hex!("fd076287532e86365e841e92bfc50d8c")))), NAR_CONTENTS_HELLOWORLD.as_slice() )]
    #[case::nar_symlink_sha1(Some(CAHash::Nar(NixHash::Sha1(hex!("f24eeaaa9cc016bab030bf007cb1be6483e7ba9e")))), NAR_CONTENTS_SYMLINK.as_slice())]
    #[tokio::test]
    async fn ingest_with_cahash_mismatch(
        #[case] ca_hash: Option<CAHash>,
        #[case] nar_content: &[u8],
    ) {
        use snix_castore::utils::gen_test_directory_service;

        let err = ingest_nar_and_hash(
            gen_test_blob_service(),
            gen_test_directory_service(),
            &mut Cursor::new(nar_content),
            &ca_hash,
        )
        .await
        .expect_err("Ingestion should have failed");
        assert!(
            matches!(err, NarIngestionError::HashMismatch { .. }),
            "CAHash should have mismatched"
        );
    }

    #[rstest]
    #[case::nar_sha256(Some(CAHash::Nar(NixHash::Sha256(hex!("ebd52279a8df024c9fd5718de4103bf5e760dc7f2cf49044ee7dea87ab16911a")))), &NAR_CONTENTS_COMPLICATED.clone())]
    #[case::nar_sha512(Some(CAHash::Nar(NixHash::Sha512(Box::new(hex!("1f5d43941411f35f09211f8596b426ee6e4dd3af1639e0ed2273cbe44b818fc4a59e3af02a057c5b18fbfcf435497de5f1994206c137f469b3df674966a922f0"))))), &NAR_CONTENTS_COMPLICATED.clone())]
    #[case::flat_md5(Some(CAHash::Flat(NixHash::Md5(hex!("ed076287532e86365e841e92bfc50d8c")))), &NAR_CONTENTS_HELLOWORLD.clone())]
    #[case::nar_symlink_sha1(Some(CAHash::Nar(NixHash::Sha1(hex!("424eeaaa9cc016bab030bf007cb1be6483e7ba9e")))), &NAR_CONTENTS_SYMLINK.clone())]
    #[tokio::test]
    async fn ingest_with_cahash_correct(
        #[case] ca_hash: Option<CAHash>,
        #[case] nar_content: &[u8],
    ) {
        ingest_nar_and_hash(
            snix_castore::utils::gen_test_blob_service(),
            snix_castore::utils::gen_test_directory_service(),
            &mut Cursor::new(nar_content),
            &ca_hash,
        )
        .await
        .expect("CAHash should have matched");
    }

    #[rstest]
    #[case::nar_sha256(Some(CAHash::Flat(NixHash::Sha256(hex!("ebd52279a8df024c9fd5718de4103bf5e760dc7f2cf49044ee7dea87ab16911a")))), &NAR_CONTENTS_COMPLICATED.clone())]
    #[case::nar_symlink_sha1(Some(CAHash::Flat(NixHash::Sha1(hex!("424eeaaa9cc016bab030bf007cb1be6483e7ba9e")))), &NAR_CONTENTS_SYMLINK.clone())]
    #[tokio::test]
    async fn ingest_with_flat_non_file(
        #[case] ca_hash: Option<CAHash>,
        #[case] nar_content: &[u8],
    ) {
        let err = ingest_nar_and_hash(
            snix_castore::utils::gen_test_blob_service(),
            snix_castore::utils::gen_test_directory_service(),
            &mut Cursor::new(nar_content),
            &ca_hash,
        )
        .await
        .expect_err("Ingestion should have failed");

        assert!(
            matches!(err, NarIngestionError::TypeMismatch),
            "Flat cahash should only be allowed for single file nars"
        );
    }
}
