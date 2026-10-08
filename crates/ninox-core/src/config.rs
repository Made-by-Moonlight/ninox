use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
};

use crate::harness::{HarnessRegistry, HarnessSpec};
use crate::worktree::RepositoryIdentity;

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeVariant {
    Light,
    #[default]
    Dark,
    Ninox,
}

// ---------------------------------------------------------------------------
// Editor
// ---------------------------------------------------------------------------

/// Which external editor the "Open in editor" action launches on a worker's
/// workspace directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditorChoice {
    #[default]
    VsCode,
    Cursor,
    Neovim,
}

impl std::fmt::Display for EditorChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            EditorChoice::VsCode => "VS Code",
            EditorChoice::Cursor => "Cursor",
            EditorChoice::Neovim => "Neovim",
        })
    }
}

impl EditorChoice {
    /// All variants, in display order — for the settings dropdown.
    pub const ALL: [EditorChoice; 3] = [EditorChoice::VsCode, EditorChoice::Cursor, EditorChoice::Neovim];
}

// ---------------------------------------------------------------------------
// Agent configuration
// ---------------------------------------------------------------------------

/// Which agent harness and model to use for a session type.
///
/// Example `~/.config/ninox/config.toml`:
/// ```toml
/// [orchestrator]
/// harness = "claude-code"
/// model = "claude-opus-4-5"
///
/// [worker]
/// harness = "codex"
/// model = "gpt-4o"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Agent harness: `"claude-code"`, `"codex"`, `"aider"`, or `"opencode"`.
    #[serde(default = "default_harness")]
    pub harness: String,
    /// Model identifier passed to the harness CLI.
    /// Omit to use the harness default.
    pub model: Option<String>,
}

fn default_harness() -> String {
    "claude-code".to_string()
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self { harness: default_harness(), model: None }
    }
}

impl AgentConfig {
    /// Switch harness, clearing the model: ids from one harness must not
    /// leak into another's launch command. Returns whether it changed.
    pub fn set_harness(&mut self, harness: &str) -> bool {
        if self.harness == harness {
            return false;
        }
        self.harness = harness.to_string();
        self.model = None;
        true
    }
}

// Launch-command construction lives in `crate::harness` — `AgentConfig` is
// only the per-role/per-spawn pointer (harness name + model) into the
// registry; resolve via `AppConfig::registry().interactive_cmd/worker_cmd`.

// ---------------------------------------------------------------------------
// Brain configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BrainConfig {
    pub path: Option<PathBuf>,
    /// Remote backing for the default brain: `s3://bucket/prefix`.
    pub remote: Option<String>,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub cache_ttl_secs: Option<u64>,
    /// Additional named knowledge bases selectable when spawning an
    /// orchestrator. The implicit "default" catalogue (this config's
    /// `resolved_brain_path()`) is always offered first by
    /// `AppConfig::catalogue_options()` and is not duplicated even if an
    /// entry here is also named "default".
    #[serde(default)]
    pub catalogues: Vec<CatalogueRef>,
}

/// A named, selectable knowledge-base catalogue.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CatalogueRef {
    pub name: String,
    pub path: PathBuf,
    /// Optional remote backing: `s3://bucket/prefix`. On first open the
    /// local dir is materialized with a `.sync.toml` built from these
    /// fields (see `brain_sync::config::ensure_sync_toml`).
    pub remote: Option<String>,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub cache_ttl_secs: Option<u64>,
}

// ---------------------------------------------------------------------------
// Brain harvest configuration
// ---------------------------------------------------------------------------

/// Opt-in-by-default background knowledge capture: when a worker session's
/// PR is first detected, a short-lived `claude -p` subprocess reads its diff
/// and writes facts into the brain vault. See `lifecycle::brain_harvest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrainHarvestConfig {
    #[serde(default = "default_brain_harvest_enabled")]
    pub enabled: bool,
}

fn default_brain_harvest_enabled() -> bool {
    true
}

impl Default for BrainHarvestConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

// ---------------------------------------------------------------------------
// Inbox messaging configuration
// ---------------------------------------------------------------------------

/// How `ninox send` and `Engine::send_to_session` get a message into a
/// running agent session. Exactly one of these is in effect at a time —
/// see `crate::messaging::deliver_message` for the dispatch and for what
/// each path falls back to when the target cannot accept it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendMechanism {
    /// `tmux::send_keys` — the message is typed at the target's prompt as
    /// verified keyboard input (hardened in PR #69 with a pre-Enter delay
    /// and verify/retry). Works against any harness in a tmux session, and
    /// is what the other two mechanisms fall back to.
    Keystrokes,
    /// The per-session file-based inbox (`crate::inbox`), drained by the
    /// Stop/UserPromptSubmit hooks installed in the worker's worktree
    /// settings (see `ninox_app::spawn_util::ensure_statusline_settings`).
    /// Keystrokes are then only a best-effort idle-wake nudge
    /// (`tmux::wake_idle_session`), not the message itself.
    Inbox,
    /// Claude Code's own cross-session messaging socket
    /// (`crate::session_socket`) — the transport its `SendMessage` tool
    /// uses between sessions. The message is written as one JSON line to
    /// the Unix socket the target session advertises in its registry
    /// record, and is enqueued for its next turn.
    ///
    /// The default: unlike keystrokes it cannot collide with whatever the
    /// human happens to be typing, and unlike the inbox it needs no hooks
    /// installed ahead of time and reaches an idle session without relying
    /// on a nudge landing.
    #[default]
    SessionSocket,
}

impl SendMechanism {
    /// Every mechanism, in the order the settings picker offers them.
    pub const ALL: [SendMechanism; 3] =
        [SendMechanism::SessionSocket, SendMechanism::Inbox, SendMechanism::Keystrokes];

    /// One line on what this mechanism does, for the settings card.
    pub fn description(&self) -> &'static str {
        match self {
            SendMechanism::SessionSocket =>
                "Writes to the target's Claude Code messaging socket, the same transport its \
                 SendMessage tool uses between sessions. Nothing is typed, so nothing can collide \
                 with what you are typing, and an idle session receives the message immediately. \
                 Falls back to keystrokes for sessions that advertise no socket.",
            SendMechanism::Inbox =>
                "Writes the message to a per-session file drained by Stop/UserPromptSubmit hooks, \
                 installed into worker worktrees created from now on. Keystrokes are used only to \
                 wake an idle session, and a message can sit undelivered if that nudge misses.",
            SendMechanism::Keystrokes =>
                "Types the message into the session's prompt as verified keyboard input. Works \
                 with every harness, and is what the other two fall back to.",
        }
    }
}

impl std::fmt::Display for SendMechanism {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SendMechanism::Keystrokes    => "Keystrokes",
            SendMechanism::Inbox         => "File-based inbox",
            SendMechanism::SessionSocket => "Session socket",
        })
    }
}

/// Which delivery mechanism orchestrator↔worker messaging uses.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct MessagingConfig {
    #[serde(default)]
    pub mechanism: SendMechanism,
}

/// Superseded by [`MessagingConfig`] — retained only so an existing
/// `[inbox_messaging].enabled = true` keeps selecting
/// [`SendMechanism::Inbox`] after upgrading, rather than silently moving
/// that user onto the new default. See `AppConfig::send_mechanism`.
///
/// Never written back once the mechanism is set explicitly
/// (`AppConfig::set_send_mechanism` clears it), and omitted from a saved
/// config entirely while it holds its default.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InboxMessagingConfig {
    #[serde(default)]
    pub enabled: bool,
}

impl InboxMessagingConfig {
    fn is_default(&self) -> bool {
        !self.enabled
    }
}

// ---------------------------------------------------------------------------
// PR watch configuration
// ---------------------------------------------------------------------------

/// Opt-in (default OFF) consolidated PR watching.
///
/// Off (default): the poller's per-session REST polling
/// (`poll_github` + `poll_pr_reconciliation`) runs exactly as before.
///
/// On: one batched GraphQL query per tick covers every watched PR —
/// session-attached PRs plus explicit `ninox open --pr` registry entries
/// (see `store::PrWatch`) — via `github_graphql::GithubBatchApi`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PrWatchConfig {
    #[serde(default)]
    pub enabled: bool,
}

// ---------------------------------------------------------------------------
// Remote machines configuration
// ---------------------------------------------------------------------------

