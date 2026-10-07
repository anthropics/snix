use std::sync::Arc;

use futures::{StreamExt, stream::BoxStream};
use snix_castore::{
    combinators,
    composition::{CompositionContext, ServiceBuilder},
};
use tonic::async_trait;

use crate::{
    nar::NarCalculationService,
    pathinfoservice::{self, PathInfo, PathInfoService},
};

/// Fans out requests to multiple stores in parallel, returning the first positive or erroneous answer.
///
/// A negative response is returned if all backends return them.
/// Write requests are not implemented.
pub struct Race<PS> {
    #[allow(unused)]
    instance_name: String,
    services: Vec<PS>,
}

impl<PS> Race<PS> {
    /// Construct from an iterator of services.
    pub fn new<I: IntoIterator<Item = PS>>(instance_name: String, iter: I) -> Race<PS> {
        Self {
            instance_name,
            services: Vec::from_iter(iter),
        }
    }

    /// Add another sevice to the list.
    pub fn add(&mut self, svc: PS) {
        self.services.push(svc);
    }
}

#[async_trait]
impl<PS> PathInfoService for Race<PS>
where
    PS: PathInfoService,
{
    async fn get(&self, digest: [u8; 20]) -> Result<Option<PathInfo>, pathinfoservice::Error> {
        Ok(combinators::race::race_unary(&self.services, |svc| async {
            // Skip over `Ok(None)` by returning None,
            // but keep the Option<PathInfo> in the returned Ok() value.
            Some(svc.get(digest).await.transpose()?.map(Some))
        })
        .await
        .map_err(Error::Racing)?)
    }

    async fn has(&self, digest: [u8; 20]) -> Result<bool, pathinfoservice::Error> {
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

    async fn put(&self, _path_info: PathInfo) -> Result<PathInfo, pathinfoservice::Error> {
        return Err(Error::Unimplemented.into());
    }

    fn list(&self) -> BoxStream<'static, Result<PathInfo, pathinfoservice::Error>> {
        futures::stream::once(async { Err(Box::new(Error::Unimplemented))? }).boxed()
    }

    fn nar_calculation_service(&self) -> Option<Arc<dyn NarCalculationService>> {
        // We can't possibly know which one has all contents to calculate.
        None
    }
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("wrong arguments: {0}")]
    WrongConfig(&'static str),

    #[error("error from racing")]
    Racing(combinators::race::Error<pathinfoservice::Error>),

    #[error("unimplemented")]
    Unimplemented,
}

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
    type Output = dyn PathInfoService;
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
    use snix_castore::combinators;

    use crate::{
        fixtures::PATH_INFO,
        pathinfoservice::{MockPathInfoService, PathInfoService},
    };

    use super::{Error, Race};

    static PATH_INFO_DIGEST: LazyLock<[u8; 20]> = LazyLock::new(|| *PATH_INFO.store_path.digest());

    /// backends are tried exhaustively if all report None.
    #[tokio::test]
    async fn get_tries_exhaustively_on_none() {
        let first = {
            let mut svc = MockPathInfoService::new();
            svc.expect_get()
                .with(predicate::eq(*PATH_INFO_DIGEST))
                .once()
                .returning(|_| Ok(None));
            svc
        };
        let second = {
            let mut svc = MockPathInfoService::new();
            svc.expect_get()
                .with(predicate::eq(*PATH_INFO_DIGEST))
                .once()
                .returning(|_| Ok(None));
            svc
        };

        let uut = Race::new("uut".to_string(), [first, second]);

        assert!(
            uut.get(*PATH_INFO_DIGEST)
                .await
                .expect("to succeed")
                .is_none()
        )
    }

    /// backends are tried exhaustively if all report None.
    #[tokio::test]
    async fn has_tries_exhaustively_on_none() {
        let first = {
            let mut svc = MockPathInfoService::new();
            svc.expect_has()
                .with(predicate::eq(*PATH_INFO_DIGEST))
                .once()
                .returning(|_| Ok(false));
            svc
        };
        let second = {
            let mut svc = MockPathInfoService::new();
            svc.expect_has()
                .with(predicate::eq(*PATH_INFO_DIGEST))
                .once()
                .returning(|_| Ok(false));
            svc
        };

        let uut = Race::new("uut".to_string(), [first, second]);

        assert!(!uut.has(*PATH_INFO_DIGEST).await.expect("to succeed"),);
    }

    // if one has it and one does not, we return the positive result.
    #[tokio::test]
    async fn get_returns_positive() {
        let first = {
            let mut svc = MockPathInfoService::new();
            svc.expect_get()
                .with(predicate::eq(*PATH_INFO_DIGEST))
                .once()
                .returning(|_| Ok(Some(PATH_INFO.clone())));
            svc
        };

        let second = {
            let mut svc = MockPathInfoService::new();
            svc.expect_get()
                .with(predicate::eq(*PATH_INFO_DIGEST))
                // We cannot be certain this is called at all, so no `once()` here.
                .returning(|_| Ok(None));
            svc
        };

        let uut = Race::new("uut".to_string(), [first, second]);

        assert_eq!(
            Some(PATH_INFO.clone()),
            uut.get(*PATH_INFO_DIGEST).await.expect("to succeed")
        )
    }

    /// Errors are bubbled up, and the error contains the correct service index.
    #[tokio::test]
    async fn get_return_error() {
        let first = {
            let mut svc = MockPathInfoService::new();
            svc.expect_get()
                .with(predicate::eq(*PATH_INFO_DIGEST))
                .once()
                .returning(|_| Err("".into()));
            svc
        };

        // Ideally this one would be just slower than `first`.
        let second = {
            let mut svc = MockPathInfoService::new();
            svc.expect_get()
                .with(predicate::eq(*PATH_INFO_DIGEST))
                // We cannot be certain this is called at all, so no `once()` here.
                .returning(|_| Ok(None));
            svc
        };

        let uut = Race::new("uut".to_string(), [first, second]);

        let err = uut.get(*PATH_INFO_DIGEST).await.expect_err("to fail");
        let err = err.downcast_ref::<Error>().unwrap();
        assert_matches!(err, Error::Racing(combinators::race::Error::Backend(0, _)))
    }

    // FUTUREWORK: ideally we'd be constructing mocks that take longer than others / never return,
    // but that's not supported in automock: https://github.com/asomers/mockall/issues/189
    // So it's a bit tough to create test cases reliably.
}
