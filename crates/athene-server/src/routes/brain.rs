use athene_core::{BrainEntry, BrainIndex, QueryFilters};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub fn brain_router(brain: Arc<BrainIndex>) -> Router {
    Router::new()
        .route("/index", post(rebuild_index))
        .route("/query", get(query_entries))
        .route("/entry/*path", get(get_entry))
        .with_state(brain)
}

#[derive(Serialize)]
struct IndexResponse {
    count: usize,
}

async fn rebuild_index(
    State(brain): State<Arc<BrainIndex>>,
) -> Result<Json<IndexResponse>, StatusCode> {
    match brain.rebuild() {
        Ok(count) => Ok(Json(IndexResponse { count })),
        Err(err) => {
            tracing::error!("brain rebuild: {err}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

#[derive(Deserialize)]
struct QueryParams {
    q: Option<String>,
    #[serde(rename = "type")]
    entry_type: Option<String>,
    tag: Option<String>,
}

async fn query_entries(
    State(brain): State<Arc<BrainIndex>>,
    Query(params): Query<QueryParams>,
) -> Result<Json<Vec<BrainEntry>>, StatusCode> {
    let text = params.q.unwrap_or_default();
    let filters = QueryFilters {
        entry_type: params.entry_type,
        tag: params.tag,
    };
    match brain.query(&text, filters) {
        Ok(entries) => Ok(Json(entries)),
        Err(err) => {
            tracing::error!("brain query: {err}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn get_entry(
    State(brain): State<Arc<BrainIndex>>,
    Path(path): Path<String>,
) -> Result<Json<BrainEntry>, StatusCode> {
    match brain.get(&path) {
        Ok(Some(entry)) => Ok(Json(entry)),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(err) => {
            tracing::error!("brain get {path}: {err}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}