/// Opt-in (default OFF) SSH-connected remote machines — see
/// `docs/superpowers/specs/2026-10-05-remote-sessions-design.md`.
///
/// Mirrors Herdr's security model exactly: ninox implements no auth of its
/// own here. Every field below is opaque connection *identity*, never
/// session content or secrets — authentication is delegated entirely to the
/// user's own SSH setup (`~/.ssh/config`, agent, keys, `known_hosts`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoteMachinesConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub machines: Vec<MachineProfile>,
}

/// A saved remote machine connection. Pure connection metadata — no
/// session content, no credentials, no secrets of any kind. Never store
/// anything else on this struct; `ninox_core::config::tests` asserts its
/// serialized field set stays exactly this shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineProfile {
    /// Opaque, generated at `machine add` time (a UUIDv4). Never parsed or
    /// derived from anything — purely a local handle.
    pub id: String,
    /// Human-readable label shown in `ninox machine list` / the sidebar /
    /// TUI. Defaults to the host portion of `ssh_target`.
    pub label: String,
    /// `user@host` or a `~/.ssh/config` alias — passed to `ssh`/`scp`
    /// verbatim. This is the only thing that identifies the machine; ninox
    /// never resolves or stores an IP/fingerprint itself.
    pub ssh_target: String,
    /// The remote orchestrator/session name this profile tracks (e.g.
    /// `"default"`), chosen at `machine add` time from the remote host's
    /// own `ninox list --json`.
    pub remote_session: String,
    /// Whether this profile is currently active. `machine remove` deletes
    /// the entry outright; this flag is for a user who wants to pause a
    /// machine without losing its saved connection metadata.
    #[serde(default)]
    pub enabled: bool,
}

impl MachineProfile {
    /// A fresh opaque id for a new profile — purely a local handle, never
    /// parsed or derived from the SSH target.
    pub fn new_id() -> String {
        uuid::Uuid::new_v4().to_string()
    }
}

/// What engine startup does with sessions reconciliation found interrupted
/// (spec §5.4). See `crate::fleet::startup`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RestorePolicy {
    /// Nothing happens until someone runs `ninox fleet restore`. Default:
    /// an orchestrator must never act unattended without consent.
    #[default]
    Manual,
    /// Record a pending-restore flag that the TUI offers to act on.
    Prompt,
    /// Restore the fleet immediately after startup reconciliation.
    Auto,
}

/// `[fleet]` — durable-fleet behaviour. Opt-in, default `manual`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FleetConfig {
    #[serde(default)]
    pub restore_policy: RestorePolicy,
}

// ---------------------------------------------------------------------------
// Auto-reap configuration
// ---------------------------------------------------------------------------

/// Opt-out (default ON) automatic reaping of a worker the moment its PR
/// merges. Default-on preserves the unconditional behavior that predates
/// this toggle, so existing setups are unaffected — only someone who wants
/// the post-merge validation window turns it off.
///
/// On (default): merge detection immediately runs `Engine::cleanup_session`
/// — kills the worker's tmux session, removes its worktree, and marks it
/// `Done`.
///
/// Off: the merged notification and worker-done reaction still fire (once —
/// see `Session::merged_at`), but the worker session and its worktree
/// survive, so the orchestrator can run post-merge validation in the same
/// worker that produced the PR and reap it explicitly afterwards
/// (`ninox reap <id> --force`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoReapConfig {
    #[serde(default = "default_auto_reap_enabled")]
    pub enabled: bool,
}

fn default_auto_reap_enabled() -> bool {
    true
}

impl Default for AutoReapConfig {
    fn default() -> Self {
        Self { enabled: default_auto_reap_enabled() }
    }
}

// ---------------------------------------------------------------------------
// Session retention configuration
// ---------------------------------------------------------------------------

/// How long a completed session record lingers after reaching a terminal
/// state before the poller's retention sweep purges it — giving the fleet
/// board a grace window to show what just finished instead of it vanishing
/// the instant `Done`/`Terminated` is set. See
/// `ninox_core::lifecycle::poller::Poller::sweep_retired_sessions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRetentionConfig {
    /// Days a `Done`/`Terminated` session stays queryable after reaching
    /// that terminal state. Default: 2.
    #[serde(default = "default_done_retention_days")]
    pub done_retention_days: u64,
}

fn default_done_retention_days() -> u64 {
    2
}

impl Default for SessionRetentionConfig {
    fn default() -> Self {
        Self { done_retention_days: default_done_retention_days() }
    }
}

impl SessionRetentionConfig {
    /// The retention window expressed in milliseconds, for comparison
    /// against `Session::terminal_at` (Unix epoch milliseconds).
    pub fn retention_millis(&self) -> i64 {
        self.done_retention_days as i64 * 24 * 60 * 60 * 1000
    }
}

// ---------------------------------------------------------------------------
// Shared Rust compilation cache
// ---------------------------------------------------------------------------

pub const DEFAULT_RUST_CACHE_SIZE_GIB: u16 = 10;
pub const MAX_RUST_CACHE_SIZE_GIB: u16 = 1024;

/// Opt-in worker-only sccache policy. Cargo target directories remain
/// checkout-local; only sccache's content-addressed cache is shared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RustCacheConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_sccache_executable")]
    pub executable: PathBuf,
    /// Omit for the platform cache directory (`<cache>/ninox/sccache`).
    #[serde(default)]
    pub cache_dir: Option<PathBuf>,
    #[serde(default = "default_rust_cache_size_gib")]
    pub cache_size_gib: u16,
    #[serde(default)]
    pub prune_on_release: bool,
}

fn default_sccache_executable() -> PathBuf { PathBuf::from("sccache") }
fn default_rust_cache_size_gib() -> u16 { DEFAULT_RUST_CACHE_SIZE_GIB }

impl Default for RustCacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            executable: default_sccache_executable(),
            cache_dir: None,
            cache_size_gib: default_rust_cache_size_gib(),
            prune_on_release: false,
        }
    }
}

impl RustCacheConfig {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.executable.as_os_str().is_empty(),
            "rust_cache.executable must not be empty"
        );
        anyhow::ensure!(
            (1..=MAX_RUST_CACHE_SIZE_GIB).contains(&self.cache_size_gib),
            "rust_cache.cache_size_gib must be between 1 and {MAX_RUST_CACHE_SIZE_GIB}"
        );
        Ok(())
    }

    pub fn resolved_cache_dir(&self) -> PathBuf {
        self.cache_dir.clone().map_or_else(
            || {
                dirs::cache_dir()
                    .or_else(dirs::config_dir)
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join("ninox")
                    .join("sccache")
            },
            AppConfig::resolve_root_path,
        )
    }

    pub fn resolved_executable(&self) -> PathBuf {
        if self.executable.components().count() == 1 {
            self.executable.clone()
        } else {
            AppConfig::resolve_root_path(self.executable.clone())
        }
    }
}

// ---------------------------------------------------------------------------
// Session runtime configuration
// ---------------------------------------------------------------------------

/// Which runtime hosts newly created agent sessions. Existing sessions stay
/// on whichever runtime holds them — see `crate::runtime`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeConfig {
    #[serde(default)]
    pub backend: crate::runtime::Backend,
}

// ---------------------------------------------------------------------------
// TUI configuration
// ---------------------------------------------------------------------------

/// `[tui]`: settings for the terminal UI and the `ninox pane attach` bridge.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TuiConfig {
    /// Prefix chord, written `Ctrl+<key>` (e.g. `Ctrl+Space`, `Ctrl+g`, `Ctrl+\\`; the older `C-<key>` form also parses).
    /// Defaults to `C-\\` on macOS, where the system claims Ctrl-Space for
    /// switching input sources, and `C-Space` elsewhere; neither is bound by
    /// Claude Code or Codex. `C-a`/`C-b` are rejected because they
    /// collide with users' own screen/tmux, as are chords that alias
    /// Tab/Enter/Escape/interrupt (`C-i`, `C-m`, `C-[`, `C-c`).
    /// Left out of a saved config while it holds the platform default, so
    /// saving settings never pins one platform's default on another.
    #[serde(default = "default_tui_prefix", skip_serializing_if = "is_default_tui_prefix")]
    pub prefix: String,
    /// `terminal` (default) draws the chrome in the host terminal's own
    /// default colours and 16-colour ANSI palette; `field-notes` paints the
    /// desktop app's theme in RGB.
    #[serde(default)]
    pub colors: TuiColors,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TuiColors {
    #[default]
    Terminal,
    FieldNotes,
}

