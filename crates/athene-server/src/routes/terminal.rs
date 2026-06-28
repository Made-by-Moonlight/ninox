use athene_core::events::{Engine, Event};
use axum::{
    extract::{
        ws::{Message, WebSocket},
        Path, State, WebSocketUpgrade,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use futures::{SinkExt, StreamExt};
use std::sync::Arc;

pub fn terminal_router(engine: Arc<Engine>) -> Router {
    Router::new()
        .route("/:id/terminal", get(ws_handler))
        .with_state(engine)
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Path(session_id): Path<String>,
    State(engine): State<Arc<Engine>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle(socket, session_id, engine))
}

async fn handle(socket: WebSocket, session_id: String, engine: Arc<Engine>) {
    let (mut tx, mut rx) = socket.split();

    // Browser → PTY: forward binary frames to the PTY writer if one exists
    let e2 = engine.clone();
    let sid = session_id.clone();
    tokio::spawn(async move {
        while let Some(Ok(Message::Binary(b))) = rx.next().await {
            if let Some(w) = e2.get_pty_writer(&sid).await {
                let _ = w.send(b.to_vec());
            }
            // If no PTY writer, drop the message silently
        }
    });

    // PTY → Browser: subscribe to engine events and forward TerminalOutput for this session
    let mut event_rx = engine.subscribe();
    loop {
        match event_rx.recv().await {
            Ok(Event::TerminalOutput { session_id: sid, bytes }) if sid == session_id => {
                if tx.send(Message::Binary(bytes.into())).await.is_err() {
                    break;
                }
            }
            Err(_) => break,
            _ => {}
        }
    }
}
