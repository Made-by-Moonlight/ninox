//! Capability registry — the single source of truth for the agent-facing
//! skills Ninox seeds and advertises.
//!
//! Every capability an agent can use is one [`Capability`] entry in
//! [`REGISTRY`], and every surface that needs to know about capabilities
//! loops that slice rather than restating the list:
//!
//! - `ninox_core::orchestrator_root::setup_orchestrator_root` seeds each *enabled*
//!   orchestrator entry's [`Capability::orchestrator_md`] as
//!   `<root>/.claude/skills/<name>/SKILL.md` and builds AGENTS.md's
//!   "Available Skills" bullets from the same (gated) set.
//! - `ninox_app::spawn_util::seed_worker_skills` seeds each *enabled*
//!   worker entry's [`Capability::worker_md`] into a worker's worktree.
//! - `ninox capabilities` prints the registry with each entry's live
//!   enabled/disabled state.
//!
//! Adding a capability means adding one markdown file per audience under
//! `crates/ninox-core/skills/` and one entry here — nothing else.
//!
//! # Markdown, not Rust strings
//!
//! Skill bodies live as real `.md` files under `skills/{orchestrator,worker}/`
//! and are pulled in with `include_str!`, so they can be read and edited as
//! markdown (and linted by a skill-aware editor) instead of being escaped
//! into Rust string literals.
//!
//! # Placeholders
//!
//! A few orchestrator skills need values only known at seed time (the
//! resolved `ninox` binary path, the active config file path). Rather than
//! reverting those bodies to `format!` templates, the markdown carries
//! literal placeholder tokens that [`render`] substitutes when the file is
//! written:
//!
//! | Token              | Replaced with                              |
//! |--------------------|--------------------------------------------|
//! | `{{NINOX_BIN}}`    | the invoking `ninox` binary path           |
//! | `{{CONFIG_PATH}}`  | the active `config.toml` path              |
//!
//! Only orchestrator markdown goes through [`render`]; worker markdown is
//! seeded verbatim (it refers to the `ninox` shim already on a worker's
//! PATH) and is asserted placeholder-free by this module's tests.
//!
//! # Gating
//!
//! [`Capability::enabled`] is the single gate for a capability, evaluated
//! against the live [`AppConfig`] once per seeding pass — call sites no
//! longer thread per-capability booleans around. A capability whose gate
//! differs by audience (today: `watch-pr`, gated on `[pr_watch].enabled`
//! for workers but always seeded for orchestrators) is expressed as two
//! entries sharing a name, one per audience.

use crate::config::AppConfig;

/// Which side of the fleet a capability is meant for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audience {
    /// Worker/standalone sessions only.
    Worker,
    /// Orchestrator sessions only.
    Orchestrator,
    /// Both, via separate `worker_md` / `orchestrator_md` bodies.
    Both,
}

impl Audience {
    /// Whether an entry declared for `self` should surface when filtering
    /// for `other`. [`Audience::Both`] matches (and is matched by)
    /// everything.
    pub fn includes(self, other: Audience) -> bool {
        self == other || self == Audience::Both || other == Audience::Both
    }

    /// The lowercase token used in `ninox capabilities --json`.
    pub fn as_str(self) -> &'static str {
        match self {
            Audience::Worker => "worker",
            Audience::Orchestrator => "orchestrator",
            Audience::Both => "both",
        }
    }
}

/// A `PreToolUse` (or other Claude Code hook event) guard a capability
/// installs into `.claude/settings.json`, gated by the same `enabled` fn as
/// its skill markdown. There is exactly one `ToolHook` value per capability,
/// used verbatim for every audience it targets — unlike skill markdown
/// (which has a separate `orchestrator_md`/`worker_md` body per audience
/// because the wording usually differs), a hook's enforcement logic must
/// not drift between audiences, so there is no per-audience copy to keep in
/// sync.
#[derive(Debug, Clone, Copy)]
pub struct ToolHook {
    /// Claude Code hook event name, e.g. `"PreToolUse"`.
    pub event: &'static str,
    /// The settings.json hooks-array `matcher` (a tool-name glob/regex,
    /// e.g. `"Bash"` — matched against `tool_name`, not the command text;
    /// the script itself inspects `tool_input` for the command).
    pub matcher: &'static str,
    /// File name written under `.claude/`, e.g. `"pr-watch-enforcement.cjs"`.
    pub script_name: &'static str,
    /// The hook script body, written verbatim.
    pub script: &'static str,
}

