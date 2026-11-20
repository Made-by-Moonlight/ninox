use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use crate::harness::{HarnessRegistry, HarnessSpec};

const INSTANCE_DB_FILE: &str = "db-path";

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
    /// Agent harness: `"claude-code"`, `"codex"`, `"aider"`, `"opencode"` or `"pi"`.
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
    /// This machine's extra write rules for the default brain. Appended to
    /// the brain's own `_rules.md` and winning where they conflict; never
    /// synced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_rules_override: Option<String>,
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
    /// Whether this source is included in search ("Include in search" in
    /// the Sources view). Defaults to true so a catalogue saved before this
    /// field existed stays visible rather than silently dropping out.
    #[serde(default = "default_catalogue_enabled")]
    pub enabled: bool,
    /// Sync cadence preference — `"manual"` (default), `"on_launch"`, or
    /// `"every_10_min"`. Stored only: there is no background scheduler
    /// (changing the sync engine's semantics is out of scope for the GUI
    /// that reads this), so only an explicit "Sync now" or launch-time
    /// pull actually syncs anything today.
    #[serde(default = "default_cadence")]
    pub cadence: String,
    /// Access preference — `"read_write"` (default) or `"read_only"`.
    /// Stored only: `BrainSync::sync` still always attempts both pull and
    /// push regardless of this value, for the same reason as `cadence`.
    #[serde(default = "default_access")]
    pub access: String,
    /// This machine's extra write rules for the catalogue; same semantics
    /// as `BrainConfig::write_rules_override`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_rules_override: Option<String>,
}

fn default_catalogue_enabled() -> bool {
    true
}

fn default_cadence() -> String {
    "manual".to_string()
}

fn default_access() -> String {
    "read_write".to_string()
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

/// Nudges for a worker that has gone quiet without messaging anyone; see
/// `Poller::check_message_stalls`. On by default: a read-back left only in
/// the worker's own pane otherwise reaches nobody. `0` disables it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageStallConfig {
    #[serde(default = "default_stall_nudge_after_mins")]
    pub nudge_after_mins: u64,
}

fn default_stall_nudge_after_mins() -> u64 {
    30
}

impl Default for MessageStallConfig {
    fn default() -> Self {
        Self { nudge_after_mins: default_stall_nudge_after_mins() }
    }
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

/// Consolidated PR watching, on by default.
///
/// On: one batched GraphQL query per tick covers every watched PR —
/// session-attached PRs plus explicit `ninox open --pr` registry entries
/// (see `store::PrWatch`) — via `github_graphql::GithubBatchApi`.
///
/// Off: the poller's per-session REST polling (`poll_github` +
/// `poll_pr_reconciliation`), which costs several requests per session per
/// tick, keeps the cross-repo 404 self-heal, and never delivers
/// `ninox open --pr` watches.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrWatchConfig {
    #[serde(default = "default_pr_watch_enabled")]
    pub enabled: bool,
}

fn default_pr_watch_enabled() -> bool {
    true
}

impl Default for PrWatchConfig {
    fn default() -> Self {
        Self { enabled: default_pr_watch_enabled() }
    }
}

/// What the automatic reset-from-plan trigger does. Off is the default and
/// the value for anything unrecognised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ResetMode {
    /// No automatic plan request, no reset.
    #[default]
    Off,
    /// Ask for the plan at the threshold, then notify; the user opens the
    /// plan and clicks to reset.
    Prompt,
    /// Ask for the plan at the threshold, then reset without a click. A plan
    /// that is never updated is never reset from.
    Auto,
}

impl<'de> Deserialize<'de> for ResetMode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = ResetMode;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("\"off\", \"prompt\" or \"auto\"")
            }
            fn visit_str<E>(self, v: &str) -> Result<ResetMode, E> {
                Ok(match v.trim().to_ascii_lowercase().as_str() {
                    "prompt" => ResetMode::Prompt,
                    "auto" => ResetMode::Auto,
                    _ => ResetMode::Off,
                })
            }
            fn visit_bool<E>(self, _: bool) -> Result<ResetMode, E> {
                Ok(ResetMode::Off)
            }
            fn visit_i64<E>(self, _: i64) -> Result<ResetMode, E> {
                Ok(ResetMode::Off)
            }
            fn visit_u64<E>(self, _: u64) -> Result<ResetMode, E> {
                Ok(ResetMode::Off)
            }
            fn visit_f64<E>(self, _: f64) -> Result<ResetMode, E> {
                Ok(ResetMode::Off)
            }
        }
        d.deserialize_any(V)
    }
}

pub const DEFAULT_AUTO_RESET_AT_CONTEXT_PCT: u8 = 40;
/// The range `auto_at_context_pct` is accepted in.
pub const AUTO_RESET_PCT_RANGE: std::ops::RangeInclusive<u8> = 10..=95;

/// `[orchestrator_reset]`: reset an orchestrator onto a fresh conversation,
/// continuing from its plan doc, once its context window is
/// `auto_at_context_pct` percent full. Off by default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrchestratorResetConfig {
    /// The threshold only; `mode` switches the feature on and off.
    #[serde(default = "default_auto_reset_pct")]
    pub auto_at_context_pct: u8,
    #[serde(default)]
    pub mode: ResetMode,
}

fn default_auto_reset_pct() -> u8 {
    DEFAULT_AUTO_RESET_AT_CONTEXT_PCT
}

impl Default for OrchestratorResetConfig {
    fn default() -> Self {
        Self { auto_at_context_pct: default_auto_reset_pct(), mode: ResetMode::default() }
    }
}

impl OrchestratorResetConfig {
    pub fn enabled(&self) -> bool {
        self.mode != ResetMode::Off
    }

    /// The threshold, held to [`AUTO_RESET_PCT_RANGE`] whatever the file says.
    pub fn threshold(&self) -> u8 {
        self.auto_at_context_pct.clamp(*AUTO_RESET_PCT_RANGE.start(), *AUTO_RESET_PCT_RANGE.end())
    }
}

/// Which prebuilt release stream `ninox update` and the daemon's update
/// check follow. The alpha channel is a separate CodeArtifact package, so a
/// stable user never receives an alpha unless they opt in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdateChannel {
    #[default]
    Stable,
    Alpha,
}

impl UpdateChannel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Alpha => "alpha",
        }
    }
}

impl std::fmt::Display for UpdateChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for UpdateChannel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "stable" => Ok(Self::Stable),
            "alpha" => Ok(Self::Alpha),
            other => Err(format!("unknown update channel {other:?} (expected `stable` or `alpha`)")),
        }
    }
}

/// `[update]` — see [`UpdateChannel`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UpdateConfig {
    #[serde(default)]
    pub channel: UpdateChannel,
    /// AWS profile used for the CodeArtifact update lookups when the
    /// process has no `AWS_PROFILE` of its own (a launchd-started server
    /// never inherits the shell the installer ran in).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_profile: Option<String>,
}

pub const GITHUB_MIN_POLL_INTERVAL_SECS: u64 = 15;

/// Cadence of the batched GitHub poll. Defaults suit a handful of PRs;
/// raise `poll_interval_secs` for fleets tracking many.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GithubConfig {
    /// Seconds between light status sweeps (floored at 15).
    #[serde(default = "default_github_poll_interval_secs")]
    pub poll_interval_secs:  u64,
    /// Longest a PR's full detail (checks, reviews, comments) may go
    /// unread when its light status shows no change.
    #[serde(default = "default_github_detail_refresh_secs")]
    pub detail_refresh_secs: u64,
}

fn default_github_poll_interval_secs() -> u64 { 60 }
fn default_github_detail_refresh_secs() -> u64 { 600 }

impl Default for GithubConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs:  default_github_poll_interval_secs(),
            detail_refresh_secs: default_github_detail_refresh_secs(),
        }
    }
}

impl GithubConfig {
    pub fn effective_poll_interval_secs(&self) -> u64 {
        self.poll_interval_secs.max(GITHUB_MIN_POLL_INTERVAL_SECS)
    }

    pub fn effective_detail_refresh_secs(&self) -> u64 {
        self.detail_refresh_secs.max(GITHUB_MIN_POLL_INTERVAL_SECS)
    }
}

/// `[idle_workers]` — what the Idle view, `ninox idle` and the "prompt
/// orchestrators" action treat as an idle worker, and how often one
/// orchestrator may be prompted about the same set. `0` minutes counts
/// every Idle/Blocked worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IdleWorkersConfig {
    #[serde(default = "default_idle_minutes")]
    pub idle_minutes: u64,
    #[serde(default = "default_prompt_cooldown_minutes")]
    pub prompt_cooldown_minutes: u64,
}

fn default_idle_minutes() -> u64 {
    30
}

fn default_prompt_cooldown_minutes() -> u64 {
    30
}

impl Default for IdleWorkersConfig {
    fn default() -> Self {
        Self { idle_minutes: default_idle_minutes(), prompt_cooldown_minutes: default_prompt_cooldown_minutes() }
    }
}

/// Thresholds for `crate::pr_events`. Unlike most opt-in tables this is on
/// by default: a worker waiting on a quiet PR otherwise hangs silently.
/// `0` disables the corresponding check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrEventsConfig {
    /// A check queued or running this long on the same head SHA is
    /// reported once as `ci_pending_stuck`.
    #[serde(default = "default_ci_stuck_after_mins")]
    pub ci_stuck_after_mins:   u64,
    /// A worker idle this long, with an open PR and no PR event for as long,
    /// gets a status nudge; its orchestrator is told once per quiet spell.
    #[serde(default = "default_hang_nudge_after_mins")]
    pub hang_nudge_after_mins: u64,
}

fn default_ci_stuck_after_mins() -> u64 {
    45
}

fn default_hang_nudge_after_mins() -> u64 {
    30
}

impl Default for PrEventsConfig {
    fn default() -> Self {
        Self {
            ci_stuck_after_mins:   default_ci_stuck_after_mins(),
            hang_nudge_after_mins: default_hang_nudge_after_mins(),
        }
    }
}

/// Hard cap on `ninox spawn-sub-orchestrator` nesting depth. A request that
/// would exceed it is rejected outright — see
/// `docs/superpowers/specs/2026-10-21-nested-orchestration-design.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NestedOrchestrationConfig {
    #[serde(default = "default_max_orchestrator_depth")]
    pub max_depth: u32,
}

fn default_max_orchestrator_depth() -> u32 {
    3
}

impl Default for NestedOrchestrationConfig {
    fn default() -> Self {
        Self { max_depth: default_max_orchestrator_depth() }
    }
}

