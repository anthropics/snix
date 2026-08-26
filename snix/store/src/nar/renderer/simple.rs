use nix_compat::nar::writer::r#async as nar_writer;
use snix_castore::{Node, blobservice::BlobService, directoryservice::DirectoryService};
use tokio::io::{self, AsyncWrite, BufReader};

use crate::nar::RenderError;

/// Accepts a [Node] pointing to the root of a (store) path,
/// and uses the passed blob_service and directory_service to perform the
/// necessary lookups as it traverses the structure.
/// The contents in NAR serialization are writen to the passed [AsyncWrite].
///
/// This function is very linear, so roundtrip times add up quickly.
/// You might want to use [crate::nar::write_nar] instead, which opens more
/// blobs concurrently.
pub async fn write_nar<W, BS, DS>(
    mut w: W,
    root_node: &Node,
    blob_service: BS,
    directory_service: DS,
) -> Result<(), RenderError>
where
    W: AsyncWrite + Unpin + Send,
    BS: BlobService,
    DS: DirectoryService,
{
    // Initialize NAR writer
    let nar_root_node = nar_writer::open(&mut w)
        .await
        .map_err(RenderError::NARWriterError)?;

    walk_node(
        nar_root_node,
        root_node,
        b"",
        blob_service,
        directory_service,
    )
    .await?;

    Ok(())
}

/// Process an intermediate node in the structure.
/// This consumes the node.
async fn walk_node<BS, DS>(
    nar_node: nar_writer::Node<'_, '_>,
    castore_node: &Node,
    name: &[u8],
    blob_service: BS,
    directory_service: DS,
) -> Result<(BS, DS), RenderError>
where
    BS: BlobService + Send,
    DS: DirectoryService + Send,
{
    match castore_node {
        Node::Symlink { target, .. } => {
            nar_node
                .symlink(target.as_ref())
                .await
                .map_err(RenderError::NARWriterError)?;
        }
        Node::File {
            digest,
            size,
            executable,
        } => {
            let mut blob_reader = match blob_service
                .open_read(digest)
                .await
                .map_err(RenderError::BlobService)?
            {
                Some(blob_reader) => Ok(BufReader::new(blob_reader)),
                None => Err(RenderError::NARWriterError(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("blob with digest {} not found", digest),
                ))),
            }?;

            nar_node
                .file(*executable, *size, &mut blob_reader)
                .await
                .map_err(|err| match err.kind() {
                    io::ErrorKind::UnexpectedEof => {
                        io::Error::new(io::ErrorKind::InvalidData, "blob short read")
                    }
                    io::ErrorKind::InvalidInput => io::Error::new(
                        io::ErrorKind::InvalidData,
                        "blob continued to yield data beyond end",
                    ),
                    _ => err,
                })
                .map_err(RenderError::NARWriterError)?;
        }
        Node::Directory { digest, .. } => {
            // look it up with the directory service
            let directory = directory_service
                .get(digest)
                .await
                .map_err(RenderError::DirectoryService)?
                .ok_or_else(|| {
                    RenderError::DirectoryNotFound(*digest, bytes::Bytes::copy_from_slice(name))
                })?;

            // start a directory node
            let mut nar_node_directory = nar_node
                .directory()
                .await
                .map_err(RenderError::NARWriterError)?;

            // We put blob_service, directory_service back here whenever we come up from
            // the recursion.
            let mut blob_service = blob_service;
            let mut directory_service = directory_service;

            // for each node in the directory, create a new entry with its name,
            // and then recurse on that entry.
            for (name, node) in directory.nodes() {
                let child_node = nar_node_directory
                    .entry(name.as_ref())
                    .await
                    .map_err(RenderError::NARWriterError)?;

                (blob_service, directory_service) = Box::pin(walk_node(
                    child_node,
                    node,
                    name.as_ref(),
                    blob_service,
                    directory_service,
                ))
                .await?;
            }

            // close the directory
            nar_node_directory
                .close()
                .await
                .map_err(RenderError::NARWriterError)?;

            return Ok((blob_service, directory_service));
        }
    }

    Ok((blob_service, directory_service))
}
