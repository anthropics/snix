use std::sync::Arc;

#[cfg(target_os = "linux")]
use crate::buildservice::bwrap::BubblewrapBuildService;

use super::{BuildService, DummyBuildService, grpc::GRPCBuildService};
use snix_castore::{blob_engine::BlobEngine, directoryservice::DirectoryService};
use url::Url;

#[cfg(target_os = "linux")]
use super::oci::OCIBuildService;
#[cfg(all(not(target_os = "linux"), doc))]
struct OCIBuildService;
#[cfg(all(not(target_os = "linux"), doc))]
struct BubblewrapBuildService;

/// Constructs a new instance of a [BuildService] from an URI.
///
/// The following schemes are supported by the following services:
/// - `dummy:` ([DummyBuildService])
/// - `oci:` ([OCIBuildService])
/// - `grpc+*:` ([GRPCBuildService])
/// - `bwrap:` ([BubblewrapBuildService])
///
/// As some of these [BuildService] need to talk to a [BlobEngine] and
/// [DirectoryService], these also need to be passed in.
#[cfg_attr(target_os = "macos", allow(unused_variables))]
pub async fn from_addr<BE, DS>(
    uri: &str,
    blob_engine: BE,
    directory_service: DS,
) -> std::io::Result<Arc<dyn BuildService>>
where
    BE: BlobEngine + Send + Sync + Clone + 'static,
    DS: DirectoryService + Send + Sync + Clone + 'static,
{
    let url =
        Url::parse(uri).map_err(|e| std::io::Error::other(format!("unable to parse url: {e}")))?;

    Ok(match url.scheme() {
        "dummy" => {
            // dummy wants no authority, path etc.
            if url.has_authority() {
                Err(std::io::Error::other("dummy must not have authority"))?
            }
            if !url.path().is_empty() {
                Err(std::io::Error::other("dummy must not have path"))?
            }
            Arc::new(DummyBuildService::default())
        }
        #[cfg(target_os = "linux")]
        "oci" => {
            // oci wants no authority component in the URI
            if url.has_authority() {
                Err(std::io::Error::other("oci must not have authority"))?
            }
            // oci wants a path in which it creates bundles.
            if url.path().is_empty() {
                Err(std::io::Error::other("oci needs a bundle dir as path"))?
            }

            // TODO: make sandbox shell and rootless_uid_gid

            Arc::new(OCIBuildService::new(
                url.path().into(),
                blob_engine,
                directory_service,
            ))
        }
        #[cfg(target_os = "linux")]
        "bwrap" => {
            // bwrap wants no authority component in the URI
            if url.has_authority() {
                Err(std::io::Error::other("bwrap must not have authority"))?
            }
            // bwrap wants a path in which it creates bundles.
            if url.path().is_empty() {
                Err(std::io::Error::other("bwap needs a bundle dir as path"))?
            }

            Arc::new(BubblewrapBuildService::new(
                url.path().into(),
                blob_engine,
                directory_service,
            ))
        }
        scheme => {
            if scheme.starts_with("grpc+") {
                let client =
                    crate::proto::build_service_client::BuildServiceClient::with_interceptor(
                        snix_castore::tonic::channel_from_url(&url)
                            .await
                            .map_err(std::io::Error::other)?,
                        snix_tracing::propagate::tonic::send_trace,
                    );
                // FUTUREWORK: also allow responding to {blob,directory}_service
                // requests from the remote BuildService?
                Arc::new(GRPCBuildService::from_client(client))
            } else {
                Err(std::io::Error::other(format!(
                    "unknown scheme: {}",
                    url.scheme()
                )))?
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::from_addr;
    use rstest::rstest;
    #[cfg(target_os = "linux")]
    use std::sync::LazyLock;
    #[cfg(target_os = "linux")]
    use tempfile::TempDir;

    #[cfg(target_os = "linux")]
    static TMPDIR_OCI_1: LazyLock<TempDir> = LazyLock::new(|| TempDir::new().unwrap());
    #[cfg(target_os = "linux")]
    static TMPDIR_OCI_2: LazyLock<TempDir> = LazyLock::new(|| TempDir::new().unwrap());
    #[cfg(target_os = "linux")]
    static TMPDIR_BWRAP_1: LazyLock<TempDir> = LazyLock::new(|| TempDir::new().unwrap());
    #[cfg(target_os = "linux")]
    static TMPDIR_BWRAP_2: LazyLock<TempDir> = LazyLock::new(|| TempDir::new().unwrap());

    #[rstest]
    /// This uses an unsupported scheme.
    #[case::unsupported_scheme("http://foo.example/test", false)]
    /// This configures dummy
    #[case::valid_dummy("dummy:", true)]
    /// This configures dummy, but with authority, which is wrong.
    #[case::invalid_dummy_authority("dummy://", false)]
    /// Correct scheme to connect to a unix socket.
    #[case::grpc_valid_unix_socket("grpc+unix:/path/to/somewhere", true)]
    /// unix socket, but with authority.
    #[case::grpc_invalid_unix_socket_authority("grpc+unix:///path/to/somewhere", false)]
    /// Correct scheme for unix socket, but setting a host too, which is invalid.
    #[case::grpc_invalid_unix_socket_and_host("grpc+unix://host.example/path/to/somewhere", false)]
    /// Correct scheme to connect to localhost, with port 12345
    #[case::grpc_valid_ipv6_localhost_port_12345("grpc+http://[::1]:12345", true)]
    /// Correct scheme to connect to localhost over http, without specifying a port.
    #[case::grpc_valid_http_host_without_port("grpc+http://localhost", true)]
    /// Correct scheme to connect to localhost over http, without specifying a port.
    #[case::grpc_valid_https_host_without_port("grpc+https://localhost", true)]
    /// Correct scheme to connect to localhost over http, but with additional path, which is invalid.
    #[case::grpc_invalid_host_and_path("grpc+http://localhost/some-path", false)]
    /// This configures OCI, but doesn't specify the bundle path
    #[cfg_attr(target_os = "linux", case::oci_missing_bundle_dir("oci:", false))]
    /// This configures OCI, specifying the bundle path
    #[cfg_attr(target_os = "linux", case::oci_bundle_path(&format!("oci:{}", TMPDIR_OCI_1.path().to_str().unwrap()), true))]
    /// oci, but with authority.
    #[cfg_attr(
        target_os = "linux",
        case::oci_bundle_path_authority(&format!("oci://{}", TMPDIR_OCI_2.path().to_str().unwrap()), false)
    )]
    /// This configures bwrap, but doesn't specify the bundle path
    #[cfg_attr(target_os = "linux", case::bwrap_missing_bundle_dir("bwrap:", false))]
    /// This configures bwrap, specifying the bundle path
    #[cfg_attr(target_os = "linux", case::bwrap_bundle_path(&format!("bwrap:{}", TMPDIR_BWRAP_1.path().to_str().unwrap()), true))]
    /// bwrap, but with authority.
    #[cfg_attr(
        target_os = "linux",
        case::bwrap_bundle_path_authority(&format!("bwrap://{}", TMPDIR_BWRAP_2.path().to_str().unwrap()), false)
    )]
    #[tokio::test]
    async fn test_from_addr(#[case] uri_str: &str, #[case] exp_succeed: bool) {
        let blob_service = snix_castore::utils::gen_test_blob_engine();
        let directory_service = snix_castore::utils::gen_test_directory_service();

        let resp = from_addr(uri_str, blob_service, directory_service).await;

        if exp_succeed {
            resp.expect("should succeed");
        } else {
            assert!(resp.is_err(), "should fail");
        }
    }
}
