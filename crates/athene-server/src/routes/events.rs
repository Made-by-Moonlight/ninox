// Stub — will be implemented in Task 7 (SSE event stream).
use axum::{http::StatusCode, routing::get, Router};

pub fn events_router() -> Router {
    Router::new().route("/", get(|| async { StatusCode::NOT_IMPLEMENTED }))
}
