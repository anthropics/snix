#![cfg_attr(docsrs, feature(doc_cfg))]

use std::{
    io::{Error, Result},
    sync::Arc,
};

use nix_compat::{
    derived_path::DerivedPath,
    nix_daemon::{
        NixDaemonIO,
        types::{BuildMode, KeyedBuildResult, NarHash, UnkeyedValidPathInfo, ValidPathInfo},
    },
    nixbase32,
    nixhash::CAHashMode,
    store_path::{StorePath, build_ca_path},
};
use snix_castore::{blobservice::BlobService, directoryservice::DirectoryService};
use snix_store::{nar::ingest_nar_and_hash, path_info::PathInfo, pathinfoservice::PathInfoService};
use tracing::{instrument, warn};

#[allow(dead_code)]
pub struct SnixDaemon {
    blob_service: Arc<dyn BlobService>,
    directory_service: Arc<dyn DirectoryService>,
    path_info_service: Arc<dyn PathInfoService>,
}

impl SnixDaemon {
    pub fn new(
        blob_service: Arc<dyn BlobService>,
        directory_service: Arc<dyn DirectoryService>,
        path_info_service: Arc<dyn PathInfoService>,
    ) -> Self {
        Self {
            blob_service,
            directory_service,
            path_info_service,
        }
    }
}

/// Implements [NixDaemonIO] backed by snix services.
impl NixDaemonIO for SnixDaemon {
    #[instrument(skip_all, fields(path), level = "debug", ret(Debug))]
    async fn query_path_info(&self, path: &StorePath) -> Result<Option<UnkeyedValidPathInfo>> {
        if let Some(path_info) = self
            .path_info_service
            .get(*path.digest())
            .await
            .map_err(std::io::Error::other)?
            && path_info.store_path.name() == path.name()
        {
            return Ok(Some(into_unkeyed_path_info(path_info)));
        }
        Ok(None)
    }

    #[instrument(skip_all, fields(hash=nix_compat::nixbase32::encode(hash)), level = "debug", ret(Debug))]
    async fn query_path_from_hash_part(&self, hash: &[u8]) -> Result<Option<UnkeyedValidPathInfo>> {
        let digest = hash
            .try_into()
            .map_err(|_| Error::other("invalid digest length"))?;
        match self
            .path_info_service
            .get(digest)
            .await
            .map_err(std::io::Error::other)?
        {
            Some(path_info) => Ok(Some(into_unkeyed_path_info(path_info))),
            None => Ok(None),
        }
    }

    #[instrument(skip_all, fields(request), level = "debug", ret(Debug))]
    async fn add_to_store_nar<R>(
        &self,
        info: ValidPathInfo,
        reader: &mut R,
        _repair: bool,
        _dont_check_sigs: bool,
    ) -> Result<()>
    where
        R: tokio::io::AsyncRead + Send + Unpin,
    {
        let (root_node, nar_sha256, nar_size) = ingest_nar_and_hash(
            self.blob_service.clone(),
            &self.directory_service,
            reader,
            &info.info.ca,
        )
        .await
        .map_err(|e| Error::other(e.to_string()))?;

        if nar_size != info.info.nar_size || nar_sha256 != *info.info.nar_hash {
            warn!(
                nar_hash.expected = nixbase32::encode(&*info.info.nar_hash),
                nar_hash.actual = nixbase32::encode(&nar_sha256),
                "nar hash mismatch"
            );
            return Err(Error::other(
                "ingested nar ended up different from what was specified in the request",
            ));
        }

        if let Some(cahash) = &info.info.ca {
            let actual_path = build_ca_path(
                info.path.name(),
                cahash.mode() == CAHashMode::Nar,
                &cahash.hash(),
                info.info.references.iter().map(|p| p.as_ref()),
                false,
            )
            .map_err(Error::other)?;

            if actual_path != info.path.as_ref() {
                return Err(Error::other("path mismatch"));
            }
        }

        let path_info = PathInfo {
            store_path: info.path,
            node: root_node,
            references: info.info.references,
            nar_size,
            nar_sha256,
            signatures: info.info.signatures,
            deriver: info.info.deriver,
            ca: info.info.ca,
        };
        self.path_info_service
            .put(path_info)
            .await
            .map_err(|e| Error::other(e.to_string()))?;
        Ok(())
    }

    async fn build_paths(&self, _derived_paths: Vec<DerivedPath>, _mode: BuildMode) -> Result<()> {
        Ok(())
    }

    async fn build_paths_with_results(
        &self,
        _derived_paths: Vec<DerivedPath>,
        _mode: BuildMode,
    ) -> Result<Vec<KeyedBuildResult>> {
        Err(std::io::Error::other(
            "Operation BuildPathsWithResults is not implemented",
        ))
    }
}

// PathInfo lives in the snix-store crate, but does not depend on nix-compat's wire feature,
// while UnkeyedValidPathInfo is only available if that feature is enabled. To avoid complexity
// we manually convert as opposed to creating a From<PathInfo>.
fn into_unkeyed_path_info(info: PathInfo) -> UnkeyedValidPathInfo {
    UnkeyedValidPathInfo {
        deriver: info.deriver,
        nar_hash: NarHash::from_digest(info.nar_sha256),
        references: info.references,
        registration_time: 0,
        nar_size: info.nar_size,
        ultimate: false,
        signatures: info.signatures,
        ca: info.ca,
    }
}