/// One agent-facing capability: the skill directory name, who it's for, the
/// markdown seeded for each audience, and the config gate that decides
/// whether it is currently live.
pub struct Capability {
    /// Skill directory name, e.g. `"watch-pr"` → `.claude/skills/watch-pr/`.
    pub name: &'static str,
    pub audience: Audience,
    /// Body seeded into an orchestrator root (after [`render`]).
    pub orchestrator_md: Option<&'static str>,
    /// Body seeded verbatim into a worker's worktree.
    pub worker_md: Option<&'static str>,
    /// Gate against the live config; `|_| true` for always-on capabilities.
    pub enabled: fn(&AppConfig) -> bool,
    /// An optional hook this capability also installs, alongside its skill
    /// markdown.
    pub hook: Option<ToolHook>,
}

impl Capability {
    /// The markdown this capability offers to `aud`, preferring an exact
    /// audience match and falling back to whichever body exists (so a
    /// filter-less listing still has something to describe).
    pub fn md_for(&self, aud: Audience) -> Option<&'static str> {
        match aud {
            Audience::Orchestrator => self.orchestrator_md.or(self.worker_md),
            Audience::Worker => self.worker_md.or(self.orchestrator_md),
            Audience::Both => self.orchestrator_md.or(self.worker_md),
        }
    }
}

/// `PreToolUse` guard script for `pr-watch-enforcement`: denies the exact
/// polling shapes `watch-pr`'s own markdown tells every agent never to run
/// (`gh pr checks --watch`, `gh run watch`, or a loop/`sleep` wrapped around
/// `gh pr checks`/`gh pr view`/`gh pr status`) — a single one-off
/// `gh pr view` is left alone, matching that doc's own wording.
///
/// Two normalization passes run before any pattern match: backslash-newline
/// line continuations collapse to a space (so a `--watch` flag wrapped onto
/// its own continuation line is still seen on the same logical line), and
/// quoted string contents are stripped (so a commit message or `--body`
/// string that happens to mention "gh pr checks" isn't mistaken for an
/// actual invocation). The `gh ... pr checks` / `gh ... run watch` patterns
/// tolerate an arbitrary run of global flags between `gh` and the
/// subcommand (`gh --repo owner/repo pr checks --watch`), bounded by the
/// nearest command separator (`;`, `&&`, `||`, a pipe, or a newline) so it
/// can't bridge into an unrelated chained command.
const PR_WATCH_ENFORCEMENT_HOOK_SCRIPT: &str = r#"#!/usr/bin/env node
const { readFileSync } = require("node:fs");
let raw = "";
try { raw = readFileSync(0, "utf-8"); } catch { process.exit(0); }
let payload;
try { payload = JSON.parse(raw || "{}"); } catch { process.exit(0); }
if (payload.tool_name !== "Bash") process.exit(0);
const rawCmd = (payload.tool_input && payload.tool_input.command) || "";
const noContinuations = rawCmd.replace(/\\\r?\n/g, " ");
const code = noContinuations.replace(/'[^']*'|"(?:[^"\\]|\\.)*"/g, "");

const sameSeg = "(?:(?!;|&&|\\|\\||\\n).)*?";
const ghPrStatusCmd = new RegExp(`\\bgh\\b${sameSeg}\\bpr\\s+(?:checks|view|status)\\b`);
const ghRunWatch = new RegExp(`\\bgh\\b${sameSeg}\\brun\\s+watch\\b`);
const ghPrChecksWatchFlag = new RegExp(`\\bgh\\b${sameSeg}\\bpr\\s+checks\\b${sameSeg}--watch\\b`);

const denied =
  ghPrChecksWatchFlag.test(code) ||
  ghRunWatch.test(code) ||
  (/\b(?:while|until|for)\b/.test(code) && ghPrStatusCmd.test(code)) ||
  (/\bsleep\b/.test(code) && ghPrStatusCmd.test(code));
if (!denied) process.exit(0);
process.stdout.write(JSON.stringify({
  hookSpecificOutput: {
    hookEventName: "PreToolUse",
    permissionDecision: "deny",
    permissionDecisionReason:
      "Raw gh CI/PR polling (gh pr checks --watch, gh run watch, or a loop/sleep " +
      "around gh pr checks/gh pr view/gh pr status) is blocked — register a " +
      "watch instead: `ninox open --pr <url>` (`ninox close --pr <url>` when done).",
  },
}) + "\n");
process.exit(0);
"#;

