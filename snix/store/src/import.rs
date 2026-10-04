use snix_castore::{
    blob_engine::BlobServiceEngine, blobservice::BlobService, directoryservice::DirectoryService,
    import::fs::ingest_path,
};
use tracing::instrument;

use nix_compat::{
    nixhash::{CAHash, NixHash},
    store_path,
};

use crate::{
    nar::NarCalculationService,
    pathinfoservice::{PathInfo, PathInfoService},
    proto::nar_info,
};

impl From<CAHash> for nar_info::Ca {
    fn from(value: CAHash) -> Self {
        let hash_type: nar_info::ca::Hash = (&value).into();
        let digest: bytes::Bytes = value.hash().to_string().into();
        nar_info::Ca {
            r#type: hash_type.into(),
            digest,
        }
    }
}

/// Ingest the contents at the given path `path` into castore, and registers the
/// resulting root node in the passed PathInfoService, using the "NAR sha256
/// digest" and the passed name for output path calculation.
/// Inserts the PathInfo into the PathInfoService and returns it back to the caller.
/// The `name` should have been checked by [nix_compat::store_path::validate_name]
/// before, to avoid unnecessarily importing, but will prevent the PathInfo from
/// being created in case of an invalid name.
#[instrument(skip_all, fields(name=name.as_ref(), path=?path.as_ref()), err)]
pub async fn import_path_as_nar_ca<BS, DS, PS, NS, P>(
    path: P,
    name: impl AsRef<str>,
    blob_service: BS,
    directory_service: DS,
    path_info_service: PS,
    nar_calculation_service: NS,
) -> Result<PathInfo, std::io::Error>
where
    P: AsRef<std::path::Path>,
    BS: BlobService + Clone,
    DS: DirectoryService,
    PS: AsRef<dyn PathInfoService>,
    NS: NarCalculationService,
{
    // Ingest the contents at the given path `path` into castore.
    let root_node = ingest_path::<_, _, _, &[u8]>(
        BlobServiceEngine(blob_service),
        directory_service,
        path,
        None,
    )
    .await
    .map_err(std::io::Error::other)?;

    // Ask for the NAR size and sha256
    let (nar_size, nar_sha256) = nar_calculation_service
        .calculate_nar(&root_node)
        .await
        .map_err(std::io::Error::other)?;

    let hash = NixHash::Sha256(nar_sha256);

    // Calculate the output path. Will fail if the previously passed name doesn't pass
    // the [nix_compat::store_path::validate_name] check.
    let output_path =
        store_path::build_ca_path(name.as_ref(), true, &hash, [], false).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid name: {0}", name.as_ref()),
            )
        })?;

    // Insert a PathInfo. On success, return it back to the caller.
    path_info_service
        .as_ref()
        .put(PathInfo {
            store_path: output_path.to_owned(),
            node: root_node,
            // There's no reference scanning on imported paths
            references: vec![],
            nar_size,
            nar_sha256,
            signatures: vec![],
            deriver: None,
            ca: Some(CAHash::Nar(hash)),
        })
        .await
        .map_err(std::io::Error::other)
}
