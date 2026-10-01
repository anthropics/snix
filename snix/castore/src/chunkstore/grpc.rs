//! A gRPC client implementation.

use futures::TryStreamExt;
use tonic::{Code, async_trait};
use tracing::instrument;

use crate::{
    B3Digest,
    chunkstore::{Chunk, ChunkStore},
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
        let mut chunk_builder = ChunkBuilder::default();
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

/// Allows assembling a [Chunk] from one or more [bytes::Bytes]
/// If the Chunk is constructed from just one bytes::Bytes, we reuse it zero-copy.
#[derive(Default)]
enum ChunkBuilder {
    #[default]
    Empty,
    Single(bytes::Bytes),
    Multiple(Vec<u8>),
}

impl ChunkBuilder {
    /// Adds a bytes::Bytes
    fn append_bytes(&mut self, b: bytes::Bytes) {
        match std::mem::take(self) {
            ChunkBuilder::Empty => *self = ChunkBuilder::Single(b),
            ChunkBuilder::Single(old_buf) => {
                *self = ChunkBuilder::Multiple({
                    let mut v: Vec<u8> = old_buf.into();
                    v.extend(b);
                    v
                })
            }
            ChunkBuilder::Multiple(mut v) => {
                *self = ChunkBuilder::Multiple({
                    v.extend(b);
                    v
                })
            }
        }
    }

    /// Returns a [Chunk] with all the data appended so far.
    fn build(self) -> Chunk {
        match self {
            ChunkBuilder::Empty => Chunk::from_static(&[]),
            ChunkBuilder::Single(b) => Chunk::from(b),
            ChunkBuilder::Multiple(v) => Chunk::from(v),
        }
    }
}
