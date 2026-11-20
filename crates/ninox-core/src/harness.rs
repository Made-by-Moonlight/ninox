//! Data-driven agent-harness registry — harness definitions are DATA, not
//! code (spec: field-notes-design.md §V "Backend (registry, not enum)").
//! Four known harnesses ship as compiled-in default specs; `[harnesses.*]`
//! config entries override (whole-spec replace) or extend the registry, so
//! adding a future harness requires zero Rust changes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::AgentConfig;
use crate::worker_type::WorkerTypeSpec;

/// Whether a launch keeps the harness's own permission checks
/// (`Standard`) or bypasses them (`Bypass`, claude-code's
/// `--dangerously-skip-permissions`). A session's fresh launch and every
/// later resume/re-file/restore must use the same mode — see
/// [`permission_mode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    Standard,
    Bypass,
}

impl PermissionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionMode::Standard => "standard",
            PermissionMode::Bypass => "bypass",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "standard" => Some(PermissionMode::Standard),
            "bypass" => Some(PermissionMode::Bypass),
            _ => None,
        }
    }
}

/// How a session is launched: a human-attended interactive session
/// (orchestrator, standalone, prompt agent) or an unattended `ninox spawn`
/// worker that runs its prompt to completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchKind {
    Interactive,
    Worker,
}

/// The single source of a session's permission mode, for its fresh launch
/// and every relaunch alike. A worker type with `permissions.enabled` is
/// always `Standard`, since a bypassed harness only honours `deny` rules
/// and its allow/ask profile would silently stop applying. Otherwise only
/// unattended workers bypass.
pub fn permission_mode(kind: LaunchKind, worker_type: Option<&WorkerTypeSpec>) -> PermissionMode {
    if worker_type.is_some_and(|t| t.permissions.enabled) {
        return PermissionMode::Standard;
    }
    match kind {
        LaunchKind::Interactive => PermissionMode::Standard,
        LaunchKind::Worker => PermissionMode::Bypass,
    }
}

/// The mode a relaunch (resume, re-file, restore, restart) of `session`
/// must use: what its fresh launch recorded, never wider. An enforced
/// worker type wins over the record, and over the bypass every resume used
/// before modes were recorded. A row with no record falls back to
/// [`permission_mode`] with the kind inferred from the row: only a worker
/// attached to an orchestrator is known to have been a `ninox spawn`
/// launch, so anything else fails closed to `Standard`.
pub fn relaunch_permission_mode(
    recorded: Option<PermissionMode>,
    session: &crate::types::Session,
    is_orchestrator: bool,
    worker_type: Option<&WorkerTypeSpec>,
) -> PermissionMode {
    if worker_type.is_some_and(|t| t.permissions.enabled) {
        return PermissionMode::Standard;
    }
    if let Some(mode) = recorded {
        return mode;
    }
    let kind = if !is_orchestrator && session.orchestrator_id.is_some() {
        LaunchKind::Worker
    } else {
        LaunchKind::Interactive
    };
    permission_mode(kind, worker_type)
}

/// The bypass flag claude-code accepts. Also stripped from configured arg
/// lists in `Standard` mode: configs saved before `skip_permissions_args`
/// existed carry it inline in `worker_args`/`resume_args`.
pub const CLAUDE_SKIP_PERMISSIONS_FLAG: &str = "--dangerously-skip-permissions";

/// One harness definition. Template vars in args: `{model}`, `{prompt}`,
/// `{session_id}`.
/// An element containing `{model}` is dropped entirely when no model is
/// set — together with the immediately preceding `-`-prefixed literal flag
/// (so `["--model", "{model}"]` vanishes as a pair).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct HarnessSpec {
    /// Offered in the Spawn modal / Workers picker. Off by default for
    /// everything except claude-code (which is forced on — see
    /// [`HarnessRegistry::from_config`]).
    #[serde(default)]
    pub enabled: bool,
    /// Binary to run. Defaults to the harness name.
    #[serde(default)]
    pub binary: Option<String>,
    /// Spec-level default model, used when `AgentConfig.model` is unset.
    #[serde(default)]
    pub model: Option<String>,
    /// Args for interactive sessions (orchestrator / standalone).
    #[serde(default)]
    pub interactive_args: Vec<String>,
    /// Args for one-shot worker spawns. `None` = worker mode unverified for
    /// this harness: selectable interactively, never offered for workers.
    #[serde(default)]
    pub worker_args: Option<Vec<String>>,
    /// Args for resuming a previously-interrupted session by its
    /// `claude_session_id`. Empty = this harness can't resume — a session
    /// under it never gets `SessionStatus::Interrupted` (see the reconciliation
    /// logic in `app.rs`) and never shows a Resume affordance.
    #[serde(default)]
    pub resume_args: Vec<String>,
    /// `resume_args` continue "the latest conversation in this cwd" rather
    /// than a specific id (e.g. `--continue`), so a session is only
    /// resumable when no other session row shares its `workspace_path` —
    /// see [`HarnessRegistry::session_can_resume`].
    #[serde(default)]
    pub resume_cwd_scoped: bool,
    /// The harness has no permission prompts at all (pi), so a worker type's
    /// allow/ask/deny profile can never be enforced — see
    /// [`HarnessRegistry::ensure_permissions_enforceable`].
    #[serde(default)]
    pub no_permission_prompts: bool,
    /// Repo-relative skills directory this harness reads, in addition to
    /// `.claude/skills` (pi scans `.agents/skills`, not `.claude/skills`).
    /// Seeded from the same capability registry and git-excluded the same way.
    #[serde(default)]
    pub extra_skills_dir: Option<String>,
    /// Flag taking text to append to the harness's system prompt. How an
    /// orchestrator in a custom workspace gets the role text AGENTS.md would
    /// otherwise carry. `None`: the text goes at the top of the first message.
    #[serde(default)]
    pub append_system_prompt_flag: Option<String>,
    /// Shown when the binary isn't on PATH (e.g. the npm package to install).
    #[serde(default)]
    pub install_hint: Option<String>,
    /// Args prepended in `PermissionMode::Bypass`. `None` = no switch:
    /// `Bypass` runs the arg lists as written, which is how a config saved
    /// before this field existed (flag inline in `worker_args`) keeps
    /// working. `Standard` strips the flag from the lists either way.
    #[serde(default)]
    pub skip_permissions_args: Option<Vec<String>>,
    /// Curated fallback for the model picker.
    #[serde(default)]
    pub known_models: Vec<String>,
    /// Optional live model discovery command (argv). Run on demand by the
    /// app, cached per app run, warn-and-fall-through on failure.
    #[serde(default)]
    pub models_cmd: Option<Vec<String>>,
}

