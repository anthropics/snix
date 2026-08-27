use super::{Directory, DirectoryPutter, DirectoryService};
use crate::B3Digest;
use crate::composition::{CompositionContext, ServiceBuilder};
use crate::directoryservice::order_validator::{self, OrderValidator, RootToLeaves};
use crate::proto::{self, get_directory_request::ByWhat};
use futures::StreamExt;
use futures::stream::BoxStream;
use std::sync::Arc;
use tokio::spawn;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tonic::{Code, Status, async_trait};
use tracing::{Instrument as _, instrument, warn};

/// Maximum size of proto messages endoded and decoded.
/// Unfortunately there's directories out there larger than 4MiB, so bump this
/// by a bit.
pub const MAX_DECODING_MESSAGE_SIZE: usize = 10 * 1024 * 1024;

/// Connects to a (remote) snix-store DirectoryService over gRPC.
#[derive(Clone)]
pub struct GRPCDirectoryService<T> {
    instance_name: String,
    /// The internal reference to a gRPC client.
    /// Cloning it is cheap, and it internally handles concurrent requests.
    grpc_client: proto::directory_service_client::DirectoryServiceClient<T>,
}

impl<T> GRPCDirectoryService<T> {
    /// construct a [GRPCDirectoryService] from a [proto::directory_service_client::DirectoryServiceClient].
    /// panics if called outside the context of a tokio runtime.
    pub fn from_client(
        instance_name: String,
        grpc_client: proto::directory_service_client::DirectoryServiceClient<T>,
    ) -> Self {
        Self {
            instance_name,
            grpc_client,
        }
    }
}

#[async_trait]
impl<T> DirectoryService for GRPCDirectoryService<T>
where
    T: tonic::client::GrpcService<tonic::body::Body> + Send + Sync + Clone + 'static,
    T::ResponseBody: tonic::codegen::Body<Data = tonic::codegen::Bytes> + Send + 'static,
    <T::ResponseBody as tonic::codegen::Body>::Error: Into<tonic::codegen::StdError> + Send,
    T::Future: Send,
{
    #[instrument(level = "trace", skip_all, fields(directory.digest = %digest, instance_name = %self.instance_name))]
    async fn get(&self, digest: &B3Digest) -> Result<Option<Directory>, super::Error> {
        // clone the client, as it takes a &mut.
        // We retrieve the first message only, then close the stream (we set recursive to false)
        match self
            .grpc_client
            .clone()
            .get(proto::GetDirectoryRequest {
                recursive: false,
                by_what: Some(ByWhat::Digest((*digest).into())),
            })
            .await
            .map_err(Error::Tonic)?
            .into_inner()
            .message()
            .await
        {
            Ok(Some(proto_directory)) => {
                // Validate the retrieved Directory indeed has the
                // digest we expect it to have, to detect corruptions.
                let actual_digest = proto_directory.digest();
                if &actual_digest != digest {
                    Err(Error::WrongDigest {
                        expected: *digest,
                        actual: actual_digest,
                    })?
                } else {
                    let directory =
                        Directory::try_from(proto_directory).map_err(Error::DirectoryValidation)?;
                    Ok(Some(directory))
                }
            }
            Ok(None) => Ok(None),
            Err(e) if e.code() == Code::NotFound => Ok(None),
            Err(e) => Err(Error::Tonic(e))?,
        }
    }

    #[instrument(level = "trace", skip_all, fields(directory.digest = %directory.digest(), instance_name = %self.instance_name))]
    async fn put(&self, directory: Directory) -> Result<B3Digest, super::Error> {
        let resp = self
            .grpc_client
            .clone()
            .put(tokio_stream::once(proto::Directory::from(directory)))
            .await
            .map_err(Error::Tonic)?;

        let digest = resp
            .into_inner()
            .root_digest
            .try_into()
            .map_err(|_| Error::InvalidDigestLen)?;

        Ok(digest)
    }

    #[instrument(level = "trace", skip_all, fields(directory.digest = %root_directory_digest, instance_name = %self.instance_name))]
    fn get_recursive(
        &self,
        root_directory_digest: &B3Digest,
    ) -> BoxStream<'static, Result<Directory, super::Error>> {
        let mut grpc_client = self.grpc_client.clone();
        let root_directory_digest = *root_directory_digest;

        let mut order_validator = RootToLeaves::new_with_root_digest(root_directory_digest);

        async_stream::try_stream! {
            let mut directories = grpc_client
                .get(proto::GetDirectoryRequest {
                    recursive: true,
                    by_what: Some(ByWhat::Digest((root_directory_digest).into())),
                })
                .await
                .map_err(Error::Tonic)?
                .into_inner();

            while let Some(proto_directory) = directories.message().await.map_err(Error::Tonic)? {
                let directory = Directory::try_from(proto_directory).map_err(Error::DirectoryValidation)?;
                order_validator.try_accept(&directory).map_err(Error::DirectoryOrdering)?;

                yield directory;
            }
        }
        .boxed()
    }

    #[instrument(skip_all)]
    fn put_multiple_start(&self) -> Box<dyn DirectoryPutter + 'static> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        let task = spawn({
            let mut grpc_client = self.grpc_client.clone();

            async move {
                Ok::<_, Status>(
                    grpc_client
                        .put(UnboundedReceiverStream::new(rx))
                        .await?
                        .into_inner(),
                )
            }
            // instrument the task with the current span, this is not done by default
            .in_current_span()
        });

        Box::new(GRPCPutter {
            rq: Some((task, tx)),
        })
    }
}

