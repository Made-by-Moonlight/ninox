//! CLI-side HTTP client for `ninox-server`'s per-session ACP connection
//! route — see `ninox_server::routes::sessions::send_acp_message` and the
//! cross-harness-messaging design spec's "Connection ownership" section. A
//! `ninox send` invocation is a short-lived process that never holds an ACP
//! connection itself; `ninox-server` does, for the session's full lifetime,
//! so a CLI-invoked send becomes this HTTP call instead of an in-process
//! [`crate::runtime::SessionBackend`] call (which is what
//! `events::InProcessAcpRouter` uses instead, for reactions already running
//! inside the process that holds the connection).

use crate::messaging::AcpRouter;
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::time::Duration;

/// Generous for a local loopback call, but not unbounded: a hung
/// `ninox-server` must not hang `ninox send` forever on an unrelated
/// (non-ACP) target just because the daemon happens to be stuck.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// [`AcpRouter`] that asks `ninox-server` (always `127.0.0.1`, per
/// `ninox_server::server::start`) rather than holding any connection
/// itself.
pub struct HttpAcpRouter {
    pub port: u16,
}

#[async_trait]
impl AcpRouter for HttpAcpRouter {
    /// A connection error here means nothing is listening on `port` at
    /// all — and an ACP connection cannot exist without the server process
    /// that holds it (see the design spec), so "no server" is
    /// indistinguishable from "not an ACP session" and gets the same
    /// treatment: fall through, not an error. A `404` means the server IS
    /// up but holds no ACP connection for this id — same treatment. Any
    /// other non-2xx status is a genuine delivery failure against a
    /// connection the server does recognize, and surfaces as `Err` — per
    /// the design spec, "a real error to surface, not something to paper
    /// over."
    async fn send(&self, session_id: &str, message: &str) -> Result<Option<()>> {
        let url = format!("http://127.0.0.1:{}/api/v1/sessions/{session_id}/acp/send", self.port);
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("build http client for acp routing")?;
        let response = match client.post(&url).json(&serde_json::json!({ "text": message })).send().await {
            Ok(response) => response,
            Err(e) if e.is_connect() || e.is_timeout() => return Ok(None),
            Err(e) => return Err(e).context("acp send request failed"),
        };
        if response.status().is_success() {
            return Ok(Some(()));
        }
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("acp send to {session_id} failed ({status}): {body}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Binds an ephemeral loopback listener, accepts exactly one HTTP
    /// request, and replies with `response` verbatim (a full status
    /// line + headers + body) — enough to drive [`HttpAcpRouter::send`]
    /// against a chosen response without a real axum server.
    async fn respond_once(response: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).await;
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.flush().await.unwrap();
        });
        port
    }

    #[tokio::test]
    async fn a_200_response_is_delivered() {
        let port = respond_once("HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n").await;
        let outcome = HttpAcpRouter { port }.send("s1", "hi").await.unwrap();
        assert_eq!(outcome, Some(()));
    }

    #[tokio::test]
    async fn a_404_response_is_not_an_acp_session_not_an_error() {
        let port = respond_once("HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n").await;
        let outcome = HttpAcpRouter { port }.send("s1", "hi").await.unwrap();
        assert_eq!(outcome, None);
    }

    #[tokio::test]
    async fn a_502_response_is_a_real_error_carrying_the_server_s_message() {
        let body = "acp agent process exited";
        let response =
            format!("HTTP/1.1 502 Bad Gateway\r\ncontent-length: {}\r\n\r\n{body}", body.len());
        let port = respond_once(Box::leak(response.into_boxed_str())).await;
        let err = HttpAcpRouter { port }.send("s1", "hi").await.expect_err("a known-session failure must surface");
        assert!(err.to_string().contains(body));
    }

    #[tokio::test]
    async fn no_server_listening_is_not_an_acp_session_not_an_error() {
        // Nothing bound here — a real refused-connection case, the common
        // "daemon isn't running" state for the vast majority of `ninox
        // send` invocations today.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let outcome = HttpAcpRouter { port }.send("s1", "hi").await.unwrap();
        assert_eq!(outcome, None);
    }
}
