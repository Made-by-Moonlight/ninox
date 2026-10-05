use crate::config::AppConfig;

/// Writes one `<skills_dir>/<name>/SKILL.md` per entry in `caps` whose
/// [`Capability::enabled`] gate is satisfied by `config`, with the seed-time
/// placeholders substituted, and returns the entries actually seeded (in the
/// order given) so callers can advertise exactly what landed on disk.
///
/// Split out of [`setup_orchestrator_root`] so the gating loop can be tested
/// against a caller-supplied capability slice — the real `REGISTRY` is a
/// `const`, so a deliberately-disabled entry can't be injected into it.
///
/// Files are always overwritten: an upgraded ninox re-seeds the current
/// wording over whatever the previous version wrote. Note that a
/// now-disabled capability's *stale* file is left in place rather than
/// deleted — removing files from a directory the user may have added their
/// own skills to is not this function's call to make.
async fn seed_orchestrator_skills<'a>(
    skills_dir: &std::path::Path,
    config: &AppConfig,
    caps: &[&'a crate::capabilities::Capability],
    ninox_bin: &str,
    config_path: &str,
) -> anyhow::Result<Vec<&'a crate::capabilities::Capability>> {
    use crate::capabilities;
    use tokio::fs;

    let mut seeded = Vec::new();
    for cap in caps {
        let Some(md) = cap.orchestrator_md else { continue };
        if !(cap.enabled)(config) {
            continue;
        }
        let dir = skills_dir.join(cap.name);
        fs::create_dir_all(&dir).await?;
        fs::write(dir.join("SKILL.md"), capabilities::render(md, ninox_bin, config_path)).await?;
        seeded.push(*cap);
    }
    Ok(seeded)
}

