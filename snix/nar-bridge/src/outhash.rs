//* Handlers for $outhash.{narinfo,ls} paths.

use axum::{http::StatusCode, response::IntoResponse};
use bytes::Bytes;
use nix_compat::{
    narinfo::{NarInfo, Signature},
    nix_http,
};
use snix_castore::proto::write_infused_nar_path;
use snix_store::pathinfoservice::PathInfo;
use tracing::{Span, instrument, warn};

use crate::AppState;

#[instrument(skip_all, fields(path_info.digest=tracing::field::Empty))]
pub async fn head(
    axum::extract::Path(p): axum::extract::Path<String>,
    axum::extract::State(AppState {
        path_info_service, ..
    }): axum::extract::State<AppState>,
) -> Result<impl IntoResponse, StatusCode> {
    let (digest, _request_type) = nix_http::parse_outhash_str(&p).ok_or(StatusCode::NOT_FOUND)?;
    Span::current().record("path_info.digest", &p[0..32]);

    if path_info_service.has(digest).await.map_err(|e| {
        warn!(err=%e, "failed to get PathInfo");
        StatusCode::INTERNAL_SERVER_ERROR
    })? {
        Ok(([("content-type", nix_http::MIME_TYPE_NARINFO)], ""))
    } else {
        warn!("PathInfo not found");
        Err(StatusCode::NOT_FOUND)
    }
}

