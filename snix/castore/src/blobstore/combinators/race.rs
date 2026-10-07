use std::sync::Arc;

use tonic::async_trait;
use tracing::instrument;

use crate::{
    B3Digest,
    blobstore::{self, BlobMeta, BlobStore},
    combinators,
    composition::{CompositionContext, ServiceBuilder},
};

/// Fans out requests to multiple stores in parallel, returning the first positive or erroneous answer.
///
/// A negative response is returned if all backends return them.
/// Write requests are not implemented.
pub struct Race<BS> {
    instance_name: String,
    services: Vec<BS>,
}

impl<BS> Race<BS> {
    /// Construct from an iterator of services.
    pub fn new<I: IntoIterator<Item = BS>>(instance_name: String, iter: I) -> Race<BS> {
        Self {
            instance_name,
            services: Vec::from_iter(iter),
        }
    }

    /// Add another service to the list.
    pub fn add(&mut self, svc: BS) {
        self.services.push(svc);
    }
}

#[async_trait]
impl<BS> BlobStore for Race<BS>
where
    BS: BlobStore,
{
    #[instrument(skip(self, digest), fields(blob.digest=%digest, instance_name=%self.instance_name))]
    async fn get(&self, digest: &B3Digest) -> Result<Option<BlobMeta>, blobstore::Error> {
        Ok(combinators::race::race_unary(&self.services, |svc| async {
            // Skip over `Ok(None)` by returning None,
            // but keep the Option<Chunk> in the returned Ok() value.
            Some(svc.get(digest).await.transpose()?.map(Some))
        })
        .await
        .map_err(Error::Racing)?)
    }

    #[instrument(skip(self, digest), fields(blob.digest=%digest, instance_name=%self.instance_name))]
    async fn has(&self, digest: &B3Digest) -> Result<bool, blobstore::Error> {
        Ok(combinators::race::race_unary(&self.services, |svc| async {
            // Skip over Ok(false) by returning None.
            match svc.has(digest).await {
                Ok(false) => None,
                resp => Some(resp),
            }
        })
        .await
        .map_err(Error::Racing)?)
    }

    #[instrument(skip_all, fields(instance_name=%self.instance_name))]
    async fn put(&self, _digest: &B3Digest, _blob_meta: BlobMeta) -> Result<(), blobstore::Error> {
        Err(Error::Unimplemented.into())
    }
}

/// Error returned from [Race].
#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("wrong arguments: {0}")]
    WrongConfig(&'static str),

    #[error("error from racing")]
    Racing(#[source] combinators::race::Error<blobstore::Error>),

    #[error("puts are unimplemented")]
    Unimplemented,
}

impl From<Error> for blobstore::Error {
    fn from(value: Error) -> Self {
        Self(Box::new(value))
    }
}

/// Configuration for a [Race] [BlobStore].
#[derive(serde::Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct RaceConfig {
    services: Vec<String>,
}

impl TryFrom<url::Url> for RaceConfig {
    type Error = Box<dyn std::error::Error + Send + Sync>;
    fn try_from(url: url::Url) -> Result<Self, Self::Error> {
        if url.has_authority() || !url.path().is_empty() {
            return Err(Error::WrongConfig("no authority or path allowed").into());
        }
        Ok(serde_qs::from_str(url.query().unwrap_or_default())?)
    }
}

#[async_trait]
impl ServiceBuilder for RaceConfig {
    type Output = dyn BlobStore;
    async fn build<'a>(
        &'a self,
        instance_name: &str,
        context: &CompositionContext,
    ) -> Result<Arc<Self::Output>, Box<dyn std::error::Error + Send + Sync>> {
        let services =
            futures::future::try_join_all(self.services.iter().map(|instance_ref| async move {
                context.resolve::<Self::Output>(instance_ref).await
            }))
            .await?;

        Ok(Arc::new(Race::new(instance_name.to_string(), services)))
    }
}

#[cfg(test)]
mod test {
    use std::sync::LazyLock;

    use mockall::predicate;
    use pretty_assertions::assert_matches;

