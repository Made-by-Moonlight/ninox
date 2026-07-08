use ninox_core::{events::Engine, lifecycle::brain_harvest, types::Session};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use serde::Serialize;
use std::sync::Arc;

pub fn sessions_router(engine: Arc<Engine>) -> Router {
    Router::new()
        .route("/", get(list_sessions))
        .route("/:id", axum::routing::delete(terminate_session))
        .route("/:id/diff", get(session_diff))
        .with_state(engine)
}

async fn list_sessions(State(e): State<Arc<Engine>>) -> Result<Json<Vec<Session>>, StatusCode> {
    e.store
        .list_sessions()
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[derive(Serialize)]
struct DiffResponse {
    /// `None` when the session has no recorded workspace, or its workspace
    /// has no diff against the default branch yet.
    diff: Option<String>,
}

/// The session's current unfiltered diff against its default branch —
/// computed live on every call (no caching), since this is meant to reflect
/// an in-progress session's diff as it changes, not just the one-shot diff
/// snapshotted after a PR opens (see `lifecycle::brain_harvest`).
async fn session_diff(
    State(e): State<Arc<Engine>>,
    Path(id): Path<String>,
) -> Result<Json<DiffResponse>, StatusCode> {
    let session = e
        .store
        .get_session(&id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let Some(workspace) = session.workspace_path else {
        return Ok(Json(DiffResponse { diff: None }));
    };

    let diff = brain_harvest::compute_diff(std::path::Path::new(&workspace)).await;
    Ok(Json(DiffResponse { diff }))
}

async fn terminate_session(
    State(e): State<Arc<Engine>>,
    Path(id): Path<String>,
) -> StatusCode {
    match e.terminate_session(&id).await {
        Ok(_) => StatusCode::NO_CONTENT,
        Err(err) => {
            tracing::error!("terminate {id}: {err}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::{events::Engine, store::Store, types::*};
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
                model: None,
                context_tokens: None,
                catalogue_path: None,
                context_used_pct: None,
                context_total_tokens: None,
                context_window_size: None,
                claude_session_id: None,
                summary: None,
                terminal_at: None,
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

    #[tokio::test]
    async fn diff_returns_404_for_unknown_session() {
        let response = sessions_router(test_engine())
            .oneshot(Request::builder().uri("/nope/diff").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn diff_is_none_without_a_recorded_workspace() {
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
                model: None,
                context_tokens: None,
                catalogue_path: None,
                context_used_pct: None,
                context_total_tokens: None,
                context_window_size: None,
                claude_session_id: None,
                terminal_at: None,
            })
            .unwrap();
        let response = sessions_router(engine)
            .oneshot(Request::builder().uri("/s1/diff").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["diff"].is_null());
    }

    #[tokio::test]
    async fn diff_returns_workspace_diff_including_lockfile_only_changes() {
        // Real git repo — this route must not apply brain-harvest's
        // lockfile-only filtering, unlike `compute_nontrivial_diff`.
        let repo = tempdir().unwrap().keep();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(["-C", repo.to_str().unwrap()])
                .args(args)
                .output()
                .unwrap()
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(repo.join("README.md"), "x").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
        run(&["checkout", "-q", "-b", "feature"]);
        std::fs::write(repo.join("Cargo.lock"), "version = 3\n").unwrap();
        run(&["add", "Cargo.lock"]);
        run(&["commit", "-q", "-m", "bump lockfile"]);

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
                workspace_path: Some(repo.to_str().unwrap().to_string()),
                pid: None,
                model: None,
                context_tokens: None,
                catalogue_path: None,
                context_used_pct: None,
                context_total_tokens: None,
                context_window_size: None,
                claude_session_id: None,
                terminal_at: None,
            })
            .unwrap();
        let response = sessions_router(engine)
            .oneshot(Request::builder().uri("/s1/diff").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["diff"].as_str().unwrap().contains("Cargo.lock"));
    }

    #[tokio::test]
    async fn delete_returns_no_content() {
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
                model: None,
                context_tokens: None,
                catalogue_path: None,
                context_used_pct: None,
                context_total_tokens: None,
                context_window_size: None,
                claude_session_id: None,
                summary: None,
                terminal_at: None,
            })
            .unwrap();
        let response = sessions_router(engine)
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/s1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }
}