#[instrument(skip_all, fields(path_info.digest=tracing::field::Empty))]
pub async fn get(
    axum::extract::Path(p): axum::extract::Path<String>,
    axum::extract::State(AppState {
        directory_service,
        path_info_service,
        ..
    }): axum::extract::State<AppState>,
) -> Result<impl IntoResponse, StatusCode> {
    let (digest, request_type) = nix_http::parse_outhash_str(&p).ok_or(StatusCode::NOT_FOUND)?;
    Span::current().record("path_info.digest", &p[0..32]);

    // fetch the PathInfo
    let path_info = path_info_service
        .get(digest)
        .await
        .map_err(|e| {
            warn!(err=%e, "failed to get PathInfo");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;

    match request_type {
        nix_http::RequestType::Narinfo => Ok((
            [("content-type", nix_http::MIME_TYPE_NARINFO)],
            gen_narinfo_str(&path_info),
        )),
        nix_http::RequestType::Listing => {
            // render the listing
            let listing = snix_store::nar::produce_listing(&path_info.node, &directory_service)
                .await
                .map_err(|err| {
                    warn!(%err, "failed to produce listing");
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;

            let listing_str = serde_json::to_string(&listing).map_err(|err| {
                warn!(%err, "failed to serialize listing");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

            Ok((
                [("content-type", nix_http::MIME_TYPE_NAR_LISTING)],
                listing_str,
            ))
        }
    }
}

/// The size limit for NARInfo uploads nar-bridge receives
const NARINFO_SIZE_LIMIT: usize = 2 * 1024 * 1024;

#[instrument(skip_all, fields(path_info.digest=tracing::field::Empty))]
pub async fn put(
    axum::extract::Path(p): axum::extract::Path<String>,
    axum::extract::State(AppState {
        path_info_service,
        root_nodes,
        ..
    }): axum::extract::State<AppState>,
    request: axum::extract::Request,
) -> Result<&'static str, StatusCode> {
    let (digest, request_type) = nix_http::parse_outhash_str(&p).ok_or(StatusCode::NOT_FOUND)?;
    Span::current().record("path_info.digest", &p[0..32]);

    match request_type {
        // rest of the function body
        nix_http::RequestType::Narinfo => {}
        nix_http::RequestType::Listing => {
            // Nix might want to upload them, but we don't really care.
            // FUTUREWORK: We could potentially compare what it uploads
            // with what we synthesize and fail out if it's not identical.
            // Right now we just pretend we uploaded and call it a day.
            return Ok("");
        }
    }

    let narinfo_bytes: Bytes = axum::body::to_bytes(request.into_body(), NARINFO_SIZE_LIMIT)
        .await
        .map_err(|e| {
            warn!(err=%e, "unable to fetch body");
            StatusCode::BAD_REQUEST
        })?;

    // Parse the narinfo from the body.
    let narinfo_str = std::str::from_utf8(narinfo_bytes.as_ref()).map_err(|e| {
        warn!(err=%e, "unable decode body as string");
        StatusCode::BAD_REQUEST
    })?;

    let narinfo = NarInfo::parse(narinfo_str).map_err(|e| {
        warn!(err=%e, "unable to parse narinfo");
        StatusCode::BAD_REQUEST
    })?;

    if &digest != narinfo.store_path.digest() {
        warn!("digest in URL doesn't match store path in NARInfo");
        Err(StatusCode::BAD_REQUEST)?
    }

    // Lookup root node with peek, as we don't want to update the LRU list.
    // We need to be careful to not hold the RwLock across the await point.
    let maybe_root_node: Option<snix_castore::Node> =
        root_nodes.read().peek(&narinfo.nar_hash).cloned();

    match maybe_root_node {
        Some(root_node) => {
            // Persist the PathInfo.
            path_info_service
                .put(PathInfo {
                    store_path: narinfo.store_path.to_owned(),
                    node: root_node,
                    references: narinfo.references.iter().map(|sp| sp.to_owned()).collect(),
                    nar_sha256: narinfo.nar_hash,
                    nar_size: narinfo.nar_size,
                    signatures: narinfo
                        .signatures
                        .into_iter()
                        .map(|s| {
                            Signature::<String>::new(s.name().to_string(), s.bytes().to_owned())
                        })
                        .collect(),
                    deriver: narinfo.deriver.as_ref().map(|sp| sp.to_owned()),
                    ca: narinfo.ca,
                })
                .await
                .map_err(|e| {
                    warn!(err=%e, "failed to persist the PathInfo");
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;

            Ok("")
        }
        None => {
            warn!("received narinfo with unknown NARHash");
            Err(StatusCode::BAD_REQUEST)
        }
    }
}

/// Constructs a String in NARInfo format for the given [PathInfo].
fn gen_narinfo_str(path_info: &PathInfo) -> String {
    let mut narinfo = path_info.to_narinfo();
    let mut url = String::new();
    write_infused_nar_path(&mut url, path_info.node.clone(), narinfo.nar_size)
        .expect("write into string");
    narinfo.url = &url;

    // Set FileSize to NarSize, as otherwise progress reporting in Nix looks very broken
    narinfo.file_size = Some(narinfo.nar_size);

    narinfo.to_string()
}

#[cfg(test)]
mod tests {
    use std::{num::NonZero, sync::Arc};

    use axum::http::Method;
    use nix_compat::nixbase32;
    use snix_castore::{
        blobservice::BlobService,
        directoryservice::DirectoryService,
        utils::{gen_test_blob_service, gen_test_directory_service},
    };
    use snix_store::{
        fixtures::{DUMMY_PATH_DIGEST, NAR_CONTENTS_SYMLINK, PATH_INFO_SYMLINK},
        path_info::PathInfo,
        pathinfoservice::PathInfoService,
        utils::gen_test_pathinfo_service,
    };
    use tracing_test::traced_test;

    use crate::AppState;

    /// Accepts a router without state, and returns a [axum_test::TestServer].
    /// Also returns the underlying services, so they can be poked with during testing.
    fn gen_server(
        router: axum::Router<AppState>,
    ) -> (
        axum_test::TestServer,
        impl BlobService,
        impl DirectoryService,
        impl PathInfoService,
    ) {
        let blob_service = Arc::new(gen_test_blob_service());
        let directory_service = Arc::new(gen_test_directory_service());
        let path_info_service = Arc::new(gen_test_pathinfo_service());

        let app = router.with_state(AppState::new(
            blob_service.clone(),
            directory_service.clone(),
            path_info_service.clone(),
            NonZero::new(100).unwrap(),
        ));

        (
            axum_test::TestServer::new(app),
            blob_service,
            directory_service,
            path_info_service,
        )
    }

    fn gen_nix_like_narinfo(path_info: &PathInfo) -> String {
        let mut narinfo = path_info.to_narinfo();

        let url = format!("nar/{}.nar", nixbase32::encode(&path_info.nar_sha256));
        narinfo.url = &url;
        narinfo.to_string()
    }

    /// HEAD and GET for a NARInfo for which there's no PathInfo should fail.
    /// Same for the listing endpoint.
    #[traced_test]
    #[tokio::test]
    async fn test_get_head_not_found() {
        let (server, _blob_service, _directory_service, _path_info_service) =
            gen_server(crate::gen_router(100));

        let narinfo_url = &format!("{}.narinfo", nixbase32::encode(&DUMMY_PATH_DIGEST));
        server
            .method(Method::HEAD, narinfo_url)
            .expect_failure()
            .await
            .assert_status_not_found();

        server
            .get(narinfo_url)
            .expect_failure()
            .await
            .assert_status_not_found();

        let listing_url = &format!("{}.ls", nixbase32::encode(&DUMMY_PATH_DIGEST));
        server
            .method(Method::HEAD, listing_url)
            .expect_failure()
            .await
            .assert_status_not_found();
        server
            .get(listing_url)
            .expect_failure()
            .await
            .assert_status_not_found();
    }

    /// HEAD and GET for a NARInfo for which there's a PathInfo stored succeeds.
    /// Same for the listing endpoint.
    #[traced_test]
    #[tokio::test]
    async fn test_get_head_found() {
        let (server, _blob_service, _directory_service, path_info_service) =
            gen_server(crate::gen_router(100));

        let narinfo_url = &format!("{}.narinfo", nixbase32::encode(&DUMMY_PATH_DIGEST));
        path_info_service
            .put(PATH_INFO_SYMLINK.clone())
            .await
            .expect("put pathinfo");

        server
            .method(Method::HEAD, narinfo_url)
            .expect_success()
            .await
            .assert_status_ok();

        // Compare NARInfo
        let narinfo_bytes = server.get(narinfo_url).expect_success().await.into_bytes();
        assert_eq!(
            super::gen_narinfo_str(&PATH_INFO_SYMLINK),
            narinfo_bytes,
            "expect NARInfo to match"
        );

        let listing_url = &format!("{}.ls", nixbase32::encode(&DUMMY_PATH_DIGEST));
        server
            .method(Method::HEAD, listing_url)
            .expect_success()
            .await
            .assert_status_ok();

        // Compare listing
        let listing_bytes = server.get(listing_url).expect_success().await.into_bytes();
        assert_eq!(
            r#"{"root":{"target":"/nix/store/somewhereelse","type":"symlink"},"version":1}"#,
            listing_bytes,
            "expect listing to match"
        );
    }

    /// Uploading a NARInfo without the NAR previously uploaded should fail.
    #[traced_test]
    #[tokio::test]
    async fn test_put_without_prev_nar_fail() {
        let (server, _blob_service, _directory_service, _path_info_service) =
            gen_server(crate::gen_router(100));

        // Produce a NARInfo the same way nix does.
        // FUTUREWORK: add tests for NARInfo with unsupported formats
        // (again referring with compression for example)
        let narinfo_str = gen_nix_like_narinfo(&PATH_INFO_SYMLINK);

        server
            .put(&format!(
                "{}.narinfo",
                nixbase32::encode(&PATH_INFO_SYMLINK.nar_sha256)
            ))
            .text(narinfo_str)
            .content_type(nix_compat::nix_http::MIME_TYPE_NARINFO)
            .expect_failure()
            .await;
    }

    // Upload a NAR, then a PathInfo referring to that upload.
    #[traced_test]
    #[tokio::test]
    async fn test_upload_nar_then_narinfo() {
        let (server, _blob_service, _directory_service, _path_info_service) =
            gen_server(crate::gen_router(100));

        // upload NAR
        server
            .put(&format!(
                "/nar/{}.nar",
                nixbase32::encode(&PATH_INFO_SYMLINK.nar_sha256)
            ))
            .bytes(NAR_CONTENTS_SYMLINK[..].into())
            .expect_success()
            .await;

        let narinfo_str = gen_nix_like_narinfo(&PATH_INFO_SYMLINK);

        // upload NARInfo
        server
            .put(&format!(
                "/{}.narinfo",
                nixbase32::encode(PATH_INFO_SYMLINK.store_path.digest())
            ))
            .text(narinfo_str)
            .content_type(nix_compat::nix_http::MIME_TYPE_NARINFO)
            .expect_success()
            .await;
    }
}