/// The default `[tui] prefix` spelling for this platform.
pub const DEFAULT_PREFIX: &str = if cfg!(target_os = "macos") { "Ctrl+\\" } else { "Ctrl+Space" };

fn default_tui_prefix() -> String {
    DEFAULT_PREFIX.to_string()
}

/// Any spelling of the default chord (`Ctrl+\\`, `C-\\`, …) counts, so a
/// save never pins it.
fn is_default_tui_prefix(prefix: &str) -> bool {
    parse_prefix(prefix) == Ok(DEFAULT_PREFIX_BYTE)
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self { prefix: default_tui_prefix(), colors: TuiColors::default() }
    }
}

/// The control byte for `DEFAULT_PREFIX` (Ctrl-\ is FS, Ctrl-Space NUL);
/// what the prefix falls back to.
pub const DEFAULT_PREFIX_BYTE: u8 = if cfg!(target_os = "macos") { 0x1c } else { 0x00 };

impl TuiConfig {
    /// The prefix as the control byte a terminal sends for it, or `Err`
    /// with a reason when the setting is unparseable or disallowed.
    pub fn prefix_byte(&self) -> Result<u8, String> {
        parse_prefix(&self.prefix)
    }

    /// `prefix_byte`, falling back to the platform default on a bad setting.
    pub fn prefix_byte_or_default(&self) -> u8 {
        self.prefix_byte().unwrap_or(DEFAULT_PREFIX_BYTE)
    }
}

fn parse_prefix(spec: &str) -> Result<u8, String> {
    let lower = spec.trim().to_ascii_lowercase();
    let key = ["c-", "ctrl-", "ctrl+", "control-", "^"]
        .iter()
        .find_map(|p| lower.strip_prefix(p))
        .ok_or_else(|| format!("prefix {spec:?} must be a Ctrl chord like Ctrl+Space"))?;
    let byte = match key {
        "space" | "spc" | " " | "@" | "2" => 0x00,
        "\\" => 0x1c,
        "]" => 0x1d,
        "^" | "6" => 0x1e,
        "_" | "-" => 0x1f,
        k if k.len() == 1 && k.as_bytes()[0].is_ascii_lowercase() => k.as_bytes()[0] - b'a' + 1,
        "[" => 0x1b,
        _ => return Err(format!("prefix {spec:?} is not a recognised Ctrl chord")),
    };
    match byte {
        0x01 | 0x02 => Err(format!("prefix {spec:?} collides with screen/tmux; pick another (default {DEFAULT_PREFIX})")),
        0x03 | 0x09 | 0x0d | 0x1b => Err(format!("prefix {spec:?} aliases interrupt/Tab/Enter/Escape")),
        b => Ok(b),
    }
}

// ---------------------------------------------------------------------------
// App configuration
// ---------------------------------------------------------------------------

fn default_worker_checkout_cap() -> u32 { 5 }

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir if normalized.file_name().is_some() => {
                normalized.pop();
            }
            Component::ParentDir => normalized.push(component),
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component);
            }
        }
    }
    normalized
}

/// Default UI zoom factor (`AppConfig::zoom`) — 1.0 is unscaled. Used by
/// serde when the field is absent from an older config file.
fn default_zoom() -> f64 { 1.0 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub port:      u16,
    pub font_size: f32,
    /// Global UI zoom factor applied via Iced's native `scale_factor`,
    /// driven by the Cmd/Ctrl +/-/0 shortcuts. Persisted so the zoom
    /// level survives app restarts. Clamped to `[0.5, 3.0]` in the app.
    #[serde(default = "default_zoom")]
    pub zoom:      f64,
    #[serde(default)]
    pub theme:     ThemeVariant,
    /// External editor launched by the "Open in editor" action on a worker's
    /// workspace directory. Default: VS Code.
    #[serde(default)]
    pub editor:    EditorChoice,
    /// Override for the orchestrator root directory.
    /// Defaults to `~/ninox/orchestrators` (see `resolved_orchestrator_root`).
    #[serde(default)]
    pub orchestrator_root: Option<PathBuf>,
    /// Root for Ninox-managed worker worktrees.
    #[serde(default)]
    pub worktree_root: Option<PathBuf>,
    /// Root containing repositories eligible for pooled worker checkouts.
    /// Pooling remains disabled when unset.
    #[serde(default)]
    pub repositories_root: Option<PathBuf>,
    /// Default maximum retained or active checkout-backed workers per
    /// canonical repository pool.
    #[serde(default = "default_worker_checkout_cap", alias = "worker_checkout_cap")]
    pub worker_checkout_default_cap: u32,
    /// Per-repository pool limits. Keys are user-facing repository paths;
    /// online aliases are matched through their canonical Git identity.
    #[serde(default)]
    pub worker_checkout_repository_caps: BTreeMap<String, u32>,
    /// Agent harness and model for orchestrator sessions.
    #[serde(default)]
    pub orchestrator: AgentConfig,
    /// Agent harness and model for worker sessions spawned by `ninox spawn`.
    #[serde(default)]
    pub worker: AgentConfig,
    /// GitHub personal access token. If absent, falls back to GITHUB_TOKEN env var.
    /// Requires `repo` scope for private repos, `public_repo` for public.
    #[serde(default)]
    pub github_token: Option<String>,
    /// Knowledge base (brain) configuration.
    #[serde(default)]
    pub brain: BrainConfig,
    /// Background brain-harvest toggle. See `BrainHarvestConfig`.
    #[serde(default)]
    pub brain_harvest: BrainHarvestConfig,
    /// Grace-period retention for completed session records — see
    /// `SessionRetentionConfig`.
    #[serde(default)]
    pub session_retention: SessionRetentionConfig,
    /// Which mechanism orchestrator↔worker messages are delivered by.
    /// Absent means "never chosen explicitly" — resolved by
    /// `send_mechanism()`, which is the only thing that should read this.
    #[serde(default)]
    pub messaging: Option<MessagingConfig>,
    /// Legacy pre-`[messaging]` toggle, kept for migration only — see
    /// `InboxMessagingConfig` and `send_mechanism()`.
    #[serde(default, skip_serializing_if = "InboxMessagingConfig::is_default")]
    pub inbox_messaging: InboxMessagingConfig,
    /// Bounded worker-only shared sccache and release-pruning policy.
    #[serde(default)]
    pub rust_cache: RustCacheConfig,
    /// Consolidated batched-GraphQL PR watching. Opt-in, default off — see
    /// `PrWatchConfig`.
    #[serde(default)]
    pub pr_watch: PrWatchConfig,
    /// Reap a worker automatically the moment its PR merges. Opt-out,
    /// default on — see `AutoReapConfig`.
    #[serde(default)]
    pub auto_reap: AutoReapConfig,
    /// Durable-fleet restore policy — see `FleetConfig`.
    #[serde(default)]
    pub fleet: FleetConfig,
    /// Theme file name (resolves to `~/.config/ninox/themes/<name>.toml`) or
    /// an absolute/`~`-relative path. `None` uses `themes/field-notes.toml`
    /// if present, else the built-in Field Notes palettes.
    #[serde(default)]
    pub theme_file: Option<String>,
    /// Width in logical pixels of the left sidebar. Persisted when a resize
    /// drag commits (see `app::App::update`'s `MouseReleased` arm) and
    /// clamped to the 150–400 drag range on load. Default 220.
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: f32,
    /// Whether the left sidebar is collapsed/hidden. Toggled by the sidebar
    /// header control and Cmd/Ctrl+B; the last-used width is retained in
    /// `sidebar_width` so showing it again restores the prior size.
    #[serde(default)]
    pub sidebar_hidden: bool,
    /// `[runtime] backend = "ptyd" | "tmux"` — see `RuntimeConfig`.
    #[serde(default)]
    pub runtime: RuntimeConfig,
    /// Terminal UI settings — see `TuiConfig`.
    #[serde(default)]
    pub tui: TuiConfig,
    /// SSH-connected remote machines. Opt-in, default off — see
    /// `RemoteMachinesConfig`.
    #[serde(default)]
    pub remote_machines: RemoteMachinesConfig,
    /// Agent-harness registry overrides/extensions (`[harnesses.<name>]`).
    /// Builtin specs for claude-code/codex/opencode/aider/freebuff apply
    /// when a name is absent here. See `crate::harness`. Kept last so TOML
    /// serialization emits this table-of-tables after every scalar field.
    #[serde(default)]
    pub harnesses: BTreeMap<String, HarnessSpec>,
}