/// Allows uploading multiple Directory messages in the same gRPC stream.
pub struct GRPCPutter {
    /// Data about the current request - a handle to the task, and the tx part
    /// of the channel.
    /// The tx part of the pipe is used to send [proto::Directory] to the ongoing request.
    /// The task will yield a [proto::PutDirectoryResponse] once the stream is closed.
    #[allow(clippy::type_complexity)] // lol
    rq: Option<(
        JoinHandle<Result<proto::PutDirectoryResponse, Status>>,
        UnboundedSender<proto::Directory>,
    )>,
}

#[async_trait]
impl DirectoryPutter for GRPCPutter {
    #[instrument(level = "trace", skip_all, fields(directory.digest=%directory.digest()), err)]
    async fn put(&mut self, directory: Directory) -> Result<(), super::Error> {
        let (_, directory_sender) = self
            .rq
            .as_ref()
            .ok_or_else(|| Error::DirectoryPutterAlreadyClosed)?;
        // If we're not already closed, send the directory to directory_sender.
        if directory_sender.send(directory.into()).is_err() {
            // If the channel has been prematurely closed, invoke close (so we can peek at the error code)
            // That error code is much more helpful, because it
            // contains the error message from the server.
            self.close().await?;
        }
        Ok(())
    }

    /// Closes the stream for sending, and returns the value.
    #[instrument(level = "trace", skip_all, ret, err)]
    async fn close(&mut self) -> Result<B3Digest, super::Error> {
        // get self.rq, and replace it with None.
        // This ensures we can only close it once.
        let (task, directory_sender) =
            std::mem::take(&mut self.rq).ok_or_else(|| Error::DirectoryPutterAlreadyClosed)?;

        // close directory_sender, so blocking on task will finish.
        drop(directory_sender);

        let resp = task
            .await
            .map_err(Error::TokioJoin)?
            .map_err(Error::Tonic)?;

        Ok(B3Digest::try_from(resp.root_digest).map_err(|_| Error::InvalidDigestLen)?)
    }
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Directory Graph ordering error: {0}")]
    DirectoryOrdering(#[from] order_validator::OrderingError),

    #[error("DirectoryPutter already closed")]
    DirectoryPutterAlreadyClosed,

    #[error("requested directory has wrong digest, expected {expected}, actual {actual}")]
    WrongDigest {
        expected: B3Digest,
        actual: B3Digest,
    },

    #[error("tonic status: {0}")]
    Tonic(#[from] tonic::Status),

    #[error("invalid digest length returned from put")]
    InvalidDigestLen,

    #[error("failed to decode protobuf: {0}")]
    ProtobufDecode(#[from] prost::DecodeError),
    #[error("failed to validate directory: {0}")]
    DirectoryValidation(#[from] crate::DirectoryError),

    #[error("join error: {0}")]
    TokioJoin(#[from] tokio::task::JoinError),
    #[error("io error: {0}")]
    IO(#[from] std::io::Error),
}

impl From<Error> for super::Error {
    fn from(value: Error) -> Self {
        Self(Box::new(value))
    }
}

#[derive(serde::Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct GRPCDirectoryServiceConfig {
    url: String,
}

impl TryFrom<url::Url> for GRPCDirectoryServiceConfig {
    type Error = Box<dyn std::error::Error + Send + Sync>;
    fn try_from(url: url::Url) -> Result<Self, Self::Error> {
        //   This is normally grpc+unix for unix sockets, and grpc+http(s) for the HTTP counterparts.
        // - In the case of unix sockets, there must be a path, but may not be a host.
        // - In the case of non-unix sockets, there must be a host, but no path.
        // Constructing the channel is handled by snix_castore::channel::from_url.
        Ok(GRPCDirectoryServiceConfig {
            url: url.to_string(),
        })
    }
}

#[async_trait]
impl ServiceBuilder for GRPCDirectoryServiceConfig {
    type Output = dyn DirectoryService;
    async fn build<'a>(
        &'a self,
        instance_name: &str,
        _context: &CompositionContext,
    ) -> Result<Arc<Self::Output>, Box<dyn std::error::Error + Send + Sync>> {
        let client = proto::directory_service_client::DirectoryServiceClient::with_interceptor(
            crate::tonic::channel_from_url(&self.url.parse()?).await?,
            snix_tracing::propagate::tonic::send_trace,
        );

        let client = client.max_decoding_message_size(MAX_DECODING_MESSAGE_SIZE);

        Ok(Arc::new(GRPCDirectoryService::from_client(
            instance_name.to_string(),
            client,
        )))
    }
}
#[cfg(test)]
mod tests {
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::net::UnixListener;
    use tokio_retry::{Retry, strategy::ExponentialBackoff};
    use tokio_stream::wrappers::UnixListenerStream;

    use crate::{
        directoryservice::{DirectoryService, GRPCDirectoryService},
        fixtures,
        proto::{GRPCDirectoryServiceWrapper, directory_service_client::DirectoryServiceClient},
        utils::gen_test_directory_service,
    };

    /// This ensures connecting via gRPC works as expected.
    #[tokio::test]
    async fn test_valid_unix_path_ping_pong() {
        let tmpdir = TempDir::new().unwrap();
        let socket_path = tmpdir.path().join("daemon");

        let path_clone = socket_path.clone();

        // Spin up a server
        tokio::spawn(async {
            let uds = UnixListener::bind(path_clone).unwrap();
            let uds_stream = UnixListenerStream::new(uds);

            // spin up a new server
            let mut server = tonic::transport::Server::builder();
            let router = server.add_service(
                crate::proto::directory_service_server::DirectoryServiceServer::new(
                    GRPCDirectoryServiceWrapper::new(Box::new(gen_test_directory_service())),
                ),
            );
            router.serve_with_incoming(uds_stream).await
        });

        // wait for the socket to be created
        Retry::start(
            ExponentialBackoff::from_millis(20).max_delay(Duration::from_secs(10)),
            || async {
                if socket_path.exists() {
                    Ok(())
                } else {
                    Err(())
                }
            },
        )
        .await
        .expect("failed to wait for socket");

        // prepare a client
        let grpc_client = {
            let url = url::Url::parse(&format!(
                "grpc+unix:{}?wait-connect=1",
                socket_path.display()
            ))
            .expect("must parse");
            let client = DirectoryServiceClient::new(
                crate::tonic::channel_from_url(&url)
                    .await
                    .expect("must succeed"),
            );
            GRPCDirectoryService::from_client("test-instance".into(), client)
        };

        assert!(
            grpc_client
                .get(&fixtures::DIRECTORY_A.digest())
                .await
                .expect("must not fail")
                .is_none()
        )
    }
}
