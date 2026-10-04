use super::{PathInfo, PathInfoService};
use crate::{
    nar::{NarIngestionError, ingest_nar_and_hash},
    pathinfoservice::{self, nix_http::castore_infused::try_infused_nar_path},
};
use futures::{TryStreamExt, stream::BoxStream};
use nix_compat::{
    narinfo::{self, NarInfo, Signature},
    nixbase32,
    nixhash::NixHash,
};
use reqwest::StatusCode;
use snix_castore::{
    blob_engine::BlobServiceEngine,
    blobservice::{self, BlobService},
    directoryservice::{self, DirectoryService},
    proto::{
        blob_service_client::BlobServiceClient, directory_service_client::DirectoryServiceClient,
    },
};
use snix_castore::{
    composition::{CompositionContext, ServiceBuilder},
    directoryservice::GRPCDirectoryService,
};
use std::sync::Arc;
use tokio::io::{self, AsyncRead};
use tonic::{async_trait, transport::Channel};
use tracing::{Span, instrument, warn};
use url::Url;

mod castore_infused;

/// NixHTTPPathInfoService acts as a bridge in between the Nix HTTP Binary cache
/// protocol provided by Nix binary caches such as cache.nixos.org, and the Snix
/// Store Model.
/// It implements the [PathInfoService] trait in an interesting way:
/// Every [PathInfoService::get] fetches the .narinfo and referred NAR file,
/// inserting components into a [BlobService] and [DirectoryService], then
/// returning a [PathInfo] struct with the root.
///
/// Due to this being quite a costly operation, clients are expected to layer
/// this service with store composition, so they're only ingested once.
///
/// The client is expected to be (indirectly) using the same [BlobService] and
/// [DirectoryService], so able to fetch referred Directories and Blobs.
/// [PathInfoService::put] is not implemented and returns an error if called.
/// TODO: what about reading from nix-cache-info?
pub struct NixHTTPPathInfoService<BS: Clone, DS> {
    instance_name: String,
    base_url: url::Url,
    http_client: reqwest_middleware::ClientWithMiddleware,

    blob_service: BS,
    directory_service: DS,

    /// A BlobService Cache with 'far' talking to the configured endpoint over gRPC.
    /// This is used when validating castore-infused NAR URLs in NARInfos.
    /// We rely on Cache to *insert* into 'near'.
    layered_blob_service: blobservice::Cache<BS, blobservice::GRPCBlobService<Channel>>,
    /// A DirectoryService Cache with 'far' talking to the configured endpoint over gRPC.
    /// This is used when validating castore-infused NAR URLs in NARInfos.
    /// We rely on Cache to *insert* into 'near'.
    layered_directory_service:
        directoryservice::combinators::Cache<DS, GRPCDirectoryService<Channel>>,

    /// An optional list of [narinfo::VerifyingKey].
    /// If the list is not empty, the .narinfo files received need to have
    /// correct signature by at least one of these.
    trusted_public_keys: Vec<narinfo::VerifyingKey>,

    /// Force the download of NAR files, even if there's castore-infused NAR URLs.
    force_download_nar: bool,
}

