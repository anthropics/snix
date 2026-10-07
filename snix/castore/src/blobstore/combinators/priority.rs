use std::sync::Arc;

use tonic::async_trait;
use tracing::instrument;

use crate::{
    B3Digest,
    blobstore::{self, BlobMeta, BlobStore},
    composition::{CompositionContext, ServiceBuilder},
};

/// Holds references to many different [BlobStore]s.
/// Read requests try services sequentially.
/// Any error in a service bubbles up.
/// Write requests are not implemented.
pub struct Priority<BS> {
    instance_name: String,
    services: Vec<BS>,
}

impl<BS> Priority<BS> {
    /// Construct from an iterator of services.
    pub fn new<I: IntoIterator<Item = BS>>(instance_name: String, iter: I) -> Priority<BS> {
        Self {
            instance_name,
            services: Vec::from_iter(iter),
        }
    }
}

#[async_trait]
impl<BS> BlobStore for Priority<BS>
where
    BS: BlobStore,
{
    #[instrument(skip(self, digest), fields(blob.digest=%digest, instance_name=%self.instance_name))]
    async fn get(&self, digest: &B3Digest) -> Result<Option<BlobMeta>, blobstore::Error> {
        // traverse the list of services. If any service has it, return from there.
        // Errors cause the combinator to bail out early.
        for (idx, service) in self.services.iter().enumerate() {
            if let Some(chunk) = service
                .get(digest)
                .await
                .map_err(|err| Error::Backend(idx, err))?
            {
                return Ok(Some(chunk));
            }
        }

        Ok(None)
    }

    #[instrument(skip(self, digest), fields(blob.digest=%digest, instance_name=%self.instance_name))]
    async fn has(&self, digest: &B3Digest) -> Result<bool, blobstore::Error> {
        // traverse the list of services. If any service has it, return true.
        // Errors cause the combinator to bail out early.
        for (idx, service) in self.services.iter().enumerate() {
            if service
                .has(digest)
                .await
                .map_err(|err| Error::Backend(idx, err))?
            {
                return Ok(true);
            }
        }

        Ok(false)
    }

    #[instrument(skip_all, fields(instance_name=%self.instance_name))]
    async fn put(&self, _digest: &B3Digest, _blob_meta: BlobMeta) -> Result<(), blobstore::Error> {
        Err(Error::Unimplemented.into())
    }
}

/// Error returned from [Priority].
#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("wrong arguments: {0}")]
    WrongConfig(&'static str),

    #[error("error from service with index {0}")]
    Backend(usize, #[source] blobstore::Error),

    #[error("puts are unimplemented")]
    Unimplemented,
}

impl From<Error> for blobstore::Error {
    fn from(value: Error) -> Self {
        Self(Box::new(value))
    }
}

/// Configuration for a [Priority] [BlobStore].
#[derive(serde::Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct PriorityConfig {
    services: Vec<String>,
}

impl TryFrom<url::Url> for PriorityConfig {
    type Error = Box<dyn std::error::Error + Send + Sync>;
    fn try_from(url: url::Url) -> Result<Self, Self::Error> {
        if url.has_authority() || !url.path().is_empty() {
            return Err(Error::WrongConfig("no authority or path allowed").into());
        }
        Ok(serde_qs::from_str(url.query().unwrap_or_default())?)
    }
}

#[async_trait]
impl ServiceBuilder for PriorityConfig {
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

        Ok(Arc::new(Priority::new(instance_name.to_string(), services)))
    }
}

#[cfg(test)]
mod test {
    use std::sync::LazyLock;

    use mockall::{Sequence, predicate};
    use pretty_assertions::{assert_eq, assert_matches};

    use crate::{
        blobstore::{self, BlobMeta, BlobStore, MockBlobStore},
        chunkstore::Chunk,
    };

    use super::{Error, Priority};

    const CHUNK: Chunk = Chunk::from_static(b"foo");
    static BLOB_META: LazyLock<BlobMeta> =
        LazyLock::new(|| BlobMeta::from_digests_and_sizes([(CHUNK.digest(), CHUNK.len() as u64)]));

