use std::sync::Arc;

use crate::blobservice::BlobService;
use crate::proto::GRPCBlobServiceWrapper;
use crate::proto::blob_service_client::BlobServiceClient;
use crate::utils::gen_test_blob_service;
use crate::{blobservice::GRPCBlobService, proto::blob_service_server::BlobServiceServer};
use hyper_util::rt::TokioIo;
use tonic::transport::{Endpoint, Server, Uri};

/// Constructs and returns a gRPC BlobService.
/// The server part is a [MemoryBlobService], exposed via the
/// [GRPCBlobServiceWrapper], and connected through a DuplexStream
pub async fn make_grpc_blob_service_client() -> Box<dyn BlobService> {
    let (left, right) = tokio::io::duplex(64);

    // spin up a server, which will only connect once, to the left side.
    tokio::spawn(async {
        let mut server = Server::builder();
        let router = server.add_service(BlobServiceServer::new(GRPCBlobServiceWrapper::new(
            Arc::new(gen_test_blob_service()) as Arc<dyn BlobService>,
        )));

        router
            .serve_with_incoming(tokio_stream::once(Ok::<_, std::io::Error>(left)))
            .await
    });

    // Create a client, connecting to the right side. The URI is unused.
    let mut maybe_right = Some(right);

    Box::new(GRPCBlobService::from_client(
        "root".into(),
        BlobServiceClient::new(
            Endpoint::try_from("http://[::]:50051")
                .unwrap()
                .connect_with_connector(tower::service_fn(move |_: Uri| {
                    let right = maybe_right.take().unwrap();
                    async move { Ok::<_, std::io::Error>(TokioIo::new(right)) }
                }))
                .await
                .unwrap(),
        ),
    ))
}
