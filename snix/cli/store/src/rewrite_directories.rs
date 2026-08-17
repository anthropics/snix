use nix_compat::nixbase32;
use snix_castore::{
    Node,
    directoryservice::{DirectoryService, DirectoryServiceGraphExt},
};
use snix_store::{path_info::PathInfo, pathinfoservice::PathInfoService};
use tracing::{Span, info};

/// Consumes the given PathInfo, if it describes a directory, (re)calculates its closure.
///
/// If there's any change, it'll put an updated [PathInfo] into the [PathInfoService].
///
/// In the success case, the returned boolean describes whether an update was necessary or not.
#[tracing::instrument(err, skip_all, fields(
    path_info.name = path_info.store_path.name(),
    path_info.digest = nixbase32::encode(path_info.store_path.digest()),
    directory.digest = tracing::field::Empty,
    directory.size = tracing::field::Empty,
))]
pub async fn rewrite_pathinfo<PS, DS>(
    mut path_info: PathInfo,
    dry_run: bool,
    path_info_service: PS,
    directory_service: DS,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync + 'static>>
where
    PS: PathInfoService,
    DS: DirectoryService,
{
    let Node::Directory { digest, size } = path_info.node else {
        return Ok(false);
    };

    Span::current()
        .record("directory.digest", digest.to_string())
        .record("directory.size", size);

    let directory_graph = directory_service
        .get_directory_graph(&digest)
        .await?
        .ok_or("Directory closure not found")?;

    let (digest_new, size_new) = {
        let d = directory_graph.root();
        (d.digest(), d.size())
    };

    if digest != digest_new {
        info!(
            directory.digest_new=%digest_new,
            directory.size_new=size_new,
            "directory root node changed"
        );

        if !dry_run {
            let root_digest = directory_service
                .put_directory_graph(directory_graph)
                .await?;
            assert_eq!(
                digest_new, root_digest,
                "Snix bug expected root digest to match our calculations"
            );

            path_info.node = Node::Directory {
                digest: digest_new,
                size: size_new,
            };
            path_info_service.put(path_info).await?;

            return Ok(true);
        }
    }

    Ok(false)
}
