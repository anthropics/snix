use std::{
    collections::{HashMap, hash_map},
    io::{self, Cursor},
    pin::pin,
    sync::Arc,
    task::Poll,
};

use data_encoding::HEXLOWER;
use fastcdc::v2020::AsyncStreamCDC;
use futures::{Future, TryStreamExt};
use object_store::{ObjectStore, ObjectStoreExt, ObjectStoreScheme, path::Path};
use pin_project_lite::pin_project;
use prost::Message;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio_util::io::InspectReader;
use tonic::async_trait;
use tracing::{Level, debug, instrument, trace};
use url::Url;

use crate::{
    B3Digest,
    blobstore::BlobMeta,
    composition::{CompositionContext, ServiceBuilder},
    proto::{self, stat_blob_response::ChunkMeta},
};

use super::{BlobReader, BlobService, BlobWriter, ChunkedReader};

#[cfg(feature = "cloud")]
mod aws;

/// The number of chunks that will be uploaded in parallel, per blob.
const CONCURRENT_CHUNK_UPLOADS: usize = 64;

/// Uses any object storage supported by the [object_store] crate to provide a
/// snix-castore [BlobService].
///
/// # Data format
/// Data is organized in "blobs" and "chunks".
/// Blobs don't hold the actual data, but instead contain a list of more
/// granular chunks that assemble to the contents requested.
/// This allows clients to seek, and not download chunks they already have
/// locally, as it's referred to from other files.
/// Check `rpc_blobstore` and more general BlobStore docs on that.
///
/// ## Blobs
/// Stored at `${base_path}/blobs/b3/$digest_key`. They contains the serialized
/// StatBlobResponse for the blob with the digest.
///
/// ## Chunks
/// Chunks are stored at `${base_path}/chunks/b3/$digest_key`. They contain
/// the literal contents of the chunk, but are zstd-compressed.
///
/// ## Digest key sharding
/// The blake3 digest encoded in lower hex, and sharded after the second
/// character.
/// The blob for "Hello World" is stored at
/// `${base_path}/blobs/b3/41/41f8394111eb713a22165c46c90ab8f0fd9399c92028fd6d288944b23ff5bf76`.
///
/// This reduces the number of files in the same directory, which would be a
/// problem at least when using [object_store::local::LocalFileSystem].
///
/// # Future changes
/// There's no guarantees about this being a final format yet.
/// Once object_store gets support for additional metadata / content-types,
/// we can eliminate some requests (small blobs only consisting of a single
/// chunk can be stored as-is, without the blob index file).
/// It also allows signalling any compression of chunks in the content-type.
/// Migration *should* be possible by simply adding the right content-types to
/// all keys stored so far, but no promises ;-)
#[derive(Clone)]
pub struct ObjectStoreBlobService {
    instance_name: String,
    object_store: Arc<dyn ObjectStore>,
    base_path: Path,

    /// Average chunk size for FastCDC, in bytes.
    /// min value is half, max value double of that number.
    avg_chunk_size: u32,
}

#[instrument(level=Level::TRACE, skip_all,fields(base_path=%base_path,blob.digest=%digest),ret(Display))]
fn derive_blob_path(base_path: &Path, digest: &B3Digest) -> Path {
    base_path
        .clone()
        .join("blobs")
        .join("b3")
        .join(HEXLOWER.encode(&digest[..2]))
        .join(HEXLOWER.encode(&digest[..]))
}

#[instrument(level=Level::TRACE, skip_all,fields(base_path=%base_path,chunk.digest=%digest),ret(Display))]
fn derive_chunk_path(base_path: &Path, digest: &B3Digest) -> Path {
    base_path
        .clone()
        .join("chunks")
        .join("b3")
        .join(HEXLOWER.encode(&digest[..2]))
        .join(HEXLOWER.encode(&digest[..]))
}

