use super::{PathInfo, PathInfoService};
use crate::pathinfoservice;
use crate::{nar::NarCalculationService, proto};
use async_stream::try_stream;
use futures::{StreamExt, stream::BoxStream};
use nix_compat::nixbase32;
use snix_castore::Node;
use snix_castore::composition::{CompositionContext, ServiceBuilder};
use std::sync::Arc;
use tonic::{Code, async_trait};
use tracing::{Span, instrument, warn};
use tracing_indicatif::span_ext::IndicatifSpanExt;

/// Connects to a (remote) snix-store PathInfoService over gRPC.
#[derive(Clone)]
pub struct GRPCPathInfoService<T> {
    instance_name: String,

    /// The internal reference to a gRPC client.
    /// Cloning it is cheap, and it internally handles concurrent requests.
    grpc_client: proto::path_info_service_client::PathInfoServiceClient<T>,
}

impl<T> GRPCPathInfoService<T> {
    /// construct a [GRPCPathInfoService] from a [proto::path_info_service_client::PathInfoServiceClient].
    /// panics if called outside the context of a tokio runtime.
    pub fn from_client(
        instance_name: String,
        grpc_client: proto::path_info_service_client::PathInfoServiceClient<T>,
    ) -> Self {
        Self {
            instance_name,
            grpc_client,
        }
    }
}

#[async_trait]
impl<T> PathInfoService for GRPCPathInfoService<T>
where
    T: tonic::client::GrpcService<tonic::body::Body> + Send + Sync + Clone + 'static,
    T::ResponseBody: tonic::codegen::Body<Data = tonic::codegen::Bytes> + Send + 'static,
    <T::ResponseBody as tonic::codegen::Body>::Error: Into<tonic::codegen::StdError> + Send,
    T::Future: Send,
{
    #[instrument(level = "trace", skip_all, fields(path_info.digest = nixbase32::encode(&digest), instance_name = %self.instance_name))]
    async fn get(&self, digest: [u8; 20]) -> Result<Option<PathInfo>, pathinfoservice::Error> {
        match self
            .grpc_client
            .clone()
            .get(proto::GetPathInfoRequest {
                by_what: Some(proto::get_path_info_request::ByWhat::ByOutputHash(
                    digest.to_vec().into(),
                )),
            })
            .await
        {
            Ok(path_info) => Ok(Some(
                PathInfo::try_from(path_info.into_inner()).map_err(Error::PathInfoValidation)?,
            )),
            Err(e) if e.code() == Code::NotFound => Ok(None),
            Err(e) => Err(Error::Tonic(e))?,
        }
    }

    #[instrument(level = "trace", skip_all, fields(path_info.root_node = ?path_info.node, instance_name = %self.instance_name))]
    async fn put(&self, path_info: PathInfo) -> Result<PathInfo, pathinfoservice::Error> {
        let path_info = self
            .grpc_client
            .clone()
            .put(proto::PathInfo::from(path_info))
            .await
            .map_err(Error::Tonic)?
            .into_inner();
        Ok(PathInfo::try_from(path_info).map_err(Error::PathInfoValidation)?)
    }

    #[instrument(level = "trace", skip_all)]
    fn list(&self) -> BoxStream<'static, Result<PathInfo, pathinfoservice::Error>> {
        let mut grpc_client = self.grpc_client.clone();

        try_stream! {
            let resp = grpc_client.list(proto::ListPathInfoRequest::default()).await;

            let mut stream = resp.map_err(Error::Tonic)?.into_inner();

            while let Some(path_info_proto) =  stream.message().await.map_err(Error::Tonic)? {
                yield PathInfo::try_from(path_info_proto).map_err(Error::PathInfoValidation)?
            }
        }
        .boxed()
    }

    #[instrument(level = "trace", skip_all)]
    fn nar_calculation_service(&self) -> Option<Arc<dyn NarCalculationService>> {
        Some(Arc::new(self.clone()) as Arc<dyn NarCalculationService>)
    }
}

#[async_trait]
impl<T> NarCalculationService for GRPCPathInfoService<T>
where
    T: tonic::client::GrpcService<tonic::body::Body> + Send + Sync + Clone + 'static,
    T::ResponseBody: tonic::codegen::Body<Data = tonic::codegen::Bytes> + Send + 'static,
    <T::ResponseBody as tonic::codegen::Body>::Error: Into<tonic::codegen::StdError> + Send,
    T::Future: Send,
{
    #[instrument(level = "trace", skip_all, fields(root_node = ?root_node, indicatif.pb_show=tracing::field::Empty))]
    async fn calculate_nar(
        &self,
        root_node: &Node,
    ) -> Result<(u64, [u8; 32]), pathinfoservice::Error> {
        let span = Span::current();
        span.pb_set_message("Waiting for NAR calculation");
        span.pb_start();

        let path_info = self
            .grpc_client
            .clone()
            .calculate_nar(snix_castore::proto::Entry::from_name_and_node(
                "".into(),
                root_node.to_owned(),
            ))
            .await
            .map_err(Error::Tonic)?
            .into_inner();

        let nar_sha256: [u8; 32] = path_info
            .nar_sha256
            .to_vec()
            .try_into()
            .map_err(|_| Error::NarCalcInvalidDigestLen)?;

        Ok((path_info.nar_size, nar_sha256))
    }
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("failed to decode protobuf: {0}")]
    ProtobufDecode(#[from] prost::DecodeError),
    #[error("failed to validate PathInfo: {0}")]
    PathInfoValidation(#[from] crate::proto::ValidatePathInfoError),

    #[error("tonic status: {0}")]
    Tonic(#[from] tonic::Status),

    #[error("invalid digest length returned from nar calculation")]
    NarCalcInvalidDigestLen,

    #[error("join error: {0}")]
    TokioJoin(#[from] tokio::task::JoinError),
    #[error("io error: {0}")]
    IO(#[from] std::io::Error),
}

#[derive(serde::Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct GRPCPathInfoServiceConfig {
    url: url::Url,
}

impl TryFrom<url::Url> for GRPCPathInfoServiceConfig {
    type Error = Box<dyn std::error::Error + Send + Sync>;
    fn try_from(url: url::Url) -> Result<Self, Self::Error> {
        Ok(GRPCPathInfoServiceConfig { url })
    }
}

#[async_trait]
impl ServiceBuilder for GRPCPathInfoServiceConfig {
    type Output = dyn PathInfoService;
    async fn build<'a>(
        &'a self,
        instance_name: &str,
        _context: &CompositionContext,
    ) -> Result<Arc<Self::Output>, Box<dyn std::error::Error + Send + Sync>> {
        let client = proto::path_info_service_client::PathInfoServiceClient::with_interceptor(
            snix_castore::tonic::channel_from_url(&self.url).await?,
            snix_tracing::propagate::tonic::send_trace,
        );
        Ok(Arc::new(GRPCPathInfoService::from_client(
            instance_name.to_string(),
            client,
        )))
    }
}

#[cfg(test)]
mod tests {
    use crate::pathinfoservice::PathInfoService;
    use crate::pathinfoservice::tests::make_grpc_path_info_service_client;

    /// This ensures connecting via gRPC works as expected.
    #[tokio::test]
    async fn test_valid_unix_path_ping_pong() {
        let (_blob_service, _directory_service, path_info_service) =
            make_grpc_path_info_service_client().await;

        let path_info = path_info_service
            .get(crate::fixtures::DUMMY_PATH_DIGEST)
            .await
            .expect("must not be error");

        assert!(path_info.is_none());
    }
}