impl<BS, DS> NixHTTPPathInfoService<BS, DS>
where
    BS: Clone,
    DS: DirectoryService + Clone,
{
    pub fn try_build(
        instance_name: String,
        config: NixHTTPPathInfoServiceConfig,
        blob_service: BS,
        directory_service: DS,
    ) -> Result<Self, Error> {
        let mut trusted_public_keys = Vec::new();
        for s in config.params.trusted_public_keys {
            trusted_public_keys.push(
                narinfo::VerifyingKey::parse(&s).map_err(|e| Error::ParseTrustedPublicKey(s, e))?,
            )
        }

        let (layered_blob_service, layered_directory_service) = {
            let grpc_url = {
                let url_str = format!("grpc+{}", config.base_url);
                let mut url: Url = url_str.parse().expect("url to parse");
                url.set_path("");
                url
            };

            let channel =
                snix_castore::tonic::TonicConnector::from_url(&grpc_url)?.connect_expect_lazy();

            let instance_name_layered = format!("{}-layered", instance_name);
            let instance_name_grpc = format!("{}-grpc", instance_name);

            (
                blobservice::Cache::new(
                    instance_name_layered.clone(),
                    blob_service.clone(),
                    blobservice::GRPCBlobService::from_client(
                        instance_name_grpc.clone(),
                        BlobServiceClient::new(channel.clone()),
                    ),
                ),
                directoryservice::combinators::Cache::new(
                    instance_name_layered,
                    directory_service.clone(),
                    {
                        GRPCDirectoryService::from_client(
                            instance_name_grpc,
                            DirectoryServiceClient::new(channel).max_decoding_message_size(
                                directoryservice::GRPC_MAX_DECODING_MESSAGE_SIZE,
                            ),
                        )
                    },
                ),
            )
        };

        Ok(Self {
            instance_name,
            base_url: {
                // Help https://example.com/cache survive Url::join
                let mut base_url = config.base_url;
                if !base_url.path().ends_with('/') {
                    let with_slash = format!("{}/", base_url.path());
                    base_url.set_path(&with_slash);
                }
                base_url
            },
            http_client: reqwest_middleware::ClientBuilder::new(
                reqwest::Client::builder()
                    .user_agent(crate::USER_AGENT)
                    .build()
                    .map_err(reqwest_middleware::Error::Reqwest)?,
            )
            .with(snix_tracing::propagate::reqwest::tracing_middleware())
            .build(),
            blob_service,
            directory_service,

            layered_blob_service,
            layered_directory_service,

            trusted_public_keys,
            force_download_nar: config.params.force_download_nar,
        })
    }

    #[instrument(level=tracing::Level::TRACE, skip_all,fields(path.digest=nixbase32::encode(&digest)),err)]
    fn derive_narinfo_url(&self, digest: [u8; 20]) -> Result<Url, Error> {
        let s = format!("{}.narinfo", nixbase32::encode(&digest));
        self.base_url
            .join(&s)
            .map_err(|e| Error::JoinUrl(self.base_url.to_owned(), s.to_owned(), e))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("wrong arguments: {0}")]
    WrongConfig(&'static str),
    #[error("serde-qs error: {0}")]
    SerdeQS(#[from] serde_qs::Error),
    #[error("unable to parse pubkey {0}")]
    ParseTrustedPublicKey(String, nix_compat::narinfo::VerifyingKeyError),
    #[error("unable to construct tonic channel: {0}")]
    TonicChannel(#[from] snix_castore::tonic::Error),

    #[error("unable to join URL {0} with {1}")]
    JoinUrl(Url, String, url::ParseError),
    #[error("reqwest error")]
    Reqwest(#[from] reqwest_middleware::Error),
    #[error("unable to decode NARInfo response as string")]
    DecodeBody(reqwest::Error),
    #[error("unable to parse NARInfo")]
    ParseNARInfo(nix_compat::narinfo::Error),
    #[error("no valid signature found")]
    NoValidSignature,
    #[error("failed to request NAR, status {0}")]
    FailedToRequestNAR(reqwest::StatusCode),
    #[error("unsupported NAR compression: {0}")]
    UnsupportedNARCompression(String),
    #[error("failed to ingest NAR")]
    IngestNAR(NarIngestionError),
    #[error("NARSize mismatch, narinfo size {narinfo_size}, actual size {actual_size}")]
    NARSizeMismatch { narinfo_size: u64, actual_size: u64 },
    #[error("NARHash mismatch, narinfo NARHash {exp}, actual NARHash {act}",
        exp = NixHash::Sha256(*.narinfo_nar_sha256),
        act = NixHash::Sha256(*.actual_nar_sha256))]
    NARHashMismatch {
        narinfo_nar_sha256: [u8; 32],
        actual_nar_sha256: [u8; 32],
    },

    #[error("put not supported")]
    PutNotSupported,
    #[error("list not supported")]
    ListNotSupported,
}

#[async_trait]
impl<BS, DS> PathInfoService for NixHTTPPathInfoService<BS, DS>
where
    BS: BlobService + Send + Sync + Clone + 'static,
    DS: DirectoryService + Send + Sync + Clone + 'static,
{
    #[instrument(skip_all, err, fields(
        path.digest=nixbase32::encode(&digest),
        instance_name=%self.instance_name,
        narinfo.url=tracing::field::Empty,
        nar.url=tracing::field::Empty,
    ))]
    async fn get(&self, digest: [u8; 20]) -> Result<Option<PathInfo>, pathinfoservice::Error> {
        let narinfo_url = self.derive_narinfo_url(digest)?;

        let span = Span::current();
        span.record("narinfo.url", narinfo_url.to_string());

        let resp = self
            .http_client
            .get(narinfo_url)
            .send()
            .await
            .map_err(Error::Reqwest)?;

        // In the case of a 404, return a NotFound.
        // We also return a NotFound in case of a 403 - this is to match the behaviour as Nix,
        // when querying nix-cache.s3.amazonaws.com directly, rather than cache.nixos.org.
        if resp.status() == StatusCode::NOT_FOUND || resp.status() == StatusCode::FORBIDDEN {
            return Ok(None);
        }

        let narinfo_str = resp.text().await.map_err(Error::DecodeBody)?;

        // parse the received narinfo
        let narinfo = NarInfo::parse(&narinfo_str).map_err(Error::ParseNARInfo)?;

        // ensure the store path digest in the returned NARInfo matches the one we requested.
        if narinfo.store_path.digest() != &digest {
            return Err("Store path digest in NARInfo doesn't match".into());
        }

        // if [self.trusted_public_keys] is set, ensure there's at least one valid signature.
        if !self.trusted_public_keys.is_empty() {
            let fingerprint = narinfo.fingerprint();

            if !self.trusted_public_keys.iter().any(|pubkey| {
                narinfo
                    .signatures
                    .iter()
                    .any(|sig| pubkey.verify(&fingerprint, sig))
            }) {
                Err(Error::NoValidSignature)?
            }
        }

        // FUTUREWORK: Keep some database around mapping from narsha256 to
        // (unnamed) rootnode, so we can use that and avoid downloading the same
        // NAR a second time.

        // To construct the full PathInfo, we also need to populate the node field,
        // and for this we need to download the NAR file and ingest it into castore.
        // We can use a shortcut - if the NAR URL is castore-infused we can try
        // that route and maybe save some downloading, as we can leverage already locally present castore subnodes.
        // Get the root node, either by using the infused nar path or by ingesting the entire NAR.
        let root_node = if !self.force_download_nar
            && let Some(root_node) = try_infused_nar_path(
                &narinfo,
                self.layered_blob_service.clone(),
                &self.layered_directory_service,
            )
            .await
            .unwrap_or_else(|err| {
                warn!(%err, "unable to use infused store path");
                None
            }) {
            root_node
        } else {
            // create a request for the NAR file itself.
            let nar_url = self
                .base_url
                .join(narinfo.url)
                .map_err(|e| Error::JoinUrl(self.base_url.clone(), narinfo.url.to_owned(), e))?;
            span.record("nar.url", nar_url.to_string());

            let resp = self
                .http_client
                .get(nar_url.clone())
                .send()
                .await
                .map_err(Error::Reqwest)?;

            // if the request is not successful, return an error.
            if !resp.status().is_success() {
                Err(Error::FailedToRequestNAR(resp.status()))?;
            }

            // get a reader of the response body.
            let r = tokio_util::io::StreamReader::new(resp.bytes_stream().map_err(|e| {
                let e = e.without_url();
                warn!(e=%e, "failed to get response body");
                io::Error::new(io::ErrorKind::BrokenPipe, e.to_string())
            }));

            // handle decompression, depending on the compression field.
            let mut r: Box<dyn AsyncRead + Send + Unpin> = match narinfo.compression {
                None => Box::new(r) as Box<dyn AsyncRead + Send + Unpin>,
                Some("bzip2") => Box::new(async_compression::tokio::bufread::BzDecoder::new(r))
                    as Box<dyn AsyncRead + Send + Unpin>,
                Some("gzip") => Box::new(async_compression::tokio::bufread::GzipDecoder::new(r))
                    as Box<dyn AsyncRead + Send + Unpin>,
                Some("xz") => Box::new(async_compression::tokio::bufread::XzDecoder::new(r))
                    as Box<dyn AsyncRead + Send + Unpin>,
                Some("zstd") => {
                    // NARs are often many concatenated zstd frames; the default decoder
                    // stops after the first.
                    let mut decoder = async_compression::tokio::bufread::ZstdDecoder::new(r);
                    decoder.multiple_members(true);
                    Box::new(decoder) as Box<dyn AsyncRead + Send + Unpin>
                }
                Some(comp_str) => Err(Error::UnsupportedNARCompression(comp_str.to_owned()))?,
            };

            let (root_node, nar_hash, nar_size) = ingest_nar_and_hash(
                BlobServiceEngine(self.blob_service.clone()),
                &self.directory_service,
                &mut r,
                &narinfo.ca,
            )
            .await
            .map_err(Error::IngestNAR)?;

            // ensure the ingested narhash and narsize do actually match.
            if narinfo.nar_size != nar_size {
                Err(Error::NARSizeMismatch {
                    narinfo_size: narinfo.nar_size,
                    actual_size: nar_size,
                })?
            }
            if narinfo.nar_hash != nar_hash {
                Err(Error::NARHashMismatch {
                    narinfo_nar_sha256: narinfo.nar_hash,
                    actual_nar_sha256: nar_hash,
                })?
            }
            root_node
        };

        Ok(Some(PathInfo {
            store_path: narinfo.store_path.to_owned(),
            node: root_node,
            references: narinfo.references.iter().map(|sp| sp.to_owned()).collect(),
            nar_size: narinfo.nar_size,
            nar_sha256: narinfo.nar_hash,
            deriver: narinfo.deriver.as_ref().map(|sp| sp.to_owned()),
            signatures: narinfo
                .signatures
                .into_iter()
                .map(|s| Signature::<String>::new(s.name().to_string(), s.bytes().to_owned()))
                .collect(),
            ca: narinfo.ca,
        }))
    }

    #[instrument(skip_all, err, fields(
        path.digest=nixbase32::encode(&digest),
        instance_name=%self.instance_name,
        narinfo.url=tracing::field::Empty,
    ))]
    async fn has(&self, digest: [u8; 20]) -> Result<bool, pathinfoservice::Error> {
        let narinfo_url = self.derive_narinfo_url(digest)?;

        let span = Span::current();
        span.record("narinfo.url", narinfo_url.to_string());

        let resp = self
            .http_client
            .head(narinfo_url)
            .send()
            .await
            .map_err(Error::Reqwest)?;

        // In the case of a 404, return a NotFound.
        // We also return a NotFound in case of a 403 - this is to match the behaviour as Nix,
        // when querying nix-cache.s3.amazonaws.com directly, rather than cache.nixos.org.
        if resp.status() == StatusCode::NOT_FOUND || resp.status() == StatusCode::FORBIDDEN {
            Ok(false)
        } else {
            Ok(true)
        }
    }

    #[instrument(skip_all, fields(path_info=?_path_info, instance_name=%self.instance_name))]
    async fn put(&self, _path_info: PathInfo) -> Result<PathInfo, pathinfoservice::Error> {
        Err(Box::new(Error::PutNotSupported))
    }

    fn list(&self) -> BoxStream<'static, Result<PathInfo, pathinfoservice::Error>> {
        Box::pin(futures::stream::once(async {
            Err(Error::ListNotSupported)?
        }))
    }
}

