#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod app_state;
pub mod router;
pub mod routes;

use std::path;

use snix_castore::{
    B3Digest, Directory, Node, Path, SymlinkTarget,
    blob_engine::BlobEngine,
    directoryservice::{DirectoryService, traversal::descend_to},
};

use axum::{
    body::Body,
    http::{StatusCode, header},
    response::{AppendHeaders, Html, IntoResponse, Redirect, Response},
};
use axum_extra::{TypedHeader, headers::Range};
use axum_range::{KnownSize, Ranged};
use path_clean::PathClean;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use tokio_util::io::ReaderStream;
use tracing::{debug, error, instrument, warn};

/// Helper function, descending from the given `root_node` to the `requested_path` specified.
/// Returns HTTP Responses or Status Codes.
/// If the path points to a regular file, it serves its contents.
/// If the path points to a symlink, it sends a redirect to the target (prepending `base_path`, if relative)
/// If the path points to a directory, files of `index_names` are tried,
/// If no files matched then a directory listing is returned if `auto_index` is enabled.
///
/// Uses the passed [BlobEngine] and [DirectoryService]
#[allow(clippy::too_many_arguments)]
#[instrument(level = "trace", skip_all, fields(base_path, requested_path), err)]
pub async fn get_root_node_contents<BE: BlobEngine, DS: DirectoryService, S: AsRef<str>>(
    blob_engine: BE,
    directory_service: DS,
    base_path: &path::Path,
    root_node: &Node,
    requested_path: &Path,
    range_header: Option<TypedHeader<Range>>,
    index_names: &[S],
    auto_index: bool,
) -> Result<Response, StatusCode> {
    match root_node {
        Node::Directory { .. } => {
            let requested_node = descend_to(&directory_service, root_node.clone(), requested_path)
                .await
                .map_err(|err| {
                    error!(err=%err, "an error occurred descending");
                    StatusCode::INTERNAL_SERVER_ERROR
                })?
                .ok_or_else(|| {
                    error!("requested path doesn't exist");
                    StatusCode::NOT_FOUND
                })?;
            match requested_node {
                Node::Directory { digest, .. } => {
                    let requested_directory = directory_service
                        .get(&digest)
                        .await
                        .map_err(|err| {
                            error!(err=%err, "an error occurred getting the directory");
                            StatusCode::INTERNAL_SERVER_ERROR
                        })?
                        .ok_or_else(|| {
                            error!("directory doesn't exist");
                            StatusCode::NOT_FOUND
                        })?;

                    // If there was one or more index configured, try to find it
                    // in the directory requested by the client, by comparing the bytes
                    // of each directories immediate child's path with the bytes of the
                    // configured index name
                    for index_name in index_names {
                        if let Some((found_index_file_path, found_index_node)) = requested_directory
                            .nodes()
                            .find(|(path, _node)| index_name.as_ref().as_bytes() == path.as_ref())
                        {
                            match found_index_node {
                                Node::File { digest, size, .. } => {
                                    let extension = found_index_file_path
                                        .extension()
                                        .and_then(|b| std::str::from_utf8(b).ok());

                                    return respond_file(
                                        blob_engine,
                                        extension,
                                        range_header,
                                        digest,
                                        *size,
                                    )
                                    .await;
                                }
                                _ => {
                                    debug!(
                                        path = %found_index_file_path,
                                        "One of the configured index names matched with a
                                        node located in the root node's directory which is
                                        not a file"
                                    );
                                }
                            }
                        }
                    }
                    if auto_index {
                        return respond_directory_list(&requested_directory, requested_path).await;
                    }
                    Err(StatusCode::FORBIDDEN)
                }
                Node::File { digest, size, .. } => {
                    respond_file(
                        blob_engine,
                        requested_path
                            .extension()
                            .and_then(|b| std::str::from_utf8(b).ok()),
                        range_header,
                        &digest,
                        size,
                    )
                    .await
                }
                Node::Symlink { target } => {
                    let requested_path =
                        path::Path::new(OsStr::from_bytes(requested_path.as_bytes()));
                    respond_symlink(base_path, &target, Some(requested_path)).await
                }
            }
        }
        Node::File { digest, size, .. } => {
            if requested_path.to_string() == "" {
                respond_file(blob_engine, None, range_header, digest, *size).await
            } else {
                warn!(
                    "The client requested a path but the configured root
                    node being served is a file"
                );
                Err(StatusCode::BAD_REQUEST)
            }
        }
        Node::Symlink { target } => {
            if requested_path.to_string() == "" {
                respond_symlink(base_path, target, None).await
            } else {
                warn!(
                    "The client requested a path but the configured root
                    node being served is a symlink"
                );
                Err(StatusCode::BAD_REQUEST)
            }
        }
    }
}