/// Ghost-plan proposals (`ninox plan propose`): a pending plan that nobody
/// accepts within `expiry_days` is marked expired, never auto-accepted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanProposalsConfig {
    #[serde(default = "default_proposal_expiry_days")]
    pub expiry_days: u32,
}

fn default_proposal_expiry_days() -> u32 {
    7
}

impl Default for PlanProposalsConfig {
    fn default() -> Self {
        Self { expiry_days: default_proposal_expiry_days() }
    }
}

impl PlanProposalsConfig {
    pub fn expiry_ms(&self) -> i64 {
        i64::from(self.expiry_days.max(1)) * 24 * 60 * 60 * 1000
    }
}

// ---------------------------------------------------------------------------
// Role rehydration configuration
// ---------------------------------------------------------------------------

/// Opt-in (default OFF) session self-identification (`ninox whoami` and its
/// `UserPromptSubmit`/`SessionStart` reminder hooks) — see
/// `docs/superpowers/specs/2026-10-05-session-self-identification-design.md`.
/// Off (default) while hook-wiring rolls out: the `whoami` capability isn't
/// advertised and no reminder hooks are seeded, but the `ninox whoami`
/// command itself always works regardless of this flag.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoleRehydrationConfig {
    #[serde(default)]
    pub enabled: bool,
}

// ---------------------------------------------------------------------------
// Concurrent session limits
// ---------------------------------------------------------------------------

/// Per-location cap on concurrently running worker sessions; spawns over the
/// cap are queued (see `crate::session_limits`). `0` means unlimited.
/// Orchestrators and the prompt agent don't count and are never queued.
///
/// Remote limits live here keyed by `MachineProfile::id` rather than on the
/// profile itself, which must stay pure connection metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitsConfig {
    /// Local worker cap (not orchestrators, not the prompt agent).
    #[serde(default = "default_max_concurrent_local")]
    pub max_concurrent_local: u32,
    /// Absent machine = unlimited.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub remote: BTreeMap<String, u32>,
}

fn default_max_concurrent_local() -> u32 {
    6
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self { max_concurrent_local: default_max_concurrent_local(), remote: BTreeMap::new() }
    }
}

impl LimitsConfig {
    /// `None` = unlimited. `machine_id` `None` = local.
    pub fn limit_for(&self, machine_id: Option<&str>) -> Option<u32> {
        let raw = match machine_id {
            None => self.max_concurrent_local,
            Some(id) => self.remote.get(id).copied().unwrap_or(0),
        };
        (raw > 0).then_some(raw)
    }

    /// `limit` `None`/`Some(0)` = unlimited.
    pub fn set_limit(&mut self, machine_id: Option<&str>, limit: Option<u32>) {
        let raw = limit.unwrap_or(0);
        match machine_id {
            None => self.max_concurrent_local = raw,
            Some(id) if raw == 0 => { self.remote.remove(id); }
            Some(id) => { self.remote.insert(id.to_string(), raw); }
        }
    }
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

// ---------------------------------------------------------------------------
// Shared build cache (per-repo)
// ---------------------------------------------------------------------------

/// `[workspaces]` — worktree disk hygiene and per-repo build env.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspacesConfig {
    /// Delete a session's gitignored build-output dirs once it finishes
    /// (Done/Terminated, self-completed, or merged and idle), even when the
    /// worktree itself is kept for uncommitted or unpushed work.
    #[serde(default = "default_free_build_output_on_finish")]
    pub free_build_output_on_finish: bool,
    /// Worktree-relative build-output dirs eligible for freeing. Each is
    /// deleted only if git ignores it and it holds no tracked or untracked
    /// source.
    #[serde(default = "default_build_output_dirs")]
    pub build_output_dirs: Vec<String>,
    /// Free-space percentage on the repos' volume below which the poller
    /// frees finished builds, sweeps orphans, then notifies.
    #[serde(default = "default_disk_low_percent")]
    pub disk_low_percent: f64,
    /// `[[workspaces.shared_build]]` — opt-in per-repo env vars injected
    /// into every spawned worker whose workspace is inside that repo, e.g.
    /// to route `cargo` through a shared compile cache so parallel
    /// worktrees don't each pay a full rebuild. Default empty.
    #[serde(default)]
    pub shared_build: Vec<SharedBuildEntry>,
}

fn default_free_build_output_on_finish() -> bool {
    true
}

fn default_build_output_dirs() -> Vec<String> {
    vec!["target".into(), "node_modules/.cache".into()]
}

fn default_disk_low_percent() -> f64 {
    10.0
}

impl Default for WorkspacesConfig {
    fn default() -> Self {
        Self {
            free_build_output_on_finish: default_free_build_output_on_finish(),
            build_output_dirs: default_build_output_dirs(),
            disk_low_percent: default_disk_low_percent(),
            shared_build: Vec::new(),
        }
    }
}

/// One repo's shared-build env injection. `repo` is the repo's main
/// checkout (not a worktree) — a worker's workspace matches an entry when
/// it is that path or a descendant of it (e.g. a `.claude/worktrees/<br>`
/// worktree); when more than one entry matches, the one with the longest
/// (most specific) `repo` path wins. Each value in `env` may contain a
/// literal `{repo}` placeholder, substituted with this entry's `repo` path.
///
/// Do not point `CARGO_TARGET_DIR`, or cargo's `build.build-dir`, at one
/// literal path shared across worktrees of the same repo: cargo's unit
/// hash doesn't disambiguate two worktrees building the same crate
/// name/version, so `cargo test --workspace` can silently reuse another
/// worktree's stale or deleted test binary — confirmed for both
/// mechanisms, not just `CARGO_TARGET_DIR` (see
/// `docs/superpowers/plans/2026-10-29-shared-build-cache-plan.md`). Prefer
/// `RUSTC_WRAPPER = "sccache"`, which caches compiled objects by content
/// hash rather than by target-dir path, so it stays correct across
/// worktrees while still sharing the compile cache.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedBuildEntry {
    pub repo: PathBuf,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Env var name prefixes/exact names ninox's own spawn plumbing relies on
/// (session attribution, config/brain discovery, the cross-session
/// messaging gate) — a `[[workspaces.shared_build]].env` entry can never
/// override these, so a typo'd or copy-pasted key in user config can't
/// silently break a worker's own identity or messaging.
fn is_reserved_env_key(key: &str) -> bool {
    key.starts_with("NINOX_") || key == crate::session_socket::CLAUDE_MESSAGING_GATE_ENV
}

impl WorkspacesConfig {
    /// Env vars from the most specific (longest `repo` path) entry that is
    /// an ancestor of (or equal to) `workspace`, with `{repo}` substituted
    /// in each value and any reserved ninox env key dropped. `None` if no
    /// entry matches `workspace` at all (distinct from `Some` of an empty
    /// map, which a user can configure to explicitly opt a repo out of
    /// `effective_shared_build_env_for`'s ninox default below).
    fn shared_build_env_for(&self, workspace: &Path) -> Option<Vec<(String, String)>> {
        let canon_workspace = std::fs::canonicalize(workspace).unwrap_or_else(|e| {
            tracing::warn!("canonicalize({}) failed, matching against the raw path: {e}", workspace.display());
            workspace.to_path_buf()
        });
        self.shared_build
            .iter()
            .filter_map(|entry| {
                let canon_repo = std::fs::canonicalize(&entry.repo).unwrap_or_else(|e| {
                    tracing::warn!("canonicalize({}) failed, matching against the raw path: {e}", entry.repo.display());
                    entry.repo.clone()
                });
                canon_workspace.starts_with(&canon_repo).then_some((canon_repo, entry))
            })
            .max_by_key(|(canon_repo, _)| canon_repo.as_os_str().len())
            .map(|(_, entry)| {
                let repo_str = entry.repo.display().to_string();
                entry
                    .env
                    .iter()
                    .filter(|(k, _)| !is_reserved_env_key(k))
                    .map(|(k, v)| (k.clone(), v.replace("{repo}", &repo_str)))
                    .collect()
            })
    }

    /// `shared_build_env_for`, falling back to ninox's own default for its
    /// own repo (identified by its known crate layout, since a checkout
    /// path can't be predicted across installs) when no explicit entry
    /// matches `workspace`: `CARGO_INCREMENTAL=0` and
    /// `CARGO_PROFILE_DEV_DEBUG=line-tables-only` always (incremental
    /// artifacts are a large share of `target/`'s size, and sccache can't
    /// cache an incremental build anyway), plus `RUSTC_WRAPPER=sccache`
    /// when `sccache` is on `PATH`. None of this carries the stale-artifact
    /// risk of a shared `CARGO_TARGET_DIR`/`build-dir` (see
    /// `SharedBuildEntry`'s doc comment): every worktree keeps its own
    /// target dir, and sccache only shares compiled objects, keyed by
    /// content hash. A worker without `sccache` installed still gets the
    /// disk-reducing flags.
    pub fn effective_shared_build_env_for(&self, workspace: &Path) -> Vec<(String, String)> {
        if let Some(env) = self.shared_build_env_for(workspace) {
            return env;
        }
        if !is_ninox_repo(workspace) {
            return Vec::new();
        }
        let mut env = vec![
            ("CARGO_INCREMENTAL".to_string(), "0".to_string()),
            ("CARGO_PROFILE_DEV_DEBUG".to_string(), "line-tables-only".to_string()),
        ];
        if sccache_on_path() {
            env.push(("RUSTC_WRAPPER".to_string(), "sccache".to_string()));
        }
        env
    }
}

/// Ninox's own crate layout plus a `Cargo.toml` naming `ninox-core`, used
/// only to default the shared-build-cache feature on for ninox's own repo
/// without requiring every install to hand-configure its own checkout
/// path. The extra `Cargo.toml` check narrows an otherwise-plausible
/// false positive on an unrelated repo that happens to share these three
/// directory names.
fn is_ninox_repo(workspace: &Path) -> bool {
    workspace.join("crates/ninox-core").is_dir()
        && workspace.join("crates/ninox-ui-shared").is_dir()
        && workspace.join("crates/ninox-tui").is_dir()
        && std::fs::read_to_string(workspace.join("Cargo.toml")).is_ok_and(|s| s.contains("ninox-core"))
}

fn sccache_on_path() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| binary_on_path("sccache"))
}

