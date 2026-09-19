use crate::{buildservice::BuildService, proto};
use futures::StreamExt;
use futures::stream::BoxStream;
use tonic::{Response, async_trait};

/// Implements the gRPC server trait ([crate::proto::build_service_server::BuildService]
/// for anything implementing [BuildService].
pub struct GRPCBuildServiceWrapper<BUILD> {
    inner: BUILD,
}

impl<BUILD> GRPCBuildServiceWrapper<BUILD> {
    pub fn new(build_service: BUILD) -> Self {
        Self {
            inner: build_service,
        }
    }
}

#[async_trait]
impl<BUILD> crate::proto::build_service_server::BuildService for GRPCBuildServiceWrapper<BUILD>
where
    BUILD: BuildService + Clone + 'static,
{
    type DoBuildStream = BoxStream<'static, tonic::Result<proto::BuildUpdate>>;

    async fn do_build(
        &self,
        request: tonic::Request<proto::BuildRequest>,
    ) -> tonic::Result<Response<Self::DoBuildStream>> {
        let proto_build_request = request.into_inner();
        let build_request = crate::buildservice::BuildRequest::try_from(proto_build_request)
            .map_err(|err| tonic::Status::new(tonic::Code::InvalidArgument, err.to_string()))?;

        let build_service = self.inner.clone();
        let stream = async_stream::try_stream! {
            let mut build_updates = build_service.do_build(build_request);
            while let Some(build_update) = build_updates.next().await {
                yield proto::BuildUpdate::from(build_update);
            }
        };

        Ok(Response::new(stream.boxed()))
    }
}