#[instrument(level = "trace", skip_all)]
pub async fn respond_symlink(
    base_path: &path::Path,
    symlink_target: &SymlinkTarget,
    requested_path: Option<&path::Path>,
) -> Result<Response, StatusCode> {
    if symlink_target.as_ref() == b"." {
        error!("There was a symlink with target '.'");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    let symlink_target_path = match std::str::from_utf8(symlink_target.as_ref()) {
        Ok(s) => path::Path::new(s),
        Err(_) => {
            error!("Symlink target contains invalid UTF-8");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    let symlink_target_path = if symlink_target_path.is_absolute() {
        symlink_target_path.to_path_buf()
    } else if let Some(requested_path) = requested_path {
        let requested_path_parent = requested_path.parent().ok_or_else(|| {
            error!("failed to retrieve parent path for requested path");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        base_path
            .join(requested_path_parent)
            .join(symlink_target_path)
    } else {
        base_path.join(symlink_target_path)
    };

    let symlink_target_path = symlink_target_path.clean();

    if symlink_target_path.starts_with(path::Component::ParentDir) {
        error!("the symlink's target path points to a non-existing path");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    let symlink_target_path_str = symlink_target_path.to_str().ok_or(StatusCode::NOT_FOUND)?;
    Ok(Redirect::temporary(symlink_target_path_str).into_response())
}

#[instrument(level = "trace", skip_all, fields(directory_path, directory))]
pub async fn respond_directory_list(
    directory: &Directory,
    directory_path: &Path,
) -> Result<Response, StatusCode> {
    let mut directory_list_html = String::new();
    for (path_component, _node) in directory.nodes() {
        let directory_path = directory_path
            .try_join(path_component.as_ref())
            .expect("Join path");
        directory_list_html.push_str(&format!(
            "<li><a href=\"/{directory_path}\">{path_component}</a></li>"
        ))
    }
    Ok(Html(format!(
        "<!DOCTYPE html><html><body>{directory_list_html}</body></html>"
    ))
    .into_response())
}

#[instrument(level = "trace", skip_all, fields(digest, size))]
pub async fn respond_file(
    blob_engine: impl BlobEngine,
    extension: Option<&str>,
    range_header: Option<TypedHeader<Range>>,
    digest: &B3Digest,
    file_size: u64,
) -> Result<Response, StatusCode> {
    let blob_reader = blob_engine
        .open_read(digest, Some(file_size))
        .await
        .map_err(|err| {
            error!(err=%err, "failed to read blob");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or_else(|| {
            error!("blob doesn't exist");
            StatusCode::NOT_FOUND
        })?;

    let mime_type = extension
        .and_then(|extension| mime_guess::from_ext(extension).first())
        .unwrap_or(mime::APPLICATION_OCTET_STREAM);
    match range_header {
        None => Ok((
            StatusCode::OK,
            AppendHeaders([
                (header::CONTENT_TYPE, mime_type.to_string()),
                (header::CONTENT_LENGTH, file_size.to_string()),
            ]),
            Body::from_stream(ReaderStream::new(blob_reader)),
        )
            .into_response()),
        Some(TypedHeader(range)) => Ok((
            StatusCode::OK,
            AppendHeaders([
                (header::CONTENT_TYPE, mime_type.to_string()),
                (header::CONTENT_LENGTH, file_size.to_string()),
            ]),
            Ranged::new(Some(range), KnownSize::sized(blob_reader, file_size)).into_response(),
        )
            .into_response()),
    }
}