fn binary_on_path(name: &str) -> bool {
    std::process::Command::new(name)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
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

/// Legacy `[auto_reap] enabled` toggle, superseded by `[worker_completion]`.
/// Still parsed so an existing config keeps its explicit choice: when
/// `[worker_completion] mode` is unset, `true` maps to `reap_on_merge` and
/// `false` to `manual` (see `AppConfig::completion_mode`).
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

/// `[worker_completion] mode` — who ends a worker once its PR merges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionMode {
    /// The worker decides it is done and finishes itself with
    /// `ninox complete`. A merge never reaps; it only nudges the worker.
    #[default]
    SelfComplete,
    /// Legacy `auto_reap = true`: Ninox reaps the worker at merge when its
    /// Definition of done is only "PR merged".
    ReapOnMerge,
    /// Legacy `auto_reap = false`: the worker is kept alive after merge and
    /// the orchestrator (or user) reaps it.
    Manual,
}

impl CompletionMode {
    pub const ALL: [CompletionMode; 3] = [Self::SelfComplete, Self::ReapOnMerge, Self::Manual];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::SelfComplete => "self",
            Self::ReapOnMerge  => "reap_on_merge",
            Self::Manual       => "manual",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == s)
    }
}

/// `[worker_completion]`. `mode` is deliberately optional so an absent value
/// can fall back to the legacy `[auto_reap]` choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerCompletionConfig {
    #[serde(default, skip_serializing_if = "Option::is_none", with = "completion_mode_opt")]
    pub mode: Option<CompletionMode>,
    /// Minutes a worker may sit idle after its PR merged before it gets one
    /// reminder to finish with `ninox complete`; 0 disables the reminder.
    #[serde(default = "default_idle_after_merge_nudge_mins")]
    pub idle_after_merge_nudge_mins: u64,
}

fn default_idle_after_merge_nudge_mins() -> u64 {
    30
}

impl Default for WorkerCompletionConfig {
    fn default() -> Self {
        Self { mode: None, idle_after_merge_nudge_mins: default_idle_after_merge_nudge_mins() }
    }
}

