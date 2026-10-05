use crate::{
    B3Digest,
    blobstore::{BlobMeta, BlobStore},
    proto,
};
use tonic::{Request, Response, Status, async_trait};
use tracing::{Span, instrument, warn};

/// Wraps a [BlobStore] into a gRPC
/// [proto::blob_store_service_server::BlobStoreService].
pub struct GRPCBlobStoreWrapper<T> {
    blob_store: T,
}

impl<T> GRPCBlobStoreWrapper<T> {
    /// Constructs a new [GRPCBlobStoreWrapper] from a [BlobStore].
    pub fn new(blob_store: T) -> Self {
        Self { blob_store }
    }
}

#[async_trait]
impl<T> proto::blob_store_service_server::BlobStoreService for GRPCBlobStoreWrapper<T>
where
    T: BlobStore + 'static,
{
    #[instrument(skip_all, fields(blob.digest=tracing::field::Empty))]
    async fn stat_blob(
        &self,
        request: Request<proto::BsStatBlobRequest>,
    ) -> Result<Response<proto::BsStatBlobResponse>, Status> {
        let digest: B3Digest = request
            .into_inner()
            .digest
            .try_into()
            .map_err(|_| Status::invalid_argument("invalid digest length"))?;

        let span = Span::current();
        span.record("blob.digest", digest.to_string());

        match self.blob_store.has(&digest).await {
            Ok(true) => Ok(Response::new(proto::BsStatBlobResponse {})),
            Ok(false) => Err(Status::not_found(format!("blob {digest} not found"))),
            Err(err) => {
                warn!(%err, "failed to check for blob");
                Err(Status::internal(err.to_string()))
            }
        }
    }

    #[instrument(skip_all, fields(blob.digest=tracing::field::Empty))]
    async fn get_blob(
        &self,
        request: Request<proto::GetBlobRequest>,
    ) -> Result<Response<proto::GetBlobResponse>, Status> {
        let digest: B3Digest = request
            .into_inner()
            .digest
            .try_into()
            .map_err(|_| Status::invalid_argument("invalid digest length"))?;

        let span = Span::current();
        span.record("blob.digest", digest.to_string());

        match self.blob_store.get(&digest).await {
            Ok(Some(blob_meta)) => Ok(Response::new(proto::GetBlobResponse {
                blob_meta: Some(blob_meta.into()),
            })),
            Ok(None) => Err(Status::not_found(format!("blob {digest} not found"))),
            Err(e) => {
                warn!(err = %e, "failed to get blob");
                Err(Status::internal(e.to_string()))
            }
        }
    }

    #[instrument(skip_all, fields(blob.digest=tracing::field::Empty))]
    async fn put_blob(
        &self,
        request: Request<proto::PutBlobRequest>,
    ) -> Result<Response<proto::BsPutBlobResponse>, Status> {
        let proto::PutBlobRequest { digest, blob_meta } = request.into_inner();

        let digest: B3Digest = digest
            .try_into()
            .map_err(|_| Status::invalid_argument("invalid digest length"))?;

        let span = Span::current();
        span.record("blob.digest", digest.to_string());

        let blob_meta =
            blob_meta.ok_or_else(|| Status::invalid_argument("no blob_meta provided"))?;
        let blob_meta =
            BlobMeta::try_from(blob_meta).map_err(|e| Status::invalid_argument(e.to_string()))?;

        self.blob_store
            .put(&digest, blob_meta)
            .await
            .map_err(|err| {
                warn!(%err, "failed to put blob");
                Status::internal(err.to_string())
            })?;

        Ok(Response::new(proto::BsPutBlobResponse {}))
    }
}