const PR_WATCH_ENFORCEMENT_HOOK: ToolHook = ToolHook {
    event:       "PreToolUse",
    matcher:     "Bash",
    script_name: "pr-watch-enforcement.cjs",
    script:      PR_WATCH_ENFORCEMENT_HOOK_SCRIPT,
};

pub const REGISTRY: &[Capability] = &[
    Capability {
        name: "spawn-worker",
        audience: Audience::Orchestrator,
        orchestrator_md: Some(include_str!("../skills/orchestrator/spawn-worker.md")),
        worker_md: None,
        enabled: |_| true,
        hook: None,
    },
    Capability {
        name: "reap-workers",
        audience: Audience::Orchestrator,
        orchestrator_md: Some(include_str!("../skills/orchestrator/reap-workers.md")),
        worker_md: None,
        enabled: |_| true,
        hook: None,
    },
    Capability {
        name: "spawn-orchestrator",
        audience: Audience::Orchestrator,
        orchestrator_md: Some(include_str!("../skills/orchestrator/spawn-orchestrator.md")),
        worker_md: None,
        enabled: |_| true,
        hook: None,
    },
    Capability {
        name: "set-agent-config",
        audience: Audience::Orchestrator,
        orchestrator_md: Some(include_str!("../skills/orchestrator/set-agent-config.md")),
        worker_md: None,
        enabled: |_| true,
        hook: None,
    },
    Capability {
        name: "brain",
        audience: Audience::Both,
        orchestrator_md: Some(include_str!("../skills/orchestrator/brain.md")),
        worker_md: Some(include_str!("../skills/worker/brain.md")),
        enabled: |_| true,
        hook: None,
    },
    // `watch-pr` is two entries, not one `Both`: the worker copy is gated on
    // `[pr_watch].enabled` (a worker told to `ninox open --pr` while the
    // poller is off would be following advice that does nothing), while the
    // orchestrator copy is always seeded — matching the behavior this
    // registry replaced.
    Capability {
        name: "watch-pr",
        audience: Audience::Orchestrator,
        orchestrator_md: Some(include_str!("../skills/orchestrator/watch-pr.md")),
        worker_md: None,
        enabled: |_| true,
        hook: None,
    },
    Capability {
        name: "watch-pr",
        audience: Audience::Worker,
        orchestrator_md: None,
        worker_md: Some(include_str!("../skills/worker/watch-pr.md")),
        enabled: |cfg| cfg.pr_watch.enabled,
        hook: None,
    },
    Capability {
        name: "plan",
        audience: Audience::Orchestrator,
        orchestrator_md: Some(include_str!("../skills/orchestrator/plan.md")),
        worker_md: None,
        enabled: |_| true,
        hook: None,
    },
    Capability {
        name: "worker-status",
        audience: Audience::Both,
        orchestrator_md: Some(include_str!("../skills/orchestrator/worker-status.md")),
        worker_md: Some(include_str!("../skills/worker/worker-status.md")),
        enabled: |_| true,
        hook: None,
    },
    Capability {
        name: "read-worker-screen",
        audience: Audience::Orchestrator,
        orchestrator_md: Some(include_str!("../skills/orchestrator/read-worker-screen.md")),
        worker_md: None,
        enabled: |_| true,
        hook: None,
    },
    Capability {
        name: "fleet-recovery",
        audience: Audience::Both,
        orchestrator_md: Some(include_str!("../skills/orchestrator/fleet-recovery.md")),
        worker_md: Some(include_str!("../skills/worker/fleet-recovery.md")),
        enabled: |_| true,
        hook: None,
    },
    Capability {
        name: "restart-session",
        audience: Audience::Both,
        orchestrator_md: Some(include_str!("../skills/orchestrator/restart-session.md")),
        worker_md: Some(include_str!("../skills/worker/restart-session.md")),
        enabled: |_| true,
        hook: None,
    },
    // Both the skill (explains the hook) and the hook itself (enforces it)
    // are seeded for both audiences unconditionally — true parity, not
    // gated on `[pr_watch].enabled` like `watch-pr`'s worker copy, because
    // the ban on loop/`--watch` polling holds even when the watch mechanism
    // itself is off (the worker `watch-pr` skill's own fallback section
    // says as much: a disabled watch still means "single one-off calls,
    // never a watch loop").
    Capability {
        name: "pr-watch-enforcement",
        audience: Audience::Both,
        orchestrator_md: Some(include_str!("../skills/orchestrator/pr-watch-enforcement.md")),
        worker_md: Some(include_str!("../skills/worker/pr-watch-enforcement.md")),
        enabled: |_| true,
        hook: Some(PR_WATCH_ENFORCEMENT_HOOK),
    },
    // Machine management (`ninox machine add/list/remove`) is a fleet-level
    // concern an orchestrator drives, not something a worker needs to know
    // about mid-task — mirrors `plan`/`spawn-worker`'s orchestrator-only shape
    // rather than `watch-pr`'s two-entries-per-audience split.
    Capability {
        name: "remote-machines",
        audience: Audience::Orchestrator,
        orchestrator_md: Some(include_str!("../skills/orchestrator/remote-machines.md")),
        worker_md: None,
        enabled: |cfg| cfg.remote_machines.enabled,
        hook: None,
    },
];

