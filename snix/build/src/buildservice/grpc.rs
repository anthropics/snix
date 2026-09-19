use futures::stream::BoxStream;
use futures::{StreamExt, TryStreamExt};
use tracing::instrument;

use crate::buildservice::{BuildFailure, BuildRequest, BuildUpdate};
use crate::proto::build_service_client::BuildServiceClient;

use super::BuildService;

pub struct GRPCBuildService<T> {
    client: BuildServiceClient<T>,
}

impl<T> GRPCBuildService<T> {
    #[allow(dead_code)]
    pub fn from_client(client: BuildServiceClient<T>) -> Self {
        Self { client }
    }
}

impl<T> BuildService for GRPCBuildService<T>
where
    T: tonic::client::GrpcService<tonic::body::Body> + Send + Sync + Clone + 'static,
    T::ResponseBody: tonic::codegen::Body<Data = tonic::codegen::Bytes> + Send + 'static,
    <T::ResponseBody as tonic::codegen::Body>::Error: Into<tonic::codegen::StdError> + Send,
    T::Future: Send,
{
    #[instrument(skip(self))]
    fn do_build(&self, request: BuildRequest) -> BoxStream<'_, BuildUpdate> {
        let mut client = self.client.clone();

        async_stream::try_stream! {
            let mut stream = client
                .do_build(tonic::Request::new(request.into()))
                .await?
                .into_inner();

            while let Some(proto_build_update) = stream.try_next().await? {
                yield match BuildUpdate::try_from(proto_build_update) {
                    Ok(build_update) => build_update,
                    Err(err) => Err::<_, BuildFailure>(err.into())?,
                }
            }
        }
        .map(|elem: Result<BuildUpdate, BuildFailure>| match elem {
            Ok(build_update) => build_update,
            Err(err) => err.into(),
        })
        .boxed()
    }
}