    use crate::{
        blobstore::{self, BlobMeta, BlobStore, MockBlobStore},
        chunkstore::Chunk,
        combinators,
    };

    use super::{Error, Race};

    const CHUNK: Chunk = Chunk::from_static(b"foo");
    static BLOB_META: LazyLock<BlobMeta> =
        LazyLock::new(|| BlobMeta::from_digests_and_sizes([(CHUNK.digest(), CHUNK.len() as u64)]));

    /// backends are tried exhaustively if all report None.
    #[tokio::test]
    async fn get_tries_exhaustively_on_none() {
        let first = {
            let mut svc = MockBlobStore::new();
            svc.expect_get()
                .with(predicate::eq(CHUNK.digest()))
                .once()
                .returning(|_| Ok(None));
            svc
        };
        let second = {
            let mut svc = MockBlobStore::new();
            svc.expect_get()
                .with(predicate::eq(CHUNK.digest()))
                .once()
                .returning(|_| Ok(None));
            svc
        };

        let uut = Race::new("uut".to_string(), [first, second]);

        assert!(
            uut.get(&CHUNK.digest())
                .await
                .expect("to succeed")
                .is_none()
        )
    }

    /// backends are tried exhaustively if all report false.
    #[tokio::test]
    async fn has_tries_exhaustively_on_false() {
        let first = {
            let mut svc = MockBlobStore::new();
            svc.expect_has()
                .with(predicate::eq(CHUNK.digest()))
                .once()
                .returning(|_| Ok(false));
            svc
        };
        let second = {
            let mut svc = MockBlobStore::new();
            svc.expect_has()
                .with(predicate::eq(CHUNK.digest()))
                .once()
                .returning(|_| Ok(false));
            svc
        };

        let uut = Race::new("uut".to_string(), [first, second]);

        assert!(!uut.has(&CHUNK.digest()).await.expect("to succeed"));
    }

    /// if one has it and one does not, we return the positive result.
    #[tokio::test]
    async fn get_returns_positive() {
        let first = {
            let mut svc = MockBlobStore::new();
            svc.expect_get()
                .with(predicate::eq(CHUNK.digest()))
                .once()
                .returning(|_| Ok(Some(BLOB_META.clone())));
            svc
        };

        let second = {
            let mut svc = MockBlobStore::new();
            svc.expect_get()
                .with(predicate::eq(CHUNK.digest()))
                // We cannot be certain this is called at all, so no `once()` here.
                .returning(|_| Ok(None));
            svc
        };

        let uut = Race::new("uut".to_string(), [first, second]);

        assert_eq!(
            Some(BLOB_META.clone()),
            uut.get(&CHUNK.digest()).await.expect("to succeed")
        )
    }

    /// Errors are bubbled up, and the error contains the correct service index.
    #[tokio::test]
    async fn get_return_error() {
        let first = {
            let mut svc = MockBlobStore::new();
            svc.expect_get()
                .with(predicate::eq(CHUNK.digest()))
                .once()
                .returning(|_| Err(blobstore::Error("".into())));
            svc
        };

        // Ideally this one would be just slower than `first`.
        let second = {
            let mut svc = MockBlobStore::new();
            svc.expect_get()
                .with(predicate::eq(CHUNK.digest()))
                // We cannot be certain this is called at all, so no `once()` here.
                .returning(|_| Ok(None));
            svc
        };

        let uut = Race::new("uut".to_string(), [first, second]);

        let err = uut.get(&CHUNK.digest()).await.expect_err("to fail").0;
        let err = err.downcast_ref::<Error>().unwrap();
        assert_matches!(err, Error::Racing(combinators::race::Error::Backend(0, _)))
    }

    /// put is unsupported, and not sent to any backend.
    #[tokio::test]
    async fn put_unsupported() {
        let mut first = MockBlobStore::new();
        first.expect_put().never();

        let uut = Race::new("uut".to_string(), [first]);

        let err = uut
            .put(&CHUNK.digest(), BLOB_META.clone())
            .await
            .expect_err("must fail")
            .0;

        let err = err.downcast_ref::<Error>().unwrap();
        assert_matches!(err, Error::Unimplemented);
    }
}
