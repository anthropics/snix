use nix_compat::{
    narinfo::NarInfo,
    nixhash::{Sha256, copy_sha256},
};
use snix_castore::{
    Node, blobservice::BlobService, directoryservice::DirectoryService,
    proto::parse_infused_nar_path,
};

/// Try to parse the NAR URL in the Narinfo as castore-infused,
/// return the validated root_node if successful.
/// If the URL is not castore-infused, returns Ok(Some).
/// The passed blob_service and directory_service need to include the gRPC
/// castore services, so substitution of new castore data is possible.
pub async fn try_infused_nar_path<BS, DS>(
    narinfo: &NarInfo<'_>,
    blob_service: BS,
    directory_service: DS,
) -> Result<Option<Node>, Error>
where
    BS: BlobService,
    DS: DirectoryService,
{
    let (node, nar_size) = match parse_infused_nar_path(narinfo.url) {
        Some(e) => e,
        None => return Ok(None),
    };

    if nar_size != narinfo.nar_size {
        return Err(Error::InconsistentNarSizeInURL);
    }

    // Construct a NAR Reader for the given root node
    let mut r = crate::nar::Reader::new(&node, &blob_service, directory_service).await?;

    // Render the NAR out into a sink, while hashing and calculating nar_size at the same time.
    let (actual_nar_size, actual_nar_hash) = copy_sha256(&mut r, &mut tokio::io::sink()).await?;

    if narinfo.nar_size != actual_nar_size {
        return Err(Error::WrongNARSize {
            expected: narinfo.nar_size,
            actual: actual_nar_size,
        });
    }
    if narinfo.nar_hash != actual_nar_hash {
        return Err(Error::WrongNARHash {
            expected: narinfo.nar_hash.into(),
            actual: actual_nar_hash,
        });
    }

    Ok(Some(node))
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("found infused NAR path, but with differing nar_size!")]
    InconsistentNarSizeInURL,

    #[error("failed to render NAR: {0}")]
    RenderingNAR(#[from] crate::nar::RenderError),

    #[error("failed to read NAR: {0}")]
    IO(#[from] std::io::Error),

    #[error("got unexpected NAR size while rendering NAR: {actual}, expected {expected}")]
    WrongNARSize { expected: u64, actual: u64 },

    #[error("got unexpected NAR hash while rendering NAR: {actual:x}, expected {expected:x}")]
    WrongNARHash { expected: Sha256, actual: Sha256 },
}
