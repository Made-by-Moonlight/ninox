use crate::routes::{
    brain::brain_router,
    events::events_router,
    orchestrators::orchestrators_router,
    sessions::sessions_router,
    terminal::terminal_router,
};
use ninox_core::{embeddings::Embedder, events::Engine, server_control::ServerInfo, BrainIndex};
use axum::{routing::get, Json, Router};
use std::{net::SocketAddr, sync::Arc};
use tower_http::cors::CorsLayer;

pub async fn start(
    engine: Arc<Engine>,
    brain: Arc<BrainIndex>,
    embedder: Option<Arc<dyn Embedder>>,
    port: u16,
    info: ServerInfo,
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
        .merge(info_router(info, engine.store.clone()))
        .layer(CorsLayer::permissive());
    tracing::info!("ninox listening on {addr}");
    axum::serve(tokio::net::TcpListener::bind(addr).await?, app).await?;
    Ok(())
}

/// `GET /api/server/info`: which build is answering on this port, and its
/// pid, so a restart can find the process without `lsof`.
pub fn info_router(info: ServerInfo, store: Arc<ninox_core::Store>) -> Router {
    Router::new().route("/api/server/info", get(move || {
        let mut issues = ninox_core::server_control::current_issues();
        issues.extend(ninox_core::server_control::harness_auth_issues(&store));
        let info = ServerInfo { issues, ..info.clone() };
        async move { Json(info) }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    #[tokio::test]
    async fn info_reports_this_process() {
        let info = ServerInfo::current(40123, std::path::Path::new("/tmp/x.db"), false);
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(ninox_core::Store::open(dir.path().join("t.db")).unwrap());
        store.record_harness_auth("w1", "claude-code", Some("w1"), "Invalid API key · Please run /login").unwrap();
        let res = info_router(info.clone(), store)
            .oneshot(http::Request::get("/api/server/info").body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let body = axum::body::to_bytes(res.into_body(), 1 << 16).await.unwrap();
        let got: ServerInfo = serde_json::from_slice(&body).unwrap();
        assert_eq!((got.pid, &got.version, &got.build), (std::process::id(), &info.version, &info.build));
        let harness = got.issues.iter().find(|i| i.source == "harness_auth:claude-code").expect("harness issue");
        assert_eq!(harness.sessions, ["w1"]);
    }
}
