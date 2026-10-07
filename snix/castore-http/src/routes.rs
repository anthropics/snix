use crate::app_state::AppState;
use crate::get_root_node_contents;

use snix_castore::PathBuf;

use axum::{
    extract::{self, State},
    http::StatusCode,
    response::Response,
};
use axum_extra::{TypedHeader, headers::Range};
use std::path;
use tracing::{debug, instrument};

#[instrument(level = "trace", ret, skip_all, fields(maybe_path))]
pub async fn root_node_contents(
    maybe_path: Option<extract::Path<String>>,
    state: State<AppState>,
    range_header: Option<TypedHeader<Range>>,
) -> Result<Response, StatusCode> {
    let requested_path = maybe_path
        .map(|extract::Path(path)| PathBuf::from_host_path(path::Path::new(&path), true))
        .transpose()
        .map_err(|err| {
            debug!(%err, "User requested an invalid path");
            StatusCode::BAD_REQUEST
        })?;
    let requested_path = match requested_path.as_ref() {
        Some(p) => p.as_ref(),
        None => &PathBuf::new(),
    };

    get_root_node_contents(
        state.blob_engine.clone(),
        state.directory_service.clone(),
        path::Path::new("/"),
        &state.root_node,
        requested_path,
        range_header,
        &state.index_names,
        state.auto_index,
    )
    .await
}

#[cfg(test)]
mod tests {
    use crate::{app_state::AppConfig, router::gen_router};

    use snix_castore::{
        B3Digest, Directory, Node,
        blob_engine::BlobEngine,
        directoryservice::DirectoryService,
        fixtures::{DIRECTORY_COMPLICATED, HELLOWORLD_BLOB_CONTENTS, HELLOWORLD_BLOB_DIGEST},
        utils::{gen_test_blob_engine, gen_test_directory_service},
    };

    use axum::http::StatusCode;
    use std::io::Cursor;
    use std::sync::{Arc, LazyLock};
    use tracing_test::traced_test;

    /// Accepts a root node to be served, and returns a [axum_test::TestServer], as well as the {Blob,Directory}Service.
    fn gen_server<S: AsRef<str>>(
        root_node: Node,
        index_names: &[S],
        auto_index: bool,
    ) -> (
        axum_test::TestServer,
        impl BlobEngine + use<S>,
        impl DirectoryService + use<S>,
    ) {
        let blob_engine = Arc::new(gen_test_blob_engine());
        let directory_service = Arc::new(gen_test_directory_service());

        let app = gen_router(Arc::new(AppConfig {
            blob_engine: blob_engine.clone(),
            directory_service: directory_service.clone(),
            root_node,
            index_names: index_names
                .iter()
                .map(|index| index.as_ref().to_string())
                .collect(),
            auto_index,
        }))
        .layer(axum_tracing_opentelemetry::middleware::OtelAxumLayer::default());

        (
            axum_test::TestServer::new(app),
            blob_engine,
            directory_service,
        )
    }

    pub const INDEX_HTML_BLOB_CONTENTS: &[u8] =
        b"<!DOCTYPE html><html><body>Hello World!</body></html>";
    pub static INDEX_HTML_BLOB_DIGEST: LazyLock<B3Digest> =
        LazyLock::new(|| blake3::hash(INDEX_HTML_BLOB_CONTENTS).as_bytes().into());

    pub static DIRECTORY_NESTED_WITH_SYMLINK: LazyLock<Directory> = LazyLock::new(|| {
        Directory::try_from_iter([
            (
                "nested".try_into().unwrap(),
                Node::Directory {
                    digest: DIRECTORY_WITH_SYMLINK.digest(),
                    size: DIRECTORY_WITH_SYMLINK.size(),
                },
            ),
            (
                "index.htm".try_into().unwrap(),
                Node::File {
                    digest: *INDEX_HTML_BLOB_DIGEST,
                    size: INDEX_HTML_BLOB_CONTENTS.len() as u64,
                    executable: false,
                },
            ),
            (
                "out_of_base_path_symlink".try_into().unwrap(),
                Node::Symlink {
                    target: "../index.htm".try_into().unwrap(),
                },
            ),
        ])
        .unwrap()
    });