    /// If first has something, last is never tried.
    #[tokio::test]
    async fn get_first_gets_tried_only() {
        let mut first = MockBlobStore::new();
        let mut last = MockBlobStore::new();

        first
            .expect_get()
            .with(predicate::eq(CHUNK.digest()))
            .once()
            .returning(|_| Ok(Some(BLOB_META.clone())));

        last.expect_get().never();

        let uut = Priority::new("uut".to_string(), [first, last]);

        assert_eq!(
            Some(BLOB_META.clone()),
            uut.get(&CHUNK.digest()).await.expect("to succeed")
        )
    }

    /// If first doesn't have it, we try last.
    #[tokio::test]
    async fn get_first_then_last() {
        let mut first = MockBlobStore::new();
        let mut last = MockBlobStore::new();
        let mut seq = Sequence::new();

        first
            .expect_get()
            .with(predicate::eq(CHUNK.digest()))
            .once()
            .in_sequence(&mut seq)
            .returning(|_| Ok(None));

        last.expect_get()
            .with(predicate::eq(CHUNK.digest()))
            .once()
            .in_sequence(&mut seq)
            .returning(|_| Ok(Some(BLOB_META.clone())));

        let uut = Priority::new("uut".to_string(), [first, last]);

        assert_eq!(
            Some(BLOB_META.clone()),
            uut.get(&CHUNK.digest()).await.expect("to succeed")
        )
    }

    /// If none of the two have it, we return None.
    #[tokio::test]
    async fn get_first_then_last_not_found() {
        let mut first = MockBlobStore::new();
        let mut last = MockBlobStore::new();
        let mut seq = Sequence::new();

        first
            .expect_get()
            .with(predicate::eq(CHUNK.digest()))
            .once()
            .in_sequence(&mut seq)
            .returning(|_| Ok(None));

        last.expect_get()
            .with(predicate::eq(CHUNK.digest()))
            .once()
            .in_sequence(&mut seq)
            .returning(|_| Ok(None));

        let uut = Priority::new("uut".to_string(), [first, last]);

        assert_eq!(None, uut.get(&CHUNK.digest()).await.expect("to succeed"))
    }

    /// Errors are bubbled up from the first backend emitting the error,
    /// and the error identifies the backend that emitted the error.
    #[tokio::test]
    async fn get_bubble_up_error_first() {
        let mut first = MockBlobStore::new();
        let mut last = MockBlobStore::new();

        first
            .expect_get()
            .with(predicate::eq(CHUNK.digest()))
            .once()
            .returning(|_| Err(blobstore::Error("oh no".into())));

        last.expect_get().never();

        let uut = Priority::new("uut".to_string(), [first, last]);

        let err = uut.get(&CHUNK.digest()).await.expect_err("must fail").0;

        let err = err.downcast_ref::<Error>().unwrap();
        assert_matches!(err, Error::Backend(0, _));
    }

    /// has() returns true from the first backend that has it.
    #[tokio::test]
    async fn has_first_then_last() {
        let mut first = MockBlobStore::new();
        let mut last = MockBlobStore::new();
        let mut seq = Sequence::new();

        first
            .expect_has()
            .with(predicate::eq(CHUNK.digest()))
            .once()
            .in_sequence(&mut seq)
            .returning(|_| Ok(false));

        last.expect_has()
            .with(predicate::eq(CHUNK.digest()))
            .once()
            .in_sequence(&mut seq)
            .returning(|_| Ok(true));

        let uut = Priority::new("uut".to_string(), [first, last]);

        assert!(uut.has(&CHUNK.digest()).await.expect("to succeed"));
    }

    /// put is unsupported, and not sent to the backend.
    #[tokio::test]
    async fn put_unsupported() {
        let mut first = MockBlobStore::new();
        first.expect_put().never();

        let uut = Priority::new("uut".to_string(), [first]);

        let err = uut
            .put(&CHUNK.digest(), BLOB_META.clone())
            .await
            .expect_err("must fail")
            .0;

        let err = err.downcast_ref::<Error>().unwrap();
        assert_matches!(err, Error::Unimplemented);
    }
}