/// The effective harness set: builtin defaults overlaid by config entries.
#[derive(Debug, Clone)]
pub struct HarnessRegistry {
    specs: BTreeMap<String, HarnessSpec>,
}

/// Compiled-in default specs. Launch shapes preserve the exact commands the
/// old `AgentConfig::interactive_cmd`/`worker_cmd` built (regression-tested
/// below); the only delta is that substituted values are shell-quoted.
fn builtin_specs() -> BTreeMap<String, HarnessSpec> {
    let mut m = BTreeMap::new();
    m.insert("claude-code".to_string(), HarnessSpec {
        enabled:          true,
        binary:           Some("claude".into()),
        interactive_args: vec!["--session-id".into(), "{session_id}".into(), "--model".into(), "{model}".into()],
        worker_args:      Some(vec![
            "--session-id".into(), "{session_id}".into(),
            "--model".into(), "{model}".into(),
            "--".into(), "{prompt}".into(),
        ]),
        resume_args: vec![
            "--resume".into(), "{session_id}".into(),
            "--model".into(), "{model}".into(),
        ],
        skip_permissions_args: Some(vec![CLAUDE_SKIP_PERMISSIONS_FLAG.into()]),
        append_system_prompt_flag: Some("--append-system-prompt".into()),
        known_models:     vec![
            "claude-opus-5-5".into(),
            "claude-sonnet-5-5".into(),
            "claude-haiku-5-5".into(),
        ],
        ..HarnessSpec::default()
    });
    m.insert("codex".to_string(), HarnessSpec {
        interactive_args: vec!["--model".into(), "{model}".into()],
        worker_args:      Some(vec!["--model".into(), "{model}".into(), "-p".into(), "{prompt}".into()]),
        ..HarnessSpec::default()
    });
    m.insert("opencode".to_string(), HarnessSpec {
        interactive_args: vec!["--model".into(), "{model}".into()],
        worker_args:      Some(vec!["--model".into(), "{model}".into(), "-p".into(), "{prompt}".into()]),
        models_cmd:       Some(vec!["opencode".into(), "models".into()]),
        ..HarnessSpec::default()
    });
    m.insert("aider".to_string(), HarnessSpec {
        interactive_args: vec!["--model".into(), "{model}".into()],
        worker_args:      Some(vec!["--model".into(), "{model}".into(), "--message".into(), "{prompt}".into()]),
        // aider's `--list-models` needs a search-term argument, so no
        // reliable bare models_cmd ships by default; supply one in config.
        ..HarnessSpec::default()
    });
    m.insert("freebuff".to_string(), HarnessSpec {
        // Researched 2026-07-05: worker/one-shot mode UNVERIFIED — no
        // worker_args until someone supplies them in config.
        interactive_args: vec!["--model".into(), "{model}".into()],
        ..HarnessSpec::default()
    });
    // Researched against pi 1.5.0 (`@earendil-works/pi-coding-agent`, needs
    // Node >= 22.19). `pi [options] [messages...]` stays interactive with the
    // message as its first prompt (`-p` would exit). `--session-id` creates or
    // reopens the cwd's session with the id ninox chose, so resume uses it.
    // `-a` trusts project-local files for the run: without it pi blocks on a
    // "Trust project folder?" dialog and ignores `.agents/skills`. pi has no
    // permission prompts, so no skip_permissions_args.
    m.insert("pi".to_string(), HarnessSpec {
        interactive_args:  vec!["-a".into(), "--session-id".into(), "{session_id}".into(), "--model".into(), "{model}".into()],
        // Leading space: pi has no `--` and would parse a brief starting with
        // `-` as a flag or `@` as a file attach.
        worker_args:       Some(vec![
            "-a".into(), "--session-id".into(), "{session_id}".into(),
            "--model".into(), "{model}".into(), " {prompt}".into(),
        ]),
        resume_args:       vec!["-a".into(), "--session-id".into(), "{session_id}".into(), "--model".into(), "{model}".into()],
        no_permission_prompts: true,
        install_hint: Some("install it with `npm install -g @earendil-works/pi-coding-agent` or `curl -fsSL https://pi.dev/install.sh | sh`".into()),
        extra_skills_dir: Some(".agents/skills".into()),
        models_cmd:        Some(vec!["pi".into(), "--list-models".into()]),
        known_models:      vec![
            "anthropic/claude-sonnet-5-5".into(),
            "anthropic/claude-haiku-5-5".into(),
            "anthropic/claude-opus-5-5".into(),
        ],
        ..HarnessSpec::default()
    });
    m
}

impl HarnessRegistry {
    /// Whether `harness` runs ninox's seeded `SessionStart` hook, so session
    /// setup can arrive that way instead of through a launch prompt, an
    /// appended system prompt or a seeded file. Only claude-code does.
    pub fn supports_session_hooks(&self, harness: &str) -> bool {
        harness == CLAUDE_CODE_HARNESS
    }

    /// Builtins overlaid by config `[harnesses.*]` entries (a config entry
    /// replaces the builtin spec wholesale). claude-code is the locked-on
    /// default and cannot be disabled.
    pub fn from_config(overrides: &BTreeMap<String, HarnessSpec>) -> Self {
        let mut specs = builtin_specs();
        for (name, spec) in overrides {
            let mut spec = spec.clone();
            // Safety-relevant flags a pre-existing `[harnesses.<name>]` table
            // can't have known about: never silently lose them.
            if let Some(builtin) = specs.get(name) {
                spec.no_permission_prompts |= builtin.no_permission_prompts;
                spec.resume_cwd_scoped |= builtin.resume_cwd_scoped;
                spec.extra_skills_dir = spec.extra_skills_dir.or_else(|| builtin.extra_skills_dir.clone());
                spec.append_system_prompt_flag =
                    spec.append_system_prompt_flag.or_else(|| builtin.append_system_prompt_flag.clone());
            }
            specs.insert(name.clone(), spec);
        }
        if let Some(cc) = specs.get_mut("claude-code") {
            cc.enabled = true;
        }
        Self { specs }
    }

    /// Effective spec for a harness name. Unknown names run verbatim as the
    /// binary with no args (spec: "unknown harnesses run their name
    /// verbatim as the binary").
    pub fn spec(&self, name: &str) -> HarnessSpec {
        self.specs.get(name).cloned().unwrap_or_else(|| HarnessSpec {
            enabled: true,
            binary:  Some(name.to_string()),
            ..HarnessSpec::default()
        })
    }

