use athene_core::{events::Engine, types::Orchestrator};
use axum::{extract::State, http::StatusCode, routing::get, Json, Router};
use std::sync::Arc;

pub fn orchestrators_router(engine: Arc<Engine>) -> Router {
    Router::new()
        .route("/", get(list))
        .with_state(engine)
}

async fn list(State(e): State<Arc<Engine>>) -> Result<Json<Vec<Orchestrator>>, StatusCode> {
    e.store
        .list_orchestrators()
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}