/// Seeds `~/ninox/orchestrators/` (or the configured root) with the
/// files that orchestrator sessions need: AGENTS.md (canonical, CLAUDE.md
/// symlinks to it), one SKILL.md per *enabled* orchestrator-facing capability
/// in `ninox_core::capabilities::REGISTRY`, and the subagent-blocker
/// PreToolUse hook.
///
/// Both the seeded skills and AGENTS.md's "Available Skills" list are driven
/// by that registry, so adding a capability needs no edit here — and both are
/// filtered by each capability's `enabled` gate against `config`, evaluated
/// once per call (every orchestrator entry is currently ungated, so this
/// changes nothing today). Skill bodies go through `capabilities::render` to
/// substitute the seed-time placeholders (`{{NINOX_BIN}}`, `{{CONFIG_PATH}}`)
/// — see the `ninox_core::capabilities` module docs.
///
/// AGENTS.md and settings.json are skipped if already present (user-editable).
/// Skill files and the blocker are always overwritten to stay in sync.
pub async fn setup_orchestrator_root(
    root: &std::path::Path,
    config: &AppConfig,
    ninox_bin: &str,
    config_path: &str,
) -> anyhow::Result<()> {
    use crate::capabilities::{self, Audience};
    use tokio::fs;

    let claude_dir        = root.join(".claude");
    let claude_skills_dir = claude_dir.join("skills");
    fs::create_dir_all(&claude_dir).await?;

    let skill_path = |name: &str| claude_skills_dir.join(name).join("SKILL.md");

    let caps: Vec<_> = capabilities::for_audience(Audience::Orchestrator).collect();
    let seeded =
        seed_orchestrator_skills(&claude_skills_dir, config, &caps, ninox_bin, config_path).await?;

    // AGENTS.md is canonical; CLAUDE.md symlinks to it.
    let agents_md_path = root.join("AGENTS.md");
    if !agents_md_path.exists() {
        let mut skills = String::new();
        for cap in &seeded {
            let Some(md) = cap.orchestrator_md else { continue };
            skills.push_str(&format!(
                "- `{}` — {}\n",
                skill_path(cap.name).display(),
                capabilities::description(md).unwrap_or(""),
            ));
        }
        // The preamble points at whichever entry owns the spawn-worker
        // capability rather than a hand-typed directory name, so a rename in
        // the registry can't silently leave a dangling path here.
        let spawn_skill = seeded
            .iter()
            .find(|c| c.name == "spawn-worker")
            .expect("registry must declare an orchestrator spawn-worker capability");
        let body = format!(
            "# Ninox Orchestrator\n\n\
             Before doing anything else, read and follow: `{spawn_skill}`\n\n\
             ## Available Skills\n\n\
             {skills}\n\
             Run `{ninox_bin} capabilities --orchestrator` to list what ninox can currently do.\n",
            spawn_skill = skill_path(spawn_skill.name).display(),
            skills      = skills,
            ninox_bin   = ninox_bin,
        );
        fs::write(&agents_md_path, body).await?;
    }
    let claude_md_path = root.join("CLAUDE.md");
    if !claude_md_path.exists() {
        #[cfg(unix)]
        tokio::fs::symlink("AGENTS.md", &claude_md_path).await?;
        #[cfg(not(unix))]
        {
            let body = fs::read_to_string(&agents_md_path).await?;
            fs::write(&claude_md_path, body).await?;
        }
    }

    // subagent-blocker hook — always overwritten.
    let blocker = r#"#!/usr/bin/env node
const { readFileSync } = require("node:fs");
const callerType = process.env.NINOX_CALLER_TYPE || "";
if (callerType !== "orchestrator") process.exit(0);
let raw = "";
try { raw = readFileSync(0, "utf-8"); } catch { process.exit(0); }
let payload;
try { payload = JSON.parse(raw || "{}"); } catch { process.exit(0); }
const toolName = typeof payload.tool_name === "string" ? payload.tool_name : "";
if (toolName !== "Task" && toolName !== "Agent") process.exit(0);
const sub = (payload.tool_input?.subagent_type || "").toLowerCase();
if (sub === "explore" || sub === "plan") process.exit(0);
process.stdout.write(JSON.stringify({
  hookSpecificOutput: {
    hookEventName: "PreToolUse",
    permissionDecision: "deny",
    permissionDecisionReason: "Use `${NINOX_BIN:-ninox} spawn` instead of native subagents.",
  },
}) + "\n");
process.exit(0);
"#;
    fs::write(claude_dir.join("subagent-blocker.cjs"), blocker).await?;

    let settings_path = claude_dir.join("settings.json");
    if !settings_path.exists() {
        let settings = serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Task|Agent",
                    "hooks": [{"type": "command", "command": "node .claude/subagent-blocker.cjs", "timeout": 2000}]
                }]
            },
            "statusLine": {
                "type": "command",
                "command": format!("'{}' statusline", ninox_bin.replace('\'', "'\\''")),
                "refreshInterval": 20
            }
        });
        fs::write(&settings_path, serde_json::to_string_pretty(&settings)?).await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn spawn_skill_teaches_work_request_handling() {
        let root = tempdir().unwrap().keep();
        setup_orchestrator_root(&root, &AppConfig::default(), "ninox", "/cfg.toml").await.unwrap();

        let skill = std::fs::read_to_string(
            root.join(".claude").join("skills").join("spawn-worker").join("SKILL.md"),
        ).unwrap();
        assert!(skill.starts_with("---\n"), "skill must start with YAML frontmatter");
        assert!(skill.contains("name: spawn-worker"));
        assert!(skill.contains("description:"));
        assert!(
            skill.contains("request-work"),
            "skill must explain the worker→orchestrator work-request channel"
        );
        assert!(
            skill.contains("spawn a new worker") || skill.contains("spawn a dedicated worker"),
            "skill must tell the orchestrator to spawn a worker for requested work"
        );
        assert!(
            skill.to_lowercase().contains("never") && skill.to_lowercase().contains("widen"),
            "skill must forbid widening an existing worker's scope"
        );
    }

    #[tokio::test]
    async fn setup_orchestrator_root_seeds_watch_pr_skill() {
        let root = tempdir().unwrap().keep();
        setup_orchestrator_root(&root, &AppConfig::default(), "ninox", "/cfg.toml").await.unwrap();

        let skill_path = root.join(".claude").join("skills").join("watch-pr").join("SKILL.md");
        let skill = std::fs::read_to_string(&skill_path).unwrap();
        assert!(skill.starts_with("---\n"), "skill must start with YAML frontmatter");
        assert!(skill.contains("name: watch-pr"));
        assert!(skill.contains("description:"));
        assert!(skill.contains("ninox open --pr"));
        assert!(skill.contains("ninox close --pr"));
        assert!(skill.contains("ninox list --prs"));

        let spawn_skill = std::fs::read_to_string(
            root.join(".claude").join("skills").join("spawn-worker").join("SKILL.md"),
        ).unwrap();
        assert!(
            spawn_skill.contains("see the `watch-pr` skill"),
            "spawn-worker skill must cross-link the watch-pr skill"
        );

        let agents_md = std::fs::read_to_string(root.join("AGENTS.md")).unwrap();
        assert!(
            agents_md.contains(&skill_path.display().to_string()),
            "AGENTS.md should list the watch-pr skill in Available Skills"
        );
    }

    #[tokio::test]
    async fn setup_orchestrator_root_seeds_reap_skill() {
        let root = tempdir().unwrap().keep();
        setup_orchestrator_root(&root, &AppConfig::default(), "ninox", "/cfg.toml").await.unwrap();

        let skill_path = root.join(".claude").join("skills").join("reap-workers").join("SKILL.md");
        let skill = std::fs::read_to_string(&skill_path).unwrap();
        assert!(skill.starts_with("---\n"), "skill must start with YAML frontmatter");
        assert!(skill.contains("name: reap-workers"));
        assert!(skill.contains("description:"));
        assert!(skill.contains("ninox reap"));
        assert!(
            skill.contains("--force"),
            "skill must document the flag that reaping a live worker needs"
        );
        assert!(
            skill.to_lowercase().contains("not while a worker is still working"),
            "skill must warn against force-reaping live work"
        );

        let agents_md = std::fs::read_to_string(root.join("AGENTS.md")).unwrap();
        assert!(
            agents_md.contains(&skill_path.display().to_string()),
            "AGENTS.md should point orchestrators at the reap skill"
        );
    }

    /// The spawn-worker skill is rewritten on every startup, so it reaches
    /// orchestrator roots whose (user-editable, never-overwritten) AGENTS.md
    /// predates reaping and will never list the new skill.
    #[tokio::test]
    async fn spawn_skill_points_at_reaping() {
        let root = tempdir().unwrap().keep();
        setup_orchestrator_root(&root, &AppConfig::default(), "ninox", "/cfg.toml").await.unwrap();

        let skill = std::fs::read_to_string(
            root.join(".claude").join("skills").join("spawn-worker").join("SKILL.md"),
        ).unwrap();
        assert!(skill.contains("ninox reap"));
    }

    #[tokio::test]
    async fn setup_orchestrator_root_seeds_spawn_orchestrator_skill() {
        let root = tempdir().unwrap().keep();
        setup_orchestrator_root(&root, &AppConfig::default(), "ninox", "/cfg.toml").await.unwrap();

        let skill_path = root.join(".claude").join("skills").join("spawn-orchestrator").join("SKILL.md");
        let skill = std::fs::read_to_string(&skill_path).unwrap();
        assert!(skill.starts_with("---\n"), "skill must start with YAML frontmatter");
        assert!(skill.contains("name: spawn-orchestrator"));
        assert!(skill.contains("ninox spawn-orchestrator"));
        assert!(
            skill.contains("--user-requested"),
            "skill must name the flag the command requires"
        );
        // The whole point of this skill is the restraint, not the mechanics.
        let lower = skill.to_lowercase();
        assert!(
            lower.contains("only when the user explicitly asks"),
            "skill must state the by-request-only rule"
        );
        assert!(
            skill.contains("--description") || skill.contains("description:"),
            "skill must carry a description for discovery"
        );

        let agents_md = std::fs::read_to_string(root.join("AGENTS.md")).unwrap();
        assert!(agents_md.contains(&skill_path.display().to_string()));
    }

    #[tokio::test]
    async fn set_agent_config_skill_has_frontmatter() {
        let root = tempdir().unwrap().keep();
        setup_orchestrator_root(&root, &AppConfig::default(), "ninox", "/cfg.toml").await.unwrap();

        let skill = std::fs::read_to_string(
            root.join(".claude").join("skills").join("set-agent-config").join("SKILL.md"),
        ).unwrap();
        assert!(skill.starts_with("---\n"), "skill must start with YAML frontmatter");
        assert!(skill.contains("name: set-agent-config"));
        assert!(skill.contains("description:"));
    }

    #[tokio::test]
    async fn setup_orchestrator_root_seeds_brain_skill() {
        let root = tempdir().unwrap().keep();
        setup_orchestrator_root(&root, &AppConfig::default(), "ninox", "/cfg.toml").await.unwrap();

        let skill_path = root.join(".claude").join("skills").join("brain").join("SKILL.md");
        let skill = std::fs::read_to_string(&skill_path).unwrap();
        assert!(skill.starts_with("---\n"), "skill must start with YAML frontmatter");
        assert!(skill.contains("name: brain"));
        assert!(skill.contains("description:"));
        assert!(skill.contains("ninox brain query"));
        assert!(skill.contains("ninox brain index"));
        assert!(skill.contains("ninox brain show"));
        assert!(skill.contains("blends keyword and semantic matches"));

        let agents_md = std::fs::read_to_string(root.join("AGENTS.md")).unwrap();
        assert!(
            agents_md.contains(&skill_path.display().to_string()),
            "AGENTS.md should point orchestrators at the brain skill"
        );
    }

    /// Seeding is registry-driven: every orchestrator-facing capability gets
    /// a SKILL.md whose body is that entry's markdown with the seed-time
    /// placeholders substituted, and AGENTS.md lists all of them.
    #[tokio::test]
    async fn setup_orchestrator_root_seeds_every_registry_orchestrator_skill() {
        use crate::capabilities::{self, Audience};
        let root = tempdir().unwrap().keep();
        setup_orchestrator_root(&root, &AppConfig::default(), "/path/to/ninox", "/cfg.toml").await.unwrap();

        let agents_md = std::fs::read_to_string(root.join("AGENTS.md")).unwrap();
        for cap in capabilities::for_audience(Audience::Orchestrator) {
            let md = cap.orchestrator_md.expect("orchestrator entry has markdown");
            let path = root.join(".claude").join("skills").join(cap.name).join("SKILL.md");
            let body = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{} not seeded: {e}", cap.name));
            assert_eq!(
                body,
                capabilities::render(md, "/path/to/ninox", "/cfg.toml"),
                "{} must be seeded as the rendered registry markdown",
                cap.name,
            );
            assert!(!body.contains("{{"), "{} left an unsubstituted placeholder", cap.name);
            assert!(
                agents_md.contains(&path.display().to_string()),
                "AGENTS.md must list {} under Available Skills",
                cap.name,
            );
            assert!(
                agents_md.contains(capabilities::description(md).unwrap()),
                "AGENTS.md must describe {} from its frontmatter",
                cap.name,
            );
        }
        // Audience-scoped: an unfiltered listing would show the orchestrator
        // the worker-only variants of skills it shares (e.g. watch-pr).
        assert!(
            agents_md.contains("/path/to/ninox capabilities --orchestrator"),
            "AGENTS.md must point at the orchestrator-scoped capabilities command"
        );
    }

    /// The `enabled` gate is honored on the orchestrator side too, not just
    /// the worker side. `REGISTRY` is a `const` whose entries are all
    /// ungated today, so this drives the extracted seeding loop with a
    /// locally-built capability slice instead.
    #[tokio::test]
    async fn seed_orchestrator_skills_skips_gated_off_capabilities() {
        use crate::capabilities::{Audience, Capability};

        let on = Capability {
            name: "always-on",
            audience: Audience::Orchestrator,
            orchestrator_md: Some("---\nname: always-on\ndescription: On.\n---\n\nbody\n"),
            worker_md: None,
            enabled: |_| true,
        };
        let off = Capability {
            name: "gated-off",
            audience: Audience::Orchestrator,
            orchestrator_md: Some("---\nname: gated-off\ndescription: Off.\n---\n\nbody\n"),
            worker_md: None,
            enabled: |_| false,
        };

        let dir = tempdir().unwrap().keep();
        let seeded = seed_orchestrator_skills(
            &dir, &AppConfig::default(), &[&on, &off], "ninox", "/cfg.toml",
        )
        .await
        .unwrap();

        assert!(dir.join("always-on").join("SKILL.md").exists(), "enabled entry must be seeded");
        assert!(
            !dir.join("gated-off").join("SKILL.md").exists(),
            "a capability whose gate is off must not be seeded"
        );
        // The returned set is what AGENTS.md advertises — a disabled
        // capability must not be listed there either.
        assert_eq!(seeded.iter().map(|c| c.name).collect::<Vec<_>>(), vec!["always-on"]);
    }

    /// The skills are always re-seeded, so a stale copy from an older ninox
    /// is replaced rather than left in place (AGENTS.md, by contrast, stays
    /// user-editable and is only written when absent).
    #[tokio::test]
    async fn setup_orchestrator_root_overwrites_stale_skills_but_not_agents_md() {
        let root = tempdir().unwrap().keep();
        setup_orchestrator_root(&root, &AppConfig::default(), "ninox", "/cfg.toml").await.unwrap();

        let skill = root.join(".claude").join("skills").join("brain").join("SKILL.md");
        std::fs::write(&skill, "stale\n").unwrap();
        std::fs::write(root.join("AGENTS.md"), "hand-edited\n").unwrap();

        setup_orchestrator_root(&root, &AppConfig::default(), "ninox", "/cfg.toml").await.unwrap();

        assert_ne!(std::fs::read_to_string(&skill).unwrap(), "stale\n");
        assert_eq!(std::fs::read_to_string(root.join("AGENTS.md")).unwrap(), "hand-edited\n");
    }

    #[tokio::test]
    async fn setup_orchestrator_root_configures_statusline() {
        let root = tempdir().unwrap().keep();
        setup_orchestrator_root(&root, &AppConfig::default(), "/path/to/ninox", "/cfg.toml").await.unwrap();

        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root.join(".claude").join("settings.json")).unwrap(),
        ).unwrap();
        assert_eq!(settings["statusLine"]["type"], "command");
        assert_eq!(settings["statusLine"]["command"], "'/path/to/ninox' statusline");
        assert_eq!(settings["statusLine"]["refreshInterval"], 20);
        // The existing subagent-blocker hook must still be present.
        assert!(settings["hooks"]["PreToolUse"].is_array());
    }

    #[tokio::test]
    async fn setup_orchestrator_root_never_overwrites_existing_settings_json() {
        let root = tempdir().unwrap().keep();
        let claude_dir = root.join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(claude_dir.join("settings.json"), r#"{"userCustom": true}"#).unwrap();

        setup_orchestrator_root(&root, &AppConfig::default(), "ninox", "/cfg.toml").await.unwrap();

        let contents = std::fs::read_to_string(claude_dir.join("settings.json")).unwrap();
        assert_eq!(contents, r#"{"userCustom": true}"#, "pre-existing settings.json must be left byte-for-byte alone");
    }
}
