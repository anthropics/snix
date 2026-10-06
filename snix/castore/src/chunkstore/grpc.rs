//! A gRPC client implementation.

use futures::TryStreamExt;
use std::sync::Arc;
use tonic::{Code, async_trait};
use tracing::instrument;

use crate::{
    B3Digest,
    chunkstore::{Chunk, ChunkStore},
    composition::{CompositionContext, ServiceBuilder},
    proto::{self, GetChunkRequest, GetChunkResponse, PutChunkRequest, PutChunkResponse},
};

/// 2 MiB, well below the size limit of gRPC messages (4 MiB)
const GRPC_CHUNK_SIZE: usize = 2 * 1024 * 1024;

/// Connects to a (remote) snix-castore ChunkStoreService over gRPC.
#[derive(Clone)]
pub struct GrpcChunkStore<T> {
    instance_name: String,
    /// The internal reference to a gRPC client.
    /// Cloning it is cheap, and it internally handles concurrent requests.
    client: proto::chunk_store_service_client::ChunkStoreServiceClient<T>,
}

impl<T> GrpcChunkStore<T> {
    /// construct a [GrpcChunkStore] from a [proto::chunk_store_service_client::ChunkStoreServiceClient].
    pub fn from_client(
        instance_name: String,
        client: proto::chunk_store_service_client::ChunkStoreServiceClient<T>,
    ) -> Self {
        Self {
            instance_name,
            client,
        }
    }
}

#[async_trait]
impl<T> ChunkStore for GrpcChunkStore<T>
where
    T: tonic::client::GrpcService<tonic::body::Body> + Send + Sync + Clone + 'static,
    T::ResponseBody: tonic::codegen::Body<Data = tonic::codegen::Bytes> + Send + 'static,
    <T::ResponseBody as tonic::codegen::Body>::Error: Into<tonic::codegen::StdError> + Send,
    T::Future: Send,
{
    #[instrument(skip_all, err, fields(chunk.digest=%digest, instance_name=%self.instance_name))]
    async fn get(&self, digest: &B3Digest) -> Result<Option<Chunk>, super::Error> {
        let resp = match self
            .client
            .clone()
            .get_chunk(GetChunkRequest::with_digest(*digest))
            .await
        {
            Ok(resp) => resp,
            Err(err) if err.code() == Code::NotFound => return Ok(None),
            Err(err) => Err(Error::Tonic(err))?,
        };

        // collect messages from the stream
        let mut stream = resp.into_inner();
        let mut chunk_builder = Chunk::builder();
        while let Some(GetChunkResponse { data }) = stream.try_next().await.map_err(Error::Tonic)? {
            chunk_builder.append_bytes(data);
        }
        let chunk = chunk_builder.build();

        // verify digest
        let actual_digest = chunk.digest();
        if &chunk.digest() != digest {
            Err(Error::GetIncorrectDigest {
                expected: digest.to_owned(),
                actual: actual_digest,
            })?;
        }

        Ok(Some(chunk))
    }

    #[instrument(skip_all, err, fields(chunk.digest=%digest, instance_name=%self.instance_name))]
    async fn has(&self, digest: &B3Digest) -> Result<bool, super::Error> {
        match self
            .client
            .clone()
            .stat_chunk(proto::StatChunkRequest::with_digest(*digest))
            .await
        {
            Ok(_blob_meta) => Ok(true),
            Err(e) if e.code() == Code::NotFound => Ok(false),
            Err(err) => Err(Error::Tonic(err))?,
        }
    }

    #[instrument(skip_all, err, ret(Display), fields(instance_name=%self.instance_name))]
    async fn put(&self, chunk: Chunk) -> Result<B3Digest, super::Error> {
        let chunk_digest: B3Digest = chunk.digest();
        let chunk_len = chunk.len();
        let chunk_bytes: bytes::Bytes = chunk.into();

        let resp = self
            .client
            .clone()
            .put_chunk(futures::stream::iter(
                (0..chunk_len)
                    .step_by(GRPC_CHUNK_SIZE)
                    .map(move |i| PutChunkRequest {
                        data: chunk_bytes.slice(i..(i + GRPC_CHUNK_SIZE).min(chunk_len)),
                    }),
            ))
            .await
            .map_err(Error::Tonic)?;

        let PutChunkResponse { digest } = resp.into_inner();
        let resp_digest: B3Digest =
            B3Digest::try_from(digest).map_err(|_| Error::PutInvalidDigest)?;

        if chunk_digest != resp_digest {
            Err(Error::PutIncorrectDigest {
                expected: chunk_digest,
                actual: resp_digest,
            })?
        }

        Ok(chunk_digest)
    }
}

/// Configuration for a [GrpcChunkStore].
#[derive(serde::Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct GRPCChunkStoreConfig {
    url: String,
}

impl TryFrom<url::Url> for GRPCChunkStoreConfig {
    type Error = Box<dyn std::error::Error + Send + Sync>;
    fn try_from(url: url::Url) -> Result<Self, Self::Error> {
        //   normally grpc+unix for unix sockets, and grpc+http(s) for the HTTP counterparts.
        // - In the case of unix sockets, there must be a path, but may not be a host.
        // - In the case of non-unix sockets, there must be a host, but no path.
        // Constructing the channel is handled by snix_castore::channel::from_url.
        Ok(GRPCChunkStoreConfig {
            url: url.to_string(),
        })
    }
}

#[async_trait]
impl ServiceBuilder for GRPCChunkStoreConfig {
    type Output = dyn ChunkStore;
    async fn build<'a>(
        &'a self,
        instance_name: &str,
        _context: &CompositionContext,
    ) -> Result<Arc<Self::Output>, Box<dyn std::error::Error + Send + Sync>> {
        let client = proto::chunk_store_service_client::ChunkStoreServiceClient::with_interceptor(
            crate::tonic::channel_from_url(&self.url.parse()?).await?,
            snix_tracing::propagate::tonic::send_trace,
        );
        Ok(Arc::new(GrpcChunkStore::from_client(
            instance_name.to_string(),
            client,
        )))
    }
}

#[derive(thiserror::Error, Debug)]
enum Error {
    #[error(transparent)]
    Tonic(tonic::Status),
    #[error("chunk with unexpected digest received, expected: {expected}, got: {actual}")]
    GetIncorrectDigest {
        expected: B3Digest,
        actual: B3Digest,
    },
    #[error("got invalid B3Digest in PutChunkResponse")]
    PutInvalidDigest,
    #[error("got unexpected digest {actual} after uploading chunk with digest {expected}")]
    PutIncorrectDigest {
        expected: B3Digest,
        actual: B3Digest,
    },
}

impl From<Error> for super::Error {
    fn from(value: Error) -> Self {
        super::Error(Box::new(value))
    }
}
