use futures::{StreamExt, TryStreamExt, stream::BoxStream};
use prost::Message;
use redb::{ReadableDatabase, TableDefinition};
use std::{path::PathBuf, sync::Arc};
use tonic::async_trait;
use tracing::{instrument, warn};

use super::{Directory, DirectoryPutter, DirectoryService, traversal};
use crate::{
    B3Digest,
    composition::{CompositionContext, ServiceBuilder},
    directoryservice::{
        directory_graph::DirectoryGraphBuilder,
        order_validator::{self, LeavesToRoot},
    },
    proto,
};

const DIRECTORY_TABLE: TableDefinition<[u8; B3Digest::LENGTH], Vec<u8>> =
    TableDefinition::new("directory");

enum Db {
    ReadOnly(redb::ReadOnlyDatabase),
    ReadWrite(redb::Database),
}

impl Db {
    fn begin_read(&self) -> Result<redb::ReadTransaction, Error> {
        match self {
            Db::ReadOnly(db) => Ok(db.begin_read()?),
            Db::ReadWrite(db) => Ok(db.begin_read()?),
        }
    }

    fn begin_write(&self) -> Result<redb::WriteTransaction, Error> {
        match self {
            Db::ReadOnly(_) => Err(Error::OpenedReadonly),
            Db::ReadWrite(db) => Ok(db.begin_write()?),
        }
    }
}

#[derive(Clone)]
pub struct RedbDirectoryService {
    instance_name: String,

    /// An Arc'ed Database, read-only or writeable.
    db: Arc<Db>,
}

impl RedbDirectoryService {
    /// Constructs a new instance using the specified config.
    pub async fn new(
        instance_name: String,
        config: RedbDirectoryServiceConfig,
    ) -> Result<Self, Error> {
        if let Some(path) = config.path.as_ref() {
            if path == "" {
                return Err(Error::WrongConfig("empty path is disallowed"));
            }
            if path == "/" {
                return Err(Error::WrongConfig("cowardly refusing to open / with redb"));
            }

            if !config.read_only
                && let Some(parent) = path.parent()
            {
                tokio::fs::create_dir_all(parent).await?;
            }
        }

        let db = if config.path.is_some() {
            tokio::task::spawn_blocking(move || {
                let mut builder = redb::Database::builder();
                configure_builder(&mut builder, &config);

                let path = config.path.expect("Snix bug: path is Some");

                if config.read_only {
                    Ok::<_, Error>(Db::ReadOnly(builder.open_read_only(&path)?))
                } else {
                    let db = builder.create(&path)?;
                    create_schema(&db)?;
                    Ok(Db::ReadWrite(db))
                }
            })
            .await??
        } else {
            if config.read_only {
                return Err(Error::WrongConfig("in-memory database cannot be read-only"));
            }
            let mut builder = redb::Database::builder();
            configure_builder(&mut builder, &config);

            let db = builder
                .create_with_backend(redb::backends::InMemoryBackend::new())
                .expect("Snix bug: unable to create in-memory redb");

            create_schema(&db)?;
            Db::ReadWrite(db)
        };

        Ok(Self {
            instance_name,
            db: Arc::new(db),
        })
    }

    /// Constructs a new instance using the in-memory backend.
    /// Only used for testing purposes and mocks, use [Self::new] with a None
    /// path in config for other usecases.
    /// Sync, as there's no real IO happening.
    #[cfg(any(test, feature = "mocks"))]
    pub fn new_temporary(instance_name: String) -> Self {
        let mut builder = redb::Database::builder();
        configure_builder(
            &mut builder,
            &RedbDirectoryServiceConfig {
                path: None,
                cache_size: None,
                read_only: false,
            },
        );

        let db = builder
            .create_with_backend(redb::backends::InMemoryBackend::new())
            .expect("Snix bug: unable to create in-memory redb");

        create_schema(&db).expect("Snix bug: unable to create schema for in-memory redb");

        Self {
            instance_name,
            db: Arc::new(Db::ReadWrite(db)),
        }
    }
}

/// Applies options from [RedbDirectoryServiceConfig] to a [redb::Builder].
fn configure_builder(builder: &mut redb::Builder, config: &RedbDirectoryServiceConfig) {
    if let Some(cache_size) = config.cache_size {
        builder.set_cache_size(cache_size);
    }
}

/// Ensures all tables are present.
/// Opens a write transaction and calls open_table on DIRECTORY_TABLE, which will
/// create it if not present.
#[allow(clippy::result_large_err)]
fn create_schema(db: &redb::Database) -> Result<(), Error> {
    let txn = db.begin_write()?;
    txn.open_table(DIRECTORY_TABLE)?;
    txn.commit()?;

    Ok(())
}