#[derive(serde::Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NixHTTPPathInfoServiceConfig {
    base_url: Url,

    #[serde(flatten)]
    params: NixHTTPPathInfoServiceParams,
}

#[derive(serde::Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct NixHTTPPathInfoServiceParams {
    #[serde(default = "default_blob_service")]
    blob_service: String,
    #[serde(default = "default_directory_service")]
    directory_service: String,
    #[serde(default)]
    /// An optional list of [narinfo::VerifyingKey].
    /// If not empty, the .narinfo files received need to have correct signature by at least one of these.
    trusted_public_keys: Vec<String>,

    #[serde(default)]
    /// Force the download of NAR files, even if there's castore-infused NAR URLs.
    force_download_nar: bool,
}

fn default_blob_service() -> String {
    "&root".to_string()
}
fn default_directory_service() -> String {
    "&root".to_string()
}

impl TryFrom<Url> for NixHTTPPathInfoServiceConfig {
    type Error = Box<dyn std::error::Error + Send + Sync>;
    fn try_from(url: Url) -> Result<Self, Self::Error> {
        let scheme = url
            .scheme()
            .strip_prefix("nix+")
            .ok_or_else(|| Error::WrongConfig("scheme must start with nix+"))?;

        if !url.has_authority() {
            Err(Error::WrongConfig("url must have authority component"))?
        }
        if !url.has_host() {
            Err(Error::WrongConfig("url must have host component"))?
        }
        if !["http", "https"].contains(&scheme) {
            Err(Error::WrongConfig("unknown scheme"))?
        }

        Ok(NixHTTPPathInfoServiceConfig {
            // Stringify the URL and remove the nix+ prefix.
            // We can't use `url.set_scheme(rest)`, as it disallows
            // setting something http(s) that previously wasn't.
            // Also make sure to drop the query, we don't want to leak our
            // config to the remote HTTP endpoint we query.
            base_url: {
                let mut url: Url = url
                    .to_string()
                    .strip_prefix("nix+")
                    .unwrap()
                    .parse()
                    .expect("stripped URL to parse again");
                url.set_query(None);
                url
            },
            params: serde_qs::from_str(url.query().unwrap_or_default())?,
        })
    }
}

