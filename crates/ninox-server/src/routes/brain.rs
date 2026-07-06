use ninox_core::{embeddings::Embedder, BrainEntry, BrainIndex, QueryFilters};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone)]
pub struct BrainState {
    pub brain: Arc<BrainIndex>,
    pub embedder: Option<Arc<dyn Embedder>>,
}

pub fn brain_router(brain: Arc<BrainIndex>, embedder: Option<Arc<dyn Embedder>>) -> Router {
    Router::new()
        .route("/index", post(rebuild_index))
        .route("/query", get(query_entries))
        .route("/entry/*path", get(get_entry))
        .with_state(BrainState { brain, embedder })
}

#[derive(Serialize)]
struct IndexResponse {
    count: usize,
    embedded: usize,
    cached: usize,
}

async fn rebuild_index(
    State(state): State<BrainState>,
) -> Result<Json<IndexResponse>, StatusCode> {
    match state.brain.rebuild(state.embedder.as_deref()) {
        Ok(stats) => Ok(Json(IndexResponse {
            count: stats.indexed,
            embedded: stats.embedded,
            cached: stats.cached,
        })),
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
    State(state): State<BrainState>,
    Query(params): Query<QueryParams>,
) -> Result<Json<Vec<BrainEntry>>, StatusCode> {
    let text = params.q.unwrap_or_default();
    let filters = QueryFilters {
        entry_type: params.entry_type,
        tag: params.tag,
    };
    match state.brain.query(&text, state.embedder.as_deref(), filters) {
        Ok(entries) => Ok(Json(entries)),
        Err(err) => {
            tracing::error!("brain query: {err}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn get_entry(
    State(state): State<BrainState>,
    Path(path): Path<String>,
) -> Result<Json<BrainEntry>, StatusCode> {
    match state.brain.get(&path) {
        Ok(Some(entry)) => Ok(Json(entry)),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(err) => {
            tracing::error!("brain get {path}: {err}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}