/// Default left-sidebar width in logical pixels. Matches the historical
/// hard-coded startup width and sits inside the 150–400 drag range.
fn default_sidebar_width() -> f32 {
    220.0
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            port:             8080,
            font_size:        13.0,
            zoom:             default_zoom(),
            theme:            ThemeVariant::Dark,
            editor:           EditorChoice::default(),
            orchestrator_root: None,
            worktree_root:    None,
            repositories_root: None,
            worker_checkout_default_cap: default_worker_checkout_cap(),
            worker_checkout_repository_caps: BTreeMap::new(),
            orchestrator:     AgentConfig::default(),
            worker:           AgentConfig::default(),
            github_token:     None,
            brain:            BrainConfig::default(),
            brain_harvest:    BrainHarvestConfig::default(),
            session_retention: SessionRetentionConfig::default(),
            theme_file:       None,
            sidebar_width:    default_sidebar_width(),
            sidebar_hidden:   false,
            runtime:          RuntimeConfig::default(),
            harnesses:        BTreeMap::new(),
            messaging:        None,
            inbox_messaging:  InboxMessagingConfig::default(),
            rust_cache:       RustCacheConfig::default(),
            pr_watch:         PrWatchConfig::default(),
            auto_reap:        AutoReapConfig::default(),
            fleet:            FleetConfig::default(),
            tui:              TuiConfig::default(),
            remote_machines:  RemoteMachinesConfig::default(),
        }
    }
}

impl AppConfig {
    pub fn validated_worker_checkout_default_cap(&self) -> anyhow::Result<usize> {
        anyhow::ensure!(
            self.worker_checkout_default_cap > 0,
            "worker_checkout_default_cap must be greater than zero"
        );
        Ok(self.worker_checkout_default_cap as usize)
    }

    pub fn validated_worker_checkout_cap_for_repository(
        &self,
        repository: &Path,
    ) -> anyhow::Result<usize> {
        let identity = RepositoryIdentity::resolve(repository)?;
        self.validated_worker_checkout_cap_for_identity(
            &identity.top_level,
            Some(identity.common_git_dir.to_string_lossy().as_ref()),
        )
    }

    pub fn validated_worker_checkout_cap_for_stored_repository(
        &self,
        repository: &Path,
        repository_key: Option<&str>,
    ) -> anyhow::Result<usize> {
        anyhow::ensure!(
            repository_key.is_some_and(|key| !key.is_empty()),
            "checkout-backed worker has no canonical repository identity"
        );
        self.validated_worker_checkout_cap_for_identity(repository, repository_key)
    }

    fn validated_worker_checkout_cap_for_identity(
        &self,
        repository: &Path,
        repository_key: Option<&str>,
    ) -> anyhow::Result<usize> {
        let default = self.validated_worker_checkout_default_cap()?;
        let online_identity = RepositoryIdentity::resolve(repository).ok();
        let target_path = online_identity
            .as_ref()
            .map_or_else(|| lexical_normalize(repository), |identity| identity.top_level.clone());
        let target_key = online_identity
            .as_ref()
            .map(|identity| identity.common_git_dir.to_string_lossy())
            .or_else(|| repository_key.map(std::borrow::Cow::Borrowed));
        let mut matched = None;
        for (configured, limit) in &self.worker_checkout_repository_caps {
            anyhow::ensure!(
                *limit > 0,
                "worker checkout limit for {configured} must be greater than zero"
            );
            let configured_path =
                lexical_normalize(&Self::resolve_root_path(PathBuf::from(configured)));
            let configured_identity = RepositoryIdentity::resolve(&configured_path).ok();
            let is_match = configured_identity.as_ref().is_some_and(|identity| {
                target_key
                    .as_deref()
                    .is_some_and(|key| identity.common_git_dir.to_string_lossy() == key)
            }) || configured_path == target_path;
            if is_match {
                if let Some(previous) = matched {
                    anyhow::ensure!(
                        previous == *limit,
                        "conflicting worker checkout limits resolve to repository {}",
                        target_path.display()
                    );
                }
                matched = Some(*limit);
            }
        }
        Ok(matched.map_or(default, |limit| {
            limit.try_into().expect("u32 always fits usize on supported platforms")
        }))
    }

    pub fn set_worker_checkout_repository_cap(
        &mut self,
        repository: PathBuf,
        limit: u32,
    ) -> anyhow::Result<String> {
        anyhow::ensure!(limit > 0, "repository worker checkout limit must be greater than zero");
        let resolved = lexical_normalize(&Self::resolve_root_path(repository));
        let identity = RepositoryIdentity::resolve(&resolved).ok();
        let normalized = identity
            .as_ref()
            .map_or_else(|| resolved.clone(), |repository| repository.top_level.clone());
        let aliases = self
            .worker_checkout_repository_caps
            .keys()
            .filter(|configured| {
                let configured =
                    lexical_normalize(&Self::resolve_root_path(PathBuf::from(configured)));
                identity.as_ref().is_some_and(|target| {
                    RepositoryIdentity::resolve(&configured)
                        .is_ok_and(|candidate| candidate.common_git_dir == target.common_git_dir)
                }) || configured == normalized
            })
            .cloned()
            .collect::<Vec<_>>();
        for alias in aliases {
            self.worker_checkout_repository_caps.remove(&alias);
        }
        let key = normalized.to_string_lossy().into_owned();
        self.worker_checkout_repository_caps.insert(key.clone(), limit);
        Ok(key)
    }

    /// The effective harness registry: builtin specs overlaid by this
    /// config's `[harnesses.*]` entries.
    pub fn registry(&self) -> HarnessRegistry {
        HarnessRegistry::from_config(&self.harnesses)
    }

    /// The harness `claude-code`: always enabled, its toggle is inert.
    pub const LOCKED_HARNESS: &'static str = "claude-code";

    /// Flip a harness's `enabled`, writing the FULL effective spec — config
    /// entries replace builtin specs wholesale, so a bare `{ enabled }`
    /// would wipe the builtin's args. Returns false (no change) for
    /// `LOCKED_HARNESS`.
    pub fn toggle_harness(&mut self, name: &str) -> bool {
        if name == Self::LOCKED_HARNESS {
            return false;
        }
        let mut spec = self.registry().spec(name);
        spec.enabled = !spec.enabled;
        self.harnesses.insert(name.to_string(), spec);
        true
    }

    /// Which mechanism to deliver orchestrator↔worker messages by.
    ///
    /// An explicit `[messaging] mechanism` always wins. Failing that, a
    /// config written before `[messaging]` existed is migrated by its
    /// legacy `[inbox_messaging].enabled` flag: someone who had opted into
    /// the file-based inbox stays on it rather than being moved onto the
    /// new default behind their back. Everything else — including a config
    /// that never mentioned messaging at all — gets
    /// [`SendMechanism::default()`].
    ///
    /// Read this rather than either field directly; the fields alone don't
    /// tell you what will actually happen.
    pub fn send_mechanism(&self) -> SendMechanism {
        match self.messaging {
            Some(m) => m.mechanism,
            None if self.inbox_messaging.enabled => SendMechanism::Inbox,
            None => SendMechanism::default(),
        }
    }

    /// Choose the delivery mechanism, retiring the legacy inbox flag so the
    /// two can never disagree — after this, `send_mechanism()` is answered
    /// entirely by `[messaging]`.
    pub fn set_send_mechanism(&mut self, mechanism: SendMechanism) {
        self.messaging = Some(MessagingConfig { mechanism });
        self.inbox_messaging.enabled = false;
    }

