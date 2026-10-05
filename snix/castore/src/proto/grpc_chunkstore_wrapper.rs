use crate::{
    B3Digest,
    chunkstore::{Chunk, ChunkStore},
    proto,
};
use futures::stream::BoxStream;
use tonic::{Request, Response, Status, Streaming, async_trait};
use tracing::{Span, instrument, warn};

/// 2 MiB, well below the size limit of gRPC messages (4 MiB).
const GRPC_CHUNK_SIZE: usize = 2 * 1024 * 1024;

/// Wraps a [ChunkStore] into a gRPC
/// [proto::chunk_store_service_server::ChunkStoreService].
pub struct GRPCChunkStoreWrapper<T> {
    chunk_store: T,
}

impl<T> GRPCChunkStoreWrapper<T> {
    /// Constructs a new [GRPCChunkStoreWrapper] from a [ChunkStore].
    pub fn new(chunk_store: T) -> Self {
        Self { chunk_store }
    }
}

#[async_trait]
impl<T> proto::chunk_store_service_server::ChunkStoreService for GRPCChunkStoreWrapper<T>
where
    T: ChunkStore + 'static,
{
    type GetChunkStream = BoxStream<'static, Result<proto::GetChunkResponse, Status>>;

    #[instrument(skip_all, fields(chunk.digest=tracing::field::Empty))]
    async fn stat_chunk(
        &self,
        request: Request<proto::StatChunkRequest>,
    ) -> Result<Response<proto::StatChunkResponse>, Status> {
        let digest: B3Digest = request
            .into_inner()
            .digest
            .try_into()
            .map_err(|_| Status::invalid_argument("invalid digest length"))?;

        let span = Span::current();
        span.record("chunk.digest", digest.to_string());

        match self.chunk_store.has(&digest).await {
            Ok(true) => Ok(Response::new(proto::StatChunkResponse {})),
            Ok(false) => Err(Status::not_found(format!("chunk {digest} not found"))),
            Err(e) => {
                warn!(err = %e, "failed to check for chunk");
                Err(Status::internal(e.to_string()))
            }
        }
    }

    #[instrument(skip_all, fields(chunk.digest=tracing::field::Empty))]
    async fn get_chunk(
        &self,
        request: Request<proto::GetChunkRequest>,
    ) -> Result<Response<Self::GetChunkStream>, Status> {
        let digest: B3Digest = request
            .into_inner()
            .digest
            .try_into()
            .map_err(|_| Status::invalid_argument("invalid digest length"))?;

        let span = Span::current();
        span.record("chunk.digest", digest.to_string());

        match self.chunk_store.get(&digest).await {
            Ok(Some(chunk)) => {
                let chunk_bytes: bytes::Bytes = chunk.into();
                let chunk_len = chunk_bytes.len();
                let stream =
                    futures::stream::iter((0..chunk_len).step_by(GRPC_CHUNK_SIZE).map(move |i| {
                        Ok(proto::GetChunkResponse {
                            data: chunk_bytes.slice(i..(i + GRPC_CHUNK_SIZE).min(chunk_len)),
                        })
                    }));
                Ok(Response::new(Box::pin(stream)))
            }
            Ok(None) => Err(Status::not_found(format!("chunk {digest} not found"))),
            Err(e) => {
                warn!(err = %e, "failed to get chunk");
                Err(Status::internal(e.to_string()))
            }
        }
    }

    #[instrument(skip_all)]
    async fn put_chunk(
        &self,
        request: Request<Streaming<proto::PutChunkRequest>>,
    ) -> Result<Response<proto::PutChunkResponse>, Status> {
        let mut stream = request.into_inner();

        let mut chunk_builder = Chunk::builder();
        while let Some(proto::PutChunkRequest { data }) = stream.message().await? {
            chunk_builder.append_bytes(data);
        }
        let chunk = chunk_builder.build();

        let digest = self.chunk_store.put(chunk).await.map_err(|err| {
            warn!(%err, "failed to put chunk");
            Status::internal(err.to_string())
        })?;

        Ok(Response::new(proto::PutChunkResponse {
            digest: digest.into(),
        }))
    }
}