#[async_trait]
impl DirectoryService for RedbDirectoryService {
    #[instrument(skip(self, digest), fields(directory.digest = %digest, instance_name = %self.instance_name))]
    async fn get(&self, digest: &B3Digest) -> Result<Option<Directory>, super::Error> {
        let db = self.db.clone();
        let digest = *digest;
        // Retrieves the protobuf-encoded Directory for the corresponding digest.
        let directory_data = match tokio::task::spawn_blocking(move || -> Result<_, Error> {
            let txn = db.begin_read()?;
            let table = txn.open_table(DIRECTORY_TABLE)?;
            Ok(table.get(*digest)?)
        })
        .await
        .map_err(Error::TokioJoin)??
        {
            // The Directory was not found, return None.
            None => return Ok(None),
            Some(directory_data) => directory_data.value(),
        };

        // We check that the digest of the retrieved Directory matches the expected digest.
        let actual = B3Digest::from(blake3::hash(&directory_data));
        if actual != digest {
            return Err(Error::WrongDigest {
                expected: digest,
                actual,
            }
            .into());
        }

        // Attempt to decode the retrieved protobuf-encoded Directory
        let proto_directory =
            proto::Directory::decode(directory_data.as_slice()).map_err(Error::ProtobufDecode)?;
        let directory = Directory::try_from(proto_directory).map_err(Error::DirectoryValidation)?;

        Ok(Some(directory))
    }

    #[instrument(skip(self, directory), fields(directory.digest = %directory.digest(), instance_name = %self.instance_name))]
    async fn put(&self, directory: Directory) -> Result<B3Digest, super::Error> {
        let db = self.db.clone();
        let digest = tokio::task::spawn_blocking(move || -> Result<_, Error> {
            let digest = directory.digest();

            // Store the directory in the table.
            let txn = db.begin_write()?;
            {
                let mut table = txn.open_table(DIRECTORY_TABLE)?;
                table.insert(
                    digest.as_ref(),
                    proto::Directory::from(directory).encode_to_vec(),
                )?;
            }
            txn.commit()?;

            Ok(digest)
        })
        .await
        .map_err(Error::TokioJoin)??;

        Ok(digest)
    }

    #[instrument(skip_all, fields(directory.digest = %root_directory_digest, instance_name = %self.instance_name))]
    fn get_recursive(
        &self,
        root_directory_digest: &B3Digest,
    ) -> BoxStream<'static, Result<Directory, super::Error>> {
        // FUTUREWORK: Ideally we should have all of the directory traversing happen in a single
        // redb transaction to avoid constantly closing and opening new transactions for the
        // database.
        let svc = self.clone();
        traversal::root_to_leaves(*root_directory_digest, move |digest| {
            let svc = svc.clone();
            async move { svc.get(&digest).await }
        })
        .map_err(Error::DirectoryTraversal)
        .err_into()
        .boxed()
    }

    #[instrument(skip_all)]
    fn put_multiple_start(&self) -> Box<dyn DirectoryPutter> {
        Box::new(RedbDirectoryPutter {
            db: self.db.clone(),
            builder: Some(DirectoryGraphBuilder::<LeavesToRoot>::new()),
        })
    }
}

pub struct RedbDirectoryPutter {
    db: Arc<Db>,

    /// The directories (inside the directory validator) that we insert later,
    /// or None, if they were already inserted.
    builder: Option<DirectoryGraphBuilder<LeavesToRoot>>,
}

#[async_trait]
impl DirectoryPutter for RedbDirectoryPutter {
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

        // Insert all directories as a batch.
        let db = self.db.clone();
        let root_digest = tokio::task::spawn_blocking(move || {
            // Retrieve the validated directories.
            let directory_graph = builder.build().map_err(Error::DirectoryOrdering)?;
            let root_digest = directory_graph.root().digest();

            let txn = db.begin_write()?;
            // Looping over all the verified directories, queuing them up for a
            // batch insertion.
            {
                let mut table = txn.open_table(DIRECTORY_TABLE)?;
                for directory in directory_graph.drain_leaves_to_root() {
                    table.insert(
                        directory.digest().as_ref(),
                        proto::Directory::from(directory).encode_to_vec(),
                    )?;
                }
            }
            txn.commit()?;

            Ok::<_, Error>(root_digest)
        })
        .await
        .map_err(Error::TokioJoin)??;