mod completion_mode_opt {
    use super::CompletionMode;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(m: &Option<CompletionMode>, s: S) -> Result<S::Ok, S::Error> {
        match m {
            Some(m) => s.serialize_str(m.as_str()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<CompletionMode>, D::Error> {
        let raw = Option::<String>::deserialize(d)?;
        match raw {
            None => Ok(None),
            Some(s) => CompletionMode::parse(&s).map(Some).ok_or_else(|| {
                serde::de::Error::custom(format!(
                    "unknown worker_completion mode `{s}` (expected `self`, `reap_on_merge` or `manual`)"
                ))
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Orphan sweep configuration
// ---------------------------------------------------------------------------

/// Opt-out (default ON) periodic sweep of `.claude/worktrees/*` dirs with no
/// live session behind them — see `ninox_core::workspace::sweep_repo_orphans`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrphanSweepConfig {
    #[serde(default = "default_orphan_sweep_enabled")]
    pub enabled: bool,
    /// Hours between sweeps. Default: 1.
    #[serde(default = "default_orphan_sweep_interval_hours")]
    pub interval_hours: u64,
}

fn default_orphan_sweep_enabled() -> bool {
    true
}

fn default_orphan_sweep_interval_hours() -> u64 {
    1
}

impl Default for OrphanSweepConfig {
    fn default() -> Self {
        Self {
            enabled: default_orphan_sweep_enabled(),
            interval_hours: default_orphan_sweep_interval_hours(),
        }
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

/// Default UI zoom factor (`AppConfig::zoom`) — 1.0 is unscaled. Used by
/// serde when the field is absent from an older config file.
fn default_zoom() -> f64 { 1.0 }

/// Default server port (`AppConfig::port`) — used by serde when the field
/// is absent from an older config file.
fn default_port() -> u16 { 8080 }

/// Default UI font size (`AppConfig::font_size`) — used by serde when the
/// field is absent from an older config file.
fn default_font_size() -> f32 { 13.0 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_port")]
    pub port:      u16,
    #[serde(default = "default_font_size")]
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
    /// Agent harness and model for orchestrator sessions.
    #[serde(default)]
    pub orchestrator: AgentConfig,
    /// Agent harness and model for worker sessions spawned by `ninox spawn`.
    #[serde(default)]
    pub worker: AgentConfig,
    /// Agent harness and model for the `ninox plan generate` planning
    /// subprocess — independent of `orchestrator`/`worker`, so planning can
    /// run on a different model than the orchestrator that asked for it.
    #[serde(default)]
    pub planning: AgentConfig,
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
    /// Consolidated batched-GraphQL PR watching, default on — see
    /// `PrWatchConfig`.
    #[serde(default)]
    pub pr_watch: PrWatchConfig,
    /// Automatic reset-from-plan at a context threshold — see
    /// `OrchestratorResetConfig`.
    #[serde(default)]
    pub orchestrator_reset: OrchestratorResetConfig,
    /// Release channel for `ninox update` and the daemon's update check.
    #[serde(default)]
    pub update: UpdateConfig,
    #[serde(default)]
    pub github: GithubConfig,
    #[serde(default)]
    pub pr_events: PrEventsConfig,
    #[serde(default)]
    pub message_stalls: MessageStallConfig,
    /// Hard cap on `ninox spawn-sub-orchestrator` nesting depth, default 3
    /// — see `NestedOrchestrationConfig`.
    #[serde(default)]
    pub nested_orchestration: NestedOrchestrationConfig,
    #[serde(default)]
    pub plan_proposals: PlanProposalsConfig,
    /// Legacy `[auto_reap]`; read only when `[worker_completion] mode` is
    /// unset — see `AppConfig::completion_mode`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_reap: Option<AutoReapConfig>,
    /// Who ends a worker after its PR merges — see `WorkerCompletionConfig`.
    #[serde(default)]
    pub worker_completion: WorkerCompletionConfig,
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
    /// The Brain graph's "Whole brain" minimap is collapsed to a pill.
    #[serde(default)]
    pub brain_minimap_collapsed: bool,
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
    /// Session self-identification (`whoami`) capability and reminder hooks.
    /// Opt-in, default off — see `RoleRehydrationConfig`.
    #[serde(default)]
    pub role_rehydration: RoleRehydrationConfig,
    /// Per-repo shared build caches injected into spawned workers. Opt-in,
    /// default empty — see `WorkspacesConfig`.
    #[serde(default)]
    pub workspaces: WorkspacesConfig,
    /// Idle-worker thresholds — see `IdleWorkersConfig`.
    #[serde(default)]
    pub idle_workers: IdleWorkersConfig,
    /// Periodic orphaned-worktree sweep. Opt-out, default on — see
    /// `OrphanSweepConfig`.
    #[serde(default)]
    pub orphan_sweep: OrphanSweepConfig,
    /// Per-location concurrent session caps — see `LimitsConfig`.
    #[serde(default)]
    pub limits: LimitsConfig,
    /// Agent-harness registry overrides/extensions (`[harnesses.<name>]`).
    /// Builtin specs for claude-code/codex/opencode/ori/aider/pi apply
    /// when a name is absent here. See `crate::harness`. Kept last so TOML
    /// serialization emits this table-of-tables after every scalar field.
    #[serde(default)]
    pub harnesses: BTreeMap<String, HarnessSpec>,
    /// Worker-type registry overrides/extensions (`[[worker_types]]`).
    /// Builtin Orchestrator/Implementor/Researcher/Evaluator specs apply
    /// when a name is absent here; a same-named entry (matched by canonical
    /// name — see `crate::worker_type::canonical_name`) replaces a builtin
    /// wholesale. See `crate::worker_type`. An array-of-tables, so — like
    /// `harnesses` — it must also serialize after every scalar field.
    #[serde(default)]
    pub worker_types: Vec<crate::worker_type::WorkerTypeSpec>,
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
            orchestrator:     AgentConfig::default(),
            worker:           AgentConfig::default(),
            planning:         AgentConfig::default(),
            github_token:     None,
            brain:            BrainConfig::default(),
            brain_harvest:    BrainHarvestConfig::default(),
            session_retention: SessionRetentionConfig::default(),
            theme_file:       None,
            sidebar_width:    default_sidebar_width(),
            brain_minimap_collapsed: false,
            runtime:          RuntimeConfig::default(),
            harnesses:        BTreeMap::new(),
            worker_types:     Vec::new(),
            messaging:        None,
            inbox_messaging:  InboxMessagingConfig::default(),
            pr_watch:         PrWatchConfig::default(),
            orchestrator_reset: OrchestratorResetConfig::default(),
            update:           UpdateConfig::default(),
            github:           GithubConfig::default(),
            pr_events:        PrEventsConfig::default(),
            message_stalls:   MessageStallConfig::default(),
            nested_orchestration: NestedOrchestrationConfig::default(),
            plan_proposals: PlanProposalsConfig::default(),
            auto_reap:        None,
            worker_completion: WorkerCompletionConfig::default(),
            fleet:            FleetConfig::default(),
            tui:              TuiConfig::default(),
            remote_machines:  RemoteMachinesConfig::default(),
            role_rehydration: RoleRehydrationConfig::default(),
            workspaces:       WorkspacesConfig::default(),
            idle_workers:     IdleWorkersConfig::default(),
            orphan_sweep:     OrphanSweepConfig::default(),
            limits:           LimitsConfig::default(),
        }
    }
}

/// A `config.toml` that exists on disk but failed to parse — distinct from
/// the file simply being absent (which `load()`/`load_detailed()` treat as
/// `Ok(Self::default())`). Carries the 1-indexed line/column the `toml`
/// crate's error points at, when it has one, so the CLI/TUI/GUI can tell
/// the user exactly where to look rather than just "it's broken".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigParseError {
    pub message: String,
    pub line: Option<usize>,
    pub column: Option<usize>,
}

impl std::fmt::Display for ConfigParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.line {
            Some(line) => write!(f, "line {line}: {}", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for ConfigParseError {}

impl ConfigParseError {
    /// The single sentence every surface (CLI stderr, TUI footer, GUI
    /// banner) shows verbatim — see MLOPS-5457.
    pub fn banner_message(&self) -> String {
        match self.line {
            Some(line) => format!(
                "config.toml has an error on line {line}: {}. Running with defaults; settings won't be saved until it's fixed.",
                self.message
            ),
            None => format!(
                "config.toml has an error: {}. Running with defaults; settings won't be saved until it's fixed.",
                self.message
            ),
        }
    }

    fn from_toml(src: &str, e: toml::de::Error) -> Self {
        let (line, column) = match e.span() {
            Some(span) => {
                let (l, c) = line_col(src, span.start);
                (Some(l), Some(c))
            }
            None => (None, None),
        };
        // `toml::de::Error::message()` can span multiple lines (e.g.
        // "invalid array\nexpected `]`"); join to one space-separated line
        // so every surface (TUI footer especially) gets a single-line
        // message instead of squished-together or wrapped raw text.
        let message = e.message().lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ");
        Self { message, line, column }
    }

    /// Compact single-line form for the TUI footer, where space is scarce
    /// and the caller truncates with an ellipsis at the window edge (see
    /// `tui::view::draw_footer`). `banner_message()` is used everywhere
    /// else (GUI banner, CLI stderr), which has room for the fuller text.
    pub fn tui_footer_message(&self) -> String {
        match self.line {
            Some(line) => format!("config.toml line {line}: {} · running with defaults, saves disabled", self.message),
            None => format!("config.toml: {} · running with defaults, saves disabled", self.message),
        }
    }
}

/// 1-indexed (line, column) of `byte_offset` within `src`. Clamps the
/// offset to `src`'s length so a span pointing just past EOF (e.g. an
/// unterminated string) can't panic on an out-of-bounds slice.
fn line_col(src: &str, byte_offset: usize) -> (usize, usize) {
    let offset = byte_offset.min(src.len());
    let prefix = &src[..offset];
    let line = prefix.matches('\n').count() + 1;
    let column = match prefix.rfind('\n') {
        Some(nl) => offset - nl,
        None => offset + 1,
    };
    (line, column)
}

/// Writes `text` to `path` via a same-directory temp file + `fs::rename`
/// (atomic on the same filesystem, so neither a crash mid-write nor a
/// concurrent reader ever observes a torn file). Preserves the original
/// file's permissions when present (a `rename` replaces the whole inode, so
/// a plain `fs::write` would otherwise reset them to the process umask) and
/// `fsync`s the temp file before the rename.
pub(crate) fn write_file_atomic(path: &Path, text: &str) -> Result<()> {
    // Resolve through a symlink first: a rename onto `path` itself would
    // replace the link with a plain file (silently breaking a dotfiles-repo
    // symlink setup for `config.toml`). Falls back to `path` unchanged when
    // nothing exists there yet (a brand-new file, nothing to resolve).
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = target.with_file_name(format!(
        "{}.tmp.{}",
        target.file_name().unwrap().to_string_lossy(),
        std::process::id()
    ));
    let result = (|| -> Result<()> {
        fs::write(&tmp, text)?;
        #[cfg(unix)]
        if let Ok(existing) = fs::metadata(&target) {
            fs::set_permissions(&tmp, existing.permissions())?;
        }
        fs::File::open(&tmp)?.sync_all()?;
        fs::rename(&tmp, &target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Backs up `path` to `<path>.bak` if it currently exists, then writes
/// `text` via [`write_file_atomic`] — the single choke point every
/// `config.toml` persist path (`AppConfig::save`, `ninox config set`)
/// funnels through, so a crash or a bad edit always leaves a recent copy
/// behind.
pub(crate) fn backup_and_write_atomic(path: &Path, text: &str) -> Result<()> {
    if path.exists() {
        let bak = path.with_file_name(format!("{}.bak", path.file_name().unwrap().to_string_lossy()));
        fs::copy(path, &bak)?;
    }
    write_file_atomic(path, text)
}

impl AppConfig {
    /// The effective harness registry: builtin specs overlaid by this
    /// config's `[harnesses.*]` entries.
    pub fn registry(&self) -> HarnessRegistry {
        HarnessRegistry::from_config(&self.harnesses)
    }

    /// The effective worker-type registry: builtin specs overlaid by this
    /// config's `[[worker_types]]` entries.
    pub fn worker_type_registry(&self) -> crate::worker_type::WorkerTypeRegistry {
        crate::worker_type::WorkerTypeRegistry::from_config(&self.worker_types)
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

    /// Insert or replace a `[[worker_types]]` entry by CANONICAL name —
    /// mirrors `toggle_harness`'s whole-entry-replace semantics (a config
    /// entry fully overrides a same-named builtin, per `WorkerTypeRegistry`).
    /// Matched through `worker_type::canonical_name` rather than a raw
    /// string compare, so this replaces (rather than duplicates) an
    /// existing entry stored under a different alias of the same type —
    /// e.g. upserting "Implementor" finds and replaces a raw
    /// `name = "Implementation"` entry already on disk, instead of
    /// appending a second, unreachable-by-reset entry alongside it.
    pub fn upsert_worker_type(&mut self, spec: crate::worker_type::WorkerTypeSpec) {
        let spec = spec.for_config();
        let canonical = crate::worker_type::canonical_name(&spec.name);
        let same = |s: &crate::worker_type::WorkerTypeSpec| crate::worker_type::canonical_name(&s.name) == canonical;
        match self.worker_types.iter().position(same) {
            Some(at) => {
                self.worker_types[at] = spec;
                let mut seen = false;
                // A second entry for the same type (e.g. `prompt` and
                // `Ninox`) would otherwise override this edit on load.
                self.worker_types.retain(|s| !same(s) || !std::mem::replace(&mut seen, true));
            }
            None => self.worker_types.push(spec),
        }
    }

    /// Remove a `[[worker_types]]` entry by CANONICAL name (see
    /// `upsert_worker_type`'s doc comment on alias matching). For a
    /// customized builtin this reverts it to the compiled-in default (still
    /// listed by `worker_type_registry()`); for a pure custom type this
    /// removes it from the registry entirely. `false` (no-op) when no
    /// config entry exists for `name` — an un-customized builtin has
    /// nothing to remove.
    pub fn remove_worker_type(&mut self, name: &str) -> bool {
        let canonical = crate::worker_type::canonical_name(name);
        let before = self.worker_types.len();
        self.worker_types.retain(|s| crate::worker_type::canonical_name(&s.name) != canonical);
        self.worker_types.len() != before
    }

    /// Directory worker-type exports are written to:
    /// `<instance_dir>/worker-types/`, beside `ninox_bin_dir()`. Honors
    /// `NINOX_WORKER_TYPE_EXPORTS` as an override, mirroring
    /// `NINOX_CONFIG`/`NINOX_BRAIN`, so tests never write under a
    /// developer's real config directory.
    pub fn worker_type_exports_dir() -> PathBuf {
        if let Some(p) = crate::test_isolation::env_path("NINOX_WORKER_TYPE_EXPORTS") {
            return p;
        }
        Self::instance_dir().join("worker-types")
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
    /// Falls back to `self.brain.path` when set, else `<instance_dir>/brain`.
    pub fn resolved_brain_path(&self) -> PathBuf {
        if let Some(p) = crate::test_isolation::env_path("NINOX_BRAIN") {
            return p;
        }
        if let Some(ref p) = self.brain.path {
            return p.clone();
        }
        Self::instance_dir().join("brain")
    }

    /// The default catalogue's own location, ignoring the `NINOX_BRAIN`
    /// override — inside a session that env var names the session's write
    /// catalogue, which may be any catalogue, so it can't identify "default".
    pub fn configured_brain_path(&self) -> PathBuf {
        self.brain.path.clone().unwrap_or_else(|| Self::instance_dir().join("brain"))
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
            enabled: true,
            cadence: default_cadence(),
            access: default_access(),
            write_rules_override: self.brain.write_rules_override.clone(),
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

    /// `catalogue_options()`'s names, without resolving the default
    /// catalogue's path.
    pub fn catalogue_names(&self) -> Vec<String> {
        std::iter::once("default".to_string())
            .chain(self.brain.catalogues.iter().filter(|c| c.name != "default").map(|c| c.name.clone()))
            .collect()
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
        if self.brain.remote.is_some() && brain_path == self.configured_brain_path() {
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
        self.orchestrator_root.clone().unwrap_or_else(crate::test_isolation::default_orchestrator_root)
    }

    /// The store a command uses without `--db`: the one a scoped instance
    /// recorded in its `db-path` file (see [`Self::register_instance_db`]),
    /// else next to a scratch `NINOX_CONFIG` (see
    /// [`crate::test_isolation::scratch_config_root`]), otherwise
    /// `<data_local_dir>/ninox/ninox.db`.
    pub fn default_db_path() -> PathBuf {
        if let Some(p) = Self::registered_instance_db() {
            return p;
        }
        crate::test_isolation::scratch_config_root()
            .unwrap_or_else(crate::test_isolation::data_root)
            .join("ninox.db")
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
        if let Some(p) = crate::test_isolation::config_file() {
            return p;
        }
        if let Some(p) = crate::test_isolation::env_path("NINOX_CONFIG") {
            return p;
        }
        crate::test_isolation::config_root().join("config.toml")
    }

    /// Directory for Ninox-managed shell wrappers and the `ninox` self-shim,
    /// prepended to every spawned session's PATH: `<instance_dir>/bin`, so a
    /// scratch or trial instance never writes (or resolves `ninox` through)
    /// another instance's shim.
    pub fn ninox_bin_dir() -> PathBuf {
        Self::instance_dir().join("bin")
    }

    /// Whether this process runs a non-default instance (a `NINOX_CONFIG`
    /// or `NINOX_DATA_DIR` outside the real per-user dirs).
    pub fn is_scoped_instance() -> bool {
        crate::test_isolation::instance_root().is_some()
    }

    /// Root of this instance's own bin dir, worker-type exports and default
    /// brain: [`crate::test_isolation::instance_root`] for a non-default
    /// instance, `<config_dir>/ninox` (today's paths) for the default one.
    fn instance_dir() -> PathBuf {
        crate::test_isolation::instance_root().unwrap_or_else(crate::test_isolation::config_root)
    }

    /// Records the store a scoped instance opened in `<instance_dir>/db-path`,
    /// so `ninox` inside its sessions (which inherit its `NINOX_CONFIG` /
    /// `NINOX_DATA_DIR`, not its `--db`) opens the same store. Keyed by the
    /// instance dir rather than an env var, so a different instance started
    /// from inside one of those sessions never inherits it.
    pub fn register_instance_db(path: &Path) -> std::io::Result<()> {
        let Some(root) = crate::test_isolation::instance_root() else { return Ok(()) };
        let path = std::path::absolute(path)?;
        let file = root.join(INSTANCE_DB_FILE);
        if fs::read_to_string(&file).is_ok_and(|s| Path::new(s.trim()) == path) {
            return Ok(());
        }
        fs::create_dir_all(&root)?;
        fs::write(file, path.to_string_lossy().as_bytes())
    }

    fn registered_instance_db() -> Option<PathBuf> {
        let root = crate::test_isolation::instance_root()?;
        let raw = fs::read_to_string(root.join(INSTANCE_DB_FILE)).ok()?;
        Some(PathBuf::from(raw.trim())).filter(|p| p.is_absolute())
    }

    /// The directory `config_path()`'s `config.toml` lives in —
    /// `NINOX_CONFIG`'s parent directory when that override is set (it's a
    /// path to the file itself, so its parent is the directory), else
    /// `<config_dir>/ninox`. `sessions_dir`/`prompt_agent_workspace_dir`
    /// both derive from this, so redirecting the whole per-machine ninox
    /// data root for a test or a sandboxed run is one env var
    /// (`NINOX_CONFIG`), not one per directory.
    fn config_dir() -> PathBuf {
        if let Some(p) = crate::test_isolation::env_path("NINOX_CONFIG") {
            if let Some(parent) = p.parent() {
                return parent.to_path_buf();
            }
        }
        crate::test_isolation::config_root()
    }

    /// Directory where per-session metadata JSON files are written by wrapper hooks.
    ///
    /// Honors `NINOX_DATA_DIR` directly as a dedicated override — the same
    /// one several `ninox-ui-shared` CLI call sites (`ninox send`, `ninox
    /// request-work`, the inbox-messaging path) already apply by wrapping
    /// their own call to this function, now centralized here so every
    /// caller gets it, including ones that don't wrap it themselves (e.g.
    /// `prompt_agent::ask`'s `deliver_message` call) — then falls back to
    /// `config_dir()`, so `NINOX_CONFIG` alone still redirects it.
    ///
    /// Falls back to `<config_dir>/ninox/sessions` when neither is set.
    pub fn sessions_dir() -> PathBuf {
        if let Some(p) = crate::test_isolation::env_path("NINOX_DATA_DIR") {
            return p;
        }
        Self::config_dir().join("sessions")
    }

    /// Scratch workspace for the local machine's prompt-agent singleton
    /// (see `ninox_ui_shared::prompt_agent`) — a plain directory, never a
    /// git worktree, since that session makes no file edits of its own.
    ///
    /// Falls back to `<config_dir>/ninox/prompt-agent` when `NINOX_CONFIG`
    /// is unset.
    pub fn prompt_agent_workspace_dir() -> PathBuf {
        Self::config_dir().join("prompt-agent")
    }

    fn path() -> PathBuf { Self::config_path() }

    /// Absent file → `Ok(default)`. Present and valid → `Ok(parsed)`.
    /// Present but invalid → `Err` with the line/column the parser points
    /// at, instead of silently falling back to defaults — see MLOPS-5457.
    pub fn load_detailed() -> std::result::Result<Self, ConfigParseError> {
        let p = Self::path();
        if !p.exists() { return Ok(Self::default()); }
        let raw = fs::read_to_string(&p).map_err(|e| ConfigParseError {
            message: e.to_string(),
            line: None,
            column: None,
        })?;
        let cfg: Self = toml::from_str(&raw).map_err(|e| ConfigParseError::from_toml(&raw, e))?;
        cfg.warn_if_legacy_auto_reap();
        Ok(cfg)
    }

    pub fn load() -> Result<Self> {
        Ok(Self::load_detailed()?)
    }

    /// The effective completion mode: an explicit `[worker_completion] mode`
    /// wins, else a legacy `[auto_reap] enabled` is honoured, else `self`.
    pub fn completion_mode(&self) -> CompletionMode {
        match (self.worker_completion.mode, &self.auto_reap) {
            (Some(m), _) => m,
            (None, Some(a)) if a.enabled => CompletionMode::ReapOnMerge,
            (None, Some(_)) => CompletionMode::Manual,
            (None, None) => CompletionMode::SelfComplete,
        }
    }

    fn warn_if_legacy_auto_reap(&self) {
        static WARNED: std::sync::Once = std::sync::Once::new();
        if self.worker_completion.mode.is_some() {
            return;
        }
        if let Some(a) = &self.auto_reap {
            let mode = self.completion_mode();
            WARNED.call_once(|| {
                tracing::warn!(
                    "[auto_reap] enabled = {} is deprecated; treating it as worker_completion mode `{}`. \
                     Replace it with `[worker_completion]` / `mode = \"{}\"` (use `mode = \"self\"` to let workers finish themselves).",
                    a.enabled, mode.as_str(), mode.as_str(),
                );
            });
        }
    }

    /// `load()`, but an invalid (present, unparseable) file logs a warning
    /// and falls back to defaults instead of propagating — for the ~50 call
    /// sites that only ever wanted "best-effort config", not error
    /// handling. A file that merely doesn't exist stays silent (`load()`
    /// already treats that as the normal default case).
    pub fn load_or_default() -> Self {
        match Self::load_detailed() {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!("{}", e.banner_message());
                Self::default()
            }
        }
    }

    /// Refuses to write when the on-disk file exists but no longer parses
    /// as a valid `AppConfig` (nothing to safely back up or merge with —
    /// see MLOPS-5457's HARD SAFETY requirement), otherwise backs up the
    /// previous file to `config.toml.bak` and writes atomically.
    pub fn save(&self) -> Result<()> {
        let p = Self::path();
        if p.exists() {
            let raw = fs::read_to_string(&p)?;
            if toml::from_str::<AppConfig>(&raw).is_err() {
                anyhow::bail!(
                    "refusing to overwrite {}: it no longer parses as valid config — fix it by hand first",
                    p.display()
                );
            }
        }
        backup_and_write_atomic(&p, &toml::to_string(self)?)
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
        {
            let _iso = crate::test_isolation::isolate_with_config(&path);
            let stale = AppConfig::load().unwrap();
            let mut other = stale.clone();
            other.zoom = 1.3;
            other.save().unwrap();

            let (saved, ()) = AppConfig::update(|c| c.pr_watch.enabled = false).unwrap();
            assert!(!saved.pr_watch.enabled);
            assert_eq!(saved.zoom, 1.3, "another process's edit survives");
            assert_eq!(AppConfig::load().unwrap().zoom, 1.3);

            std::fs::write(&path, "not = [valid").unwrap();
            assert!(AppConfig::update(|c| c.pr_watch.enabled = false).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "not = [valid", "a broken file is never overwritten");
        }
    }

    #[test]
    fn upsert_worker_type_replaces_by_name_and_extends_otherwise() {
        use crate::worker_type::WorkerTypeSpec;
        let mut cfg = AppConfig::default();
        cfg.upsert_worker_type(WorkerTypeSpec { name: "Implementation".into(), harness: "codex".into(), ..WorkerTypeSpec::default() });
        assert_eq!(cfg.worker_types.len(), 1);
        assert_eq!(cfg.worker_type_registry().get("Implementation").unwrap().harness, "codex");

        cfg.upsert_worker_type(WorkerTypeSpec { name: "Tester".into(), ..WorkerTypeSpec::default() });
        assert_eq!(cfg.worker_types.len(), 2, "a new name extends rather than replacing");
        assert!(cfg.worker_type_registry().get("Tester").is_some());

        cfg.upsert_worker_type(WorkerTypeSpec { name: "Implementation".into(), harness: "aider".into(), ..WorkerTypeSpec::default() });
        assert_eq!(cfg.worker_types.len(), 2, "the same name replaces in place, not appends");
        assert_eq!(cfg.worker_type_registry().get("Implementation").unwrap().harness, "aider");
    }

    #[test]
    fn remove_worker_type_reverts_a_customized_builtin_and_drops_a_custom_type() {
        use crate::worker_type::WorkerTypeSpec;
        let mut cfg = AppConfig::default();
        cfg.upsert_worker_type(WorkerTypeSpec { name: "Implementation".into(), harness: "codex".into(), ..WorkerTypeSpec::default() });
        cfg.upsert_worker_type(WorkerTypeSpec { name: "Tester".into(), ..WorkerTypeSpec::default() });

        assert!(cfg.remove_worker_type("Implementation"));
        assert_eq!(cfg.worker_type_registry().get("Implementation").unwrap().harness, "claude-code", "reverts to the compiled-in default");
        assert!(cfg.worker_type_registry().names().contains(&"Implementor".to_string()), "canonical name, not the legacy alias used to write the override");

        assert!(cfg.remove_worker_type("Tester"));
        assert!(cfg.worker_type_registry().get("Tester").is_none(), "a pure custom type is gone entirely");

        assert!(!cfg.remove_worker_type("Researcher"), "an un-customized builtin has no config entry to remove");
    }

    /// Regression for the bug a code-review pass caught: editing/resetting
    /// a type through its CANONICAL name must still find and replace/remove
    /// a raw config entry stored under a different alias of that same
    /// type — otherwise `upsert_worker_type("Implementor", ...)` appends a
    /// second, duplicate entry alongside an existing raw
    /// `name = "Implementation"` one, and a later
    /// `remove_worker_type("Implementor")` only deletes the new entry,
    /// leaving the stale alias-named override in place (so `reset` claims
    /// success but the registry still resolves the old override).
    #[test]
    fn upsert_and_remove_by_canonical_name_replace_a_raw_entry_stored_under_an_alias() {
        use crate::worker_type::WorkerTypeSpec;
        let mut cfg = AppConfig::default();
        // Simulates a pre-existing config written under the legacy alias.
        cfg.worker_types.push(WorkerTypeSpec { name: "Implementation".into(), harness: "codex".into(), ..WorkerTypeSpec::default() });

        cfg.upsert_worker_type(WorkerTypeSpec { name: "Implementor".into(), harness: "aider".into(), ..WorkerTypeSpec::default() });
        assert_eq!(cfg.worker_types.len(), 1, "must replace the alias-named entry in place, not append a duplicate");
        assert_eq!(cfg.worker_types[0].harness, "aider");

        assert!(cfg.remove_worker_type("Implementor"), "must find and remove the entry regardless of which alias it's stored under");
        assert!(cfg.worker_types.is_empty());
        assert_eq!(
            cfg.worker_type_registry().get("Implementor").unwrap().harness, "claude-code",
            "reset must actually revert to the compiled-in default, not leave a stale override behind",
        );
    }

    #[test]
    fn missing_port_and_font_size_load_with_defaults() {
        let cfg: AppConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.port, 8080);
        assert_eq!(cfg.font_size, 13.0);
    }

    #[test]
    fn a_syntax_error_surfaces_with_a_line_number_instead_of_defaulting() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "port = 8080\nfont_size = 13.0\nbad = [not valid\n").unwrap();
        {
            let _iso = crate::test_isolation::isolate_with_config(&path);
            let err = AppConfig::load_detailed().unwrap_err();
            assert_eq!(err.line, Some(3));
            assert!(err.banner_message().contains("line 3"), "{}", err.banner_message());
            assert!(err.banner_message().contains("Running with defaults"));
        }
    }

    #[test]
    fn absent_file_gives_defaults_via_load_detailed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        {
            let _iso = crate::test_isolation::isolate_with_config(&path);
            assert!(AppConfig::load_detailed().is_ok());
        }
    }

    #[test]
    fn load_or_default_falls_back_on_an_invalid_file_without_panicking() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "not = [valid").unwrap();
        {
            let _iso = crate::test_isolation::isolate_with_config(&path);
            let cfg = AppConfig::load_or_default();
            assert_eq!(cfg.port, AppConfig::default().port);
        }
    }

    #[test]
    fn banner_message_with_no_line_omits_the_line_clause() {
        let e = ConfigParseError { message: "io error".into(), line: None, column: None };
        let msg = e.banner_message();
        assert!(msg.contains("io error"));
        assert!(!msg.to_lowercase().contains("line"));
    }

    #[test]
    fn a_multi_line_toml_error_is_normalized_to_a_single_space_joined_line() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "not = [valid").unwrap();
        {
            let _iso = crate::test_isolation::isolate_with_config(&path);
            let err = AppConfig::load_detailed().unwrap_err();
            assert!(!err.message.contains('\n'), "{:?}", err.message);
        }
    }

    #[test]
    fn tui_footer_message_is_compact_and_single_line() {
        let e = ConfigParseError { message: "invalid array expected `]`".into(), line: Some(1), column: Some(1) };
        assert_eq!(
            e.tui_footer_message(),
            "config.toml line 1: invalid array expected `]` · running with defaults, saves disabled"
        );
    }

    #[test]
    fn tui_footer_message_with_no_line_omits_the_line_clause() {
        let e = ConfigParseError { message: "io error".into(), line: None, column: None };
        assert_eq!(e.tui_footer_message(), "config.toml: io error · running with defaults, saves disabled");
    }

    #[test]
    fn save_refuses_to_overwrite_an_already_invalid_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "not = [valid").unwrap();
        {
            let _iso = crate::test_isolation::isolate_with_config(&path);
            assert!(AppConfig::default().save().is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), "not = [valid");
            assert!(!path.with_file_name("config.toml.bak").exists());
        }
    }

    #[test]
    fn save_backs_up_the_previous_valid_file_before_overwriting() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = AppConfig { port: 1111, ..AppConfig::default() };
        fs::write(&path, toml::to_string(&original).unwrap()).unwrap();
        {
            let _iso = crate::test_isolation::isolate_with_config(&path);
            let updated = AppConfig { port: 2222, ..AppConfig::default() };
            updated.save().unwrap();
            let bak = path.with_file_name("config.toml.bak");
            let backed_up: AppConfig = toml::from_str(&fs::read_to_string(&bak).unwrap()).unwrap();
            assert_eq!(backed_up.port, 1111);
            assert_eq!(AppConfig::load().unwrap().port, 2222);
        }
    }

    #[test]
    fn save_writes_atomically_leaving_no_temp_file_behind() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        {
            let _iso = crate::test_isolation::isolate_with_config(&path);
            AppConfig::default().save().unwrap();
            let leftovers: Vec<_> = fs::read_dir(dir.path()).unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
                .collect();
            assert!(leftovers.is_empty(), "{leftovers:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn save_writes_through_a_symlinked_config_toml_instead_of_replacing_the_link() {
        let dir = tempdir().unwrap();
        let real = dir.path().join("real-config.toml");
        let link = dir.path().join("config.toml");
        fs::write(&real, toml::to_string(&AppConfig::default()).unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        {
            let _iso = crate::test_isolation::isolate_with_config(&link);
            let updated = AppConfig { port: 4242, ..AppConfig::default() };
            updated.save().unwrap();
            assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "save() must not replace the symlink itself");
            let on_disk: AppConfig = toml::from_str(&fs::read_to_string(&real).unwrap()).unwrap();
            assert_eq!(on_disk.port, 4242, "the edit must land in the symlink's real target");
        }
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
            ssh_target: "dev@10.0.0.5".into(),
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
        let cfg = AppConfig { sidebar_width: 275.0, ..AppConfig::default() };
        fs::write(&path, toml::to_string(&cfg).unwrap()).unwrap();
        let loaded: AppConfig = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(loaded.sidebar_width, 275.0);
    }

    #[test]
    fn missing_sidebar_fields_default() {
        // Configs written before these fields existed must still load.
        let cfg: AppConfig = toml::from_str("port = 8080\nfont_size = 13.0\n").unwrap();
        assert_eq!(cfg.sidebar_width, default_sidebar_width());
    }

    #[test]
    fn a_config_still_holding_the_removed_sidebar_hidden_key_loads() {
        let cfg: AppConfig = toml::from_str("port = 8080\nsidebar_hidden = true\n").unwrap();
        assert_eq!(cfg.port, 8080);
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
        let _iso = crate::test_isolation::isolate();
        let cfg = AppConfig::default();
        assert!(cfg.resolved_orchestrator_root().ends_with("orchestrators"));
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
    fn sessions_dir_honors_ninox_data_dir_env() {
        let dir = tempdir().unwrap();
        let override_path = dir.path().join("sessions-override");

        with_env_override("NINOX_DATA_DIR", &override_path, || {
            assert_eq!(AppConfig::sessions_dir(), override_path);
        });
    }

    #[test]
    fn sessions_dir_and_prompt_agent_workspace_dir_both_derive_from_ninox_config_when_set() {
        // One env var sandboxes both — `NINOX_CONFIG` is a path to the
        // config *file*, so its parent is the dir everything else derives
        // from, mirroring `config_path()`'s own override convention.
        // `NINOX_DATA_DIR` must also be cleared for the duration — this
        // worker's own real pane environment already exports it (to the
        // real sessions dir), which would otherwise win over `NINOX_CONFIG`
        // and mask the fallback this test is checking. Both vars are set
        // under one `ENV_TEST_GUARD` acquisition — `with_env_override`
        // isn't reentrant (its guard is a plain, non-reentrant `Mutex`), so
        // nesting two calls on the same thread would deadlock.
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.toml");

        let _guard = ENV_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let prior_config = std::env::var("NINOX_CONFIG").ok();
        let prior_data_dir = std::env::var("NINOX_DATA_DIR").ok();
        std::env::set_var("NINOX_CONFIG", &config_path);
        std::env::remove_var("NINOX_DATA_DIR");

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert_eq!(AppConfig::sessions_dir(), dir.path().join("sessions"));
            assert_eq!(AppConfig::prompt_agent_workspace_dir(), dir.path().join("prompt-agent"));
        }));

        match prior_config {
            Some(v) => std::env::set_var("NINOX_CONFIG", v),
            None => std::env::remove_var("NINOX_CONFIG"),
        }
        match prior_data_dir {
            Some(v) => std::env::set_var("NINOX_DATA_DIR", v),
            None => std::env::remove_var("NINOX_DATA_DIR"),
        }
        result.unwrap();
    }

    #[test]
    fn ninox_data_dir_takes_precedence_over_ninox_config_for_sessions_dir() {
        // Both env vars are set together under one `ENV_TEST_GUARD`
        // acquisition — `with_env_override` isn't reentrant (its guard is a
        // plain, non-reentrant `Mutex`), so nesting two calls on the same
        // thread would deadlock.
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let sessions_override = dir.path().join("sessions-override");

        let _guard = ENV_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let prior_config = std::env::var("NINOX_CONFIG").ok();
        let prior_data_dir = std::env::var("NINOX_DATA_DIR").ok();
        std::env::set_var("NINOX_CONFIG", &config_path);
        std::env::set_var("NINOX_DATA_DIR", &sessions_override);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert_eq!(AppConfig::sessions_dir(), sessions_override);
            // No dedicated override for this one — still follows NINOX_CONFIG.
            assert_eq!(AppConfig::prompt_agent_workspace_dir(), dir.path().join("prompt-agent"));
        }));

