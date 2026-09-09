//! Import from a real filesystem.

use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream::BoxStream;
use std::fs::FileType;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use tokio::io::BufReader;
use tokio_util::io::InspectReader;
use tracing::Span;
use tracing::{Instrument, info_span, instrument, trace_span};
use tracing_indicatif::span_ext::IndicatifSpanExt;
use walkdir::DirEntry;
use walkdir::WalkDir;

use crate::blobservice::BlobService;
use crate::directoryservice::DirectoryService;
use crate::refscan::{ReferenceReader, ReferenceScanner};
use crate::{B3Digest, Node};

use super::IngestionEntry;
use super::IngestionError;
use super::ingest_entries;

/// Ingests the contents at a given path into the snix store, interacting with a [BlobService] and
/// [DirectoryService]. It returns the root node or an error.
///
/// It does not follow symlinks at the root, they will be ingested as actual symlinks.
///
/// This function will walk the filesystem using `walkdir` and will consume
/// `O(#number of entries)` space.
#[instrument(
    skip(blob_service, directory_service, reference_scanner),
    fields(path, indicatif.pb_show = tracing::field::Empty),
    err
)]
pub async fn ingest_path<BS, DS, P, P2>(
    blob_service: BS,
    directory_service: DS,
    path: P,
    reference_scanner: Option<&ReferenceScanner<P2>>,
) -> Result<Node, IngestionError<Error>>
where
    P: AsRef<std::path::Path>,
    BS: BlobService + Clone,
    DS: DirectoryService,
    P2: AsRef<[u8]> + Send + Sync,
{
    let span = Span::current();
    span.pb_set_style(&snix_tracing::PB_SPINNER_LONG_STYLE);
    span.pb_set_message(&format!("Ingesting {}", path.as_ref().display()));

    let iter = WalkDir::new(path.as_ref())
        .follow_links(false)
        .follow_root_links(false)
        .contents_first(true)
        .into_iter();

    ingest_entries(
        directory_service,
        dir_entries_to_ingestion_stream(blob_service, iter, path.as_ref(), reference_scanner)
            .inspect_ok(|ingestion_entry| {
                if matches!(ingestion_entry, IngestionEntry::Regular { .. }) {
                    span.pb_inc(1);
                }
            }),
    )
    .await
}

/// Converts an iterator of [walkdir::DirEntry]s into a stream of ingestion entries.
/// This can then be fed into [ingest_entries] to ingest all the entries into the castore.
///
/// The produced stream is buffered, so uploads can happen concurrently.
///
/// The root is the [std::path::Path] in the filesystem that is being ingested
/// into castore.
pub fn dir_entries_to_ingestion_stream<'a, BS, I, P>(
    blob_service: BS,
    walkdir_direntries: I,
    root: &'a std::path::Path,
    reference_scanner: Option<&'a ReferenceScanner<P>>,
) -> BoxStream<'a, Result<IngestionEntry, Error>>
where
    BS: BlobService + Clone + 'a,
    I: Iterator<Item = Result<DirEntry, walkdir::Error>> + Send + 'a,
    P: AsRef<[u8]> + Send + Sync,
{
    let prefix = root.parent().unwrap_or_else(|| std::path::Path::new(""));

    futures::stream::iter(walkdir_direntries)
        .map(move |x| {
            let blob_service = blob_service.clone();
            async move {
                match x {
                    Ok(dir_entry) => {
                        dir_entry_to_ingestion_entry(
                            blob_service,
                            &dir_entry,
                            prefix,
                            reference_scanner,
                        )
                        .await
                    }
                    Err(e) => Err(Error::Stat(
                        prefix.to_path_buf(),
                        e.into_io_error().expect("walkdir err must be some"),
                    )),
                }
            }
            .instrument(trace_span!("process_walkdir_direntry"))
        })
        .buffered(50)
        .boxed()
}