    /// All registered harness names: claude-code first, rest alphabetical.
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.specs.keys().cloned().collect();
        if let Some(pos) = v.iter().position(|n| n == "claude-code") {
            let cc = v.remove(pos);
            v.insert(0, cc);
        }
        v
    }

    pub fn enabled_names(&self) -> Vec<String> {
        self.names().into_iter().filter(|n| self.specs[n].enabled).collect()
    }

    /// Interactive launch command (orchestrator / standalone sessions).
    /// `claude_session_id` is the UUID ninox generated for this spawn (see
    /// `new_claude_session_id`) — always required so every claude-code
    /// session is resumable from birth.
    pub fn interactive_cmd(&self, agent: &AgentConfig, mode: PermissionMode, claude_session_id: &str) -> String {
        let spec = self.spec(&agent.harness);
        build_cmd(&spec, agent, &spec.interactive_args, mode, None, claude_session_id)
    }

    /// [`Self::interactive_cmd_with_prompt`] with the prompt read from `file`
    /// by the launching shell, so its size never reaches a command-line
    /// limit (tmux caps a command at about 16 KiB).
    pub fn interactive_cmd_with_prompt_file(&self, agent: &AgentConfig, mode: PermissionMode, file: &str, claude_session_id: &str) -> Option<String> {
        const SENTINEL: &str = "NINOXPROMPTFILEPLACEHOLDER";
        let cmd = self.interactive_cmd_with_prompt(agent, mode, SENTINEL, claude_session_id)?;
        let read = format!("\"$(cat {})\"", shell_quote(file));
        Some(cmd.replace(&format!("'{SENTINEL}'"), &read))
    }

    /// A fresh interactive launch that starts on `prompt` as its first
    /// input, or `None` when the harness has no such shape. Supported when
    /// the spec's `worker_args` end in a positional `-- {prompt}` (Claude
    /// Code's interactive-with-prompt form); harnesses whose prompt flag is
    /// non-interactive (`-p`, `--message`) must be fed through the pane.
    pub fn interactive_cmd_with_prompt(&self, agent: &AgentConfig, mode: PermissionMode, prompt: &str, claude_session_id: &str) -> Option<String> {
        let spec = self.spec(&agent.harness);
        let w = spec.worker_args.as_ref()?;
        let [.., dashes, placeholder] = w.as_slice() else { return None };
        if dashes != "--" || placeholder != "{prompt}" {
            return None;
        }
        let mut args = spec.interactive_args.clone();
        args.extend(["--".to_string(), "{prompt}".to_string()]);
        Some(build_cmd(&spec, agent, &args, mode, Some(prompt), claude_session_id))
    }

    /// `cmd` (from [`Self::interactive_cmd`] / [`Self::resume_cmd`]) with
    /// `prompt` appended to the harness's system prompt, or `None` when this
    /// harness has no such flag. Must be applied to resumes too: the flag is
    /// per-process, not part of the stored conversation. Goes before a `--`
    /// prompt separator when the command has one.
    pub fn with_system_prompt(&self, agent: &AgentConfig, cmd: &str, prompt: &str) -> Option<String> {
        let flag = self.spec(&agent.harness).append_system_prompt_flag?;
        let extra = format!("{flag} {}", shell_quote(prompt));
        Some(match cmd.find(" -- ") {
            Some(at) => format!("{} {extra}{}", &cmd[..at], &cmd[at..]),
            None => format!("{cmd} {extra}"),
        })
    }

    /// Worker launch command, or `None` when the spec has no `worker_args`
    /// (worker mode unverified for this harness).
    pub fn worker_cmd(&self, agent: &AgentConfig, mode: PermissionMode, prompt: &str, claude_session_id: &str) -> Option<String> {
        let spec = self.spec(&agent.harness);
        let wargs = spec.worker_args.as_ref()?;
        Some(build_cmd(&spec, agent, wargs, mode, Some(prompt), claude_session_id))
    }

    /// Why this agent's launch may not end up accepting cross-session
    /// messages despite ninox's own `--settings`, for the spawn result to
    /// surface: the harness args carry a `--settings <file>` ninox can't
    /// merge into, or an inline value other than `accept`. `None` when
    /// fine or not claude-code.
    pub fn cross_session_warning(&self, agent: &AgentConfig) -> Option<String> {
        if agent.harness != CLAUDE_CODE_HARNESS {
            return None;
        }
        let spec = self.spec(&agent.harness);
        let lists = [Some(&spec.interactive_args), spec.worker_args.as_ref(), Some(&spec.resume_args)];
        let found = lists.into_iter().flatten().find_map(|args| {
            let (_, value, _) = find_settings_flag(args)?;
            match inline_settings(value) {
                None => Some(format!(
                    "the claude-code harness args pass `--settings {value}`; ninox can't add {CROSS_SESSION_KEY} to a file, \
                     so make sure it sets \"{CROSS_SESSION_KEY}\": \"{CROSS_SESSION_ACCEPT}\" or inter-session messages may be held"
                )),
                Some(map) => match map.get(CROSS_SESSION_KEY).and_then(|v| v.as_str()) {
                    Some(v) if v != CROSS_SESSION_ACCEPT => Some(format!(
                        "the claude-code harness args set {CROSS_SESSION_KEY} to \"{v}\"; inter-session messages to this session will not be delivered"
                    )),
                    _ => None,
                },
            }
        });
        found
    }

    /// Refuses a worker type with `permissions.enabled` on a harness that has
    /// no permission prompts: the profile would be silently ignored and the
    /// session would run unrestricted.
    pub fn ensure_permissions_enforceable(&self, harness: &str, worker_type: Option<&WorkerTypeSpec>) -> anyhow::Result<()> {
        if let Some(t) = worker_type.filter(|t| t.permissions.enabled) {
            anyhow::ensure!(
                !self.spec(harness).no_permission_prompts,
                "worker type '{}' has permission rules enabled, but harness '{harness}' has no permission prompts \
                 and would ignore them — disable the type's permissions or use claude-code",
                t.name,
            );
        }
        Ok(())
    }

    /// The spec's install hint when its binary isn't on this process's PATH.
    pub fn missing_binary_hint(&self, harness: &str) -> Option<String> {
        let spec = self.spec(harness);
        let hint = spec.install_hint.clone()?;
        let bin = spec.binary.clone().unwrap_or_else(|| harness.to_string());
        let found = std::env::var_os("PATH")
            .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(&bin).is_file()));
        (!found).then(|| format!("'{bin}' is not on PATH — {hint}"))
    }

    /// Whether this harness can resume a conversation at all.
    pub fn can_resume(&self, agent: &AgentConfig) -> bool {
        !self.spec(&agent.harness).resume_args.is_empty()
    }

    /// Whether `session` can be resumed: the harness supports it and, for a
    /// cwd-scoped resume (pi), no other session in `all` ever ran in the same
    /// directory — otherwise `--continue` could reopen that one's conversation.
    pub fn session_can_resume(&self, session: &crate::types::Session, all: &[crate::types::Session]) -> bool {
        let agent = AgentConfig { harness: session.agent_type.clone(), model: session.model.clone() };
        if !self.can_resume(&agent) {
            return false;
        }
        if !self.spec(&agent.harness).resume_cwd_scoped {
            return true;
        }
        let Some(cwd) = session.workspace_path.as_deref() else { return false };
        !all.iter().any(|o| o.id != session.id && o.workspace_path.as_deref() == Some(cwd))
    }

    /// Resume command for a previously-interrupted session, or `None` when
    /// the harness has no `resume_args` (resume unsupported). Always
    /// rebuilds from `interactive`-style args (never `worker_args`) — a
    /// resumed session already has its original prompt in the transcript;
    /// replaying it via `worker_args`' trailing `-- {prompt}` would
    /// re-inject a stale instruction instead of continuing. `mode` must be
    /// the session's launch mode, so a resume never widens permissions.
    pub fn resume_cmd(&self, agent: &AgentConfig, mode: PermissionMode, claude_session_id: &str) -> Option<String> {
        let spec = self.spec(&agent.harness);
        if spec.resume_args.is_empty() {
            return None;
        }
        Some(build_cmd(&spec, agent, &spec.resume_args, mode, None, claude_session_id))
    }
}