    /// Path to the knowledge-base (brain) directory.
    ///
    /// Honors the `NINOX_BRAIN` environment variable as an override: if
    /// set, it is treated as an absolute path to the brain directory and
    /// returned as-is, mirroring how `config_path()` honors `NINOX_CONFIG`.
    /// This lets a selected catalogue (see `catalogue_options()`) be handed
    /// to a spawned orchestrator session via its environment without
    /// mutating `config.toml`, and lets tests redirect brain reads/writes
    /// without touching the real user brain directory.
    ///
    /// Falls back to `self.brain.path` when set, else `<config_dir>/ninox/brain`.
    pub fn resolved_brain_path(&self) -> PathBuf {
        if let Ok(p) = std::env::var("NINOX_BRAIN") {
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
        if let Some(ref p) = self.brain.path {
            return p.clone();
        }
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("ninox")
            .join("brain")
    }

    /// All selectable knowledge-base catalogues: the implicit "default"
    /// (this config's `resolved_brain_path()`) followed by any additional
    /// catalogues configured under `[[brain.catalogues]]` — skipping any
    /// entry literally named "default" to avoid a confusing duplicate.
    pub fn catalogue_options(&self) -> Vec<CatalogueRef> {
        let mut options = vec![CatalogueRef {
            name: "default".to_string(),
            path: self.resolved_brain_path(),
            remote: self.brain.remote.clone(),
            endpoint: self.brain.endpoint.clone(),
            region: self.brain.region.clone(),
            cache_ttl_secs: self.brain.cache_ttl_secs,
        }];
        options.extend(
            self.brain
                .catalogues
                .iter()
                .filter(|c| c.name != "default")
                .cloned(),
        );
        options
    }

    /// The remote-sync settings configured for `brain_path`, if any: the
    /// default brain's `[brain]` remote fields when `brain_path` is the
    /// resolved default, else a `[[brain.catalogues]]` entry whose `path`
    /// matches exactly. Returns the `.sync.toml` payload to materialize.
    pub fn remote_config_for(&self, brain_path: &std::path::Path) -> Option<crate::brain_sync::SyncToml> {
        let build = |remote: &Option<String>, endpoint: &Option<String>, region: &Option<String>, ttl: &Option<u64>| {
            remote.as_ref().map(|r| crate::brain_sync::SyncToml {
                remote: r.clone(),
                endpoint: endpoint.clone(),
                region: region.clone(),
                cache_ttl_secs: ttl.unwrap_or(0),
            })
        };
        if self.brain.remote.is_some() && brain_path == self.resolved_brain_path() {
            return build(&self.brain.remote, &self.brain.endpoint, &self.brain.region, &self.brain.cache_ttl_secs);
        }
        self.brain
            .catalogues
            .iter()
            .find(|c| c.path == brain_path)
            .and_then(|c| build(&c.remote, &c.endpoint, &c.region, &c.cache_ttl_secs))
    }

    /// Falls back to `~/ninox/orchestrators` when `orchestrator_root` is
    /// unset. This moved off `<config_dir>/ninox/orchestrator` (an
    /// OS-private, easy-to-miss location) in MLOPS-4659; the change only
    /// affects the fallback, not the `[orchestrator_root]` override, and
    /// existing users on the old default simply start fresh at the new path
    /// on next launch — their old orchestrator directory is left in place
    /// untouched, not migrated or deleted.
    pub fn resolved_orchestrator_root(&self) -> PathBuf {
        self.orchestrator_root.clone().unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("ninox")
                .join("orchestrators")
        })
    }

    pub fn resolved_worktree_root(&self) -> PathBuf {
        let path = self.worktree_root.clone().unwrap_or_else(|| {
            dirs::data_dir()
                .or_else(dirs::config_dir)
                .unwrap_or_else(|| PathBuf::from("."))
                .join("ninox")
                .join("worktrees")
        });
        Self::resolve_root_path(path)
    }

    pub fn resolved_repositories_root(&self) -> Option<PathBuf> {
        self.repositories_root.clone().map(Self::resolve_root_path)
    }

    pub fn resolved_rust_cache_dir(&self) -> PathBuf {
        self.rust_cache.resolved_cache_dir()
    }

    fn resolve_root_path(path: PathBuf) -> PathBuf {
        if path == std::path::Path::new("~") {
            return dirs::home_dir().unwrap_or(path);
        }
        if let Ok(rest) = path.strip_prefix("~/") {
            if let Some(home) = dirs::home_dir() {
                return home.join(rest);
            }
        }
        if path.is_absolute() {
            return path;
        }
        Self::config_path()
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(|parent| parent.join(&path))
            .unwrap_or_else(|| {
                dirs::config_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join("ninox")
                    .join(path)
            })
    }

    /// Path to the `config.toml` file.
    ///
    /// Honors the `NINOX_CONFIG` environment variable as an override: if
    /// set, it is treated as an absolute path to the config file itself
    /// (not a directory) and returned as-is. This is the same override
    /// consumed by spawned agent sessions (see the `NINOX_CONFIG` env var
    /// set alongside `NINOX_BIN` when launching orchestrator sessions), and
    /// it also lets tests redirect config reads/writes away from the real
    /// user config file (e.g. `~/Library/Application Support/ninox/config.toml`
    /// on macOS) without mutating developer machine state.
    ///
    /// Falls back to `<config_dir>/ninox/config.toml` when unset.
    pub fn config_path() -> PathBuf {
        if let Ok(p) = std::env::var("NINOX_CONFIG") {
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("ninox")
            .join("config.toml")
    }

    /// Directory for Ninox-managed shell wrappers prepended to agent PATH.
    /// Default: `~/.config/ninox/bin/`
    pub fn ninox_bin_dir() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("ninox")
            .join("bin")
    }

    /// Directory where per-session metadata JSON files are written by wrapper hooks.
    /// Default: `~/.config/ninox/sessions/`
    pub fn sessions_dir() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("ninox")
            .join("sessions")
    }

    fn path() -> PathBuf { Self::config_path() }

    pub fn load() -> Result<Self> {
        let p = Self::path();
        if !p.exists() { return Ok(Self::default()); }
        Ok(toml::from_str(&fs::read_to_string(p)?)?)
    }

    pub fn save(&self) -> Result<()> {
        let p = Self::path();
        fs::create_dir_all(p.parent().unwrap())?;
        fs::write(p, toml::to_string(self)?)?;
        Ok(())
    }

    /// Read → apply → write against the file on disk, so a change made by
    /// one process never clobbers another's edits with a stale in-memory
    /// copy. A file that doesn't parse is left untouched (the error is
    /// returned), and an `f` that changes nothing writes nothing.
    pub fn update<R>(f: impl FnOnce(&mut Self) -> R) -> Result<(Self, R)> {
        let mut cfg = Self::load()?;
        let before = toml::to_string(&cfg)?;
        let r = f(&mut cfg);
        if toml::to_string(&cfg)? != before || !Self::path().exists() {
            cfg.save()?;
        }
        Ok((cfg, r))
    }
}

