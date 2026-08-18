//! Capability registry — the single source of truth for the agent-facing
//! skills Ninox seeds and advertises.
//!
//! Every capability an agent can use is one [`Capability`] entry in
//! [`REGISTRY`], and every surface that needs to know about capabilities
//! loops that slice rather than restating the list:
//!
//! - `ninox_app::app::setup_orchestrator_root` seeds each *enabled*
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

pub const REGISTRY: &[Capability] = &[
    Capability {
        name: "spawn-worker",
        audience: Audience::Orchestrator,
        orchestrator_md: Some(include_str!("../skills/orchestrator/spawn-worker.md")),
        worker_md: None,
        enabled: |_| true,
    },
    Capability {
        name: "set-agent-config",
        audience: Audience::Orchestrator,
        orchestrator_md: Some(include_str!("../skills/orchestrator/set-agent-config.md")),
        worker_md: None,
        enabled: |_| true,
    },
    Capability {
        name: "brain",
        audience: Audience::Both,
        orchestrator_md: Some(include_str!("../skills/orchestrator/brain.md")),
        worker_md: Some(include_str!("../skills/worker/brain.md")),
        enabled: |_| true,
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
    },
    Capability {
        name: "watch-pr",
        audience: Audience::Worker,
        orchestrator_md: None,
        worker_md: Some(include_str!("../skills/worker/watch-pr.md")),
        enabled: |cfg| cfg.pr_watch.enabled,
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
        assert_eq!(orch, vec!["spawn-worker", "set-agent-config", "brain", "watch-pr"]);
        let worker: Vec<_> = for_audience(Audience::Worker).map(|c| c.name).collect();
        assert_eq!(worker, vec!["brain", "watch-pr"]);
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
        for cap in REGISTRY.iter().filter(|c| c.name != "watch-pr") {
            assert!((cap.enabled)(&cfg), "{} must be always-on", cap.name);
        }
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
}