        match prior_config {
            Some(v) => std::env::set_var("NINOX_CONFIG", v),
            None => std::env::remove_var("NINOX_CONFIG"),
        }
        match prior_data_dir {
            Some(v) => std::env::set_var("NINOX_DATA_DIR", v),
            None => std::env::remove_var("NINOX_DATA_DIR"),
        }
        result.unwrap();
    }

    #[test]
    fn catalogue_options_defaults_to_single_entry() {
        let _iso = crate::test_isolation::isolate();
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
    fn nested_orchestration_defaults_to_max_depth_3() {
        assert_eq!(AppConfig::default().nested_orchestration.max_depth, 3);
    }

    #[test]
    fn nested_orchestration_missing_table_defaults_to_3() {
        let cfg: AppConfig = toml::from_str("port = 8080\nfont_size = 13.0\n").unwrap();
        assert_eq!(cfg.nested_orchestration.max_depth, 3);
    }

    #[test]
    fn nested_orchestration_can_be_configured() {
        let toml_src = "port = 8080\nfont_size = 13.0\n\n[nested_orchestration]\nmax_depth = 5\n";
        let cfg: AppConfig = toml::from_str(toml_src).unwrap();
        assert_eq!(cfg.nested_orchestration.max_depth, 5);
    }

    #[test]
    fn planning_config_defaults_and_round_trips() {
        let toml_src = "port = 8080\nfont_size = 13.0\n\n[planning]\nharness = \"claude-code\"\nmodel = \"claude-opus-4-5\"\n";
        let cfg: AppConfig = toml::from_str(toml_src).unwrap();
        assert_eq!(cfg.planning.harness, "claude-code");
        assert_eq!(cfg.planning.model.as_deref(), Some("claude-opus-4-5"));
        let reparsed: AppConfig = toml::from_str(&toml::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(reparsed.planning.model.as_deref(), Some("claude-opus-4-5"));
    }

    #[test]
    fn planning_config_absent_uses_defaults() {
        let cfg: AppConfig = toml::from_str("port = 8080\nfont_size = 13.0\n").unwrap();
        assert_eq!(cfg.planning.harness, "claude-code");
        assert!(cfg.planning.model.is_none());
    }

    #[test]
    fn message_stall_nudges_default_on_at_30_minutes() {
        let cfg: AppConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.message_stalls.nudge_after_mins, 30);
        let cfg: AppConfig = toml::from_str("[message_stalls]\nnudge_after_mins = 0\n").unwrap();
        assert_eq!(cfg.message_stalls.nudge_after_mins, 0);
    }

    #[test]
    fn pr_events_default_on_with_conservative_thresholds() {
        let cfg: AppConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.pr_events, PrEventsConfig { ci_stuck_after_mins: 45, hang_nudge_after_mins: 30 });
        let cfg: AppConfig = toml::from_str("[pr_events]\nhang_nudge_after_mins = 0\n").unwrap();
        assert_eq!(cfg.pr_events.hang_nudge_after_mins, 0);
        assert_eq!(cfg.pr_events.ci_stuck_after_mins, 45, "an omitted field keeps its default");
    }

    #[test]
    fn github_polling_defaults_and_floor() {
        let d = AppConfig::default().github;
        assert_eq!((d.poll_interval_secs, d.detail_refresh_secs), (60, 600));
        let cfg: AppConfig = toml::from_str("port = 8080\n\n[github]\npoll_interval_secs = 5\n").unwrap();
        assert_eq!(cfg.github.poll_interval_secs, 5);
        assert_eq!(cfg.github.effective_poll_interval_secs(), 15, "below the floor is clamped");
        assert_eq!(cfg.github.detail_refresh_secs, 600, "unset key keeps its default");
        let cfg: AppConfig = toml::from_str("port = 8080\n\n[github]\npoll_interval_secs = 120\n").unwrap();
        assert_eq!(cfg.github.effective_poll_interval_secs(), 120);
    }

    #[test]
    fn saving_the_update_settings_merges_into_an_existing_config() {
        let _iso = crate::test_isolation::isolate();
        let path = AppConfig::config_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "port = 9123\n").unwrap();
        AppConfig::update(|c| {
            c.update.channel = UpdateChannel::Alpha;
            c.update.aws_profile = Some("prd-build-readonly".into());
        })
        .unwrap();
        let cfg = AppConfig::load().unwrap();
        assert_eq!(cfg.port, 9123, "existing keys survive");
        assert_eq!(cfg.update.channel, UpdateChannel::Alpha);
        assert_eq!(cfg.update.aws_profile.as_deref(), Some("prd-build-readonly"));
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("[update]") && raw.contains("channel = \"alpha\"") && raw.contains("aws_profile = \"prd-build-readonly\""), "{raw}");
    }

    #[test]
    fn update_channel_defaults_to_stable() {
        assert_eq!(AppConfig::default().update.channel, UpdateChannel::Stable);
        let cfg: AppConfig = toml::from_str("port = 8080\n").unwrap();
        assert_eq!(cfg.update.channel, UpdateChannel::Stable);
        let cfg: AppConfig = toml::from_str("port = 8080\n\n[update]\n").unwrap();
        assert_eq!(cfg.update.channel, UpdateChannel::Stable);
    }

    #[test]
    fn update_channel_round_trips_through_toml() {
        let mut cfg = AppConfig::default();
        cfg.update.channel = UpdateChannel::Alpha;
        let text = toml::to_string(&cfg).unwrap();
        assert!(text.contains("[update]\nchannel = \"alpha\""), "{text}");
        let back: AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(back.update.channel, UpdateChannel::Alpha);
        assert!(toml::from_str::<AppConfig>("[update]\nchannel = \"beta\"\n").is_err());
    }

    #[test]
    fn update_channel_parses_cli_values() {
        assert_eq!("alpha".parse::<UpdateChannel>(), Ok(UpdateChannel::Alpha));
        assert_eq!("stable".parse::<UpdateChannel>(), Ok(UpdateChannel::Stable));
        assert!("nightly".parse::<UpdateChannel>().is_err());
    }

    #[test]
    fn orchestrator_reset_is_off_by_default_with_a_40_percent_threshold() {
        let d = AppConfig::default().orchestrator_reset;
        assert_eq!((d.mode, d.auto_at_context_pct, d.enabled()), (ResetMode::Off, 40, false));
        for src in ["port = 8080\n", "port = 8080\n\n[orchestrator_reset]\n", "port = 8080\n\n[orchestrator_reset]\nauto_at_context_pct = 40\n"] {
            let cfg: AppConfig = toml::from_str(src).unwrap();
            assert!(!cfg.orchestrator_reset.enabled(), "{src:?}: the threshold alone does not switch it on");
        }
    }

    #[test]
    fn orchestrator_reset_mode_reads_off_prompt_auto_and_anything_else_is_off() {
        let mode = |v: &str| -> ResetMode {
            toml::from_str::<AppConfig>(&format!("port = 8080\n\n[orchestrator_reset]\nmode = {v}\n")).unwrap().orchestrator_reset.mode
        };
        assert_eq!(mode("\"off\""), ResetMode::Off);
        assert_eq!(mode("\"prompt\""), ResetMode::Prompt);
        assert_eq!(mode("\"auto\""), ResetMode::Auto);
        assert_eq!(mode("\"Prompt\""), ResetMode::Prompt);
        for unknown in ["\"confirm\"", "\"yes\"", "\"\"", "true", "7"] {
            assert_eq!(mode(unknown), ResetMode::Off, "{unknown}");
        }
        let cfg: AppConfig = toml::from_str("port = 8080\n\n[orchestrator_reset]\nauto_at_context_pct = 55\nmode = \"auto\"\n").unwrap();
        assert!(cfg.orchestrator_reset.enabled());
        let back: AppConfig = toml::from_str(&toml::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back.orchestrator_reset, cfg.orchestrator_reset, "serializes in a position TOML accepts");
    }

    #[test]
    fn the_threshold_is_held_to_10_through_95() {
        let at = |n: u8| OrchestratorResetConfig { auto_at_context_pct: n, mode: ResetMode::Prompt }.threshold();
        assert_eq!((at(0), at(9), at(10), at(40), at(95), at(96), at(255)), (10, 10, 10, 40, 95, 95, 95));
    }

    #[test]
    fn pr_watch_defaults_to_enabled() {
        assert!(AppConfig::default().pr_watch.enabled);
    }

    #[test]
    fn pr_watch_missing_table_or_key_defaults_to_enabled() {
        let cfg: AppConfig = toml::from_str("port = 8080\nfont_size = 13.0\n").unwrap();
        assert!(cfg.pr_watch.enabled);
        let cfg: AppConfig = toml::from_str("port = 8080\n\n[pr_watch]\n").unwrap();
        assert!(cfg.pr_watch.enabled);
    }

    #[test]
    fn pr_watch_can_be_disabled_via_config() {
        let toml_src = "port = 8080\nfont_size = 13.0\n\n[pr_watch]\nenabled = false\n";
        let cfg: AppConfig = toml::from_str(toml_src).unwrap();
        assert!(!cfg.pr_watch.enabled);
    }

    #[test]
    fn role_rehydration_defaults_to_disabled() {
        assert!(!AppConfig::default().role_rehydration.enabled);
    }

    #[test]
    fn role_rehydration_missing_table_defaults_to_disabled() {
        let cfg: AppConfig = toml::from_str("port = 8080\nfont_size = 13.0\n").unwrap();
        assert!(!cfg.role_rehydration.enabled);
    }

    #[test]
    fn role_rehydration_can_be_enabled_via_config() {
        let toml_src = "port = 8080\nfont_size = 13.0\n\n[role_rehydration]\nenabled = true\n";
        let cfg: AppConfig = toml::from_str(toml_src).unwrap();
        assert!(cfg.role_rehydration.enabled);
    }

    #[test]
    fn completion_mode_defaults_to_self_when_nothing_is_set() {
        assert_eq!(AppConfig::default().completion_mode(), CompletionMode::SelfComplete);
        let cfg: AppConfig = toml::from_str("port = 8080\nfont_size = 13.0\n").unwrap();
        assert_eq!(cfg.completion_mode(), CompletionMode::SelfComplete);
        assert_eq!(cfg.worker_completion.idle_after_merge_nudge_mins, 30);
    }

    #[test]
    fn legacy_auto_reap_maps_to_an_explicit_mode() {
        let on: AppConfig = toml::from_str("[auto_reap]\nenabled = true\n").unwrap();
        assert_eq!(on.completion_mode(), CompletionMode::ReapOnMerge);
        let off: AppConfig = toml::from_str("[auto_reap]\nenabled = false\n").unwrap();
        assert_eq!(off.completion_mode(), CompletionMode::Manual);
        let empty: AppConfig = toml::from_str("[auto_reap]\n").unwrap();
        assert_eq!(empty.completion_mode(), CompletionMode::ReapOnMerge);
    }

    #[test]
    fn explicit_worker_completion_mode_beats_legacy_auto_reap() {
        for (m, want) in [
            ("self", CompletionMode::SelfComplete),
            ("reap_on_merge", CompletionMode::ReapOnMerge),
            ("manual", CompletionMode::Manual),
        ] {
            let cfg: AppConfig =
                toml::from_str(&format!("[auto_reap]\nenabled = false\n\n[worker_completion]\nmode = \"{m}\"\n")).unwrap();
            assert_eq!(cfg.completion_mode(), want);
        }
        assert!(toml::from_str::<AppConfig>("[worker_completion]\nmode = \"bogus\"\n").is_err());
    }

    #[test]
    fn legacy_choice_survives_a_save_round_trip_and_mode_serializes_before_harnesses() {
        let cfg: AppConfig = toml::from_str("[auto_reap]\nenabled = false\n").unwrap();
        let back: AppConfig = toml::from_str(&toml::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back.completion_mode(), CompletionMode::Manual);
        assert!(back.worker_completion.mode.is_none(), "saving must not silently pin a mode");
        let mut cfg = AppConfig::default();
        cfg.worker_completion.mode = Some(CompletionMode::ReapOnMerge);
        let out = toml::to_string(&cfg).unwrap();
        assert!(toml::from_str::<AppConfig>(&out).is_ok());
        assert!(out.contains("mode = \"reap_on_merge\""));
    }

    #[test]
    fn orphan_sweep_defaults_to_enabled_hourly() {
        assert!(AppConfig::default().orphan_sweep.enabled);
        assert_eq!(AppConfig::default().orphan_sweep.interval_hours, 1);
        let cfg: AppConfig = toml::from_str("port = 8080\nfont_size = 13.0\n").unwrap();
        assert!(cfg.orphan_sweep.enabled);
        assert_eq!(cfg.orphan_sweep.interval_hours, 1);
        let cfg: AppConfig = toml::from_str("port = 8080\nfont_size = 13.0\n\n[orphan_sweep]\n").unwrap();
        assert!(cfg.orphan_sweep.enabled, "an empty [orphan_sweep] table must still default enabled");
    }

    #[test]
    fn orphan_sweep_can_be_disabled_and_recadenced_via_config() {
        let toml_src = "port = 8080\nfont_size = 13.0\n\n[orphan_sweep]\nenabled = false\ninterval_hours = 6\n";
        let cfg: AppConfig = toml::from_str(toml_src).unwrap();
        assert!(!cfg.orphan_sweep.enabled);
        assert_eq!(cfg.orphan_sweep.interval_hours, 6);
    }

    #[test]
    fn catalogue_options_appends_configured_catalogues_and_skips_duplicate_default() {
        let _iso = crate::test_isolation::isolate();
        let mut cfg = AppConfig::default();
        cfg.brain.catalogues = vec![
            CatalogueRef { name: "docs".to_string(), path: PathBuf::from("/tmp/docs-brain"), remote: None, endpoint: None, region: None, cache_ttl_secs: None, enabled: true, cadence: "manual".to_string(), access: "read_write".to_string(), write_rules_override: None },
            CatalogueRef { name: "default".to_string(), path: PathBuf::from("/tmp/should-be-skipped"), remote: None, endpoint: None, region: None, cache_ttl_secs: None, enabled: true, cadence: "manual".to_string(), access: "read_write".to_string(), write_rules_override: None },
        ];
        let options = cfg.catalogue_options();
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].name, "default");
        assert_eq!(options[1].name, "docs");
        assert_eq!(options[1].path, PathBuf::from("/tmp/docs-brain"));
    }

    #[test]
    fn catalogue_names_matches_catalogue_options_without_resolving_paths() {
        let mut cfg = AppConfig::default();
        cfg.brain.catalogues = vec![
            CatalogueRef { name: "docs".to_string(), path: PathBuf::from("/tmp/docs-brain"), remote: None, endpoint: None, region: None, cache_ttl_secs: None, enabled: true, cadence: "manual".to_string(), access: "read_write".to_string(), write_rules_override: None },
            CatalogueRef { name: "default".to_string(), path: PathBuf::from("/tmp/should-be-skipped"), remote: None, endpoint: None, region: None, cache_ttl_secs: None, enabled: true, cadence: "manual".to_string(), access: "read_write".to_string(), write_rules_override: None },
        ];
        let names = cfg.catalogue_names();
        let _iso = crate::test_isolation::isolate();
        let expected: Vec<String> = cfg.catalogue_options().into_iter().map(|c| c.name).collect();
        assert_eq!(names, expected);
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
            enabled: true,
            cadence: default_cadence(),
            access: default_access(),
            write_rules_override: None,
        }];
        let sync = cfg.remote_config_for(std::path::Path::new("/tmp/team-brain")).unwrap();
        assert_eq!(sync.remote, "s3://team-brains/main");
        assert_eq!(sync.region.as_deref(), Some("eu-west-1"));
        assert_eq!(sync.cache_ttl_secs, 60);
        assert!(cfg.remote_config_for(std::path::Path::new("/tmp/other")).is_none());
    }

    #[test]
    fn remote_config_for_matches_default_brain() {
        let _iso = crate::test_isolation::isolate();
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
            enabled: true,
            cadence: default_cadence(),
            access: default_access(),
            write_rules_override: None,
        }];
        assert!(cfg.remote_config_for(std::path::Path::new("/tmp/local-brain")).is_none());
    }

    #[test]
    fn catalogue_ref_without_new_fields_in_toml_gets_sensible_defaults() {
        // A `config.toml` saved before `enabled`/`cadence`/`access` existed
        // must still deserialize — and its catalogue must default to
        // visible/manual/read-write rather than silently dropping out of
        // search or picking up an unintended value.
        let toml_str = r#"
            name = "team"
            path = "/tmp/team-brain"
        "#;
        let parsed: CatalogueRef = toml::from_str(toml_str).unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.cadence, "manual");
        assert_eq!(parsed.access, "read_write");
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