fn build_cmd(
    spec: &HarnessSpec,
    agent: &AgentConfig,
    args: &[String],
    mode: PermissionMode,
    prompt: Option<&str>,
    claude_session_id: &str,
) -> String {
    let binary = spec.binary.clone().unwrap_or_else(|| agent.harness.clone());
    let model = agent.model.as_deref().or(spec.model.as_deref());
    let skip = spec.skip_permissions_args.as_deref().unwrap_or(&[]);
    let mut out: Vec<String> = match mode {
        PermissionMode::Bypass => skip.to_vec(),
        PermissionMode::Standard => Vec::new(),
    };
    let mut shape = without_run(args, skip);
    if mode == PermissionMode::Standard {
        shape = without_run(&shape, &[CLAUDE_SKIP_PERMISSIONS_FLAG.to_string()]);
    }
    let mut expanded = expand_args(&shape, model, prompt, claude_session_id);
    if agent.harness == CLAUDE_CODE_HARNESS {
        expanded = with_cross_session_accept(expanded);
    }
    out.extend(expanded);
    join_cmd(binary, out)
}

/// The built-in Claude Code harness; the only one that understands
/// `--settings`.
pub const CLAUDE_CODE_HARNESS: &str = "claude-code";
/// Claude Code setting that lets a session receive messages from other
/// sessions. Its default holds them for a session that bypasses permission
/// prompts, which every ninox worker does.
pub const CROSS_SESSION_KEY: &str = "crossSessionInbound";
pub const CROSS_SESSION_ACCEPT: &str = "accept";

/// The text a shell would pass for a single-quoted token, including the
/// `'\''` idiom for an embedded quote.
fn unquote_single(s: &str) -> String {
    let inner = s.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')).unwrap_or(s);
    inner.replace("'\\''", "'")
}

/// Index of a `--settings <v>` / `--settings=<v>` flag in `args`, with its
/// value and whether it is the `=` form.
fn find_settings_flag(args: &[String]) -> Option<(usize, &str, bool)> {
    args.iter().enumerate().find_map(|(i, a)| {
        if a == "--settings" {
            Some((i, args.get(i + 1).map_or("", String::as_str), false))
        } else {
            a.strip_prefix("--settings=").map(|v| (i, v, true))
        }
    })
}