    pub static DIRECTORY_WITH_SYMLINK: LazyLock<Directory> = LazyLock::new(|| {
        Directory::try_from_iter([
            (
                "index.html".try_into().unwrap(),
                Node::File {
                    digest: *INDEX_HTML_BLOB_DIGEST,
                    size: INDEX_HTML_BLOB_CONTENTS.len() as u64,
                    executable: false,
                },
            ),
            (
                "dot".try_into().unwrap(),
                Node::Symlink {
                    target: ".".try_into().unwrap(),
                },
            ),
            (
                "symlink".try_into().unwrap(),
                Node::Symlink {
                    target: "index.html".try_into().unwrap(),
                },
            ),
            (
                "dot_symlink".try_into().unwrap(),
                Node::Symlink {
                    target: "./index.html".try_into().unwrap(),
                },
            ),
            (
                "dotdot_symlink".try_into().unwrap(),
                Node::Symlink {
                    target: "../index.htm".try_into().unwrap(),
                },
            ),
            (
                "dotdot_same_symlink".try_into().unwrap(),
                Node::Symlink {
                    target: "../nested/index.html".try_into().unwrap(),
                },
            ),
        ])
        .unwrap()
    });

    #[traced_test]
    #[tokio::test]
    async fn test_lists_directory_contents_if_auto_index_enabled() {
        let root_node = Node::Directory {
            digest: DIRECTORY_COMPLICATED.digest(),
            size: DIRECTORY_COMPLICATED.size(),
        };

        // No index but auto-index is enabled
        let (server, _blob_service, directory_service) = gen_server::<&str>(root_node, &[], true);

        directory_service
            .put(DIRECTORY_COMPLICATED.clone())
            .await
            .expect("Failed to insert directory");

        server
            .get("/")
            .expect_success()
            .await
            .assert_text_contains("<html><body><li><a href=\"/.keep\">.keep</a></li><li><a href=\"/aa\">aa</a></li><li><a href=\"/keep\">keep</a></li></body></html>");
    }

    #[traced_test]
    #[tokio::test]
    async fn test_lists_directory_contents_if_auto_index_enabled_for_nested_dir() {
        let root_node = Node::Directory {
            digest: DIRECTORY_NESTED_WITH_SYMLINK.digest(),
            size: DIRECTORY_NESTED_WITH_SYMLINK.size(),
        };

        // No index but auto-index is enabled
        let (server, _blob_service, directory_service) = gen_server::<&str>(root_node, &[], true);
        let mut directory_service_handle = directory_service.put_multiple_start();
        directory_service_handle
            .put(DIRECTORY_WITH_SYMLINK.clone())
            .await
            .expect("Failed to insert directory");
        directory_service_handle
            .put(DIRECTORY_NESTED_WITH_SYMLINK.clone())
            .await
            .expect("Failed to insert directory");
        directory_service_handle
            .close()
            .await
            .expect("Failed to close handle");

        server
            .get("/nested")
            .expect_success()
            .await
            .assert_text_contains("<!DOCTYPE html><html><body><li><a href=\"/nested/dot\">dot</a></li><li><a href=\"/nested/dot_symlink\">dot_symlink</a></li><li><a href=\"/nested/dotdot_same_symlink\">dotdot_same_symlink</a></li><li><a href=\"/nested/dotdot_symlink\">dotdot_symlink</a></li><li><a href=\"/nested/index.html\">index.html</a></li><li><a href=\"/nested/symlink\">symlink</a></li></body></html>");
    }

    #[traced_test]
    #[tokio::test]
    async fn test_responds_index_file_if_configured() {
        let root_node = Node::Directory {
            digest: DIRECTORY_COMPLICATED.digest(),
            size: DIRECTORY_COMPLICATED.size(),
        };

        // .keep is a index file in this test scenario, auto-index is off
        let (server, blob_service, directory_service) =
            gen_server::<&str>(root_node, &[".keep"], false);

        directory_service
            .put(DIRECTORY_COMPLICATED.clone())
            .await
            .expect("Failed to insert directory");

        let mut blob_writer = blob_service.open_write().await;
        tokio::io::copy(&mut Cursor::new(vec![]), &mut blob_writer)
            .await
            .expect("Failed to copy file to BlobWriter");
        blob_writer
            .close()
            .await
            .expect("Failed to close the BlobWriter");

        server.get("/").expect_success().await;
    }

