use athene_core::events::{Engine, Event};
use axum::{
    extract::State,
    response::{
        sse::{Event as SseEvent, KeepAlive, Sse},
        IntoResponse,
    },
    routing::get,
    Router,
};
use futures::StreamExt;
use std::sync::Arc;
use tokio_stream::wrappers::BroadcastStream;

pub fn events_router(engine: Arc<Engine>) -> Router {
    Router::new().route("/", get(sse_handler)).with_state(engine)
}

async fn sse_handler(State(engine): State<Arc<Engine>>) -> impl IntoResponse {
    let stream = BroadcastStream::new(engine.subscribe()).filter_map(|r| async move {
        let data = event_to_json(&r.ok()?)?;
        Some(Ok::<_, std::convert::Infallible>(SseEvent::default().data(data)))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn event_to_json(event: &Event) -> Option<String> {
    let v = match event {
        Event::OrchestratorSpawned(o) => serde_json::json!({"type": "orchestrator_spawned", "payload": o}),
        Event::SessionUpdated(s) => serde_json::json!({"type": "session_updated", "payload": s}),
        Event::SessionSpawned(s) => serde_json::json!({"type": "worker_spawned", "payload": s}),
        Event::SessionDone(id) => {
            serde_json::json!({"type": "worker_done", "payload": {"session_id": id}})
        }
        Event::CiUpdated { pr_id, status } => {
            serde_json::json!({"type": "ci_update", "payload": {"pr_id": pr_id, "status": status}})
        }
        Event::PrOpened { session_id, pr } => {
            serde_json::json!({"type": "pr_event", "payload": {"session_id": session_id, "pr": pr}})
        }
        Event::ReviewComment { pr_id, comment } => {
            serde_json::json!({"type": "review_comment", "payload": {"pr_id": pr_id, "comment": comment}})
        }
        Event::Notification(n) => serde_json::json!({"type": "notification", "payload": n}),
        Event::TerminalOutput { .. } => return None,
    };
    serde_json::to_string(&v).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use athene_core::events::Event;

    #[test]
    fn session_done_format() {
        let line = event_to_json(&Event::SessionDone("s1".into())).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["type"], "worker_done");
        assert_eq!(v["payload"]["session_id"], "s1");
    }

    #[test]
    fn terminal_output_skipped() {
        let event = Event::TerminalOutput {
            session_id: "s1".into(),
            bytes: b"hello".to_vec(),
        };
        assert!(event_to_json(&event).is_none());
    }
}
