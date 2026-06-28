use crate::routes::{orchestrators::orchestrators_router, sessions::sessions_router};
use athene_core::events::Engine;
use axum::Router;
use std::{net::SocketAddr, sync::Arc};
use tower_http::cors::CorsLayer;

pub async fn start(engine: Arc<Engine>, port: u16) -> anyhow::Result<()> {
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let app = Router::new()
        .nest("/api/v1/sessions", sessions_router(engine.clone()))
        .nest("/api/v1/orchestrators", orchestrators_router(engine.clone()))
        .layer(CorsLayer::permissive());
    tracing::info!("athene listening on {addr}");
    axum::serve(tokio::net::TcpListener::bind(addr).await?, app).await?;
    Ok(())
}