    #[traced_test]
    #[tokio::test]
    async fn test_responds_index_file_if_configured_in_nested_dir() {
        let root_node = Node::Directory {
            digest: DIRECTORY_NESTED_WITH_SYMLINK.digest(),
            size: DIRECTORY_NESTED_WITH_SYMLINK.size(),
        };

        // .keep is a index file in this test scenario, auto-index is off
        let (server, blob_service, directory_service) =
            gen_server::<&str>(root_node, &["index.html"], false);

        let mut directory_service_handle = directory_service.put_multiple_start();
        directory_service_handle
            .put(DIRECTORY_WITH_SYMLINK.clone())
            .await
            .expect("Failed to insert directory");
        directory_service_handle
            .put(DIRECTORY_NESTED_WITH_SYMLINK.clone())
            .await
            .expect("Failed to insert directory");
        directory_service_handle
            .close()
            .await
            .expect("Failed to close handle");

        let mut blob_writer = blob_service.open_write().await;
        tokio::io::copy(&mut Cursor::new(INDEX_HTML_BLOB_CONTENTS), &mut blob_writer)
            .await
            .expect("Failed to copy file to BlobWriter");
        let digest = blob_writer
            .close()
            .await
            .expect("Failed to close the BlobWriter");
        assert_eq!(digest, *INDEX_HTML_BLOB_DIGEST);

        server.get("/nested").expect_success().await;
    }

    #[traced_test]
    #[tokio::test]
    async fn test_responds_forbidden_if_no_index_configured_nor_auto_index_enabled() {
        let root_node = Node::Directory {
            digest: DIRECTORY_COMPLICATED.digest(),
            size: DIRECTORY_COMPLICATED.size(),
        };

        // no index configured and auto-index disabled
        let (server, _blob_service, directory_service) = gen_server::<&str>(root_node, &[], false);

        directory_service
            .put(DIRECTORY_COMPLICATED.clone())
            .await
            .expect("Failed to insert directory");

        let response = server.get("/").expect_failure().await;
        response.assert_status(StatusCode::FORBIDDEN);
    }

    #[traced_test]
    #[tokio::test]
    async fn test_responds_file() {
        let root_node = Node::Directory {
            digest: DIRECTORY_COMPLICATED.digest(),
            size: DIRECTORY_COMPLICATED.size(),
        };

        let (server, blob_service, directory_service) = gen_server::<&str>(root_node, &[], false);

        directory_service
            .put(DIRECTORY_COMPLICATED.clone())
            .await
            .expect("Failed to insert directory");

        let mut blob_writer = blob_service.open_write().await;
        tokio::io::copy(&mut Cursor::new(vec![]), &mut blob_writer)
            .await
            .expect("Failed to copy file to BlobWriter");
        blob_writer
            .close()
            .await
            .expect("Failed to close the BlobWriter");

        server.get("/.keep").expect_success().await;
    }

    #[traced_test]
    #[tokio::test]
    async fn test_responds_file_and_correct_content_type() {
        let root_node = Node::Directory {
            digest: DIRECTORY_NESTED_WITH_SYMLINK.digest(),
            size: DIRECTORY_NESTED_WITH_SYMLINK.size(),
        };

        let (server, blob_service, directory_service) = gen_server::<&str>(root_node, &[], false);

        let mut directory_service_handle = directory_service.put_multiple_start();
        directory_service_handle
            .put(DIRECTORY_WITH_SYMLINK.clone())
            .await
            .expect("Failed to insert directory");
        directory_service_handle
            .put(DIRECTORY_NESTED_WITH_SYMLINK.clone())
            .await
            .expect("Failed to insert directory");
        directory_service_handle
            .close()
            .await
            .expect("Failed to close handle");

        let mut blob_writer = blob_service.open_write().await;
        tokio::io::copy(&mut Cursor::new(INDEX_HTML_BLOB_CONTENTS), &mut blob_writer)
            .await
            .expect("Failed to copy file to BlobWriter");
        let digest = blob_writer
            .close()
            .await
            .expect("Failed to close the BlobWriter");
        assert_eq!(digest, *INDEX_HTML_BLOB_DIGEST);

        let response = server.get("/nested/index.html").expect_success().await;
        response.assert_header("Content-Type", "text/html");
    }

