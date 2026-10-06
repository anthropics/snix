use std::collections::HashMap;
use std::collections::hash_map;
use std::sync::Arc;

use data_encoding::HEXLOWER;
use futures::SinkExt;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream::BoxStream;
use object_store::ObjectStoreExt;
use object_store::{ObjectStore, path::Path};
use prost::Message;
use tokio::io::AsyncWriteExt;
use tokio_util::codec::LengthDelimitedCodec;
use tonic::async_trait;
use tracing::{Level, instrument, trace, warn};
use url::Url;

use super::{Directory, DirectoryPutter, DirectoryService};
use crate::composition::{CompositionContext, ServiceBuilder};
use crate::directoryservice::directory_graph::DirectoryGraphBuilder;
use crate::directoryservice::order_validator::{self, LeavesToRoot, OrderValidator, RootToLeaves};
use crate::{B3Digest, Node, proto};

/// Stores directory closures in an object store.
/// Notably, this makes use of the option to disallow accessing child directories except when
/// fetching them recursively via the top-level directory, since all batched writes
/// (using `put_multiple_start`) are stored in a single object.
/// Directories are stored in a length-delimited format with a 1MiB limit. The length field is a
/// u32 and the directories are stored in root-to-leaves topological order, the same way they will
/// be returned to the client in get_recursive.
#[derive(Clone)]
pub struct ObjectStoreDirectoryService {
    instance_name: String,
    object_store: Arc<dyn ObjectStore>,
    base_path: Path,
}

#[instrument(level=Level::TRACE, skip_all,fields(base_path=%base_path,blob.digest=%digest),ret(Display))]
fn derive_dirs_path(base_path: &Path, digest: &B3Digest) -> Path {
    base_path
        .clone()
        .join("dirs")
        .join("b3")
        .join(HEXLOWER.encode(&digest.as_slice()[..2]))
        .join(HEXLOWER.encode(digest.as_slice()))
}

/// Helper function, parsing protobuf-encoded Directories into [crate::Directory].
fn parse_proto_directory(encoded_directory: &[u8]) -> Result<crate::Directory, Error> {
    let directory_proto = proto::Directory::decode(encoded_directory)?;

    Ok(Directory::try_from(directory_proto)?)
}

#[allow(clippy::identity_op)]
const MAX_FRAME_LENGTH: usize = 1 * 1024 * 1024 * 1000; // 1 MiB
//
impl ObjectStoreDirectoryService {
    /// Constructs a new [ObjectStoreDirectoryService] from a [Url] supported by
    /// [object_store].
    /// Any path suffix becomes the base path of the object store.
    /// additional options, the same as in [object_store::parse_url_opts] can
    /// be passed.
    pub fn parse_url_opts<I, K, V>(url: &Url, options: I) -> Result<Self, object_store::Error>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: Into<String>,
    {
        let (object_store, path) = object_store::parse_url_opts(url, options)?;

        Ok(Self {
            instance_name: "root".into(),
            object_store: Arc::new(object_store),
            base_path: path,
        })
    }

    /// Like [Self::parse_url_opts], except without the options.
    pub fn parse_url(url: &Url) -> Result<Self, object_store::Error> {
        Self::parse_url_opts(url, Vec::<(String, String)>::new())
    }

    pub fn new(instance_name: String, object_store: Arc<dyn ObjectStore>, base_path: Path) -> Self {
        Self {
            instance_name,
            object_store,
            base_path,
        }
    }
}

#[async_trait]
impl DirectoryService for ObjectStoreDirectoryService {
    /// This is the same steps as for get_recursive anyways, so we just call get_recursive and
    /// return the first element of the stream and drop the request.
    #[instrument(level = "trace", skip_all, fields(directory.digest = %digest, instance_name = %self.instance_name))]
    async fn get(&self, digest: &B3Digest) -> Result<Option<Directory>, super::Error> {
        self.get_recursive(digest).take(1).next().await.transpose()
    }