/// Serializes tests that mutate process-global env vars (`NINOX_CONFIG`,
/// `NINOX_BRAIN`) against each other — `cargo test` runs test fns on
/// parallel threads, so without this guard one test's env mutation could
/// leak into another's read. `pub(crate)` and shared with
/// `lifecycle::poller`'s tests, which also mutate `NINOX_CONFIG`.
#[cfg(test)]
pub(crate) static ENV_TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Set `key=value` for the duration of `f`, restoring the prior value (or
/// unsetting it) afterward. Serialized via `ENV_TEST_GUARD` since env vars
/// are process-global state shared across parallel test threads. Mirrors
/// `ninox_app::app::tests::with_env_override`.
#[cfg(test)]
pub(crate) fn with_env_override<T>(
    key: &str,
    value: impl AsRef<std::ffi::OsStr>,
    f: impl FnOnce() -> T,
) -> T {
    let _guard = ENV_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let prior = std::env::var(key).ok();
    std::env::set_var(key, value);

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));

    match prior {
        Some(v) => std::env::set_var(key, v),
        None    => std::env::remove_var(key),
    }
    result.unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn update_applies_to_the_file_not_a_stale_copy() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        with_env_override("NINOX_CONFIG", &path, || {
            let stale = AppConfig::load().unwrap();
            let mut other = stale.clone();
            other.zoom = 1.3;
            other.save().unwrap();

            let (saved, ()) = AppConfig::update(|c| c.pr_watch.enabled = true).unwrap();
            assert!(saved.pr_watch.enabled);
            assert_eq!(saved.zoom, 1.3, "another process's edit survives");
            assert_eq!(AppConfig::load().unwrap().zoom, 1.3);

            std::fs::write(&path, "not = [valid").unwrap();
            assert!(AppConfig::update(|c| c.pr_watch.enabled = false).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "not = [valid", "a broken file is never overwritten");
        });
    }

    #[test]
    fn round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = AppConfig { port: 9090, font_size: 14.0, theme: ThemeVariant::Light, ..AppConfig::default() };
        fs::write(&path, toml::to_string(&cfg).unwrap()).unwrap();
        let loaded: AppConfig = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(loaded.port, 9090);
        assert_eq!(loaded.theme, ThemeVariant::Light);
        assert!(loaded.orchestrator_root.is_none());
    }

    /// `MachineProfile` must persist only opaque connection metadata — no
    /// session content, no credentials. Asserts the exact serialized key
    /// set so a future field addition is a deliberate, reviewed decision
    /// rather than an accidental leak.
    #[test]
    fn machine_profile_serializes_only_opaque_metadata() {
        let profile = MachineProfile {
            id: "11111111-1111-1111-1111-111111111111".into(),
            label: "build-box".into(),
            ssh_target: "ethan@10.0.0.5".into(),
            remote_session: "default".into(),
            enabled: true,
        };
        let value = toml::Value::try_from(&profile).unwrap();
        let table = value.as_table().unwrap();
        let mut keys: Vec<&str> = table.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(keys, vec!["enabled", "id", "label", "remote_session", "ssh_target"]);
    }

    #[test]
    fn machine_profile_round_trips_through_app_config() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = AppConfig::default();
        cfg.remote_machines.enabled = true;
        cfg.remote_machines.machines.push(MachineProfile {
            id: "m1".into(),
            label: "laptop".into(),
            ssh_target: "me@laptop.local".into(),
            remote_session: "default".into(),
            enabled: true,
        });
        fs::write(&path, toml::to_string(&cfg).unwrap()).unwrap();
        let loaded: AppConfig = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(loaded.remote_machines.enabled);
        assert_eq!(loaded.remote_machines.machines.len(), 1);
        assert_eq!(loaded.remote_machines.machines[0].ssh_target, "me@laptop.local");
    }

    /// Default config has the feature off and no machines — an untouched
    /// install stays exactly as before this feature existed.
    #[test]
    fn remote_machines_defaults_to_disabled_and_empty() {
        let cfg = AppConfig::default();
        assert!(!cfg.remote_machines.enabled);
        assert!(cfg.remote_machines.machines.is_empty());
    }

    #[test]
    fn runtime_backend_defaults_to_tmux_and_round_trips() {
        use crate::runtime::Backend;
        assert_eq!(AppConfig::default().runtime.backend, Backend::Tmux);
        let legacy: AppConfig = toml::from_str("port = 8080\nfont_size = 13.0\n").unwrap();
        assert_eq!(legacy.runtime.backend, Backend::Tmux);

        let cfg = AppConfig { runtime: RuntimeConfig { backend: Backend::Ptyd }, ..AppConfig::default() };
        let serialized = toml::to_string(&cfg).unwrap();
        assert!(serialized.contains("[runtime]\nbackend = \"ptyd\""), "{serialized}");
        let loaded: AppConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(loaded.runtime.backend, Backend::Ptyd);

        let with_harness = AppConfig {
            harnesses: [("x".to_string(), crate::harness::HarnessSpec::default())].into(),
            ..cfg
        };
        let loaded: AppConfig = toml::from_str(&toml::to_string(&with_harness).unwrap()).unwrap();
        assert_eq!(loaded.runtime.backend, Backend::Ptyd);
    }

    #[test]
    fn default_theme_is_dark() {
        assert_eq!(AppConfig::default().theme, ThemeVariant::Dark);
    }

    #[test]
    fn sidebar_geometry_round_trips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = AppConfig { sidebar_width: 275.0, sidebar_hidden: true, ..AppConfig::default() };
        fs::write(&path, toml::to_string(&cfg).unwrap()).unwrap();
        let loaded: AppConfig = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(loaded.sidebar_width, 275.0);
        assert!(loaded.sidebar_hidden);
    }

    #[test]
    fn missing_sidebar_fields_default() {
        // Configs written before these fields existed must still load.
        let cfg: AppConfig = toml::from_str("port = 8080\nfont_size = 13.0\n").unwrap();
        assert_eq!(cfg.sidebar_width, default_sidebar_width());
        assert!(!cfg.sidebar_hidden);
    }

    #[test]
    fn session_retention_defaults_to_two_days() {
        let cfg = SessionRetentionConfig::default();
        assert_eq!(cfg.done_retention_days, 2);
        assert_eq!(cfg.retention_millis(), 2 * 24 * 60 * 60 * 1000);
    }

    #[test]
    fn missing_session_retention_field_defaults() {
        let cfg: AppConfig = toml::from_str("port = 8080\nfont_size = 13.0\n").unwrap();
        assert_eq!(cfg.session_retention.done_retention_days, 2);
    }

    #[test]
    fn missing_theme_field_defaults_to_dark() {
        let cfg: AppConfig = toml::from_str("port = 8080\nfont_size = 13.0\n").unwrap();
        assert_eq!(cfg.theme, ThemeVariant::Dark);
    }

    #[test]
    fn agent_config_round_trip() {
        let toml = "port = 8080\nfont_size = 13.0\n\n[orchestrator]\nharness = \"claude-code\"\nmodel = \"claude-opus-4-5\"\n\n[worker]\nharness = \"codex\"\n";
        let cfg: AppConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.orchestrator.harness, "claude-code");
        assert_eq!(cfg.orchestrator.model.as_deref(), Some("claude-opus-4-5"));
        assert_eq!(cfg.worker.harness, "codex");
        assert!(cfg.worker.model.is_none());
    }

    // Launch-shape tests for the four known harnesses moved to
    // `crate::harness::tests` with the registry.

    #[test]
    fn resolved_orchestrator_root_default() {
        let cfg = AppConfig::default();
        assert!(cfg.resolved_orchestrator_root().ends_with("ninox/orchestrators"));
    }

    #[test]
    fn worker_workspace_roots_default_compatibly() {
        let cfg = AppConfig::default();
        assert!(cfg.repositories_root.is_none());
        assert!(cfg.resolved_repositories_root().is_none());
        assert_eq!(
            cfg.resolved_worktree_root(),
            dirs::data_dir()
                .or_else(dirs::config_dir)
                .unwrap_or_else(|| PathBuf::from("."))
                .join("ninox")
                .join("worktrees")
        );

        let old: AppConfig =
            toml::from_str("port = 8080\nfont_size = 13.0\n").unwrap();
        assert!(old.worktree_root.is_none());
        assert!(old.repositories_root.is_none());
    }

    #[test]
    fn worker_checkout_limits_default_to_five_and_preserve_legacy_scalar() {
        let mut config = AppConfig::default();
        assert_eq!(config.validated_worker_checkout_default_cap().unwrap(), 5);

        let legacy: AppConfig =
            toml::from_str("port = 8080\nfont_size = 13.0\nworker_checkout_cap = 7\n").unwrap();
        assert_eq!(legacy.validated_worker_checkout_default_cap().unwrap(), 7);
        assert!(toml::to_string(&legacy)
            .unwrap()
            .contains("worker_checkout_default_cap = 7"));

        config.worker_checkout_default_cap = 0;
        assert!(config.validated_worker_checkout_default_cap().is_err());
        config.worker_checkout_default_cap = 4;
        assert_eq!(config.validated_worker_checkout_default_cap().unwrap(), 4);
    }

    #[test]
    fn repository_worker_checkout_limits_support_aliases_and_preserve_offline_entries() {
        let root = tempdir().unwrap();
        let repository = root.path().join("repository");
        std::fs::create_dir(&repository).unwrap();
        let run = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(&repository)
                .args(args)
                .status()
                .unwrap()
                .success());
        };
        run(&["init", "-q"]);
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&repository, &alias).unwrap();
        let offline = root.path().join("offline");
        let mut config = AppConfig {
            worker_checkout_repository_caps: BTreeMap::from([
                (alias.to_string_lossy().into_owned(), 9),
                (offline.to_string_lossy().into_owned(), 2),
            ]),
            ..AppConfig::default()
        };

        assert_eq!(
            config
                .validated_worker_checkout_cap_for_repository(&repository)
                .unwrap(),
            9
        );
        assert_eq!(config.worker_checkout_repository_caps.len(), 2);
        config
            .set_worker_checkout_repository_cap(repository.clone(), 6)
            .unwrap();
        assert_eq!(
            config
                .validated_worker_checkout_cap_for_repository(&alias)
                .unwrap(),
            6
        );
        assert!(config.worker_checkout_repository_caps.contains_key(
            repository.canonicalize().unwrap().to_string_lossy().as_ref()
        ));
        assert!(config
            .worker_checkout_repository_caps
            .contains_key(offline.to_string_lossy().as_ref()));
        assert!(config
            .set_worker_checkout_repository_cap(repository, 0)
            .is_err());
    }

    #[test]
    fn rust_cache_defaults_are_conservative_and_bounded() {
        let config = AppConfig::default();
        assert!(!config.rust_cache.enabled);
        assert!(!config.rust_cache.prune_on_release);
        assert_eq!(config.rust_cache.executable, PathBuf::from("sccache"));
        assert_eq!(config.rust_cache.cache_size_gib, DEFAULT_RUST_CACHE_SIZE_GIB);
        config.rust_cache.validate().unwrap();
    }

    #[test]
    fn rust_cache_validation_rejects_unbounded_or_unusable_config() {
        assert!(RustCacheConfig {
            cache_size_gib: 0,
            ..RustCacheConfig::default()
        }
        .validate()
        .is_err());
        assert!(RustCacheConfig {
            cache_size_gib: MAX_RUST_CACHE_SIZE_GIB + 1,
            ..RustCacheConfig::default()
        }
        .validate()
        .is_err());
        assert!(RustCacheConfig {
            executable: PathBuf::new(),
            ..RustCacheConfig::default()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn rust_cache_policy_round_trips_and_anchors_relative_cache_dir() {
        let config_dir = tempdir().unwrap();
        let config_path = config_dir.path().join("config.toml");
        with_env_override("NINOX_CONFIG", &config_path, || {
            let config = AppConfig {
                rust_cache: RustCacheConfig {
                    enabled: true,
                    executable: PathBuf::from("/opt/bin/sccache"),
                    cache_dir: Some(PathBuf::from("cache/rust")),
                    cache_size_gib: 24,
                    prune_on_release: true,
                },
                ..AppConfig::default()
            };
            let encoded = toml::to_string(&config).unwrap();
            let decoded: AppConfig = toml::from_str(&encoded).unwrap();
            assert_eq!(decoded.rust_cache, config.rust_cache);
            assert_eq!(
                decoded.resolved_rust_cache_dir(),
                config_dir.path().join("cache/rust")
            );
        });
    }

    #[test]
    fn worker_workspace_roots_expand_home_and_anchor_relative_paths() {
        let config_dir = tempdir().unwrap();
        let config_path = config_dir.path().join("config.toml");
        with_env_override("NINOX_CONFIG", &config_path, || {
            let cfg = AppConfig {
                worktree_root: Some(PathBuf::from("~/managed")),
                repositories_root: Some(PathBuf::from("repositories")),
                ..AppConfig::default()
            };
            assert_eq!(
                cfg.resolved_worktree_root(),
                dirs::home_dir().unwrap().join("managed")
            );
            assert_eq!(
                cfg.resolved_repositories_root(),
                Some(config_dir.path().join("repositories"))
            );
        });
    }

    #[test]
    fn worker_workspace_roots_are_top_level_toml_keys() {
        let cfg: AppConfig = toml::from_str(
            "port = 8080\nfont_size = 13.0\nworktree_root = \"/tmp/wt\"\nrepositories_root = \"/tmp/repos\"\n",
        )
        .unwrap();
        assert_eq!(cfg.worktree_root, Some(PathBuf::from("/tmp/wt")));
        assert_eq!(cfg.repositories_root, Some(PathBuf::from("/tmp/repos")));
    }

    #[test]
    fn config_path_honors_ninox_config_env() {
        let dir = tempdir().unwrap();
        let override_path = dir.path().join("config_path_honors_ninox_config_env.toml");

        with_env_override("NINOX_CONFIG", &override_path, || {
            assert_eq!(AppConfig::config_path(), override_path);
        });
    }

    #[test]
    fn resolved_brain_path_honors_ninox_brain_env() {
        let dir = tempdir().unwrap();
        let override_path = dir.path().join("brain-override");

        with_env_override("NINOX_BRAIN", &override_path, || {
            let cfg = AppConfig::default();
            assert_eq!(cfg.resolved_brain_path(), override_path);
        });
    }

    #[test]
    fn catalogue_options_defaults_to_single_entry() {
        // Serialize against resolved_brain_path_honors_ninox_brain_env: this
        // test reads resolved_brain_path() twice (via catalogue_options and
        // directly) and must not straddle that test's NINOX_BRAIN window.
        let _guard = ENV_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = AppConfig::default();
        let options = cfg.catalogue_options();
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].name, "default");
        assert_eq!(options[0].path, cfg.resolved_brain_path());
    }

    #[test]
    fn brain_harvest_defaults_to_enabled() {
        assert!(AppConfig::default().brain_harvest.enabled);
    }

    #[test]
    fn brain_harvest_missing_table_defaults_to_enabled() {
        let cfg: AppConfig = toml::from_str("port = 8080\nfont_size = 13.0\n").unwrap();
        assert!(cfg.brain_harvest.enabled);
    }

    #[test]
    fn brain_harvest_can_be_disabled_via_config() {
        let toml_src = "port = 8080\nfont_size = 13.0\n\n[brain_harvest]\nenabled = false\n";
        let cfg: AppConfig = toml::from_str(toml_src).unwrap();
        assert!(!cfg.brain_harvest.enabled);
    }

    #[test]
    fn send_mechanism_defaults_to_the_session_socket() {
        assert_eq!(AppConfig::default().send_mechanism(), SendMechanism::SessionSocket);
    }

    #[test]
    fn send_mechanism_missing_tables_default_to_the_session_socket() {
        let cfg: AppConfig = toml::from_str("port = 8080\nfont_size = 13.0\n").unwrap();
        assert_eq!(cfg.send_mechanism(), SendMechanism::SessionSocket);
    }

    #[test]
    fn send_mechanism_reads_an_explicit_choice() {
        for (value, expected) in [
            ("keystrokes", SendMechanism::Keystrokes),
            ("inbox", SendMechanism::Inbox),
            ("session_socket", SendMechanism::SessionSocket),
        ] {
            let toml_src = format!("port = 8080\nfont_size = 13.0\n\n[messaging]\nmechanism = \"{value}\"\n");
            let cfg: AppConfig = toml::from_str(&toml_src).unwrap();
            assert_eq!(cfg.send_mechanism(), expected, "for mechanism = {value:?}");
        }
    }

    #[test]
    fn legacy_inbox_toggle_still_selects_the_inbox() {
        // Someone who opted into the file-based inbox before [messaging]
        // existed must stay on it across the upgrade, not be moved onto the
        // new default behind their back.
        let toml_src = "port = 8080\nfont_size = 13.0\n\n[inbox_messaging]\nenabled = true\n";
        let cfg: AppConfig = toml::from_str(toml_src).unwrap();
        assert_eq!(cfg.send_mechanism(), SendMechanism::Inbox);
    }

    #[test]
    fn an_explicit_mechanism_overrides_the_legacy_inbox_toggle() {
        let toml_src = "port = 8080\nfont_size = 13.0\n\n[messaging]\nmechanism = \"keystrokes\"\n\n[inbox_messaging]\nenabled = true\n";
        let cfg: AppConfig = toml::from_str(toml_src).unwrap();
        assert_eq!(cfg.send_mechanism(), SendMechanism::Keystrokes);
    }

    #[test]
    fn setting_a_mechanism_retires_the_legacy_inbox_toggle() {
        // Otherwise a config could carry `enabled = true` alongside an
        // explicit non-inbox mechanism — two fields disagreeing about one
        // choice, with the answer depending on which one you happened to read.
        let mut cfg: AppConfig =
            toml::from_str("port = 8080\nfont_size = 13.0\n\n[inbox_messaging]\nenabled = true\n").unwrap();
        cfg.set_send_mechanism(SendMechanism::Keystrokes);
        assert_eq!(cfg.send_mechanism(), SendMechanism::Keystrokes);
        assert!(!cfg.inbox_messaging.enabled);
    }

    #[test]
    fn a_chosen_mechanism_survives_a_save_load_round_trip() {
        // Guards the TOML shape as much as the value: `toml::to_string`
        // rejects a plain value emitted after a table, so a new table field
        // landing in the wrong position breaks saving for everyone.
        let mut cfg = AppConfig::default();
        cfg.set_send_mechanism(SendMechanism::Inbox);
        let serialized = toml::to_string(&cfg).unwrap();
        let reloaded: AppConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(reloaded.send_mechanism(), SendMechanism::Inbox);
    }

    #[test]
    fn a_default_config_does_not_write_the_legacy_inbox_table() {
        let serialized = toml::to_string(&AppConfig::default()).unwrap();
        assert!(
            !serialized.contains("inbox_messaging"),
            "the legacy table is migration-only and should not be re-emitted:\n{serialized}"
        );
    }

    #[test]
    fn fleet_restore_policy_defaults_to_manual_and_parses() {
        assert_eq!(AppConfig::default().fleet.restore_policy, RestorePolicy::Manual);
        let cfg: AppConfig = toml::from_str("port = 8080\nfont_size = 13.0\n").unwrap();
        assert_eq!(cfg.fleet.restore_policy, RestorePolicy::Manual);
        let cfg: AppConfig =
            toml::from_str("port = 8080\nfont_size = 13.0\n\n[fleet]\nrestore_policy = \"auto\"\n").unwrap();
        assert_eq!(cfg.fleet.restore_policy, RestorePolicy::Auto);
        let serialized = toml::to_string(&cfg).unwrap();
        let back: AppConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(back.fleet.restore_policy, RestorePolicy::Auto);
    }

    #[test]
    fn pr_watch_defaults_to_disabled() {
        assert!(!AppConfig::default().pr_watch.enabled);
    }

    #[test]
    fn pr_watch_missing_table_defaults_to_disabled() {
        let cfg: AppConfig = toml::from_str("port = 8080\nfont_size = 13.0\n").unwrap();
        assert!(!cfg.pr_watch.enabled);
    }

    #[test]
    fn pr_watch_can_be_enabled_via_config() {
        let toml_src = "port = 8080\nfont_size = 13.0\n\n[pr_watch]\nenabled = true\n";
        let cfg: AppConfig = toml::from_str(toml_src).unwrap();
        assert!(cfg.pr_watch.enabled);
    }

    #[test]
    fn auto_reap_defaults_to_enabled() {
        // Opt-out: default on preserves the pre-toggle cleanup-on-merge
        // behavior, both when the whole table is absent and when the table
        // is present without `enabled`.
        assert!(AppConfig::default().auto_reap.enabled);
        let cfg: AppConfig = toml::from_str("port = 8080\nfont_size = 13.0\n").unwrap();
        assert!(cfg.auto_reap.enabled);
        let cfg: AppConfig = toml::from_str("port = 8080\nfont_size = 13.0\n\n[auto_reap]\n").unwrap();
        assert!(cfg.auto_reap.enabled, "an empty [auto_reap] table must still default enabled");
    }

    #[test]
    fn auto_reap_can_be_disabled_via_config() {
        let toml_src = "port = 8080\nfont_size = 13.0\n\n[auto_reap]\nenabled = false\n";
        let cfg: AppConfig = toml::from_str(toml_src).unwrap();
        assert!(!cfg.auto_reap.enabled);
    }

    #[test]
    fn catalogue_options_appends_configured_catalogues_and_skips_duplicate_default() {
        let mut cfg = AppConfig::default();
        cfg.brain.catalogues = vec![
            CatalogueRef { name: "docs".to_string(), path: PathBuf::from("/tmp/docs-brain"), remote: None, endpoint: None, region: None, cache_ttl_secs: None },
            CatalogueRef { name: "default".to_string(), path: PathBuf::from("/tmp/should-be-skipped"), remote: None, endpoint: None, region: None, cache_ttl_secs: None },
        ];
        let options = cfg.catalogue_options();
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].name, "default");
        assert_eq!(options[1].name, "docs");
        assert_eq!(options[1].path, PathBuf::from("/tmp/docs-brain"));
    }

    #[test]
    fn remote_config_for_matches_catalogue_by_path() {
        let mut cfg = AppConfig::default();
        cfg.brain.catalogues = vec![CatalogueRef {
            name: "team".into(),
            path: PathBuf::from("/tmp/team-brain"),
            remote: Some("s3://team-brains/main".into()),
            endpoint: None,
            region: Some("eu-west-1".into()),
            cache_ttl_secs: Some(60),
        }];
        let sync = cfg.remote_config_for(std::path::Path::new("/tmp/team-brain")).unwrap();
        assert_eq!(sync.remote, "s3://team-brains/main");
        assert_eq!(sync.region.as_deref(), Some("eu-west-1"));
        assert_eq!(sync.cache_ttl_secs, 60);
        assert!(cfg.remote_config_for(std::path::Path::new("/tmp/other")).is_none());
    }

    #[test]
    fn remote_config_for_matches_default_brain() {
        let _guard = ENV_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let mut cfg = AppConfig::default();
        cfg.brain.remote = Some("s3://team-brains/default".into());
        let path = cfg.resolved_brain_path();
        let sync = cfg.remote_config_for(&path).unwrap();
        assert_eq!(sync.remote, "s3://team-brains/default");
        assert_eq!(sync.cache_ttl_secs, 0);
    }

    #[test]
    fn catalogue_without_remote_yields_none() {
        let mut cfg = AppConfig::default();
        cfg.brain.catalogues = vec![CatalogueRef {
            name: "local".into(),
            path: PathBuf::from("/tmp/local-brain"),
            remote: None,
            endpoint: None,
            region: None,
            cache_ttl_secs: None,
        }];
        assert!(cfg.remote_config_for(std::path::Path::new("/tmp/local-brain")).is_none());
    }

    #[test]
    fn catalogue_ref_remote_fields_default_to_none_in_toml() {
        let toml_src = "port = 8080\nfont_size = 13.0\n\n[[brain.catalogues]]\nname = \"docs\"\npath = \"/tmp/docs\"\n";
        let cfg: AppConfig = toml::from_str(toml_src).unwrap();
        assert!(cfg.brain.catalogues[0].remote.is_none());
        assert!(cfg.brain.catalogues[0].cache_ttl_secs.is_none());
    }
}

