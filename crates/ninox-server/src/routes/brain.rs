use ninox_core::{brain_sync::BrainSync, embeddings::Embedder, BrainEntry, BrainIndex, QueryFilters};
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
    /// Present when the brain is remote-backed; reads freshen via
    /// `pull_if_stale`, `POST /index` runs a full sync.
    pub sync: Option<Arc<BrainSync>>,
}

pub fn brain_router(
    brain: Arc<BrainIndex>,
    embedder: Option<Arc<dyn Embedder>>,
    sync: Option<Arc<BrainSync>>,
) -> Router {
    Router::new()
        .route("/index", post(rebuild_index))
        .route("/query", get(query_entries))
        .route("/entry/*path", get(get_entry))
        .with_state(BrainState { brain, embedder, sync })
}

/// Freshness check before a read (spec §4): pull-if-stale, rebuild on
/// change, degrade to the local copy on any remote failure.
async fn freshen(state: &BrainState) {
    let Some(sync) = &state.sync else { return };
    match sync.pull_if_stale().await {
        Ok(report) if report.changed_local() => {
            if let Err(e) = state.brain.rebuild(state.embedder.as_deref()) {
                tracing::warn!("brain: rebuild after pull failed: {e}");
            }
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("brain: remote check failed, serving local copy: {e}"),
    }
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
    if let Some(sync) = &state.sync {
        if let Err(e) = sync.sync().await {
            tracing::warn!("brain: remote sync failed (continuing with local rebuild): {e}");
        }
    }
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
    freshen(&state).await;
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
    freshen(&state).await;
    match state.brain.get(&path) {
        Ok(Some(entry)) => Ok(Json(entry)),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(err) => {
            tracing::error!("brain get {path}: {err}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Query as AxQuery, State};
    use ninox_core::brain_sync::{
        manifest::{entry_key, sha256_hex, Manifest, ManifestEntry, MANIFEST_KEY},
        BrainSync, InMemoryRemoteStore, RemoteStore, SyncToml,
    };
    use tempfile::tempdir;

    #[tokio::test]
    async fn query_pulls_fresh_remote_entries_before_answering() {
        let dir = tempdir().unwrap();
        let brain = Arc::new(BrainIndex::open(dir.path()).unwrap());
        brain.rebuild(None).unwrap();

        // Remote already has an entry this server has never seen.
        let store = Arc::new(InMemoryRemoteStore::default());
        let body = "---\nname: Remote Note\n---\nWritten by a teammate about widgetronic.";
        let sha = sha256_hex(body.as_bytes());
        store.put(&entry_key("notes/remote.md", &sha), body.as_bytes().to_vec()).await.unwrap();
        let mut manifest = Manifest::empty();
        manifest.generation = 1;
        manifest.entries.insert(
            "notes/remote.md".into(),
            ManifestEntry { sha256: sha, size: body.len() as u64, updated_by: "teammate".into(), updated_at: "2026-07-22T00:00:00Z".into() },
        );
        store.put(MANIFEST_KEY, manifest.to_bytes().unwrap()).await.unwrap();

        let cfg = SyncToml { remote: "s3://b/p".into(), endpoint: None, region: None, cache_ttl_secs: 0 };
        let sync = Arc::new(BrainSync::new(dir.path().to_path_buf(), cfg, store));
        let state = BrainState { brain, embedder: None, sync: Some(sync) };

        let params = QueryParams { q: Some("widgetronic".into()), entry_type: None, tag: None };
        let Json(entries) = query_entries(State(state), AxQuery(params)).await.unwrap();
        assert_eq!(entries.len(), 1, "the freshly-pulled remote entry should be queryable");
        assert_eq!(entries[0].id, "notes/remote.md");
    }

    #[tokio::test]
    async fn query_serves_local_when_remote_is_down() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("local.md"), "---\nname: Local\n---\nAbout widgetronic.").unwrap();
        let brain = Arc::new(BrainIndex::open(dir.path()).unwrap());
        brain.rebuild(None).unwrap();

        let store = Arc::new(InMemoryRemoteStore::default());
        store.fail_all.store(true, std::sync::atomic::Ordering::SeqCst);
        let cfg = SyncToml { remote: "s3://b/p".into(), endpoint: None, region: None, cache_ttl_secs: 0 };
        let sync = Arc::new(BrainSync::new(dir.path().to_path_buf(), cfg, store));
        let state = BrainState { brain, embedder: None, sync: Some(sync) };

        let params = QueryParams { q: Some("widgetronic".into()), entry_type: None, tag: None };
        let Json(entries) = query_entries(State(state), AxQuery(params)).await.unwrap();
        assert_eq!(entries.len(), 1, "a dead remote must never fail a query");
    }
}