    #[instrument(level = "trace", skip_all, fields(directory.digest = %directory.digest(), instance_name = %self.instance_name))]
    async fn put(&self, directory: Directory) -> Result<B3Digest, super::Error> {
        // Ensure the directory doesn't contain other directory children
        if directory
            .nodes()
            .any(|(_, e)| matches!(e, Node::Directory { .. }))
        {
            Err(Error::PutForDirectoryWithChildren)?
        }

        let mut handle = self.put_multiple_start();
        handle.put(directory).await?;
        handle.close().await
    }

    #[instrument(level = "trace", skip_all, fields(directory.digest = %root_directory_digest, instance_name = %self.instance_name))]
    fn get_recursive(
        &self,
        root_directory_digest: &B3Digest,
    ) -> BoxStream<'_, Result<Directory, super::Error>> {
        // Check that we are not passing on bogus from the object store to the client, and that the
        // trust chain from the root digest to the leaves is intact.
        let dir_path = derive_dirs_path(&self.base_path, root_directory_digest);
        let object_store = &self.object_store;
        let root_directory_digest = *root_directory_digest;

        async_stream::try_stream! {
                let bytes_stream = match object_store.get(&dir_path).await {
                    Ok(v) => v.into_stream(),
                    Err(object_store::Error::NotFound { .. }) => {
                        return;
                    }
                    Err(e) => Err(Error::ObjectStore(e))?,
                };

                // get a reader of the response body.
                let r = tokio_util::io::StreamReader::new(bytes_stream);
                let decompressed_stream = async_compression::tokio::bufread::ZstdDecoder::new(r);

                // the subdirectories are stored in a length delimited format
                let mut encoded_directories = LengthDelimitedCodec::builder()
                    .max_frame_length(MAX_FRAME_LENGTH)
                    .length_field_type::<u32>()
                    .new_read(decompressed_stream)
                    .err_into::<Error>();

                let mut order_validator = RootToLeaves::new_with_root_digest(root_directory_digest);
                while let Some(encoded_directory) = encoded_directories.try_next().await? {
                    // hash the encoded proto message, only proceed if we would accept a directory with this digest.
                    let digest = B3Digest::from(blake3::hash(&encoded_directory));
                    if !order_validator.would_accept(&digest) {
                        Err(Error::UnexpectedDigest(digest))?;
                    }

                    // only then proceed with parsing
                    let directory = parse_proto_directory(&encoded_directory)?;

                    // The directory can still be rejected for other reasons.
                    order_validator.try_accept(&directory).map_err(Error::DirectoryOrdering)?;

                    yield directory;
                }

                order_validator.finalize().map_err(Error::DirectoryOrdering)?;
        }
        .boxed()
    }

    #[instrument(skip_all)]
    fn put_multiple_start(&self) -> Box<dyn DirectoryPutter + '_>
    where
        Self: Clone,
    {
        Box::new(ObjectStoreDirectoryPutter::new(
            self.object_store.clone(),
            &self.base_path,
        ))
    }
}