/// Placeholder for the invoking `ninox` binary path in orchestrator markdown.
pub const NINOX_BIN_PLACEHOLDER: &str = "{{NINOX_BIN}}";
/// Placeholder for the active config file path in orchestrator markdown.
pub const CONFIG_PATH_PLACEHOLDER: &str = "{{CONFIG_PATH}}";

/// Substitute the seed-time placeholders documented on this module. Bodies
/// with no placeholders pass through unchanged.
pub fn render(md: &str, ninox_bin: &str, config_path: &str) -> String {
    md.replace(NINOX_BIN_PLACEHOLDER, ninox_bin)
        .replace(CONFIG_PATH_PLACEHOLDER, config_path)
}

/// The one-line `description:` from a skill's YAML frontmatter — what
/// AGENTS.md and `ninox capabilities` show next to the skill name.
///
/// Only the frontmatter block (between the leading `---` and its closing
/// `---`) is scanned, so a `description:` appearing later in the body can
/// never be mistaken for it. Returns `None` when the markdown has no
/// frontmatter or no `description:` key in it.
pub fn description(md: &str) -> Option<&str> {
    let body = md.strip_prefix("---\n")?;
    let end = body.find("\n---")?;
    body[..end]
        .lines()
        .find_map(|l| l.strip_prefix("description:"))
        .map(str::trim)
}

/// The registry entries whose audience includes `aud`, in declaration order.
pub fn for_audience(aud: Audience) -> impl Iterator<Item = &'static Capability> {
    REGISTRY.iter().filter(move |c| c.audience.includes(aud))
}

