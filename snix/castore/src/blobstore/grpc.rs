//! A gRPC client implementation.

use std::sync::Arc;

use tonic::{Code, async_trait};
use tracing::instrument;

use crate::{
    B3Digest,
    blobstore::{BlobMeta, BlobStore},
    composition::{CompositionContext, ServiceBuilder},
    proto::{self, BsStatBlobRequest, GetBlobRequest, GetBlobResponse, PutBlobRequest},
};

/// Connects to a (remote) snix-castore BlobStoreService over gRPC.
#[derive(Clone)]
pub struct GrpcBlobStore<T> {
    instance_name: String,
    /// The internal reference to a gRPC client.
    /// Cloning it is cheap, and it internally handles concurrent requests.
    client: proto::blob_store_service_client::BlobStoreServiceClient<T>,
}

impl<T> GrpcBlobStore<T> {
    /// construct a [GrpcBlobStore] from a [proto::blob_store_service_client::BlobStoreServiceClient].
    pub fn from_client(
        instance_name: String,
        client: proto::blob_store_service_client::BlobStoreServiceClient<T>,
    ) -> Self {
        Self {
            instance_name,
            client,
        }
    }
}
#[async_trait]
impl<T> BlobStore for GrpcBlobStore<T>
where
    T: tonic::client::GrpcService<tonic::body::Body> + Send + Sync + Clone + 'static,
    T::ResponseBody: tonic::codegen::Body<Data = tonic::codegen::Bytes> + Send + 'static,
    <T::ResponseBody as tonic::codegen::Body>::Error: Into<tonic::codegen::StdError> + Send,
    T::Future: Send,
{
    #[instrument(skip_all, err, fields(blob.digest=%digest, instance_name=%self.instance_name))]
    async fn get(&self, digest: &B3Digest) -> Result<Option<BlobMeta>, super::Error> {
        let resp = match self
            .client
            .clone()
            .get_blob(GetBlobRequest::with_digest(*digest))
            .await
        {
            Ok(resp) => resp,
            Err(err) if err.code() == Code::NotFound => return Ok(None),
            Err(err) => Err(Error::Tonic(err))?,
        };

        let GetBlobResponse { blob_meta } = resp.into_inner();
        let proto_blob_meta = blob_meta.ok_or(Error::BlobMetaMissing)?;
        let blob_meta = BlobMeta::try_from(proto_blob_meta).map_err(Error::DecodeBlobMeta)?;

        Ok(Some(blob_meta))
    }

    #[instrument(skip_all, err, fields(blob.digest=%digest, instance_name=%self.instance_name))]
    async fn has(&self, digest: &B3Digest) -> Result<bool, super::Error> {
        match self
            .client
            .clone()
            .stat_blob(BsStatBlobRequest::with_digest(*digest))
            .await
        {
            Ok(_) => Ok(true),
            Err(err) if err.code() == Code::NotFound => return Ok(false),
            Err(err) => Err(Error::Tonic(err))?,
        }
    }

    #[instrument(skip_all, err, fields(blob.digest=%digest, instance_name=%self.instance_name))]
    async fn put(&self, digest: &B3Digest, blob_meta: BlobMeta) -> Result<(), super::Error> {
        match self
            .client
            .clone()
            .put_blob(PutBlobRequest {
                digest: (*digest).into(),
                blob_meta: Some(blob_meta.into()),
            })
            .await
        {
            Ok(_) => Ok(()),
            Err(err) => Err(Error::Tonic(err))?,
        }
    }
}

/// Configuration for a [GrpcBlobStore].
#[derive(serde::Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct GRPCBlobStoreConfig {
    url: url::Url,
}

impl TryFrom<url::Url> for GRPCBlobStoreConfig {
    type Error = Box<dyn std::error::Error + Send + Sync>;
    fn try_from(url: url::Url) -> Result<Self, Self::Error> {
        Ok(GRPCBlobStoreConfig { url })
    }
}

#[async_trait]
impl ServiceBuilder for GRPCBlobStoreConfig {
    type Output = dyn BlobStore;
    async fn build<'a>(
        &'a self,
        instance_name: &str,
        _context: &CompositionContext,
    ) -> Result<Arc<Self::Output>, Box<dyn std::error::Error + Send + Sync>> {
        let client = proto::blob_store_service_client::BlobStoreServiceClient::with_interceptor(
            crate::tonic::channel_from_url(&self.url).await?,
            snix_tracing::propagate::tonic::send_trace,
        );
        Ok(Arc::new(GrpcBlobStore::from_client(
            instance_name.to_string(),
            client,
        )))
    }
}

#[derive(thiserror::Error, Debug)]
enum Error {
    #[error(transparent)]
    Tonic(tonic::Status),
    #[error("no blob_meta in GetBlobResponse")]
    BlobMetaMissing,
    #[error("failed to decode BlobMeta")]
    DecodeBlobMeta(#[from] crate::blobstore::blob_meta::DecodeError),
}

impl From<Error> for super::Error {
    fn from(value: Error) -> Self {
        super::Error(Box::new(value))
    }
}
