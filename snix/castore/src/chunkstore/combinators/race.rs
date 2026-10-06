use std::sync::Arc;

use tonic::async_trait;
use tracing::instrument;

use crate::{
    B3Digest,
    chunkstore::{self, Chunk, ChunkStore},
    combinators,
    composition::{CompositionContext, ServiceBuilder},
};

/// Holds references to multiple [ChunkStore]s.
/// Read requests try services in parallel.
/// The first positive response is returned,
/// a negative response only if all backends return this.
/// Write requests are not implemented.
pub struct Race<CS> {
    instance_name: String,
    services: Vec<CS>,
}

impl<CS> Race<CS> {
    /// Construct from an iterator of services.
    pub fn new<I: IntoIterator<Item = CS>>(instance_name: String, iter: I) -> Race<CS> {
        Self {
            instance_name,
            services: Vec::from_iter(iter),
        }
    }

    /// Add another service to the list.
    pub fn add(&mut self, svc: CS) {
        self.services.push(svc);
    }
}

#[async_trait]
impl<CS> ChunkStore for Race<CS>
where
    CS: ChunkStore,
{
    #[instrument(skip(self, digest), fields(chunk.digest=%digest, instance_name=%self.instance_name))]
    async fn get(&self, digest: &B3Digest) -> Result<Option<Chunk>, chunkstore::Error> {
        Ok(combinators::race::race_unary(&self.services, |svc| async {
            // Skip over `Ok(None)` by returning None,
            // but keep the Option<Chunk> in the returned Ok() value.
            Some(svc.get(digest).await.transpose()?.map(Some))
        })
        .await
        .map_err(Error::Racing)?)
    }

    #[instrument(skip(self, digest), fields(chunk.digest=%digest, instance_name=%self.instance_name))]
    async fn has(&self, digest: &B3Digest) -> Result<bool, chunkstore::Error> {
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
    async fn put(&self, _chunk: Chunk) -> Result<B3Digest, chunkstore::Error> {
        Err(Error::Unimplemented.into())
    }
}

/// Error returned from [Race].
#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("wrong arguments: {0}")]
    WrongConfig(&'static str),

    #[error("error from racing")]
    Racing(#[source] combinators::race::Error<chunkstore::Error>),

    #[error("puts are unimplemented")]
    Unimplemented,
}

impl From<Error> for chunkstore::Error {
    fn from(value: Error) -> Self {
        Self(Box::new(value))
    }
}

/// Configuration for a [Race] [ChunkStore].
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
    type Output = dyn ChunkStore;
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
    use mockall::predicate;
    use pretty_assertions::assert_matches;

    use crate::{
        chunkstore::{self, Chunk, ChunkStore, MockChunkStore},
        combinators,
    };

    use super::{Error, Race};

    const CHUNK: Chunk = Chunk::from_static(b"foo");

    /// backends are tried exhaustively if all report None.
    #[tokio::test]
    async fn get_tries_exhaustively_on_none() {
        let first = {
            let mut svc = MockChunkStore::new();
            svc.expect_get()
                .with(predicate::eq(CHUNK.digest()))
                .once()
                .returning(|_| Ok(None));
            svc
        };
        let second = {
            let mut svc = MockChunkStore::new();
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
            let mut svc = MockChunkStore::new();
            svc.expect_has()
                .with(predicate::eq(CHUNK.digest()))
                .once()
                .returning(|_| Ok(false));
            svc
        };
        let second = {
            let mut svc = MockChunkStore::new();
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
            let mut svc = MockChunkStore::new();
            svc.expect_get()
                .with(predicate::eq(CHUNK.digest()))
                .once()
                .returning(|_| Ok(Some(CHUNK.clone())));
            svc
        };

        let second = {
            let mut svc = MockChunkStore::new();
            svc.expect_get()
                .with(predicate::eq(CHUNK.digest()))
                // We cannot be certain this is called at all, so no `once()` here.
                .returning(|_| Ok(None));
            svc
        };

        let uut = Race::new("uut".to_string(), [first, second]);

        assert_eq!(
            Some(CHUNK.clone()),
            uut.get(&CHUNK.digest()).await.expect("to succeed")
        )
    }

    /// Errors are bubbled up, and the error contains the correct service index.
    #[tokio::test]
    async fn get_return_error() {
        let first = {
            let mut svc = MockChunkStore::new();
            svc.expect_get()
                .with(predicate::eq(CHUNK.digest()))
                .once()
                .returning(|_| Err(chunkstore::Error("".into())));
            svc
        };

        // Ideally this one would be just slower than `first`.
        let second = {
            let mut svc = MockChunkStore::new();
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
        let mut first = MockChunkStore::new();
        first.expect_put().never();

        let uut = Race::new("uut".to_string(), [first]);

        let err = uut.put(CHUNK.clone()).await.expect_err("must fail").0;

        let err = err.downcast_ref::<Error>().unwrap();
        assert_matches!(err, Error::Unimplemented);
    }
}