#[async_trait]
impl ServiceBuilder for NixHTTPPathInfoServiceConfig {
    type Output = dyn PathInfoService;
    async fn build<'a>(
        &'a self,
        instance_name: &str,
        context: &CompositionContext,
    ) -> Result<Arc<Self::Output>, Box<dyn std::error::Error + Send + Sync + 'static>> {
        let (blob_service, directory_service) = futures::join!(
            context.resolve::<dyn BlobService>(&self.params.blob_service),
            context.resolve::<dyn DirectoryService>(&self.params.directory_service)
        );
        let svc = NixHTTPPathInfoService::try_build(
            instance_name.to_string(),
            self.to_owned(),
            blob_service?,
            directory_service?,
        )?;
        Ok(Arc::new(svc))
    }
}

#[cfg(test)]
mod tests {
    use super::{NixHTTPPathInfoServiceConfig, NixHTTPPathInfoServiceParams};
    use rstest::rstest;
    use url::Url;

    #[rstest]
    /// Correct Scheme for the cache.nixos.org binary cache.
    #[case::correct_nix_https("nix+https://cache.nixos.org", Some(
        NixHTTPPathInfoServiceConfig {
            base_url: "https://cache.nixos.org".try_into().unwrap(),
            params: NixHTTPPathInfoServiceParams {
                blob_service: "&root".to_string(),
                directory_service: "&root".to_string(),
                trusted_public_keys: vec![],
                force_download_nar: false,
            }
        }
    ))]
    /// Correct Scheme for the cache.nixos.org binary cache (HTTP URL).
    #[case::correct_nix_http("nix+http://cache.nixos.org", Some(
        NixHTTPPathInfoServiceConfig {
            base_url: "http://cache.nixos.org".try_into().unwrap(),
            params: NixHTTPPathInfoServiceParams {
                blob_service: "&root".to_string(),
                directory_service: "&root".to_string(),
                trusted_public_keys: vec![],
                force_download_nar: false,
            }
        }
    ))]
    /// Correct Scheme for Nix HTTP Binary cache, with a subpath.
    #[case::correct_nix_http_with_subpath("nix+http://192.0.2.1/foo", Some(
        NixHTTPPathInfoServiceConfig {
            base_url: "http://192.0.2.1/foo".try_into().unwrap(),
            params: NixHTTPPathInfoServiceParams {
                blob_service: "&root".to_string(),
                directory_service: "&root".to_string(),
                trusted_public_keys: vec![],
                force_download_nar: false,
            }
        }
    ))]
    /// Correct Scheme for Nix HTTP Binary cache, with a subpath and port.
    #[case::correct_nix_http_with_subpath_and_port("nix+http://[::1]:8080/foo", Some(
        NixHTTPPathInfoServiceConfig {
            base_url: "http://[::1]:8080/foo".try_into().unwrap(),
            params: NixHTTPPathInfoServiceParams {
                blob_service: "&root".to_string(),
                directory_service: "&root".to_string(),
                trusted_public_keys: vec![],
                force_download_nar: false,
            }
        }

    ))]
    /// Correct Scheme for the cache.nixos.org binary cache, and correct trusted public key set
    #[case::correct_nix_https_with_trusted_public_key(
        "nix+https://cache.nixos.org?trusted_public_keys[0]=cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=", Some(
        NixHTTPPathInfoServiceConfig {
            base_url: "https://cache.nixos.org".try_into().unwrap(),
            params: NixHTTPPathInfoServiceParams {
                blob_service: "&root".to_string(),
                directory_service: "&root".to_string(),
                trusted_public_keys: vec![
                    "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=".to_string()
                ],
                force_download_nar: false,
            }
        }
    ))]
    /// Correct Scheme for the cache.nixos.org binary cache, and two correct trusted public keys set
    #[case::correct_nix_https_with_two_trusted_public_keys(
        "nix+https://cache.nixos.org?trusted_public_keys[0]=cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=&trusted_public_keys[1]=foo:jp4fCEx9tBEId/L0ZsVJ26k0wC0fu7vJqLjjIGFkup8=", Some(
        NixHTTPPathInfoServiceConfig {
            base_url: "https://cache.nixos.org".try_into().unwrap(),
            params: NixHTTPPathInfoServiceParams {
                blob_service: "&root".to_string(),
                directory_service: "&root".to_string(),
                trusted_public_keys: vec![
                    "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=".to_string(),
                    "foo:jp4fCEx9tBEId/L0ZsVJ26k0wC0fu7vJqLjjIGFkup8=".to_string()
                ],
                force_download_nar: false,
            }
        }
    ))]
    #[case::wrong_scheme("nix+grpc://example.com", None)]
    #[case::missing_host("nix+http:///", None)]
    #[case::missing_authority("nix+http:", None)]
    /// Correct cache.nixos.org binary cache URL, but wrong `trusted_public_keys` param usage (should be list)
    #[case::trusted_public_keys_no_sequence(
        "nix+https://cache.nixos.org?trusted_public_keys=cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=",
        None
    )]
    /// Correct cache.nixos.org binary cache URL, but wrong param name
    #[case::trusted_public_keys_wrong_pubkey(
        "nix+https://cache.nixos.org?trustedpublickeys=cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=",
        None
    )]
    fn parse_url(#[case] url_str: &str, #[case] exp_config: Option<NixHTTPPathInfoServiceConfig>) {
        let url: Url = url_str.parse().expect("url to parse");

        match (NixHTTPPathInfoServiceConfig::try_from(url), exp_config) {
            (Ok(_), None) => panic!("parsing url unexpectedly succeeded"),
            (Ok(config), Some(exp_config)) => assert_eq!(exp_config, config),
            (Err(_), None) => {}
            (Err(e), Some(_)) => panic!("parsing url unexpectedly failed: {e}"),
        }
    }
}
