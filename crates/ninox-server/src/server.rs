use crate::routes::{
    brain::brain_router,
    events::events_router,
    orchestrators::orchestrators_router,
    sessions::sessions_router,
    terminal::terminal_router,
};
use ninox_core::{embeddings::Embedder, events::Engine, BrainIndex};
use axum::Router;
use std::{net::SocketAddr, sync::Arc};
use tower_http::cors::CorsLayer;

pub async fn start(
    engine: Arc<Engine>,
    brain: Arc<BrainIndex>,
    embedder: Option<Arc<dyn Embedder>>,
    port: u16,
) -> anyhow::Result<()> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let sync = match ninox_core::brain_sync::BrainSync::for_brain(brain.path()).await {
        Ok(Some(s)) => Some(Arc::new(s)),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!("brain: remote unavailable, serving local-only: {e}");
            None
        }
    };
    let app = Router::new()
        .nest("/api/v1/sessions", sessions_router(engine.clone()))
        .nest("/api/v1/sessions", terminal_router(engine.clone()))
        .nest("/api/v1/orchestrators", orchestrators_router(engine.clone()))
        .nest("/api/v1/events", events_router(engine.clone()))
        .nest("/api/brain", brain_router(brain, embedder, sync))
        .layer(CorsLayer::permissive());
    tracing::info!("ninox listening on {addr}");
    axum::serve(tokio::net::TcpListener::bind(addr).await?, app).await?;
    Ok(())
}
