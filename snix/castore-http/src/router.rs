use crate::app_state::AppState;
use crate::routes;
use axum::Router;
use axum::routing::get;

pub fn gen_router(app_state: AppState) -> Router {
    Router::new()
        .route("/{*path}", get(routes::root_node_contents))
        .route("/", get(routes::root_node_contents))
        .with_state(app_state)
}
