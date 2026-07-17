pub mod worker_protocol;

use std::io::Result;

use tokio::io::AsyncRead;
use tracing::warn;
use types::{QueryMissingResult, QueryValidPaths, UnkeyedValidPathInfo, ValidPathInfo};

use crate::{
    derived_path::DerivedPath,
    nix_daemon::types::{BuildMode, KeyedBuildResult},
    store_path::StorePath,
};

pub mod framing;
pub mod handler;
pub mod types;

/// Represents all possible operations over the nix-daemon protocol.
#[cfg_attr(test, mockall::automock)]
pub trait NixDaemonIO: Sync {
    fn is_valid_path(
        &self,
        path: &StorePath,
    ) -> impl std::future::Future<Output = Result<bool>> + Send {
        async move { Ok(self.query_path_info(path).await?.is_some()) }
    }

    fn query_path_info(
        &self,
        path: &StorePath,
    ) -> impl std::future::Future<Output = Result<Option<UnkeyedValidPathInfo>>> + Send;

    fn query_path_from_hash_part(
        &self,
        hash: &[u8],
    ) -> impl std::future::Future<Output = Result<Option<UnkeyedValidPathInfo>>> + Send;

    fn query_valid_paths(
        &self,
        request: &QueryValidPaths,
    ) -> impl std::future::Future<Output = Result<Vec<StorePath>>> + Send {
        async move {
            if request.substitute {
                warn!("snix does not yet support substitution, ignoring the 'substitute' flag...");
            }

            let mut results: Vec<StorePath> = Vec::with_capacity(request.paths.len());

            for path in request.paths.iter() {
                if self.is_valid_path(path).await? {
                    results.push(path.clone());
                }
            }

            Ok(results)
        }
    }

    fn query_valid_derivers(
        &self,
        path: &StorePath,
    ) -> impl std::future::Future<Output = Result<Vec<StorePath>>> + Send {
        async move {
            let result = self.query_path_info(path).await?;
            let result: Vec<_> = result.into_iter().filter_map(|info| info.deriver).collect();
            Ok(result)
        }
    }

    fn query_missing(
        &self,
        derived_paths: Vec<DerivedPath>,
    ) -> impl std::future::Future<Output = Result<QueryMissingResult>> + Send;

    #[cfg_attr(test, mockall::concretize)]
    fn add_to_store_nar<R>(
        &self,
        info: ValidPathInfo,
        reader: &mut R,
        repair: bool,
        dont_check_sigs: bool,
    ) -> impl std::future::Future<Output = Result<()>> + Send
    where
        R: AsyncRead + Send + Unpin;

    fn build_paths(
        &self,
        derived_paths: Vec<DerivedPath>,
        mode: BuildMode,
    ) -> impl std::future::Future<Output = Result<()>> + Send;

    fn build_paths_with_results(
        &self,
        derived_paths: Vec<DerivedPath>,
        mode: BuildMode,
    ) -> impl std::future::Future<Output = Result<Vec<KeyedBuildResult>>> + Send;
}

#[cfg(test)]
mod tests {

    use crate::{
        derived_path::DerivedPath,
        nix_daemon::types::{NarHash, QueryValidPaths},
        store_path::StorePath,
    };

    use super::{NixDaemonIO, types::UnkeyedValidPathInfo};

    // Very simple mock
    // Unable to use mockall as it does not support unboxed async traits.
    pub struct MockNixDaemonIO {
        query_path_info_result: Option<UnkeyedValidPathInfo>,
    }

    impl NixDaemonIO for MockNixDaemonIO {
        async fn query_path_info(
            &self,
            _path: &StorePath,
        ) -> std::io::Result<Option<UnkeyedValidPathInfo>> {
            Ok(self.query_path_info_result.clone())
        }

        async fn query_path_from_hash_part(
            &self,
            _hash: &[u8],
        ) -> std::io::Result<Option<UnkeyedValidPathInfo>> {
            Ok(None)
        }

        async fn add_to_store_nar<R>(
            &self,
            _info: super::types::ValidPathInfo,
            _reader: &mut R,
            _repair: bool,
            _dont_check_sigs: bool,
        ) -> std::io::Result<()>
        where
            R: tokio::io::AsyncRead + Send + Unpin,
        {
            Ok(())
        }

        async fn build_paths(
            &self,
            _derived_paths: Vec<DerivedPath>,
            _mode: super::types::BuildMode,
        ) -> std::io::Result<()> {
            Ok(())
        }