/// Converts a [walkdir::DirEntry] into an [IngestionEntry], uploading blobs to the
/// provided [BlobService].
///
/// The prefix path is stripped from the path of each entry. This is usually the parent path
/// of the path being ingested so that the last element of the stream only has one component.
pub async fn dir_entry_to_ingestion_entry<BS, P>(
    blob_service: BS,
    walkdir_direntry: &DirEntry,
    prefix: &std::path::Path,
    reference_scanner: Option<&ReferenceScanner<P>>,
) -> Result<IngestionEntry, Error>
where
    BS: BlobService,
    P: AsRef<[u8]>,
{
    let file_type = walkdir_direntry.file_type();

    let fs_path = walkdir_direntry
        .path()
        .strip_prefix(prefix)
        .expect("Snix bug: failed to strip root path prefix");

    // convert to castore PathBuf
    let path = crate::path::PathBuf::from_host_path(fs_path, false)
        .unwrap_or_else(|e| panic!("Snix bug: walkdir direntry cannot be parsed: {e}"));

    if file_type.is_dir() {
        Ok(IngestionEntry::Dir { path })
    } else if file_type.is_symlink() {
        let target = tokio::fs::read_link(walkdir_direntry.path())
            .await
            .map_err(|e| Error::Stat(walkdir_direntry.path().to_path_buf(), e))?
            .into_os_string()
            .into_vec();

        if let Some(reference_scanner) = &reference_scanner {
            reference_scanner.scan(&target);
        }

        Ok(IngestionEntry::Symlink { path, target })
    } else if file_type.is_file() {
        let metadata = walkdir_direntry
            .metadata()
            .map_err(|e| Error::Stat(walkdir_direntry.path().to_path_buf(), e.into()))?;

        let digest = upload_blob(blob_service, walkdir_direntry.path(), reference_scanner).await?;

        Ok(IngestionEntry::Regular {
            path,
            size: metadata.size(),
            // If it's executable by the user, it'll become executable.
            // This matches nix's dump() function behaviour.
            executable: metadata.permissions().mode() & 64 != 0,
            digest,
        })
    } else {
        Err(Error::FileType(fs_path.to_path_buf(), file_type))
    }
}

/// Uploads the file at the provided [std::path::Path] to the [BlobService].
#[instrument(skip_all, fields(blob.path=%path.as_ref().display()), err)]
async fn upload_blob<BS, P>(
    blob_service: BS,
    path: impl AsRef<std::path::Path>,
    reference_scanner: Option<&ReferenceScanner<P>>,
) -> Result<B3Digest, Error>
where
    BS: BlobService,
    P: AsRef<[u8]>,
{
    let progress_span = info_span!("upload_blobs", "indicatif.pb_show" = tracing::field::Empty);
    progress_span.pb_set_style(&snix_tracing::PB_TRANSFER_STYLE);
    progress_span.pb_start();
    progress_span.pb_set_message(&format!("Uploading blob at {:?}", path.as_ref()));

    let file = tokio::fs::File::open(path.as_ref())
        .await
        .map_err(|e| Error::BlobRead(path.as_ref().to_path_buf(), e))?;

    let metadata = file
        .metadata()
        .await
        .map_err(|e| Error::Stat(path.as_ref().to_path_buf(), e))?;

    progress_span.pb_set_length(metadata.len());
    let reader = InspectReader::new(file, |d| {
        progress_span.pb_inc(d.len() as u64);
    });

    let mut writer = blob_service.open_write().await;
    let mut reader = BufReader::with_capacity(128 * 1024, reader);
    if let Some(reference_scanner) = reference_scanner {
        let mut reader = ReferenceReader::new(reference_scanner, reader);
        tokio::io::copy_buf(&mut reader, &mut writer)
            .await
            .map_err(|e| Error::BlobRead(path.as_ref().to_path_buf(), e))?;
    } else {
        tokio::io::copy_buf(&mut reader, &mut writer)
            .await
            .map_err(|e| Error::BlobRead(path.as_ref().to_path_buf(), e))?;
    }

    let digest = writer
        .close()
        .await
        .map_err(|e| Error::BlobFinalize(path.as_ref().to_path_buf(), e))?;

    Ok(digest)
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unsupported file type at {0}: {1:?}")]
    FileType(std::path::PathBuf, FileType),

    #[error("unable to stat {0}: {1}")]
    Stat(std::path::PathBuf, std::io::Error),

    #[error("unable to open {0}: {1}")]
    Open(std::path::PathBuf, std::io::Error),

    #[error("unable to read {0}: {1}")]
    BlobRead(std::path::PathBuf, std::io::Error),

    // TODO: proper error for blob finalize
    #[error("unable to finalize blob {0}: {1}")]
    BlobFinalize(std::path::PathBuf, std::io::Error),
}