        Ok(root_digest)
    }
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("wrong arguments: {0}")]
    WrongConfig(&'static str),
    #[error("serde-qs error: {0}")]
    SerdeQS(#[from] serde_qs::Error),

    #[error("Directory Graph ordering error")]
    DirectoryOrdering(#[from] order_validator::OrderingError),

    #[error("DirectoryPutter already closed")]
    DirectoryPutterAlreadyClosed,

    #[error("failure during directory traversal")]
    DirectoryTraversal(#[source] traversal::Error),

    #[error("requested directory has wrong digest, expected {expected}, actual {actual}")]
    WrongDigest {
        expected: B3Digest,
        actual: B3Digest,
    },
    #[error("failed to decode protobuf: {0}")]
    ProtobufDecode(#[from] prost::DecodeError),
    #[error("failed to validate directory: {0}")]
    DirectoryValidation(#[from] crate::DirectoryError),

    #[error("unable to open write txn, database opened read-only")]
    OpenedReadonly,
    #[error("redb commit error: {0}")]
    RedbCommit(#[from] redb::CommitError),
    #[error("redb database error: {0}")]
    RedbDatabase(#[from] redb::DatabaseError),
    #[error("redb error: {0}")]
    Redb(#[from] redb::Error),
    #[error("redb storage error: {0}")]
    RedbStorage(#[from] redb::StorageError),
    #[error("redb table error: {0}")]
    RedbTable(#[from] redb::TableError),
    #[error("redb txn error: {0}")]
    RedbTransaction(#[from] redb::TransactionError),

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

#[derive(Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedbDirectoryServiceConfig {
    path: Option<PathBuf>,

    /// The amount of memory (in bytes) used for caching data
    cache_size: Option<usize>,

    /// Whether to open read-only.
    #[serde(default)]
    read_only: bool,
}

impl TryFrom<url::Url> for RedbDirectoryServiceConfig {
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn try_from(url: url::Url) -> Result<Self, Self::Error> {
        if url.has_host() {
            return Err(Error::WrongConfig("no host allowed").into());
        }

        let path: Option<PathBuf> = match (url.scheme(), url.has_authority(), url.path()) {
            ("redb+memory", false, "") => None,
            ("redb+memory", false, _) => Err(Box::new(Error::WrongConfig(
                "redb+memory with path is disallowed",
            )))?,
            ("redb+memory", true, _) => Err(Box::new(Error::WrongConfig(
                "redb+memory may not have authority",
            )))?,
            ("redb", _, "") => Err(Box::new(Error::WrongConfig(
                "redb without path is disallowed, use redb+memory if you want in-memory",
            )))?,
            ("redb", true, _path) => Err(Box::new(Error::WrongConfig("authority disallowed")))?,
            ("redb", false, path) => Some(path.into()),
            (_scheme, _, _) => Err(Box::new(Error::WrongConfig("unrecognized scheme")))?,
        };

        let mut config: RedbDirectoryServiceConfig =
            serde_qs::from_str(url.query().unwrap_or_default())?;

        config.path = path;

        Ok(config)
    }
}

#[async_trait]
impl ServiceBuilder for RedbDirectoryServiceConfig {
    type Output = dyn DirectoryService;
    async fn build<'a>(
        &'a self,
        instance_name: &str,
        _context: &CompositionContext,
    ) -> Result<Arc<Self::Output>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Arc::new(
            RedbDirectoryService::new(instance_name.to_string(), self.to_owned()).await?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use crate::{
        directoryservice::{DirectoryService, RedbDirectoryService, RedbDirectoryServiceConfig},
        fixtures::DIRECTORY_A,
    };

    #[tokio::test]
    async fn reopen_as_read_only() {
        let tempdir = TempDir::new().unwrap();
        let path = tempdir.path().join("data.redb");

        let config = RedbDirectoryServiceConfig {
            path: Some(path),
            cache_size: None,
            read_only: false,
        };

        // Create a read-write directory service and insert some data.
        {
            let directory_service = RedbDirectoryService::new("rw".to_string(), config.clone())
                .await
                .expect("to construct");

            directory_service
                .put(DIRECTORY_A.clone())
                .await
                .expect("to insert");
        } // we drop the rw database here.

        // Re-open the same path in ro mode (twice)
        let ro_config = RedbDirectoryServiceConfig {
            read_only: true,
            ..config
        };

        let directory_service_ro_1 =
            RedbDirectoryService::new("ro1".to_string(), ro_config.clone())
                .await
                .expect("to construct");
        let directory_service_ro_2 = RedbDirectoryService::new("ro2".to_string(), ro_config)
            .await
            .expect("to construct");

        assert_eq!(
            directory_service_ro_1
                .get(&DIRECTORY_A.digest())
                .await
                .expect("get to succeed")
                .expect("to be Some(_)")
                .digest(),
            DIRECTORY_A.digest()
        );
        assert_eq!(
            directory_service_ro_2
                .get(&DIRECTORY_A.digest())
                .await
                .expect("get to succeed")
                .expect("to be Some(_)")
                .digest(),
            DIRECTORY_A.digest()
        );
    }

    #[tokio::test]
    async fn read_only_nonexistent() {
        let tempdir = TempDir::new().unwrap();
        let path = tempdir.path().join("data.redb");

        let config = RedbDirectoryServiceConfig {
            path: Some(path),
            cache_size: None,
            read_only: true,
        };

        // Opening a read-only redb should fail if the path doesn't exist.
        assert!(
            RedbDirectoryService::new("test".to_string(), config)
                .await
                .is_err(),
            "opening new path r/o should fail"
        );
    }

    #[tokio::test]
    async fn open_rw_and_ro() {
        let tempdir = TempDir::new().unwrap();
        let path = tempdir.path().join("data.redb");

        let config = RedbDirectoryServiceConfig {
            path: Some(path),
            cache_size: None,
            read_only: false,
        };

        let _directory_service = RedbDirectoryService::new("rw".to_string(), config.clone())
            .await
            .expect("to construct");

        // Opening a read-only redb should fail if it's already opened read-write.
        assert!(
            RedbDirectoryService::new(
                "ro".to_string(),
                RedbDirectoryServiceConfig {
                    read_only: true,
                    ..config
                }
            )
            .await
            .is_err(),
            "opening r/o should fail if still open r/w"
        );
    }
}