#[cfg(test)]
mod tui_config_tests {
    use super::*;

    fn byte(s: &str) -> Result<u8, String> {
        TuiConfig { prefix: s.into(), ..Default::default() }.prefix_byte()
    }

    #[test]
    fn default_prefix_is_ctrl_backslash_on_macos_and_ctrl_space_elsewhere() {
        let want = if cfg!(target_os = "macos") { 0x1c } else { 0x00 };
        assert_eq!(TuiConfig::default().prefix_byte(), Ok(want));
        assert_eq!(DEFAULT_PREFIX_BYTE, want);
        let cfg: AppConfig = toml::from_str("port = 1\nfont_size = 12.0\n").unwrap();
        assert_eq!(cfg.tui, TuiConfig::default());
    }

    #[test]
    fn the_default_prefix_is_not_pinned_by_a_save_but_a_choice_is() {
        let text = toml::to_string(&AppConfig::default()).unwrap();
        assert!(!text.contains("prefix"), "{text}");
        let mut cfg = AppConfig::default();
        cfg.tui.prefix = "C-g".into();
        let back: AppConfig = toml::from_str(&toml::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back.tui.prefix, "C-g");
    }

    #[test]
    fn parses_ctrl_chords() {
        assert_eq!(byte("C-g"), Ok(0x07));
        assert_eq!(byte("ctrl+G"), Ok(0x07));
        assert_eq!(byte("C-\\"), Ok(0x1c));
        assert_eq!(byte("^]"), Ok(0x1d));
    }

    #[test]
    fn rejects_colliding_and_unusable_prefixes() {
        for bad in ["C-b", "C-a", "C-c", "C-i", "C-m", "C-[", "g", "C-F1"] {
            assert!(byte(bad).is_err(), "{bad} must be rejected");
        }
        assert_eq!(TuiConfig { prefix: "C-b".into(), ..Default::default() }.prefix_byte_or_default(), DEFAULT_PREFIX_BYTE);
    }

    #[test]
    fn tui_table_round_trips_after_scalars() {
        let mut cfg = AppConfig::default();
        cfg.tui.prefix = "C-g".into();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(back.tui.prefix, "C-g");
    }
}