        async fn build_paths_with_results(
            &self,
            _derived_paths: Vec<DerivedPath>,
            _mode: super::types::BuildMode,
        ) -> std::io::Result<Vec<super::types::KeyedBuildResult>> {
            Err(std::io::Error::other(
                "Operation BuildPathsWithResults is not implemented",
            ))
        }

        async fn query_missing(
            &self,
            _derived_paths: Vec<DerivedPath>,
        ) -> std::io::Result<super::types::QueryMissingResult> {
            Err(std::io::Error::other(
                "Operation QueryMissing is not implemented",
            ))
        }
    }

    #[tokio::test]
    async fn test_is_valid_path_returns_true() {
        let path =
            StorePath::from_bytes("z6r3bn5l51679pwkvh9nalp6c317z34m-hello".as_bytes()).unwrap();
        let io = MockNixDaemonIO {
            query_path_info_result: Some(UnkeyedValidPathInfo {
                deriver: Some("00000000000000000000000000000000-_.drv".parse().unwrap()),
                nar_hash: NarHash::from_digest([0u8; 32]),
                references: Vec::new(),
                registration_time: 0,
                nar_size: 0,
                ultimate: true,
                signatures: Vec::new(),
                ca: None,
            }),
        };

        let result = io
            .is_valid_path(&path)
            .await
            .expect("expected to get a non-empty response");
        assert!(result, "expected to get true");
    }

    #[tokio::test]
    async fn test_is_valid_path_returns_false() {
        let path =
            StorePath::from_bytes("z6r3bn5l51679pwkvh9nalp6c317z34m-hello".as_bytes()).unwrap();
        let io = MockNixDaemonIO {
            query_path_info_result: None,
        };

        let result = io
            .is_valid_path(&path)
            .await
            .expect("expected to get a non-empty response");
        assert!(!result, "expected to get false");
    }

    #[tokio::test]
    async fn test_query_valid_paths_returns_empty_response() {
        let path =
            StorePath::from_bytes("z6r3bn5l51679pwkvh9nalp6c317z34m-hello".as_bytes()).unwrap();
        let io = MockNixDaemonIO {
            query_path_info_result: None,
        };

        let result = io
            .query_valid_paths(&QueryValidPaths {
                paths: vec![path],
                substitute: false,
            })
            .await
            .expect("expected to get a non-empty response");
        assert_eq!(result, vec![], "expected to get empty response");
    }

    #[tokio::test]
    async fn test_query_valid_paths_returns_non_empty_response() {
        let path =
            StorePath::from_bytes("z6r3bn5l51679pwkvh9nalp6c317z34m-hello".as_bytes()).unwrap();
        let io = MockNixDaemonIO {
            query_path_info_result: Some(UnkeyedValidPathInfo {
                deriver: Some("00000000000000000000000000000000-_.drv".parse().unwrap()),
                nar_hash: NarHash::from_digest([0u8; 32]),
                references: Vec::new(),
                registration_time: 0,
                nar_size: 0,
                ultimate: true,
                signatures: Vec::new(),
                ca: None,
            }),
        };

        let result = io
            .query_valid_paths(&QueryValidPaths {
                paths: vec![path.clone()],
                substitute: false,
            })
            .await
            .expect("expected to get a non-empty response");
        assert_eq!(result, vec![path], "expected to get non empty response");
    }

    #[tokio::test]
    async fn test_query_valid_derivers_returns_empty_response() {
        let path =
            StorePath::from_bytes("z6r3bn5l51679pwkvh9nalp6c317z34m-hello".as_bytes()).unwrap();
        let io = MockNixDaemonIO {
            query_path_info_result: None,
        };

        let result = io
            .query_valid_derivers(&path)
            .await
            .expect("expected to get a non-empty response");
        assert_eq!(result, vec![], "expected to get empty response");
    }

    #[tokio::test]
    async fn test_query_valid_derivers_returns_non_empty_response() {
        let path =
            StorePath::from_bytes("z6r3bn5l51679pwkvh9nalp6c317z34m-hello".as_bytes()).unwrap();
        let deriver =
            StorePath::from_bytes("z6r3bn5l51679pwkvh9nalp6c317z34m-hello.drv".as_bytes()).unwrap();
        let io = MockNixDaemonIO {
            query_path_info_result: Some(UnkeyedValidPathInfo {
                deriver: Some(deriver.clone()),
                nar_hash: NarHash::from_digest([0u8; 32]),
                references: vec![],
                registration_time: 0,
                nar_size: 1,
                ultimate: true,
                signatures: vec![],
                ca: None,
            }),
        };

        let result = io
            .query_valid_derivers(&path)
            .await
            .expect("expected to get a non-empty response");
        assert_eq!(result, vec![deriver], "expected to get non empty response");
    }
}
