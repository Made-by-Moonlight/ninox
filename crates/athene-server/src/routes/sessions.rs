use athene_core::{events::Engine, types::Session};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use std::sync::Arc;

pub fn sessions_router(engine: Arc<Engine>) -> Router {
    Router::new()
        .route("/", get(list_sessions))
        .route("/:id", axum::routing::delete(terminate_session))
        .with_state(engine)
}

async fn list_sessions(State(e): State<Arc<Engine>>) -> Result<Json<Vec<Session>>, StatusCode> {
    e.store
        .list_sessions()
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn terminate_session(
    State(_e): State<Arc<Engine>>,
    Path(_id): Path<String>,
) -> StatusCode {
    StatusCode::NO_CONTENT
}

#[cfg(test)]
mod tests {
    use super::*;
    use athene_core::{events::Engine, store::Store, types::*};
    use axum::body::Body;
    use http::{Request, StatusCode};
    use std::sync::Arc;
    use tempfile::tempdir;
    use tower::ServiceExt;

    fn test_engine() -> Arc<Engine> {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.db");
        let store = Arc::new(Store::open(&path).unwrap());
        // keep dir alive so the temp directory isn't removed before the test ends
        std::mem::forget(dir);
        Engine::new(store)
    }

    #[tokio::test]
    async fn list_empty() {
        let app = sessions_router(test_engine());
        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let sessions: Vec<Session> = serde_json::from_slice(&body).unwrap();
        assert!(sessions.is_empty());
    }

    #[tokio::test]
    async fn list_returns_stored() {
        let engine = test_engine();
        engine
            .store
            .upsert_session(&Session {
                id: "s1".into(),
                orchestrator_id: None,
                name: "w".into(),
                repo: "r".into(),
                status: SessionStatus::Working,
                agent_type: "c".into(),
                cost_usd: 0.0,
                started_at: 0,
                pr_number: None,
                pr_id: None,
                workspace_path: None,
                pid: None,
            })
            .unwrap();
        let app = sessions_router(engine);
        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let sessions: Vec<Session> = serde_json::from_slice(&body).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "s1");
    }
}