#[async_trait]
impl BlobService for ObjectStoreBlobService {
    #[instrument(skip_all, ret(level = Level::TRACE), err, fields(blob.digest=%digest, instance_name=%self.instance_name))]
    async fn has(&self, digest: &B3Digest) -> io::Result<bool> {
        // TODO: clarify if this should work for chunks or not, and explicitly
        // document in the proto docs.
        let p = derive_blob_path(&self.base_path, digest);

        match self.object_store.head(&p).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => {
                let p = derive_chunk_path(&self.base_path, digest);
                match self.object_store.head(&p).await {
                    Ok(_) => Ok(true),
                    Err(object_store::Error::NotFound { .. }) => Ok(false),
                    Err(e) => Err(e)?,
                }
            }
            Err(e) => Err(e)?,
        }
    }

    #[instrument(skip_all, err, fields(blob.digest=%digest, instance_name=%self.instance_name))]
    async fn open_read(&self, digest: &B3Digest) -> io::Result<Option<Box<dyn BlobReader>>> {
        // handle reading the empty blob.
        if digest.as_slice() == blake3::hash(b"").as_bytes() {
            return Ok(Some(Box::new(Cursor::new(b"")) as Box<dyn BlobReader>));
        }
        match self
            .object_store
            .get(&derive_chunk_path(&self.base_path, digest))
            .await
        {
            Ok(res) => {
                // handle reading blobs that are small enough to fit inside a single chunk:
                // fetch the entire chunk into memory, decompress, ensure the b3 digest matches,
                // and return a io::Cursor over that data.
                // FUTUREWORK: use zstd::bulk to prevent decompression bombs

                let chunk_raw_bytes = res.bytes().await?;
                let chunk_contents = zstd::stream::decode_all(Cursor::new(chunk_raw_bytes))?;

                if *digest != blake3::hash(&chunk_contents).as_bytes().into() {
                    Err(io::Error::other("chunk contents invalid"))?;
                }

                Ok(Some(Box::new(Cursor::new(chunk_contents))))
            }
            Err(object_store::Error::NotFound { .. }) => {
                // NOTE: For public-facing things, we would want to stop here.
                // Clients should fetch granularly, so they can make use of
                // chunks they have locally.
                // However, if this is used directly, without any caches, do the
                // assembly here.
                // This is subject to change, once we have store composition.
                // TODO: make this configurable, and/or clarify behaviour for
                // the gRPC server surface (explicitly document behaviour in the
                // proto docs)
                if let Some(chunks) = self.chunks(digest).await? {
                    let chunked_reader = ChunkedReader::from_chunks(
                        chunks.into_iter().map(|chunk| {
                            (
                                chunk.digest.try_into().expect("invalid b3 digest"),
                                chunk.size,
                            )
                        }),
                        Arc::new(self.clone()) as Arc<dyn BlobService>,
                    );

                    Ok(Some(Box::new(chunked_reader)))
                } else {
                    // This is neither a chunk nor a blob, return None.
                    Ok(None)
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    #[instrument(skip_all, fields(instance_name=%self.instance_name))]
    async fn open_write(&self) -> Box<dyn BlobWriter> {
        // ObjectStoreBlobWriter implements AsyncWrite, but all the chunking
        // needs an AsyncRead, so we create a pipe here.
        // In its `AsyncWrite` implementation, `ObjectStoreBlobWriter` delegates
        // writes to w. It periodically polls the future that's reading from the
        // other side.
        let (w, r) = tokio::io::duplex(self.avg_chunk_size as usize * 10);

        Box::new(ObjectStoreBlobWriter {
            writer: Some(w),
            fut: Some(Box::pin(chunk_and_upload(
                r,
                self.object_store.clone(),
                self.base_path.clone(),
                self.avg_chunk_size / 2,
                self.avg_chunk_size,
                self.avg_chunk_size * 2,
            ))),
            fut_output: None,
        })
    }

    #[instrument(skip_all, err, fields(blob.digest=%digest, instance_name=%self.instance_name))]
    async fn chunks(&self, digest: &B3Digest) -> io::Result<Option<Vec<ChunkMeta>>> {
        match self
            .object_store
            .get(&derive_blob_path(&self.base_path, digest))
            .await
        {
            Ok(get_result) => {
                // Fetch the data at the blob path
                let blob_data = get_result.bytes().await?;

                // If this is an empty file, it signals this blob only consists of a single chunk
                // (but we don't know its size).
                // In this case, the trait semantics are to respond back with an empty Vec.
                // We can check for the proto size instead of checking for the number of chunks,
                // as we never populated any other fields in [StatBlobResponse].
                if blob_data.is_empty() {
                    Ok(Some(vec![]))
                } else {
                    // Else, parse as BlobMeta
                    let proto_blob_meta = proto::BlobMeta::decode(blob_data)?;
                    let blob_meta = BlobMeta::try_from(proto_blob_meta)
                        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

                    let blob_len = blob_meta.blob_len();

                    // HACK: Convert back to the BlobMeta proto, so we can access the raw fields.
                    // The new BlobStore will just return BlobMeta.
                    let proto_blob_meta = proto::BlobMeta::from(blob_meta);

                    debug!(blob.size = blob_len, "found more granular chunks");

                    Ok(Some(
                        proto_blob_meta
                            .chunks
                            .into_iter()
                            .map(|proto::blob_meta::ChunkMeta { digest, size }| {
                                proto::stat_blob_response::ChunkMeta { digest, size }
                            })
                            .collect(),
                    ))
                }
            }
            Err(object_store::Error::NotFound { .. }) => {
                // If there's only a chunk, we must return the empty vec here, rather than None.
                match self
                    .object_store
                    .head(&derive_chunk_path(&self.base_path, digest))
                    .await
                {
                    Ok(_) => {
                        // present, but no more chunks available
                        debug!("found a single chunk");
                        Ok(Some(vec![]))
                    }
                    Err(object_store::Error::NotFound { .. }) => {
                        // Neither blob nor single chunk found
                        debug!("not found");
                        Ok(None)
                    }
                    // error checking for chunk
                    Err(e) => Err(e.into()),
                }
            }
            // error checking for blob
            Err(err) => Err(err.into()),
        }
    }
}

fn default_avg_chunk_size() -> u32 {
    256 * 1024
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectStoreBlobServiceConfig {
    object_store_url: String,
    #[serde(default = "default_avg_chunk_size")]
    avg_chunk_size: u32,
    object_store_options: HashMap<String, String>,
}

impl TryFrom<url::Url> for ObjectStoreBlobServiceConfig {
    type Error = Box<dyn std::error::Error + Send + Sync>;
    /// Constructs a new [ObjectStoreBlobService] from a [Url] supported by
    /// [object_store].
    /// Any path suffix becomes the base path of the object store.
    /// additional options, the same as in [object_store::parse_url_opts] can
    /// be passed.
    fn try_from(url: url::Url) -> Result<Self, Self::Error> {
        // We need to convert the URL to string, strip the prefix there, and then
        // parse it back as url, as Url::set_scheme() rejects some of the transitions we want to do.
        let trimmed_url = {
            let s = url.to_string();
            let mut url = Url::parse(
                s.strip_prefix("objectstore+")
                    .ok_or("Missing objectstore uri")?,
            )?;
            // trim the query pairs, they might contain credentials or local settings we don't want to send as-is.
            url.set_query(None);
            url
        };
        Ok(ObjectStoreBlobServiceConfig {
            object_store_url: trimmed_url.into(),
            object_store_options: url
                .query_pairs()
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            avg_chunk_size: 256 * 1024,
        })
    }
}

#[async_trait]
impl ServiceBuilder for ObjectStoreBlobServiceConfig {
    type Output = dyn BlobService;
    async fn build<'a>(
        &'a self,
        instance_name: &str,
        _context: &CompositionContext,
    ) -> Result<Arc<Self::Output>, Box<dyn std::error::Error + Send + Sync>> {
        let opts = {
            let mut opts: HashMap<&str, _> = self
                .object_store_options
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();

            if let hash_map::Entry::Vacant(e) =
                opts.entry(object_store::ClientConfigKey::UserAgent.as_ref())
            {
                e.insert(crate::USER_AGENT);
            }

            opts
        };

        // object_store doesn't sufficiently support the AWS credential chain.
        let object_store_url: url::Url = self.object_store_url.parse()?;
        let (object_store_scheme, path) =
            object_store::ObjectStoreScheme::parse(&object_store_url)?;

        let (object_store, path) = match object_store_scheme {
            #[cfg(feature = "cloud")]
            ObjectStoreScheme::AmazonS3 => {
                // In the AWS case, we only support s3:// URLs.
                if object_store_url.scheme() != "s3" {
                    return Err(Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "only s3://-style URLs supported",
                    )));
                }

                let store = aws::setup_aws_object_store(&object_store_url, opts).await?;
                (Box::new(store) as Box<dyn ObjectStore>, path)
            }
            _ => object_store::parse_url_opts(&object_store_url, opts)?,
        };

        Ok(Arc::new(ObjectStoreBlobService {
            instance_name: instance_name.to_string(),
            object_store: Arc::new(object_store),
            base_path: path,
            avg_chunk_size: self.avg_chunk_size,
        }))
    }
}

/// Reads blob contents from a AsyncRead, chunks and uploads them,
/// then writes chunking info and uploads it as well.
/// On success, returns the [B3Digest] of the blob written.
#[instrument(skip_all, fields(base_path=%base_path, min_chunk_size, avg_chunk_size, max_chunk_size), err)]
async fn chunk_and_upload<R: AsyncRead + Unpin>(
    r: R,
    object_store: Arc<dyn ObjectStore>,
    base_path: Path,
    min_chunk_size: u32,
    avg_chunk_size: u32,
    max_chunk_size: u32,
) -> io::Result<B3Digest> {
    // wrap reader with something calculating the blake3 hash of all data read.
    let mut hasher = blake3::Hasher::new();
    let mut b3_r = InspectReader::new(r, |data| {
        hasher.update(data);
    });
    // set up a fastcdc chunker
    let mut chunker =
        AsyncStreamCDC::new(&mut b3_r, min_chunk_size, avg_chunk_size, max_chunk_size);

    // Use the fastcdc chunker to produce a stream of chunks, and upload these
    // that don't exist to the backend.
    let digests_and_sizes = chunker
        .as_stream()
        .err_into()
        .map_ok(|chunk_data| {
            let object_store = object_store.clone();
            let chunk_digest: B3Digest = blake3::hash(&chunk_data.data).as_bytes().into();
            let chunk_path = derive_chunk_path(&base_path, &chunk_digest);
            upload_chunk(object_store, chunk_digest, chunk_path, chunk_data.data)
        })
        .try_buffered(CONCURRENT_CHUNK_UPLOADS)
        .try_collect::<Vec<_>>()
        .await?;

    // check for Blob, if it doesn't exist, persist.
    let blob_digest: B3Digest = hasher.finalize().into();
    let blob_path = derive_blob_path(&base_path, &blob_digest);

    match object_store.head(&blob_path).await {
        // blob already exists, nothing to do
        Ok(_) => {
            trace!(
                blob.digest = %blob_digest,
                blob.path = %blob_path,
                "blob already exists on backend"
            );
        }
        // blob does not yet exist, upload
        Err(object_store::Error::NotFound { .. }) => {
            debug!(
                blob.digest = %blob_digest,
                blob.path = %blob_path,
                "uploading blob"
            );

            let payload = if digests_and_sizes.len() < 2 {
                // The chunker returned only one chunk, which is the entire blob.
                // In this case, we historically wrote a StatBlobResponse with an empty list of chunks,
                // which results in an empty file.
                bytes::Bytes::from_static(&[])
            } else {
                // Write proto::BlobMeta in all other cases.
                let blob_meta =
                    proto::BlobMeta::from(BlobMeta::from_digests_and_sizes(digests_and_sizes));
                blob_meta.encode_to_vec().into()
            };

            object_store.put(&blob_path, payload.into()).await?;
        }
        Err(err) => {
            // other error
            Err(err)?
        }
    }

    Ok(blob_digest)
}

/// Upload chunk if it doesn't exist yet
#[instrument(skip_all, fields(chunk.digest = %chunk_digest, chunk.size = chunk_data.len(), chunk.path = %chunk_path), err)]
async fn upload_chunk(
    object_store: Arc<dyn ObjectStore>,
    chunk_digest: B3Digest,
    chunk_path: Path,
    chunk_data: Vec<u8>,
) -> std::io::Result<(B3Digest, u64)> {
    let chunk_size = chunk_data.len() as u64;
    match object_store.head(&chunk_path).await {
        // chunk already exists, nothing to do
        Ok(_) => {
            debug!("chunk already exists");
        }

        // chunk does not yet exist, compress and upload.
        Err(object_store::Error::NotFound { .. }) => {
            let chunk_data_compressed =
                zstd::encode_all(Cursor::new(chunk_data), zstd::DEFAULT_COMPRESSION_LEVEL)?;

            debug!(chunk.compressed_size=%chunk_data_compressed.len(), "uploading chunk");

            object_store
                .as_ref()
                .put(&chunk_path, chunk_data_compressed.into())
                .await?;
        }
        // other error
        Err(err) => Err(err)?,
    }

    Ok((chunk_digest, chunk_size))
}

pin_project! {
    /// Takes care of blob uploads.
    /// All writes are relayed to self.writer, and we continuously poll the
    /// future (which will internally read from the other side of the pipe and
    /// upload chunks).
    /// Our BlobWriter::close() needs to drop self.writer, so the other side
    /// will read EOF and can finalize the blob.
    /// The future should then resolve and return the blob digest.
    pub struct ObjectStoreBlobWriter<W, Fut>
    where
        W: AsyncWrite,
        Fut: Future,
    {
        #[pin]
        writer: Option<W>,

        #[pin]
        fut: Option<Fut>,

        fut_output: Option<io::Result<B3Digest>>
    }
}

impl<W, Fut> tokio::io::AsyncWrite for ObjectStoreBlobWriter<W, Fut>
where
    W: AsyncWrite + Send + Unpin,
    Fut: Future,
{
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, io::Error>> {
        let this = self.project();
        // poll the future.
        let fut = this.fut.as_pin_mut().expect("not future");
        let fut_p = fut.poll(cx);
        // if it's ready, the only way this could have happened is that the
        // upload failed, because we're only closing `self.writer` after all
        // writes happened.
        if fut_p.is_ready() {
            return Poll::Ready(Err(io::Error::other("upload failed")));
        }

        // write to the underlying writer
        this.writer
            .as_pin_mut()
            .expect("writer must be some")
            .poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        let this = self.project();
        // poll the future.
        let fut = this.fut.as_pin_mut().expect("not future");
        let fut_p = fut.poll(cx);
        // if it's ready, the only way this could have happened is that the
        // upload failed, because we're only closing `self.writer` after all
        // writes happened.
        if fut_p.is_ready() {
            return Poll::Ready(Err(io::Error::other("upload failed")));
        }

        // Call poll_flush on the writer
        this.writer
            .as_pin_mut()
            .expect("writer must be some")
            .poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        // There's nothing to do on shutdown. We might have written some chunks
        // that are nowhere else referenced, but cleaning them up here would be racy.
        std::task::Poll::Ready(Ok(()))
    }
}

#[async_trait]
impl<W, Fut> BlobWriter for ObjectStoreBlobWriter<W, Fut>
where
    W: AsyncWrite + Send + Unpin,
    Fut: Future<Output = io::Result<B3Digest>> + Send + Unpin,
{
    async fn close(&mut self) -> io::Result<B3Digest> {
        match self.writer.take() {
            Some(mut writer) => {
                // shut down the writer, so the other side will read EOF.
                writer.shutdown().await?;

                // take out the future.
                let fut = self.fut.take().expect("fut must be some");
                // await it.
                let resp = pin!(fut).await;

                match resp.as_ref() {
                    // In the case of an Ok value, we store it in self.fut_output,
                    // so future calls to close can return that.
                    Ok(b3_digest) => {
                        self.fut_output = Some(Ok(*b3_digest));
                    }
                    Err(e) => {
                        // for the error type, we need to cheat a bit, as
                        // they're not clone-able.
                        // Simply store a sloppy clone, with the same ErrorKind and message there.
                        self.fut_output = Some(Err(std::io::Error::new(e.kind(), e.to_string())))
                    }
                }
                resp
            }
            None => {
                // called a second time, return self.fut_output.
                match self.fut_output.as_ref().unwrap() {
                    Ok(b3_digest) => Ok(*b3_digest),
                    Err(e) => Err(std::io::Error::new(e.kind(), e.to_string())),
                }
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::{chunk_and_upload, default_avg_chunk_size};
    use crate::{
        blobservice::{BlobService, ObjectStoreBlobService},
        blobstore::BlobMeta,
        fixtures::{BLOB_A, BLOB_A_DIGEST, BLOB_B, BLOB_B_DIGEST},
        proto,
    };
    use prost::Message;
    use std::{io::Cursor, sync::Arc};
    use url::Url;

    /// Tests chunk_and_upload directly, bypassing the BlobWriter at open_write().
    #[rstest::rstest]
    #[case::a(&BLOB_A, &BLOB_A_DIGEST)]
    #[case::b(&BLOB_B, &BLOB_B_DIGEST)]
    #[tokio::test]
    async fn test_chunk_and_upload(
        #[case] blob: &bytes::Bytes,
        #[case] blob_digest: &crate::B3Digest,
    ) {
        let (object_store, base_path) =
            object_store::parse_url(&Url::parse("memory:///").unwrap()).unwrap();
        let object_store: Arc<dyn object_store::ObjectStore> = Arc::from(object_store);
        let blobsvc = Arc::new(ObjectStoreBlobService {
            instance_name: "test".into(),
            object_store: object_store.clone(),
            avg_chunk_size: default_avg_chunk_size(),
            base_path,
        });

        let inserted_blob_digest = chunk_and_upload(
            &mut Cursor::new(blob.to_vec()),
            object_store,
            object_store::path::Path::from("/"),
            1024 / 2,
            1024,
            1024 * 2,
        )
        .await
        .expect("chunk_and_upload succeeds");

        assert_eq!(blob_digest.clone(), inserted_blob_digest);

        // Now we should have the blob
        assert!(blobsvc.has(blob_digest).await.unwrap());

        // Check if it was chunked correctly
        let chunks = blobsvc.chunks(blob_digest).await.unwrap().unwrap();
        if blob.len() < 1024 / 2 {
            // The blob is smaller than the min chunk size, it should have been inserted as a whole
            assert!(chunks.is_empty());
        } else if blob.len() > 1024 * 2 {
            // The blob is larger than the max chunk size, make sure it was split up into at least
            // two chunks
            assert!(chunks.len() >= 2);
        }
    }

    #[test]
    fn stat_blob_response_empty() {
        let stat_blob_response = proto::StatBlobResponse {
            chunks: vec![],
            bao: "".into(),
        };

        assert_eq!(
            stat_blob_response.encoded_len(),
            0,
            "encoded len without chunks should be zero"
        )
    }

    #[test]
    // write a proto::StatBlobResponse, ensure we parse an equivalent BlobMeta out of it.
    fn stat_blob_response_eq() {
        let serialized = proto::StatBlobResponse {
            chunks: vec![
                proto::stat_blob_response::ChunkMeta {
                    digest: (*BLOB_A_DIGEST).into(),
                    size: BLOB_A.len() as u64,
                },
                proto::stat_blob_response::ChunkMeta {
                    digest: (*BLOB_B_DIGEST).into(),
                    size: BLOB_B.len() as u64,
                },
            ],
            bao: "".into(),
        }
        .encode_to_vec();

        let blob_meta_proto =
            proto::BlobMeta::decode(serialized.as_slice()).expect("proto to deserialize");
        let blob_meta = BlobMeta::try_from(blob_meta_proto).expect("parsing to succeed");

        assert_eq!(
            BlobMeta::from_digests_and_sizes([
                (*BLOB_A_DIGEST, BLOB_A.len() as u64),
                (*BLOB_B_DIGEST, BLOB_B.len() as u64)
            ]),
            blob_meta
        );
    }
}