/// The hooks of every registry entry targeting `aud` that is currently
/// enabled against `config`, in declaration order — the same gate
/// `seed_orchestrator_skills`/`seed_worker_skills` apply to skill markdown.
pub fn hooks_for(aud: Audience, config: &AppConfig) -> Vec<&'static ToolHook> {
    for_audience(aud)
        .filter(|c| (c.enabled)(config))
        .filter_map(|c| c.hook.as_ref())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every registry entry must actually carry markdown for each audience
    /// it claims — an entry advertising itself to workers with no
    /// `worker_md` would silently seed nothing.
    #[test]
    fn every_entry_has_markdown_for_its_audience() {
        for cap in REGISTRY {
            match cap.audience {
                Audience::Worker => assert!(
                    cap.worker_md.is_some(),
                    "{} targets workers but has no worker_md",
                    cap.name
                ),
                Audience::Orchestrator => assert!(
                    cap.orchestrator_md.is_some(),
                    "{} targets orchestrators but has no orchestrator_md",
                    cap.name
                ),
                Audience::Both => {
                    assert!(cap.worker_md.is_some(), "{} (Both) has no worker_md", cap.name);
                    assert!(
                        cap.orchestrator_md.is_some(),
                        "{} (Both) has no orchestrator_md",
                        cap.name
                    );
                }
            }
        }
    }

    /// Every markdown body must be a real skill file: YAML frontmatter with
    /// a matching `name:` and a non-empty `description:` (the latter is what
    /// AGENTS.md and `ninox capabilities` render).
    #[test]
    fn every_markdown_has_frontmatter_with_a_parseable_description() {
        for cap in REGISTRY {
            for md in [cap.orchestrator_md, cap.worker_md].into_iter().flatten() {
                assert!(md.starts_with("---\n"), "{} md must start with frontmatter", cap.name);
                assert!(
                    md.contains(&format!("name: {}", cap.name)),
                    "{} md frontmatter must declare its own name",
                    cap.name
                );
                let desc = description(md)
                    .unwrap_or_else(|| panic!("{} md has no parseable description", cap.name));
                assert!(!desc.trim().is_empty(), "{} description must be non-empty", cap.name);
            }
        }
    }

    #[test]
    fn registry_covers_every_migrated_skill() {
        let orch: Vec<_> = for_audience(Audience::Orchestrator).map(|c| c.name).collect();
        assert_eq!(
            orch,
            vec![
                "spawn-worker",
                "reap-workers",
                "spawn-orchestrator",
                "set-agent-config",
                "brain",
                "watch-pr",
                "plan",
                "worker-status",
                "read-worker-screen",
                "fleet-recovery",
                "restart-session",
                "pr-watch-enforcement",
                "remote-machines",
            ]
        );
        let worker: Vec<_> = for_audience(Audience::Worker).map(|c| c.name).collect();
        assert_eq!(
            worker,
            vec!["brain", "watch-pr", "worker-status", "fleet-recovery", "restart-session", "pr-watch-enforcement"]
        );
    }

    #[test]
    fn for_audience_includes_both_entries() {
        // `brain` is Audience::Both — it must surface in either filter.
        assert!(for_audience(Audience::Worker).any(|c| c.name == "brain"));
        assert!(for_audience(Audience::Orchestrator).any(|c| c.name == "brain"));
    }

    #[test]
    fn description_parses_the_frontmatter_line_only() {
        let md = "---\nname: x\ndescription: Do a thing.\n---\n\n# Body\n\ndescription: not this\n";
        assert_eq!(description(md), Some("Do a thing."));
    }

    #[test]
    fn description_is_none_without_frontmatter() {
        assert_eq!(description("# Just a heading\n"), None);
    }

    /// The worker `watch-pr` entry is the only gated capability: it mirrors
    /// `[pr_watch].enabled`. The orchestrator entry stays ungated.
    #[test]
    fn worker_watch_pr_is_gated_on_pr_watch_but_orchestrator_is_not() {
        let mut cfg = AppConfig::default();
        cfg.pr_watch.enabled = false;

        let worker_watch = for_audience(Audience::Worker)
            .find(|c| c.name == "watch-pr")
            .expect("worker watch-pr entry");
        assert!(!(worker_watch.enabled)(&cfg));

        cfg.pr_watch.enabled = true;
        assert!((worker_watch.enabled)(&cfg));

        let orch_watch = for_audience(Audience::Orchestrator)
            .find(|c| c.name == "watch-pr")
            .expect("orchestrator watch-pr entry");
        cfg.pr_watch.enabled = false;
        assert!((orch_watch.enabled)(&cfg), "orchestrator watch-pr must stay ungated");
    }

    #[test]
    fn everything_else_is_always_enabled() {
        let cfg = AppConfig::default();
        for cap in REGISTRY.iter().filter(|c| c.name != "watch-pr" && c.name != "remote-machines") {
            assert!((cap.enabled)(&cfg), "{} must be always-on", cap.name);
        }
    }

    /// `remote-machines` is gated on `[remote_machines].enabled`, matching
    /// the opt-in shape of `[pr_watch].enabled`.
    #[test]
    fn remote_machines_capability_is_gated_on_its_own_config() {
        let mut cfg = AppConfig::default();
        cfg.remote_machines.enabled = false;
        let cap = for_audience(Audience::Orchestrator)
            .find(|c| c.name == "remote-machines")
            .expect("remote-machines entry");
        assert!(!(cap.enabled)(&cfg));
        cfg.remote_machines.enabled = true;
        assert!((cap.enabled)(&cfg));
    }

    #[test]
    fn render_substitutes_both_placeholders() {
        let out = render("run {{NINOX_BIN}} and edit {{CONFIG_PATH}}", "/bin/ninox", "/cfg.toml");
        assert_eq!(out, "run /bin/ninox and edit /cfg.toml");
    }

    /// No placeholder may survive into a seeded orchestrator skill.
    #[test]
    fn rendering_every_orchestrator_md_leaves_no_placeholders() {
        for cap in for_audience(Audience::Orchestrator) {
            let md = cap.orchestrator_md.expect("checked above");
            let out = render(md, "/bin/ninox", "/cfg.toml");
            assert!(!out.contains("{{"), "{} leaves an unsubstituted placeholder", cap.name);
        }
    }

    /// Worker markdown is seeded verbatim (no render pass), so it must not
    /// contain placeholders at all.
    #[test]
    fn worker_md_contains_no_placeholders() {
        for cap in for_audience(Audience::Worker) {
            let md = cap.worker_md.expect("checked above");
            assert!(!md.contains("{{"), "{} worker md must not need rendering", cap.name);
        }
    }

    /// `hooks_for` must apply the same `enabled` gate as `for_audience` does
    /// for skill markdown — a hook-bearing capability that's gated off must
    /// not leak its hook through.
    #[test]
    fn hooks_for_skips_a_gated_off_hook_capability() {
        let gated_off = Capability {
            name: "gated-off-hook",
            audience: Audience::Both,
            orchestrator_md: Some("---\nname: gated-off-hook\ndescription: Off.\n---\n\nbody\n"),
            worker_md: Some("---\nname: gated-off-hook\ndescription: Off.\n---\n\nbody\n"),
            enabled: |_| false,
            hook: Some(ToolHook {
                event: "PreToolUse", matcher: "Bash",
                script_name: "gated-off-hook.cjs", script: "#!/usr/bin/env node\n",
            }),
        };
        let cfg = AppConfig::default();
        let caps = [&gated_off];
        let hooks: Vec<_> = caps
            .into_iter()
            .filter(|c| c.audience.includes(Audience::Worker))
            .filter(|c| (c.enabled)(&cfg))
            .filter_map(|c| c.hook.as_ref())
            .collect();
        assert!(hooks.is_empty(), "a disabled capability's hook must not surface");
    }

    /// The whole point of a shared `ToolHook` const: the orchestrator and
    /// worker copies of `pr-watch-enforcement` must be the exact same bytes,
    /// not two hand-synced strings.
    #[test]
    fn pr_watch_enforcement_hook_is_byte_identical_for_both_audiences() {
        let cfg = AppConfig::default();
        let orch = hooks_for(Audience::Orchestrator, &cfg);
        let worker = hooks_for(Audience::Worker, &cfg);
        assert_eq!(orch.len(), 1, "exactly one hook-bearing capability is registered for orchestrators");
        assert_eq!(worker.len(), 1, "exactly one hook-bearing capability is registered for workers");
        assert_eq!(orch[0].script, worker[0].script);
        assert_eq!(orch[0].matcher, worker[0].matcher);
        assert_eq!(orch[0].event, worker[0].event);
        assert_eq!(orch[0].script_name, worker[0].script_name);
    }

    /// Exercises the actual guard script (not just its Rust wiring) against
    /// the polling shapes `watch-pr`'s own markdown calls out by name, plus
    /// the one-off calls that must stay allowed. Skips gracefully if `node`
    /// isn't on PATH rather than failing CI environments that lack it.
    #[test]
    fn pr_watch_enforcement_script_denies_polling_and_allows_one_offs() {
        if std::process::Command::new("node").arg("--version").output().is_err() {
            eprintln!("skipping: node not found on PATH");
            return;
        }

        let run = |command: &str| -> bool {
            use std::io::Write;
            let script = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(script.path(), PR_WATCH_ENFORCEMENT_HOOK_SCRIPT).unwrap();
            let mut child = std::process::Command::new("node")
                .arg(script.path())
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let payload = serde_json::json!({"tool_name": "Bash", "tool_input": {"command": command}});
            child.stdin.take().unwrap().write_all(payload.to_string().as_bytes()).unwrap();
            let out = child.wait_with_output().unwrap();
            !String::from_utf8_lossy(&out.stdout).trim().is_empty()
        };

        for cmd in [
            "gh pr checks 42 --watch",
            "gh run watch 123456",
            "while true; do gh pr checks 42; sleep 5; done",
            "gh pr checks 42; sleep 10",
            "for i in 1 2 3; do gh pr view 42; done",
            // A `--watch` flag wrapped onto a backslash-continuation line.
            "gh pr checks 42 \\\n    --watch",
            // Global flags between `gh` and the subcommand must not evade detection.
            "gh --repo owner/repo pr checks 42 --watch",
            "gh --repo owner/repo run watch 123",
        ] {
            assert!(run(cmd), "must deny: {cmd}");
        }

        for cmd in [
            "gh pr view 42",
            "gh pr checks 42",
            "git status",
            "gh pr list",
            // The literal substring "gh pr checks" inside a quoted argument
            // (e.g. a commit message) must not be mistaken for an invocation.
            "git commit -m \"fix for gh pr checks\"",
        ] {
            assert!(!run(cmd), "must allow: {cmd}");
        }
    }
}
