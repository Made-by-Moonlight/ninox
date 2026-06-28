// Stub — will be implemented in Task 8 (WebSocket terminal).
use axum::{http::StatusCode, routing::get, Router};

pub fn terminal_router() -> Router {
    Router::new().route("/", get(|| async { StatusCode::NOT_IMPLEMENTED }))
}
