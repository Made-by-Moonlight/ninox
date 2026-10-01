//! Wire protocol between `ninox-ptyd` and its clients (the engine, the TUI,
//! `ninox pane attach` bridges).
//!
//! Transport: a Unix stream socket. Each frame is
//! `u32 LE frame_len | u32 LE header_len | header JSON | payload bytes`,
//! where `frame_len` covers everything after itself. The header is JSON so
//! the protocol stays debuggable; raw terminal bytes ride in the payload so
//! they are never base64/number-array encoded.
//!
//! Bump [`PROTOCOL_VERSION`] on any incompatible change. The host is meant to
//! be upgraded rarely, so additive changes (new optional fields, new request
//! variants) are preferred over breaking ones.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

/// Upper bound on a single frame; a screen snapshot of a 500x200 pane with
/// styles is well under this.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Stable pane identifier. Ninox uses the session id.
pub type PaneId = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClientFrame {
    /// Correlates the reply. Clients pick monotonically increasing ids.
    pub id: u64,
    pub request: Request,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Must be the first request on every connection.
    Hello { version: u32, client: String },
    Spawn(SpawnSpec),
    /// SIGHUP then SIGKILL the pane's process group and forget the pane.
    /// Idempotent: killing an unknown pane is `Ok`.
    Kill { pane: PaneId },
    /// Frame payload = bytes to write to the pane's PTY, verbatim.
    Write { pane: PaneId },
    /// Write `text` as user input, wrapped in bracketed paste when the pane
    /// has it enabled, followed by `\r` when `enter`. One ordered submission.
    Submit { pane: PaneId, text: String, enter: bool },
    Resize { pane: PaneId, cols: u16, rows: u16 },
    List,
    Info { pane: PaneId },
    /// Visible screen plus up to `scrollback` lines of history above it.
    Screen { pane: PaneId, scrollback: usize },
    /// History as ANSI bytes in the reply payload, lines `start..end`
    /// relative to the top of the visible screen (negative = scrollback),
    /// matching tmux `capture-pane -S start -E end -e` semantics.
    History { pane: PaneId, start: i64, end: i64 },
    /// Turns this connection into an event stream for `pane` (the reply is
    /// `Ok`, then only `HostFrame::Event`s follow). A client that needs
    /// requests and events opens two connections.
    Subscribe { pane: PaneId, mode: SubscribeMode },
    /// Stop the host and every pane. Used by tests and `ninox ptyd stop`.
    Shutdown,
    /// Live upgrade: sent by a successor host (`run_host_takeover`). The
    /// reply is `Ok`, after which the connection leaves this protocol and
    /// carries the pane manifest and PTY master fds (see `handoff`). Not for
    /// ordinary clients.
    Handoff,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubscribeMode {
    /// First event is `Output` carrying a full ANSI repaint of the current
    /// screen (see `ScreenSnapshot::to_ansi`), then raw PTY output as it
    /// arrives. For bridges into a real terminal (`ninox pane attach`).
    Raw,
    /// `ScreenChanged` notifications, coalesced (at most one per ~16ms).
    /// For compositing clients that pull `Screen` on change.
    Frames,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SpawnSpec {
    pub pane: PaneId,
    pub argv: Vec<String>,
    pub cwd: String,
    /// Added on top of the host's environment after `env_remove`.
    pub env: Vec<(String, String)>,
    /// Removed from the inherited environment (e.g. `CLAUDECODE`) so a pane
    /// never believes it is a child of whatever agent started the host.
    #[serde(default)]
    pub env_remove: Vec<String>,
    pub cols: u16,
    pub rows: u16,
}

/// Tagged with `"frame"`, not `"type"`: the wrapped [`Event`] is itself
/// internally tagged with `"type"`, and the two tags would collide.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum HostFrame {
    Reply { id: u64, result: ReplyResult },
    Event(Event),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ReplyResult {
    Ok(Reply),
    Err(ErrorBody),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    Ok,
    Hello { version: u32, host_pid: u32, epoch_ms: u64 },
    Spawned { pid: u32 },
    Panes { panes: Vec<PaneInfo> },
    Info { pane: PaneInfo },
    Screen { screen: ScreenSnapshot },
    /// ANSI bytes are in the frame payload.
    History,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NotFound,
    AlreadyExists,
    VersionMismatch,
    SpawnFailed,
    BadRequest,
    Internal,
    /// Transient: the pane's input queue is full, or the host is handing
    /// off to a successor. Retry shortly (after a handoff, on a new
    /// connection).
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Raw-mode output; bytes are in the frame payload.
    Output { pane: PaneId },
    /// Frames-mode change notification; `seq` is the pane's screen sequence.
    ScreenChanged { pane: PaneId, seq: u64 },
    /// The pane's process exited. The pane stays listed (alive = false) with
    /// its final screen until killed.
    Exited { pane: PaneId, code: Option<i32> },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PaneInfo {
    pub pane: PaneId,
    pub pid: u32,
    pub cols: u16,
    pub rows: u16,
    pub alive: bool,
    pub exit_code: Option<i32>,
    pub created_ms: u64,
    pub last_output_ms: u64,
    /// OSC 0/2 window title, if the application set one.
    pub title: Option<String>,
    pub cwd: String,
    /// Monotonic screen sequence; bumps on every change to the grid.
    pub seq: u64,
    /// Lines of scrollback above the visible screen (tmux `#{history_size}`).
    /// `History { start: -(history_size as i64), .. }` reaches the oldest.
    #[serde(default)]
    pub history_size: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_frame_kind_round_trips() {
        let frames = [
            HostFrame::Event(Event::Output { pane: "p".into() }),
            HostFrame::Event(Event::ScreenChanged { pane: "p".into(), seq: 3 }),
            HostFrame::Event(Event::Exited { pane: "p".into(), code: Some(1) }),
            HostFrame::Reply { id: 1, result: ReplyResult::Ok(Reply::Hello { version: 1, host_pid: 2, epoch_ms: 3 }) },
            HostFrame::Reply {
                id: 2,
                result: ReplyResult::Err(ErrorBody { code: ErrorCode::NotFound, message: "x".into() }),
            },
        ];
        for f in frames {
            let json = serde_json::to_string(&f).unwrap();
            assert_eq!(serde_json::from_str::<HostFrame>(&json).unwrap(), f, "{json}");
        }
        let req = ClientFrame {
            id: 4,
            request: Request::Spawn(SpawnSpec {
                pane: "p".into(),
                argv: vec!["sh".into()],
                cwd: "/".into(),
                env: vec![("A".into(), "b".into())],
                env_remove: vec![],
                cols: 80,
                rows: 24,
            }),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(serde_json::from_str::<ClientFrame>(&json).unwrap(), req);
    }

    #[test]
    fn pane_info_history_size_is_optional_on_the_wire() {
        let json = r#"{"pane":"p","pid":1,"cols":80,"rows":24,"alive":true,"exit_code":null,
            "created_ms":0,"last_output_ms":0,"title":null,"cwd":"/","seq":0}"#;
        let info: PaneInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.history_size, 0);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScreenSnapshot {
    pub cols: u16,
    pub rows: u16,
    pub seq: u64,
    /// Scrollback lines (oldest first) followed by exactly `rows` visible lines.
    pub lines: Vec<Line>,
    /// How many of `lines` are scrollback above the visible screen.
    pub scrollback_len: usize,
    pub cursor: Cursor,
    pub modes: Modes,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Line {
    /// Consecutive cells with identical style merged into runs. Wide chars
    /// occupy two columns; their spacer cell is omitted.
    pub runs: Vec<Run>,
    /// The line soft-wraps into the next one.
    #[serde(default)]
    pub wrapped: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Run {
    pub text: String,
    #[serde(default)]
    pub style: Style,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct Style {
    #[serde(default)]
    pub fg: Color,
    #[serde(default)]
    pub bg: Color,
    #[serde(default)]
    pub bold: bool,
    #[serde(default)]
    pub dim: bool,
    #[serde(default)]
    pub italic: bool,
    #[serde(default)]
    pub underline: bool,
    #[serde(default)]
    pub inverse: bool,
    #[serde(default)]
    pub strikethrough: bool,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(tag = "t", content = "v", rename_all = "snake_case")]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cursor {
    pub row: u16,
    pub col: u16,
    pub visible: bool,
}

/// Terminal modes a client needs to encode input correctly.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Modes {
    pub alt_screen: bool,
    pub app_cursor: bool,
    pub bracketed_paste: bool,
    /// Any mouse reporting mode (1000/1002/1003) is on.
    pub mouse_reporting: bool,
    /// SGR mouse encoding (1006).
    pub sgr_mouse: bool,
    /// Kitty keyboard protocol flags pushed by the application (0 = off).
    pub kitty_keyboard: u8,
}