/// A `--settings` value that is inline JSON (rather than a file path).
fn inline_settings(value: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    match serde_json::from_str(&unquote_single(value.trim())) {
        Ok(serde_json::Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// Make `args` carry `crossSessionInbound: "accept"`. Claude Code takes one
/// `--settings`, so when the args already hold one with inline JSON the key
/// is merged into it (a value already there wins); a `--settings <file>` is
/// left alone, since ninox doesn't own that file (see
/// [`HarnessRegistry::cross_session_warning`]).
fn with_cross_session_accept(mut args: Vec<String>) -> Vec<String> {
    let json = |map: &serde_json::Map<String, serde_json::Value>| shell_quote(&serde_json::Value::Object(map.clone()).to_string());
    match find_settings_flag(&args) {
        None => {
            let map = serde_json::Map::from_iter([(CROSS_SESSION_KEY.to_string(), CROSS_SESSION_ACCEPT.into())]);
            // Before a `--` prompt separator when there is one, else last.
            let at = args.iter().position(|a| a == "--").unwrap_or(args.len());
            args.splice(at..at, ["--settings".to_string(), json(&map)]);
        }
        Some((i, value, eq)) => {
            let Some(mut map) = inline_settings(value) else { return args };
            if !eq && i + 1 >= args.len() {
                return args;
            }
            if map.contains_key(CROSS_SESSION_KEY) {
                return args;
            }
            map.insert(CROSS_SESSION_KEY.to_string(), CROSS_SESSION_ACCEPT.into());
            if eq {
                args[i] = format!("--settings={}", json(&map));
            } else {
                args[i + 1] = json(&map);
            }
        }
    }
    args
}

/// `args` with every contiguous occurrence of `run` removed: a multi-arg
/// switch (`--permission-mode bypassPermissions`) goes as a unit, and the
/// same words elsewhere (`--permission-mode acceptEdits`) are left alone.
fn without_run(args: &[String], run: &[String]) -> Vec<String> {
    if run.is_empty() {
        return args.to_vec();
    }
    let mut out = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        if args[i..].starts_with(run) {
            i += run.len();
        } else {
            out.push(args[i].clone());
            i += 1;
        }
    }
    out
}

fn join_cmd(binary: String, args: Vec<String>) -> String {
    if args.is_empty() { binary } else { format!("{} {}", binary, args.join(" ")) }
}

/// Expand `{model}`/`{prompt}`/`{session_id}` in an arg list. When `model` is
/// `None`, an element containing `{model}` is dropped — along with the
/// immediately preceding `-`-prefixed literal flag, so a `["--model",
/// "{model}"]` pair vanishes cleanly. Substituted elements are shell-quoted;
/// literals pass through verbatim.
fn expand_args(args: &[String], model: Option<&str>, prompt: Option<&str>, claude_session_id: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let flag_for_dropped_model = model.is_none()
            && a.starts_with('-')
            && !a.contains('{')
            && args.get(i + 1).is_some_and(|n| n.contains("{model}"));
        if flag_for_dropped_model {
            i += 2;
            continue;
        }
        if a.contains("{model}") && model.is_none() {
            i += 1;
            continue;
        }
        let had_placeholder = a.contains("{model}") || a.contains("{prompt}") || a.contains("{session_id}");
        let mut s = a.replace("{model}", model.unwrap_or(""));
        s = s.replace("{session_id}", claude_session_id);
        // Last, so placeholder-looking text inside the prompt is left alone.
        if let Some(p) = prompt {
            s = s.replace("{prompt}", p);
        }
        out.push(if had_placeholder { shell_quote(&s) } else { s });
        i += 1;
    }
    out
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A fresh UUID for `--session-id`/`--resume`. Generated by ninox at every
/// spawn (and at every Re-file, which discards the old conversation and
/// starts a new one) — never by the harness itself.
pub fn new_claude_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AgentConfig;
    use std::collections::BTreeMap;

    fn reg() -> HarnessRegistry { HarnessRegistry::from_config(&BTreeMap::new()) }

    fn agent(harness: &str, model: Option<&str>) -> AgentConfig {
        AgentConfig { harness: harness.into(), model: model.map(Into::into) }
    }

    // ── Ported launch-shape regression tests (from config.rs) ──────────────
    // Substituted values are now shell-quoted; semantically identical.

    #[test]
    fn interactive_cmd_claude_with_model() {
        assert_eq!(reg().interactive_cmd(&agent("claude-code", Some("claude-opus-4-5")), PermissionMode::Standard, "sess-1"),
                   "claude --session-id 'sess-1' --model 'claude-opus-4-5' --settings '{\"crossSessionInbound\":\"accept\"}'");
    }

    #[test]
    fn interactive_cmd_claude_without_model_is_bare() {
        assert_eq!(reg().interactive_cmd(&agent("claude-code", None), PermissionMode::Standard, "sess-1"),
                   "claude --session-id 'sess-1' --settings '{\"crossSessionInbound\":\"accept\"}'");
    }

    #[test]
    fn worker_cmd_claude_code() {
        assert_eq!(reg().worker_cmd(&agent("claude-code", None), PermissionMode::Bypass, "Fix the bug", "sess-1").unwrap(),
                   "claude --dangerously-skip-permissions --session-id 'sess-1' --settings '{\"crossSessionInbound\":\"accept\"}' -- 'Fix the bug'");
    }

    #[test]
    fn worker_cmd_claude_code_with_model() {
        assert_eq!(reg().worker_cmd(&agent("claude-code", Some("claude-opus-4-5")), PermissionMode::Bypass, "Fix the bug", "sess-1").unwrap(),
                   "claude --dangerously-skip-permissions --session-id 'sess-1' --model 'claude-opus-4-5' --settings '{\"crossSessionInbound\":\"accept\"}' -- 'Fix the bug'");
    }

    #[test]
    fn worker_cmd_codex() {
        assert_eq!(reg().worker_cmd(&agent("codex", Some("gpt-4o")), PermissionMode::Bypass, "do the thing", "sess-1").unwrap(),
                   "codex --model 'gpt-4o' -p 'do the thing'");
    }

    #[test]
    fn worker_cmd_aider_uses_message_flag() {
        assert_eq!(reg().worker_cmd(&agent("aider", None), PermissionMode::Bypass, "fix it", "sess-1").unwrap(),
                   "aider --message 'fix it'");
    }

    #[test]
    fn worker_cmd_quotes_single_quotes_in_prompt() {
        assert_eq!(reg().worker_cmd(&agent("codex", None), PermissionMode::Bypass, "don't break", "sess-1").unwrap(),
                   "codex -p 'don'\\''t break'");
    }


    // ── pi ─────────────────────────────────────────────────────────────────

    #[test]
    fn pi_ships_disabled_and_toggle_keeps_its_args() {
        let r = reg();
        assert!(!r.spec("pi").enabled);
        let mut cfg = crate::config::AppConfig::default();
        assert!(cfg.toggle_harness("pi"));
        let r = cfg.registry();
        assert!(r.enabled_names().contains(&"pi".to_string()));
        assert_eq!(r.interactive_cmd(&agent("pi", Some("openai/gpt-5-codex")), PermissionMode::Standard, "s"),
                   "pi -a --session-id 's' --model 'openai/gpt-5-codex'");
    }

    #[test]
    fn pi_interactive_without_model_keeps_trust_and_session_id() {
        assert_eq!(reg().interactive_cmd(&agent("pi", None), PermissionMode::Standard, "s"), "pi -a --session-id 's'");
    }

    #[test]
    fn pi_worker_passes_brief_as_positional_first_prompt_not_print_mode() {
        let cmd = reg().worker_cmd(&agent("pi", Some("anthropic/claude-sonnet-5-5")), PermissionMode::Bypass, "it's a brief", "s").unwrap();
        assert_eq!(cmd, "pi -a --session-id 's' --model 'anthropic/claude-sonnet-5-5' ' it'\\''s a brief'");
        assert!(!cmd.contains(" -p"));
    }

    #[test]
    fn pi_has_no_permission_switch_so_bypass_equals_standard() {
        let r = reg();
        let a = agent("pi", Some("m"));
        assert_eq!(r.worker_cmd(&a, PermissionMode::Bypass, "go", "s"), r.worker_cmd(&a, PermissionMode::Standard, "go", "s"));
        assert!(r.spec("pi").skip_permissions_args.is_none());
    }

    #[test]
    fn pi_resume_reopens_the_session_ninox_chose() {
        assert_eq!(reg().resume_cmd(&agent("pi", Some("m")), PermissionMode::Standard, "ninox-id").unwrap(),
                   "pi -a --session-id 'ninox-id' --model 'm'");
    }

    #[test]
    fn pi_resume_is_not_cwd_scoped_so_shared_cwds_can_resume() {
        let r = reg();
        let a = pi_session("a", "pi", Some("/shared"));
        let b = pi_session("b", "pi", Some("/shared"));
        assert!(r.session_can_resume(&a, &[a.clone(), b]));
    }

    fn pi_session(id: &str, harness: &str, cwd: Option<&str>) -> crate::types::Session {
        crate::types::Session {
            id: id.into(), orchestrator_id: None, name: id.into(),
            repo: String::new(), status: crate::types::SessionStatus::Working,
            agent_type: harness.into(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None,
            workspace_path: cwd.map(Into::into), pid: None,
            model: None, observed_model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: Some("x".into()),
            summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }
    }

    #[test]
    fn pi_refuses_a_worker_type_with_enforced_permissions() {
        use crate::worker_type::{WorkerPermissionsConfig, WorkerTypeSpec};
        let r = reg();
        let mut t = WorkerTypeSpec {
            name: "Scoped".into(),
            permissions: WorkerPermissionsConfig { enabled: true, ..Default::default() },
            ..Default::default()
        };
        let err = r.ensure_permissions_enforceable("pi", Some(&t)).unwrap_err().to_string();
        assert!(err.contains("Scoped") && err.contains("no permission prompts"), "{err}");
        assert!(r.ensure_permissions_enforceable("claude-code", Some(&t)).is_ok());
        t.permissions.enabled = false;
        assert!(r.ensure_permissions_enforceable("pi", Some(&t)).is_ok());
        assert!(r.ensure_permissions_enforceable("pi", None).is_ok());
    }

    #[test]
    fn missing_pi_binary_names_the_npm_package() {
        let hint = reg().missing_binary_hint("pi");
        if std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("pi").is_file())) {
            assert!(hint.is_none());
        } else {
            assert!(hint.unwrap().contains("@earendil-works/pi-coding-agent"));
        }
        assert!(reg().missing_binary_hint("claude-code").is_none(), "no hint, nothing to say");
    }

    #[test]
    fn a_legacy_pi_config_table_keeps_the_safety_flags() {
        let mut over = BTreeMap::new();
        over.insert("pi".to_string(), HarnessSpec { enabled: true, ..HarnessSpec::default() });
        let spec = HarnessRegistry::from_config(&over).spec("pi");
        assert!(spec.enabled && spec.no_permission_prompts);
        assert_eq!(spec.extra_skills_dir.as_deref(), Some(".agents/skills"));
    }

    #[test]
    fn a_cwd_scoped_harness_resumes_only_when_its_cwd_is_unique_to_the_session() {
        let mut over = BTreeMap::new();
        over.insert("cwdtool".to_string(), HarnessSpec {
            resume_args: vec!["--continue".into()],
            resume_cwd_scoped: true,
            ..HarnessSpec::default()
        });
        let r = HarnessRegistry::from_config(&over);
        let a = pi_session("a", "cwdtool", Some("/wt/a"));
        let b = pi_session("b", "cwdtool", Some("/wt/b"));
        assert!(r.session_can_resume(&a, &[a.clone(), b.clone()]));
        let shared = pi_session("c", "cwdtool", Some("/wt/a"));
        assert!(!r.session_can_resume(&a, &[a.clone(), shared.clone()]), "another row in the same cwd");
        assert!(!r.session_can_resume(&shared, &[a.clone(), shared.clone()]));
        let other_harness = pi_session("d", "claude-code", Some("/wt/a"));
        assert!(!r.session_can_resume(&a, &[a.clone(), other_harness]), "any harness sharing the cwd counts");
        let nowhere = pi_session("e", "cwdtool", None);
        assert!(!r.session_can_resume(&nowhere, std::slice::from_ref(&nowhere)), "unknown cwd cannot be proven unique");
    }

    #[test]
    fn session_id_resume_harnesses_ignore_cwd_sharing() {
        let r = reg();
        let a = pi_session("a", "claude-code", Some("/shared"));
        let b = pi_session("b", "claude-code", Some("/shared"));
        assert!(r.session_can_resume(&a, &[a.clone(), b]));
        let cx = pi_session("c", "codex", Some("/x"));
        assert!(!r.session_can_resume(&cx, std::slice::from_ref(&cx)), "codex has no resume_args");
    }

    // ── Registry semantics ──────────────────────────────────────────────────

    #[test]
    fn unknown_harness_runs_its_name_verbatim() {
        assert_eq!(reg().interactive_cmd(&agent("mytool", None), PermissionMode::Standard, "sess-1"), "mytool");
        // No worker_args on a synthesized spec → not worker-capable.
        assert!(reg().worker_cmd(&agent("mytool", None), PermissionMode::Bypass, "p", "sess-1").is_none());
    }

    #[test]
    fn freebuff_ships_disabled_without_worker_args() {
        let r = reg();
        let fb = r.spec("freebuff");
        assert!(!fb.enabled);
        assert!(fb.worker_args.is_none());
        assert_eq!(r.interactive_cmd(&agent("freebuff", Some("fb-large")), PermissionMode::Standard, "sess-1"),
                   "freebuff --model 'fb-large'");
    }

    #[test]
    fn only_claude_code_enabled_by_default() {
        assert_eq!(reg().enabled_names(), vec!["claude-code".to_string()]);
    }

    #[test]
    fn names_lists_claude_code_first_then_alphabetical() {
        assert_eq!(reg().names(),
                   vec!["claude-code", "aider", "codex", "freebuff", "opencode", "pi"]
                       .into_iter().map(String::from).collect::<Vec<_>>());
    }

    #[test]
    fn config_entry_replaces_builtin_spec() {
        let mut over = BTreeMap::new();
        over.insert("codex".to_string(), HarnessSpec {
            enabled: true,
            binary:  Some("codex-nightly".into()),
            interactive_args: vec!["--model".into(), "{model}".into()],
            ..HarnessSpec::default()
        });
        let r = HarnessRegistry::from_config(&over);
        assert_eq!(r.interactive_cmd(&agent("codex", Some("o3")), PermissionMode::Standard, "sess-1"), "codex-nightly --model 'o3'");
        assert!(r.enabled_names().contains(&"codex".to_string()));
    }

    #[test]
    fn config_entry_extends_registry_with_new_harness() {
        let mut over = BTreeMap::new();
        over.insert("freebuff2".to_string(), HarnessSpec {
            enabled: true,
            worker_args: Some(vec!["--model".into(), "{model}".into(), "-p".into(), "{prompt}".into()]),
            ..HarnessSpec::default()
        });
        let r = HarnessRegistry::from_config(&over);
        // binary defaults to the harness name
        assert_eq!(r.worker_cmd(&agent("freebuff2", None), PermissionMode::Bypass, "go", "sess-1").unwrap(), "freebuff2 -p 'go'");
    }

    #[test]
    fn claude_code_cannot_be_disabled_via_config() {
        let mut over = BTreeMap::new();
        over.insert("claude-code".to_string(),
                    HarnessSpec { enabled: false, ..HarnessSpec::default() });
        let r = HarnessRegistry::from_config(&over);
        assert!(r.enabled_names().contains(&"claude-code".to_string()));
    }

    #[test]
    fn agent_model_overrides_spec_model() {
        let mut over = BTreeMap::new();
        over.insert("codex".to_string(), HarnessSpec {
            model: Some("spec-default".into()),
            interactive_args: vec!["--model".into(), "{model}".into()],
            ..HarnessSpec::default()
        });
        let r = HarnessRegistry::from_config(&over);
        assert_eq!(r.interactive_cmd(&agent("codex", Some("chosen")), PermissionMode::Standard, "sess-1"), "codex --model 'chosen'");
        assert_eq!(r.interactive_cmd(&agent("codex", None), PermissionMode::Standard, "sess-1"),           "codex --model 'spec-default'");
    }

    #[test]
    fn resume_cmd_claude_code_with_model() {
        assert_eq!(
            reg().resume_cmd(&agent("claude-code", Some("claude-opus-4-5")), PermissionMode::Standard, "sess-1").unwrap(),
            "claude --resume 'sess-1' --model 'claude-opus-4-5' --settings '{\"crossSessionInbound\":\"accept\"}'"
        );
    }

    #[test]
    fn resume_cmd_claude_code_without_model() {
        assert_eq!(
            reg().resume_cmd(&agent("claude-code", None), PermissionMode::Standard, "sess-1").unwrap(),
            "claude --resume 'sess-1' --settings '{\"crossSessionInbound\":\"accept\"}'"
        );
    }

    #[test]
    fn resume_cmd_none_for_harness_without_resume_args() {
        // codex's builtin spec has no resume_args — resuming isn't supported.
        assert!(reg().resume_cmd(&agent("codex", Some("gpt-4o")), PermissionMode::Standard, "sess-1").is_none());
    }

    fn wt(enabled: bool) -> WorkerTypeSpec {
        let mut t = WorkerTypeSpec { name: "T".into(), ..WorkerTypeSpec::default() };
        t.permissions.enabled = enabled;
        t
    }

    #[test]
    fn permission_mode_bypasses_only_unenforced_workers() {
        use LaunchKind::*;
        assert_eq!(permission_mode(Worker, None), PermissionMode::Bypass);
        assert_eq!(permission_mode(Worker, Some(&wt(false))), PermissionMode::Bypass);
        assert_eq!(permission_mode(Worker, Some(&wt(true))), PermissionMode::Standard);
        assert_eq!(permission_mode(Interactive, None), PermissionMode::Standard);
        assert_eq!(permission_mode(Interactive, Some(&wt(true))), PermissionMode::Standard);
    }

    #[test]
    fn resume_carries_the_skip_flag_only_in_bypass_mode() {
        let a = agent("claude-code", Some("m"));
        assert_eq!(reg().resume_cmd(&a, PermissionMode::Standard, "s").unwrap(), "claude --resume 's' --model 'm' --settings '{\"crossSessionInbound\":\"accept\"}'");
        assert_eq!(reg().resume_cmd(&a, PermissionMode::Bypass, "s").unwrap(),
                   "claude --dangerously-skip-permissions --resume 's' --model 'm' --settings '{\"crossSessionInbound\":\"accept\"}'");
    }

    #[test]
    fn fresh_and_resume_agree_on_the_skip_flag_in_every_mode() {
        let a = agent("claude-code", Some("m"));
        for mode in [PermissionMode::Standard, PermissionMode::Bypass] {
            let want = mode == PermissionMode::Bypass;
            let fresh_worker = reg().worker_cmd(&a, mode, "p", "s").unwrap();
            let fresh_interactive = reg().interactive_cmd(&a, mode, "s");
            let resume = reg().resume_cmd(&a, mode, "s").unwrap();
            for cmd in [&fresh_worker, &fresh_interactive, &resume] {
                assert_eq!(cmd.contains(CLAUDE_SKIP_PERMISSIONS_FLAG), want, "{mode:?}: {cmd}");
            }
        }
    }

    #[test]
    fn a_legacy_config_with_the_skip_flag_inline_is_stripped_in_standard_mode() {
        let mut builtin = reg().spec("claude-code");
        builtin.resume_args.insert(0, CLAUDE_SKIP_PERMISSIONS_FLAG.into());
        builtin.worker_args.as_mut().unwrap().insert(0, CLAUDE_SKIP_PERMISSIONS_FLAG.into());
        builtin.skip_permissions_args = None;
        let r = HarnessRegistry::from_config(&BTreeMap::from([("claude-code".to_string(), builtin)]));
        let a = agent("claude-code", None);
        assert_eq!(r.resume_cmd(&a, PermissionMode::Standard, "s").unwrap(), "claude --resume 's' --settings '{\"crossSessionInbound\":\"accept\"}'");
        assert_eq!(r.resume_cmd(&a, PermissionMode::Bypass, "s").unwrap(), "claude --dangerously-skip-permissions --resume 's' --settings '{\"crossSessionInbound\":\"accept\"}'");
        assert!(!r.worker_cmd(&a, PermissionMode::Standard, "p", "s").unwrap().contains(CLAUDE_SKIP_PERMISSIONS_FLAG));
    }

    #[test]
    fn a_custom_harness_without_a_bypass_switch_keeps_its_own_args_in_bypass_mode() {
        let spec = HarnessSpec {
            worker_args: Some(vec![CLAUDE_SKIP_PERMISSIONS_FLAG.into(), "-p".into(), "{prompt}".into()]),
            ..HarnessSpec::default()
        };
        let r = HarnessRegistry::from_config(&BTreeMap::from([("cc-nightly".to_string(), spec)]));
        let a = agent("cc-nightly", None);
        assert_eq!(r.worker_cmd(&a, PermissionMode::Bypass, "go", "s").unwrap(), "cc-nightly --dangerously-skip-permissions -p 'go'");
        assert_eq!(r.worker_cmd(&a, PermissionMode::Standard, "go", "s").unwrap(), "cc-nightly -p 'go'");
    }

    #[test]
    fn a_multi_arg_bypass_switch_is_stripped_as_a_unit() {
        let spec = HarnessSpec {
            interactive_args: vec!["--permission-mode".into(), "acceptEdits".into()],
            resume_args: vec!["--permission-mode".into(), "bypassPermissions".into(), "--resume".into(), "{session_id}".into()],
            skip_permissions_args: Some(vec!["--permission-mode".into(), "bypassPermissions".into()]),
            ..HarnessSpec::default()
        };
        let r = HarnessRegistry::from_config(&BTreeMap::from([("cc".to_string(), spec)]));
        let a = agent("cc", None);
        assert_eq!(r.interactive_cmd(&a, PermissionMode::Standard, "s"), "cc --permission-mode acceptEdits");
        assert_eq!(r.resume_cmd(&a, PermissionMode::Standard, "s").unwrap(), "cc --resume 's'");
        assert_eq!(r.resume_cmd(&a, PermissionMode::Bypass, "s").unwrap(), "cc --permission-mode bypassPermissions --resume 's'");
    }

    #[test]
    fn interactive_launch_prompt_is_claude_only_and_has_no_resume() {
        let reg = HarnessRegistry::from_config(&BTreeMap::new());
        let claude = AgentConfig { harness: "claude-code".into(), model: Some("m".into()) };
        let cmd = reg.interactive_cmd_with_prompt(&claude, PermissionMode::Standard, "it's {session_id}", "sid").unwrap();
        assert!(cmd.starts_with("claude --session-id 'sid'") && !cmd.contains("--resume"), "{cmd}");
        assert!(cmd.ends_with("-- 'it'\\''s {session_id}'"), "{cmd}");
        let codex = AgentConfig { harness: "codex".into(), model: None };
        assert!(reg.interactive_cmd_with_prompt(&codex, PermissionMode::Standard, "p", "sid").is_none());
    }

    #[test]
    fn new_claude_session_id_returns_distinct_uuids() {
        let a = new_claude_session_id();
        let b = new_claude_session_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36); // UUID v4 string form, e.g. 8-4-4-4-12 hex groups
    }

    #[test]
    fn harness_spec_round_trips_through_toml() {
        let toml_src = r#"
            [harnesses.freebuff]
            enabled = true
            binary  = "freebuff"
            model   = "fb-large"
            interactive_args = ["--model", "{model}"]
            worker_args      = ["--model", "{model}", "-p", "{prompt}"]
            known_models     = ["fb-large", "fb-mini"]
            models_cmd       = ["freebuff", "models"]
        "#;
        #[derive(serde::Deserialize)]
        struct Wrap { harnesses: BTreeMap<String, HarnessSpec> }
        let w: Wrap = toml::from_str(toml_src).unwrap();
        let fb = &w.harnesses["freebuff"];
        assert!(fb.enabled);
        assert_eq!(fb.known_models, vec!["fb-large", "fb-mini"]);
        assert_eq!(fb.models_cmd.as_deref(), Some(&["freebuff".to_string(), "models".to_string()][..]));
    }

    const ACCEPT: &str = "{\"crossSessionInbound\":\"accept\"}";

    fn registry_with_args(args: &[&str]) -> HarnessRegistry {
        let mut spec = reg().spec("claude-code");
        spec.interactive_args = args.iter().map(|s| s.to_string()).collect();
        spec.worker_args = Some(spec.interactive_args.clone());
        spec.resume_args = spec.interactive_args.clone();
        HarnessRegistry::from_config(&BTreeMap::from([("claude-code".to_string(), spec)]))
    }

    #[test]
    fn every_claude_launch_kind_carries_cross_session_accept() {
        let a = agent("claude-code", None);
        for mode in [PermissionMode::Standard, PermissionMode::Bypass] {
            for cmd in [
                reg().interactive_cmd(&a, mode, "s"),
                reg().worker_cmd(&a, mode, "p", "s").unwrap(),
                reg().resume_cmd(&a, mode, "s").unwrap(),
            ] {
                assert_eq!(cmd.matches("--settings").count(), 1, "{cmd}");
                assert!(cmd.contains(&format!("--settings '{ACCEPT}'")), "{cmd}");
            }
        }
    }

    #[test]
    fn other_harnesses_get_no_settings_flag() {
        for h in ["codex", "pi", "aider"] {
            let cmd = reg().interactive_cmd(&agent(h, None), PermissionMode::Bypass, "s");
            assert!(!cmd.contains("--settings"), "{h}: {cmd}");
        }
    }

    #[test]
    fn a_user_inline_settings_flag_gets_the_key_merged_not_a_second_flag() {
        let a = agent("claude-code", None);
        for args in [
            vec!["--settings", "'{\"model\":\"x\"}'", "--session-id", "{session_id}"],
            vec!["--settings={\"model\":\"x\"}", "--session-id", "{session_id}"],
        ] {
            let cmd = registry_with_args(&args).interactive_cmd(&a, PermissionMode::Standard, "s");
            assert_eq!(cmd.matches("--settings").count(), 1, "{cmd}");
            assert!(cmd.contains("crossSessionInbound") && cmd.contains("\"model\""), "{cmd}");
            assert!(cmd.contains("--session-id 's'"), "{cmd}");
        }
    }

    #[test]
    fn a_user_value_for_the_key_wins() {
        let a = agent("claude-code", None);
        let r = registry_with_args(&["--settings", "'{\"crossSessionInbound\":\"refuse\"}'"]);
        let cmd = r.interactive_cmd(&a, PermissionMode::Standard, "s");
        assert!(cmd.contains("refuse") && !cmd.contains("accept"), "{cmd}");
        assert!(r.cross_session_warning(&a).unwrap().contains("refuse"));
    }

    #[test]
    fn a_settings_file_arg_is_left_alone_and_warned_about() {
        let a = agent("claude-code", None);
        let r = registry_with_args(&["--settings", "/etc/my.json"]);
        let cmd = r.interactive_cmd(&a, PermissionMode::Standard, "s");
        assert_eq!(cmd, "claude --settings /etc/my.json");
        assert!(r.cross_session_warning(&a).unwrap().contains("/etc/my.json"));
    }

    #[test]
    fn no_warning_for_defaults_or_other_harnesses() {
        assert!(reg().cross_session_warning(&agent("claude-code", None)).is_none());
        assert!(reg().cross_session_warning(&agent("codex", None)).is_none());
        let r = registry_with_args(&["--settings", "'{\"model\":\"x\"}'"]);
        assert!(r.cross_session_warning(&agent("claude-code", None)).is_none());
    }

    #[test]
    fn an_inline_settings_value_with_an_escaped_quote_is_still_merged() {
        let a = agent("claude-code", None);
        let r = registry_with_args(&["--settings", r#"'{"x":"it'\''s"}'"#]);
        let cmd = r.interactive_cmd(&a, PermissionMode::Standard, "s");
        assert_eq!(cmd.matches("--settings").count(), 1, "{cmd}");
        assert!(cmd.contains("crossSessionInbound"), "{cmd}");
        assert!(r.cross_session_warning(&a).is_none());
    }

    #[test]
    fn a_trailing_settings_flag_without_a_value_is_not_duplicated() {
        let cmd = registry_with_args(&["--session-id", "{session_id}", "--settings"])
            .interactive_cmd(&agent("claude-code", None), PermissionMode::Standard, "s");
        assert_eq!(cmd.matches("--settings").count(), 1, "{cmd}");
    }
}