    #[traced_test]
    #[tokio::test]
    async fn test_responds_redirect_if_symlink() {
        let root_node = Node::Directory {
            digest: DIRECTORY_COMPLICATED.digest(),
            size: DIRECTORY_COMPLICATED.size(),
        };

        let (server, _blob_service, directory_service) = gen_server::<&str>(root_node, &[], false);

        directory_service
            .put(DIRECTORY_COMPLICATED.clone())
            .await
            .expect("Failed to insert directory");

        let response = server.get("/aa").await;
        response.assert_status(StatusCode::TEMPORARY_REDIRECT);
        response.assert_header("Location", "/nix/store/somewhereelse");
    }

    #[traced_test]
    #[tokio::test]
    async fn test_responds_redirect_with_normalized_path_if_symlink() {
        let root_node = Node::Directory {
            digest: DIRECTORY_NESTED_WITH_SYMLINK.digest(),
            size: DIRECTORY_NESTED_WITH_SYMLINK.size(),
        };

        let (server, _blob_service, directory_service) = gen_server::<&str>(root_node, &[], false);

        let mut directory_service_handle = directory_service.put_multiple_start();
        directory_service_handle
            .put(DIRECTORY_WITH_SYMLINK.clone())
            .await
            .expect("Failed to insert directory");
        directory_service_handle
            .put(DIRECTORY_NESTED_WITH_SYMLINK.clone())
            .await
            .expect("Failed to insert directory");
        directory_service_handle
            .close()
            .await
            .expect("Failed to close handle");

        let response = server.get("/nested/symlink").await;
        response.assert_status(StatusCode::TEMPORARY_REDIRECT);
        response.assert_header("Location", "/nested/index.html");

        let response = server.get("/nested/dot_symlink").await;
        response.assert_status(StatusCode::TEMPORARY_REDIRECT);
        response.assert_header("Location", "/nested/index.html");

        let response = server.get("/nested/dotdot_symlink").await;
        response.assert_status(StatusCode::TEMPORARY_REDIRECT);
        response.assert_header("Location", "/index.htm");

        let response = server.get("/out_of_base_path_symlink").await;
        response.assert_status(StatusCode::TEMPORARY_REDIRECT);
        response.assert_header("Location", "/index.htm");

        let response = server.get("/nested/dot").expect_failure().await;
        response.assert_status(StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[traced_test]
    #[tokio::test]
    async fn test_returns_bad_request_if_not_valid_path() {
        let root_node = Node::Directory {
            digest: DIRECTORY_COMPLICATED.digest(),
            size: DIRECTORY_COMPLICATED.size(),
        };

        let (server, _blob_service, _directory_service) = gen_server::<&str>(root_node, &[], false);

        // request an invalid path
        let response = server.get("//aa").expect_failure().await;
        response.assert_status(StatusCode::BAD_REQUEST);
    }

    #[traced_test]
    #[tokio::test]
    async fn test_returns_bad_request_if_root_node_is_file_and_path_requested() {
        let root_node = Node::File {
            digest: *HELLOWORLD_BLOB_DIGEST,
            size: HELLOWORLD_BLOB_CONTENTS.len() as u64,
            executable: false,
        };

        let (server, _blob_service, _directory_service) = gen_server::<&str>(root_node, &[], false);

        // request a path while the root node is a file
        let response = server.get("/some-path").expect_failure().await;
        response.assert_status(StatusCode::BAD_REQUEST);
    }

    #[traced_test]
    #[tokio::test]
    async fn test_returns_bad_request_if_root_node_is_symlink_and_path_requested() {
        let root_node = Node::Symlink {
            target: "/nix/store/somewhereelse".try_into().unwrap(),
        };

        let (server, _blob_service, _directory_service) = gen_server::<&str>(root_node, &[], false);

        // request a path while the root node is a symlink
        let response = server.get("/some-path").expect_failure().await;
        response.assert_status(StatusCode::BAD_REQUEST);
    }
}
