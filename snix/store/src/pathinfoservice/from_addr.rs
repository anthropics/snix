use super::PathInfoService;

use crate::composition::REG;
use snix_castore::composition::{
    CompositionContext, DeserializeWithRegistry, ServiceBuilder, with_registry,
};
use std::sync::Arc;
use url::Url;

/// Constructs a new instance of a [PathInfoService] from an URI.
///
/// The following URIs are supported:
/// - `redb+memory:`
///   Uses a in-memory implementation.
/// - `redb:/absolute/path/to/somewhere`
///   Uses redb, using a path on the disk for persistency. Can be only opened
///   from one process at the same time.
/// - `nix+https://cache.nixos.org?trusted_public_keys[0]=cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=`
///   Exposes the Nix binary cache as a PathInfoService, ingesting NARs into the
///   {Blob,Directory}Service. You almost certainly want to use this with some cache.
///   The `trusted_public_keys` URL parameter can be provided, which will then
///   enable signature verification.
/// - `grpc+unix:///absolute/path/to/somewhere`
///   Connects to a local snix-store gRPC service via Unix socket.
/// - `grpc+http://host:port`, `grpc+https://host:port`
///   Connects to a (remote) snix-store gRPC service.
///
/// As the [PathInfoService] needs to talk to [snix_castore::blobservice::BlobService] and
/// [snix_castore::directoryservice::DirectoryService], these also need to be passed in.
pub async fn from_addr(
    uri: &str,
    context: Option<&CompositionContext<'_>>,
) -> Result<Arc<dyn PathInfoService>, Box<dyn std::error::Error + Send + Sync>> {
    #[allow(unused_mut)]
    let mut url = Url::parse(uri).map_err(|e| format!("unable to parse url: {e}"))?;

    let path_info_service_config = with_registry(&REG, || {
        <DeserializeWithRegistry<Box<dyn ServiceBuilder<Output = dyn PathInfoService>>>>::try_from(
            url,
        )
    })?
    .0;
    let path_info_service = path_info_service_config
        .build(
            "anonymous",
            context.unwrap_or(&CompositionContext::blank(&REG)),
        )
        .await?;

    Ok(path_info_service)
}

#[cfg(test)]
mod tests {
    use super::from_addr;
    use crate::composition::REG;
    use rstest::rstest;
    use snix_castore::blobservice::{BlobService, MemoryBlobServiceConfig};
    use snix_castore::composition::{Composition, DeserializeWithRegistry, ServiceBuilder};
    use snix_castore::directoryservice::DirectoryService;
    use std::sync::LazyLock;
    use tempfile::TempDir;

    static TMPDIR_REDB_1: LazyLock<TempDir> = LazyLock::new(|| TempDir::new().unwrap());
    static TMPDIR_REDB_2: LazyLock<TempDir> = LazyLock::new(|| TempDir::new().unwrap());

    // the gRPC tests below don't fail, because we connect lazily.

    #[rstest]
    /// This uses a unsupported scheme.
    #[case::unsupported_scheme("http://foo.example/test", false)]
    /// This configures redb without a path, which should fail.
    #[case::redb_invalid_missing_path("redb://", false)]
    /// This configures redb with /, which should fail.
    #[case::redb_invalid_root("redb:///", false)]
    /// This configures redb with a host, not path, which should fail.
    #[case::redb_invalid_host("redb://foo.example", false)]
    /// This configures redb with a valid path, but with authority component, which should fail.
    #[case::redb_invalid_path_authority(&format!("redb://{}", TMPDIR_REDB_1.path().join("foo").to_str().unwrap()), false)]
    /// This configures redb with a valid path, which should succeed.
    #[case::redb_valid_path(&format!("redb:{}", TMPDIR_REDB_1.path().join("foo").to_str().unwrap()), true)]
    /// This configures redb with a host, and a valid path path, which should fail.
    #[case::redb_invalid_host_with_valid_path(&format!("redb://foo.example{}", TMPDIR_REDB_2.path().join("bar").to_str().unwrap()), false)]
    /// This configures redb in-memory.
    #[case::redb_memory_valid("redb+memory:", true)]
    /// This configures redb in-memory, but wrongly adds a path.
    #[case::redb_memory_invalid_path("redb+memory:/foo/bar", false)]
    /// This configures redb in-memory, but wrongly adds authority.
    #[case::redb_memory_invalid_authority("redb+memory://", false)]
    /// This configures redb in-memory, but wrongly adds a path (with authority).
    #[case::redb_memory_invalid_authority_path("redb+memory:///foo/bar", false)]
    #[case::nix_http(
        "nix+https://cache.nixos.org?trusted_public_keys[0]=cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=",
        true
    )]
    #[case::nix_http_force_download_nar(
        "nix+https://cache.nixos.org?trusted_public_keys[0]=cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=&force_download_nar=true",
        true
    )]
    /// Correct scheme for unix socket.
    #[case::grpc_valid_unix_socket("grpc+unix:/path/to/somewhere", true)]
    /// Scheme to connect to a unix socket, but with authority.
    #[case::grpc_invalid_unix_socket_authority("grpc+unix:///path/to/somewhere", false)]
    /// Scheme to connect to a unix socket, but with authority.
    #[case::grpc_invalid_unix_socket_and_host("grpc+unix://host.example/path/to/somewhere", false)]
    /// Scheme to connect to localhost, with port 12345
    #[case::grpc_valid_ipv6_localhost_port_12345("grpc+http://[::1]:12345", true)]
    /// Scheme to connect to localhost over http, without specifying a port.
    #[case::grpc_valid_http_host_without_port("grpc+http://localhost", true)]
    /// Scheme to connect to localhost over http, without specifying a port.
    #[case::grpc_valid_https_host_without_port("grpc+https://localhost", true)]
    /// Scheme to connect to localhost over http, but with additional path, which is invalid.
    #[case::grpc_invalid_host_and_path("grpc+http://localhost/some-path", false)]
    /// A valid example for Bigtable.
    #[cfg_attr(
        all(feature = "cloud", feature = "integration"),
        case::bigtable_valid(
            "bigtable://instance-1?project_id=project-1&table_name=table-1&family_name=cf1",
            true
        )
    )]
    /// An invalid example for Bigtable, missing fields
    #[cfg_attr(
        all(feature = "cloud", feature = "integration"),
        case::bigtable_invalid_missing_fields("bigtable://instance-1", false)
    )]
    #[tokio::test]
    async fn test_from_addr_tokio(#[case] uri_str: &str, #[case] exp_succeed: bool) {
        use snix_castore::directoryservice::RedbDirectoryServiceConfig;

        let mut comp = Composition::new(&REG);
        comp.extend(vec![(
            "root".into(),
            DeserializeWithRegistry(Box::new(MemoryBlobServiceConfig {})
                as Box<dyn ServiceBuilder<Output = dyn BlobService>>),
        )]);
        comp.extend(vec![(
            "root".into(),
            DeserializeWithRegistry(Box::new(RedbDirectoryServiceConfig::default())
                as Box<dyn ServiceBuilder<Output = dyn DirectoryService>>),
        )]);

        let resp = from_addr(uri_str, Some(&comp.context())).await;

        if exp_succeed {
            resp.expect("should succeed");
        } else {
            assert!(resp.is_err(), "should fail");
        }
    }
}
