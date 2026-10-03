#![cfg_attr(docsrs, feature(doc_cfg))]

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{head, put};
use axum::{Router, routing::get};
use lru::LruCache;
use nix_compat::nix_http;
use parking_lot::RwLock;
use snix_castore::Node;
use snix_castore::blob_engine::BlobEngine;
use snix_castore::directoryservice::DirectoryService;
use snix_store::pathinfoservice::PathInfoService;
use std::num::NonZeroUsize;
use std::sync::Arc;

mod nar;
mod outhash;

#[derive(Clone)]
pub struct AppState {
    blob_engine: Arc<dyn BlobEngine>,
    directory_service: Arc<dyn DirectoryService>,
    path_info_service: Arc<dyn PathInfoService>,

    /// Lookup table from NarHash to [Node], necessary to populate the root_node
    /// field of the PathInfo when processing the narinfo upload.
    root_nodes: Arc<RwLock<LruCache<[u8; 32], Node>>>,
}

impl AppState {
    pub fn new(
        blob_engine: Arc<dyn BlobEngine>,
        directory_service: Arc<dyn DirectoryService>,
        path_info_service: Arc<dyn PathInfoService>,
        root_nodes_cache_capacity: NonZeroUsize,
    ) -> Self {
        Self {
            blob_engine,
            directory_service,
            path_info_service,
            root_nodes: Arc::new(RwLock::new(LruCache::new(root_nodes_cache_capacity))),
        }
    }
}

pub fn gen_router(priority: u64) -> Router<AppState> {
    #[cfg(feature = "otlp")]
    let metrics_meter = opentelemetry::global::meter("nar-bridge");
    #[cfg(feature = "otlp")]
    let metrics_layer = opentelemetry_instrumentation_tower::HTTPMetricsLayerBuilder::builder()
        .with_meter(metrics_meter)
        .build()
        .unwrap();

    let router = Router::new()
        .route("/", get(root))
        .route("/nar/{nar_str}", get(StatusCode::NOT_FOUND))
        .route("/nar/{nar_str}", head(nar::head_root_nodes))
        .route("/nar/{nar_str}", put(nar::put))
        .route("/nar/snix-castore/{root_node_enc}", get(nar::get_head))
        .route("/nar/snix-castore/{root_node_enc}", head(nar::get_head))
        .route("/{outhash_str}", get(outhash::get))
        .route("/{outhash_str}", head(outhash::head))
        .route("/{outhash_str}", put(outhash::put))
        .route("/nix-cache-info", get(move || nix_cache_info(priority)));

    let router = router.layer(tower_http::compression::CompressionLayer::new());

    #[cfg(feature = "otlp")]
    return router.layer(metrics_layer);
    #[cfg(not(feature = "otlp"))]
    return router;
}

async fn root() -> &'static str {
    "Hello from nar-bridge"
}

async fn nix_cache_info(priority: u64) -> impl IntoResponse {
    (
        [("Content-Type", nix_http::MIME_TYPE_CACHE_INFO)],
        format!("StoreDir: /nix/store\nWantMassQuery: 1\nPriority: {priority}\n"),
    )
}