#[cfg(test)]
mod shared_build_tests {
    use super::*;
    use tempfile::tempdir;

    /// Default config has no shared-build entries — an untouched install
    /// stays exactly as before this feature existed.
    #[test]
    fn workspaces_shared_build_defaults_to_empty() {
        let cfg = AppConfig::default();
        assert!(cfg.workspaces.shared_build.is_empty());
    }

    #[test]
    fn shared_build_table_round_trips_after_scalars() {
        let mut cfg = AppConfig::default();
        cfg.workspaces.shared_build.push(SharedBuildEntry {
            repo: PathBuf::from("/repo"),
            env: BTreeMap::from([("RUSTC_WRAPPER".to_string(), "sccache".to_string())]),
        });
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(back.workspaces.shared_build.len(), 1);
        assert_eq!(back.workspaces.shared_build[0].repo, PathBuf::from("/repo"));
        assert_eq!(back.workspaces.shared_build[0].env.get("RUSTC_WRAPPER").map(String::as_str), Some("sccache"));
    }

    #[test]
    fn shared_build_env_matches_workspace_under_configured_repo_and_substitutes_placeholder() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("myrepo");
        let workspace = repo.join(".claude/worktrees/some-branch");
        fs::create_dir_all(&workspace).unwrap();
        let workspaces = WorkspacesConfig {
            shared_build: vec![SharedBuildEntry {
                repo: repo.clone(),
                env: BTreeMap::from([("CARGO_TARGET_DIR".to_string(), "{repo}/.claude/build-cache/target".to_string())]),
            }], ..Default::default() };
        let env = workspaces.effective_shared_build_env_for(&workspace);
        assert_eq!(env.len(), 1);
        let expected = format!("{}/.claude/build-cache/target", repo.display());
        assert_eq!(env[0], ("CARGO_TARGET_DIR".to_string(), expected));
    }

    #[test]
    fn shared_build_env_is_empty_for_a_workspace_outside_every_configured_repo() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("myrepo");
        let other = dir.path().join("unrelated");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&other).unwrap();
        let workspaces = WorkspacesConfig {
            shared_build: vec![SharedBuildEntry { repo, env: BTreeMap::from([("X".to_string(), "1".to_string())]) }], ..Default::default() };
        assert!(workspaces.effective_shared_build_env_for(&other).is_empty());
    }

    /// An explicit entry — even one with no env vars — always wins over the
    /// implicit ninox default, so a user can opt a repo out by configuring
    /// it with an empty `env` table.
    #[test]
    fn explicit_empty_entry_for_ninox_repo_overrides_the_implicit_default() {
        let dir = tempdir().unwrap();
        let workspace = ninox_shaped_repo(dir.path());
        let workspaces =
            WorkspacesConfig { shared_build: vec![SharedBuildEntry { repo: workspace.clone(), env: BTreeMap::new() }], ..Default::default() };
        assert!(workspaces.effective_shared_build_env_for(&workspace).is_empty());
    }

    #[test]
    fn non_ninox_repo_with_no_config_gets_no_implicit_default() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("some-other-repo");
        fs::create_dir_all(&workspace).unwrap();
        let workspaces = WorkspacesConfig::default();
        assert!(workspaces.effective_shared_build_env_for(&workspace).is_empty());
    }

    /// The disk-reducing flags apply to ninox's own repo unconditionally;
    /// `RUSTC_WRAPPER=sccache` is environment-gated since CI may not have
    /// `sccache` installed.
    #[test]
    fn ninox_repo_with_no_explicit_config_gets_disk_reducing_flags_and_sccache_when_available() {
        let dir = tempdir().unwrap();
        let workspace = ninox_shaped_repo(dir.path());
        let workspaces = WorkspacesConfig::default();
        let env = workspaces.effective_shared_build_env_for(&workspace);
        assert!(env.contains(&("CARGO_INCREMENTAL".to_string(), "0".to_string())));
        assert!(env.contains(&("CARGO_PROFILE_DEV_DEBUG".to_string(), "line-tables-only".to_string())));
        assert_eq!(env.contains(&("RUSTC_WRAPPER".to_string(), "sccache".to_string())), sccache_on_path());
    }

    #[test]
    fn multiple_matching_entries_prefer_the_most_specific_repo() {
        let dir = tempdir().unwrap();
        let outer = dir.path().join("monorepo");
        let inner = outer.join("services/ninox");
        let workspace = inner.join(".claude/worktrees/some-branch");
        fs::create_dir_all(&workspace).unwrap();
        let workspaces = WorkspacesConfig {
            shared_build: vec![
                SharedBuildEntry { repo: outer, env: BTreeMap::from([("X".to_string(), "outer".to_string())]) },
                SharedBuildEntry { repo: inner, env: BTreeMap::new() },
            ], ..Default::default() };
        // The more specific (inner) entry wins even though it was declared
        // second and has no env vars of its own.
        assert!(workspaces.effective_shared_build_env_for(&workspace).is_empty());
    }

    #[test]
    fn reserved_ninox_env_keys_are_never_overridable_by_config() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("myrepo");
        let workspace = repo.join(".claude/worktrees/some-branch");
        fs::create_dir_all(&workspace).unwrap();
        let workspaces = WorkspacesConfig {
            shared_build: vec![SharedBuildEntry {
                repo,
                env: BTreeMap::from([
                    ("NINOX_CONFIG".to_string(), "/evil/config.toml".to_string()),
                    ("RUSTC_WRAPPER".to_string(), "sccache".to_string()),
                ]),
            }], ..Default::default() };
        let env = workspaces.effective_shared_build_env_for(&workspace);
        assert!(!env.iter().any(|(k, _)| k == "NINOX_CONFIG"));
        assert!(env.contains(&("RUSTC_WRAPPER".to_string(), "sccache".to_string())));
    }

    fn ninox_shaped_repo(root: &std::path::Path) -> PathBuf {
        let workspace = root.join("ninox-checkout");
        for crate_dir in ["crates/ninox-core", "crates/ninox-ui-shared", "crates/ninox-tui"] {
            fs::create_dir_all(workspace.join(crate_dir)).unwrap();
        }
        fs::write(workspace.join("Cargo.toml"), "[workspace]\nmembers = [\"crates/ninox-core\"]\n").unwrap();
        workspace
    }

    #[test]
    fn is_ninox_repo_requires_the_full_known_crate_layout_and_cargo_toml() {
        let dir = tempdir().unwrap();
        assert!(is_ninox_repo(&ninox_shaped_repo(dir.path())));
        let partial = dir.path().join("partial");
        fs::create_dir_all(partial.join("crates/ninox-core")).unwrap();
        assert!(!is_ninox_repo(&partial));
        // Full crate layout but a Cargo.toml that doesn't name ninox-core —
        // the directory-only fingerprint alone would false-positive here.
        let lookalike = dir.path().join("lookalike");
        for crate_dir in ["crates/ninox-core", "crates/ninox-ui-shared", "crates/ninox-tui"] {
            fs::create_dir_all(lookalike.join(crate_dir)).unwrap();
        }
        fs::write(lookalike.join("Cargo.toml"), "[workspace]\nmembers = [\"crates/something-else\"]\n").unwrap();
        assert!(!is_ninox_repo(&lookalike));
    }

    #[test]
    fn binary_lookup_is_false_for_a_binary_that_does_not_exist() {
        assert!(!binary_on_path("this-binary-definitely-does-not-exist-ninox-test"));
    }
}
