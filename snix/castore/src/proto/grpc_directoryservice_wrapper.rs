use crate::directoryservice::DirectoryService;
use crate::directoryservice::order_validator::{LeavesToRoot, OrderValidator};
use crate::{B3Digest, DirectoryError, proto};
use futures::stream::BoxStream;
use futures::{StreamExt, TryStreamExt};
use tonic::{Request, Response, Status, Streaming, async_trait};
use tracing::{instrument, warn};

pub struct GRPCDirectoryServiceWrapper<T> {
    directory_service: T,
}

impl<T> GRPCDirectoryServiceWrapper<T> {
    pub fn new(directory_service: T) -> Self {
        Self { directory_service }
    }
}

#[async_trait]
impl<T> proto::directory_service_server::DirectoryService for GRPCDirectoryServiceWrapper<T>
where
    T: DirectoryService + Clone + Send + Sync + 'static,
{
    type GetStream = BoxStream<'static, tonic::Result<proto::Directory>>;

    #[instrument(skip_all)]
    async fn get(
        &self,
        request: Request<proto::GetDirectoryRequest>,
    ) -> Result<Response<Self::GetStream>, Status> {
        let req_inner = request.into_inner();

        match &req_inner
            .by_what
            .ok_or_else(|| Status::invalid_argument("invalid by_what"))?
        {
            proto::get_directory_request::ByWhat::Digest(digest) => {
                let digest: B3Digest = digest
                    .clone()
                    .try_into()
                    .map_err(|_e| Status::invalid_argument("invalid digest length"))?;

                let directory_service = self.directory_service.clone();

                Ok(tonic::Response::new({
                    async_stream::try_stream! {
                        if !req_inner.recursive {
                            let directory = directory_service
                                .get(&digest)
                                .await
                                .map_err(|e| {
                                    warn!(err = %e, directory.digest=%digest, "failed to get directory");
                                    tonic::Status::new(tonic::Code::Internal, e.to_string())
                                })?
                                .ok_or_else(|| {
                                    Status::not_found(format!("directory {digest} not found"))
                                })?;

                            yield directory.into();
                        } else {
                            // If recursive was requested, traverse via get_recursive.
                            // We need to use some type acrobatics as prost wants streams with 'static lifetimes.
                            let mut s = get_recursive_owned(std::sync::Arc::new(directory_service),digest)
                                .map_ok(proto::Directory::from)
                                .map_err(|e| tonic::Status::new(tonic::Code::Internal, e.to_string()));

                            while let Some(directory) = s.try_next().await? {
                                yield directory
                            }
                        }

                    }.boxed()
                }))
            }
        }
    }

    #[instrument(skip_all)]
    async fn put(
        &self,
        request: Request<Streaming<proto::Directory>>,
    ) -> Result<Response<proto::PutDirectoryResponse>, Status> {
        let mut req_inner = request.into_inner();

        // Validate all received directories.
        let mut validator = LeavesToRoot::new();

        // Insert into the backing DirectoryService as we receive.
        let mut directory_putter = self.directory_service.put_multiple_start();

        while let Some(directory) = req_inner.message().await? {
            let directory: crate::Directory =
                directory.try_into().map_err(|e: DirectoryError| {
                    tonic::Status::new(tonic::Code::Internal, e.to_string())
                })?;
            validator
                .try_accept(&directory)
                .map_err(|e| tonic::Status::new(tonic::Code::Internal, e.to_string()))?;

            directory_putter
                .put(directory)
                .await
                .map_err(|e| tonic::Status::new(tonic::Code::Internal, e.to_string()))?;
        }

        // Finalize validator, checks connectivity.
        validator
            .finalize()
            .map_err(|e| tonic::Status::new(tonic::Code::Internal, e.to_string()))?;

        Ok(Response::new(proto::PutDirectoryResponse {
            // Properly close the directory putter, returning any potential errors.
            root_digest: directory_putter
                .close()
                .await
                .map_err(|e| tonic::Status::new(tonic::Code::Internal, e.to_string()))?
                .into(),
        }))
    }
}

/// The same as [DirectoryService::get_recursive], but returning a stream with a static lifetime.
/// It's only used for the gRPC server wrapper, which requires static lifetimes.
fn get_recursive_owned<S>(
    svc: std::sync::Arc<S>,
    root_directory_digest: B3Digest,
) -> BoxStream<'static, Result<crate::Directory, crate::directoryservice::Error>>
where
    S: DirectoryService + 'static,
{
    async_stream::try_stream! {
        let mut  directories = svc.get_recursive(&root_directory_digest);
        while let Some(directory) = directories.try_next().await? {
            yield directory;
        }
    }
    .boxed()
}