#[derive(thiserror::Error, Debug)]
enum Error {
    #[error("wrong arguments: {0}")]
    WrongConfig(&'static str),
    #[error("put() may only be used for directories without children")]
    PutForDirectoryWithChildren,

    #[error("Directory Graph ordering error")]
    DirectoryOrdering(#[from] order_validator::OrderingError),
    #[error("next directory in batch has unexpected digest {0}")]
    UnexpectedDigest(B3Digest),
    #[error("failed to decode protobuf: {0}")]
    ProtobufDecode(#[from] prost::DecodeError),
    #[error("failed to validate directory: {0}")]
    DirectoryValidation(#[from] crate::DirectoryError),

    #[error("DirectoryPutter already closed")]
    DirectoryPutterAlreadyClosed,

    #[error("ObjectStore error: {0}")]
    ObjectStore(#[from] object_store::Error),

    #[error("io error: {0}")]
    IO(#[from] std::io::Error),
}
impl From<Error> for super::Error {
    fn from(value: Error) -> Self {
        Self(Box::new(value))
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectStoreDirectoryServiceConfig {
    object_store_url: String,
    #[serde(default)]
    object_store_options: HashMap<String, String>,
}

impl TryFrom<url::Url> for ObjectStoreDirectoryServiceConfig {
    type Error = Box<dyn std::error::Error + Send + Sync>;
    fn try_from(url: url::Url) -> Result<Self, Self::Error> {
        // We need to convert the URL to string, strip the prefix there, and then
        // parse it back as url, as Url::set_scheme() rejects some of the transitions we want to do.
        let trimmed_url = {
            let s = url.to_string();
            let mut url = Url::parse(s.strip_prefix("objectstore+").ok_or(Error::WrongConfig(
                "Missing objectstore+ part in URI scheme",
            ))?)?;
            // trim the query pairs, they might contain credentials or local settings we don't want to send as-is.
            url.set_query(None);
            url
        };
        Ok(ObjectStoreDirectoryServiceConfig {
            object_store_url: trimmed_url.into(),
            object_store_options: url
                .query_pairs()
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        })
    }
}

#[async_trait]
impl ServiceBuilder for ObjectStoreDirectoryServiceConfig {
    type Output = dyn DirectoryService;
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

        let (object_store, path) =
            crate::object_store::setup_object_store(&self.object_store_url.parse()?, opts).await?;
        Ok(Arc::new(ObjectStoreDirectoryService::new(
            instance_name.to_string(),
            Arc::new(object_store),
            path,
        )))
    }
}

struct ObjectStoreDirectoryPutter<'a> {
    object_store: Arc<dyn ObjectStore>,
    base_path: &'a Path,

    builder: Option<DirectoryGraphBuilder<LeavesToRoot>>,
}

impl<'a> ObjectStoreDirectoryPutter<'a> {
    fn new(object_store: Arc<dyn ObjectStore>, base_path: &'a Path) -> Self {
        Self {
            object_store,
            base_path,
            builder: Some(DirectoryGraphBuilder::<LeavesToRoot>::new()),
        }
    }
}

#[async_trait]
impl DirectoryPutter for ObjectStoreDirectoryPutter<'_> {
    #[instrument(level = "trace", skip_all, fields(directory.digest=%directory.digest()), err)]
    async fn put(&mut self, directory: Directory) -> Result<(), super::Error> {
        let builder = self
            .builder
            .as_mut()
            .ok_or_else(|| Error::DirectoryPutterAlreadyClosed)?;

        builder
            .try_insert(directory)
            .map_err(Error::DirectoryOrdering)?;

        Ok(())
    }

    #[instrument(level = "trace", skip_all, ret, err)]
    async fn close(&mut self) -> Result<B3Digest, super::Error> {
        let builder = self
            .builder
            .take()
            .ok_or_else(|| Error::DirectoryPutterAlreadyClosed)?;

        // Retrieve the validated directories.
        let directory_graph = builder.build().map_err(Error::DirectoryOrdering)?;
        let root_digest = directory_graph.root().digest();

        let dir_path = derive_dirs_path(self.base_path, &root_digest);

        match self.object_store.head(&dir_path).await {
            // directory tree already exists, nothing to do
            Ok(_) => {
                trace!("directory tree already exists");
            }

            // directory tree does not yet exist, compress and upload.
            Err(object_store::Error::NotFound { .. }) => {
                trace!("uploading directory tree");

                let object_store_writer =
                    object_store::buffered::BufWriter::new(self.object_store.clone(), dir_path);
                let compressed_writer =
                    async_compression::tokio::write::ZstdEncoder::new(object_store_writer);
                let mut directories_sink = LengthDelimitedCodec::builder()
                    .max_frame_length(MAX_FRAME_LENGTH)
                    .length_field_type::<u32>()
                    .new_write(compressed_writer);

                // Drain the graph in *Root-To-Leaves*, order, as that's how we write it to storage.
                for directory in directory_graph.drain_root_to_leaves() {
                    directories_sink
                        .send(proto::Directory::from(directory).encode_to_vec().into())
                        .await
                        .map_err(Error::IO)?;
                }

                let mut compressed_writer = directories_sink.into_inner();
                compressed_writer.shutdown().await.map_err(Error::IO)?;
            }
            // other error
            Err(err) => Err(Error::ObjectStore(err))?,
        }

        Ok(root_digest)
    }
}
