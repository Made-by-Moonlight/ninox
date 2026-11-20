//! Ninox's entries in a workspace's Claude Code project settings.
//!
//! Ninox's statusLine and hooks carry an absolute path to the ninox binary
//! that spawned the session, so they are machine-specific and belong in
//! `.claude/settings.local.json` — Claude Code's per-user, gitignored
//! project settings file — not the shareable `.claude/settings.json`.
//!
//! Both seed writers (`ninox_app::spawn_util::ensure_statusline_settings`
//! for worker worktrees, `orchestrator_root::setup_orchestrator_root`) go
//! through [`install_local`], which merges ninox's entries into whatever the
//! user already has there, and [`strip_legacy`], which removes the entries
//! older ninox builds wrote into `settings.json`. Ninox's entries are always
//! recognised by their command text ([`is_ninox_command`]) — ninox never
//! assumes it owns a whole settings file.

use std::path::Path;

use anyhow::Context as _;
use serde_json::{Map, Value};

/// Where ninox seeds its entries (relative to the workspace's `.claude/`).
pub const LOCAL_SETTINGS: &str = "settings.local.json";
/// Where ninox used to seed them; only ever cleaned up now.
pub const LEGACY_SETTINGS: &str = "settings.json";

/// Ninox subcommands its seeded statusLine/hooks invoke, as the trailing
/// text of a `'<ninox_bin>' <subcommand>` command.
const NINOX_SUBCOMMANDS: &[&str] = &[
    "statusline",
    "worker-status hook-stop",
    "worker-status hook-prompt",
    "worker-status hook-needs-input",
    "worker-status hook-permission",
    "inbox drain-stop",
    "inbox drain-prompt",
    "identity-hook prompt",
    "identity-hook session-start",
    "session-setup",
];

/// The orchestrator root's native-subagent guard script under `.claude/`.
pub const SUBAGENT_BLOCKER_SCRIPT: &str = "subagent-blocker.cjs";

/// Whether `cmd` is a statusLine/hook command ninox seeded: either
/// `<bin> <known subcommand>` where `<bin>` (optionally single-quoted) is a
/// ninox binary, or `node .claude/<script>` for a guard script ninox writes
/// (the subagent blocker or any capability hook in the registry).
pub fn is_ninox_command(cmd: &str) -> bool {
    ninox_command_key(cmd).is_some()
}

/// What a ninox command invokes — its subcommand or guard script name —
/// independent of which binary path it carries. `None` for non-ninox
/// commands.
fn ninox_command_key(cmd: &str) -> Option<&str> {
    let cmd = cmd.trim();
    if let Some(script) = cmd.strip_prefix("node ") {
        // `.claude/<script>` in ninox's own root, or `'<root>/.claude/<script>'`
        // from a custom workspace that points back at the root.
        let script = script.trim();
        let script = script.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')).unwrap_or(script);
        let script = script.strip_prefix(".claude/").or_else(|| script.rsplit_once("/.claude/").map(|(_, s)| s))?;
        let known = script == SUBAGENT_BLOCKER_SCRIPT
            || crate::capabilities::REGISTRY
                .iter()
                .filter_map(|c| c.hook.as_ref())
                .any(|h| h.script_name == script);
        return known.then_some(script);
    }
    NINOX_SUBCOMMANDS.iter().copied().find(|sub| {
        cmd.strip_suffix(sub)
            .and_then(|bin| bin.strip_suffix(' '))
            .is_some_and(is_ninox_bin)
    })
}

/// `bin` names a ninox executable: its file name is `nx` or starts with `ninox` (the
/// release binary, a `target/<profile>/ninox` build, or a cargo test
/// harness binary such as `ninox_app-<hash>`).
fn is_ninox_bin(bin: &str) -> bool {
    let bin = bin.trim();
    let bin = bin.strip_prefix('\'').and_then(|b| b.strip_suffix('\'')).unwrap_or(bin);
    Path::new(bin)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("ninox") || n == "nx")
}

fn command_of(v: &Value) -> Option<&str> {
    v.get("command").and_then(Value::as_str)
}

/// Remove ninox's own entries from a parsed settings object, leaving every
/// other key and hook untouched: a ninox `statusLine`, and every ninox hook
/// command inside `hooks.<event>[].hooks[]`. Hook groups, events, and the
/// `hooks` table emptied by the removal are dropped. Returns whether
/// anything was removed.
pub fn strip_ninox_entries(settings: &mut Map<String, Value>) -> bool {
    let mut changed = false;
    if settings.get("statusLine").and_then(command_of).is_some_and(is_ninox_command) {
        settings.shift_remove("statusLine");
        changed = true;
    }
    let Some(Value::Object(hooks)) = settings.get_mut("hooks") else {
        return changed;
    };
    for groups in hooks.values_mut() {
        let Value::Array(groups) = groups else { continue };
        groups.retain_mut(|group| {
            let Some(Value::Array(cmds)) = group.get_mut("hooks") else { return true };
            let before = cmds.len();
            cmds.retain(|h| !command_of(h).is_some_and(is_ninox_command));
            // Only drop groups this pass emptied: a user's own (odd but
            // valid) empty group is theirs to keep.
            if cmds.len() == before {
                return true;
            }
            changed = true;
            !cmds.is_empty()
        });
    }
    if changed {
        hooks.retain(|_, groups| !groups.as_array().is_some_and(Vec::is_empty));
        if hooks.is_empty() {
            settings.shift_remove("hooks");
        }
    }
    changed
}

fn read_object(path: &Path) -> anyhow::Result<Option<Map<String, Value>>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    if raw.trim().is_empty() {
        return Ok(Some(Map::new()));
    }
    match serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))? {
        Value::Object(map) => Ok(Some(map)),
        _ => anyhow::bail!("{} is not a JSON object", path.display()),
    }
}

/// Merge ninox's `entries` (a settings object holding at most `statusLine`
/// `hooks` and `crossSessionInbound`) into `<claude_dir>/settings.local.json`, creating it if
/// absent. Any ninox entries already there (e.g. from a previous spawn, with
/// a different binary path) are replaced; the user's own keys and hooks are
/// kept, and a user-defined `statusLine` or `crossSessionInbound` wins over ninox's. A file that
/// can't be parsed as a JSON object is left alone and reported as an error
/// rather than clobbered.
pub fn install_local(claude_dir: &Path, entries: &Value) -> anyhow::Result<()> {
    let path = claude_dir.join(LOCAL_SETTINGS);
    let original = read_object(&path)?;
    let mut settings = original.clone().unwrap_or_default();
    strip_ninox_entries(&mut settings);

    if let Some(accept) = entries.get(crate::harness::CROSS_SESSION_KEY) {
        settings.entry(crate::harness::CROSS_SESSION_KEY).or_insert_with(|| accept.clone());
    }
    if let Some(status_line) = entries.get("statusLine") {
        settings.entry("statusLine").or_insert_with(|| status_line.clone());
    }
    if let Some(Value::Object(ninox_hooks)) = entries.get("hooks") {
        let hooks = settings.entry("hooks").or_insert_with(|| Value::Object(Map::new()));
        let Value::Object(hooks) = hooks else {
            anyhow::bail!("{}: `hooks` is not an object", path.display());
        };
        for (event, groups) in ninox_hooks {
            let Value::Array(groups) = groups else { continue };
            let existing = hooks.entry(event.clone()).or_insert_with(|| Value::Array(Vec::new()));
            let Value::Array(existing) = existing else {
                anyhow::bail!("{}: `hooks.{event}` is not an array", path.display());
            };
            existing.extend(groups.iter().cloned());
        }
    }

    // Unchanged: leave the file (and the user's formatting) alone, so a
    // restart never races a write Claude Code made meanwhile.
    if original.as_ref() == Some(&settings) {
        return Ok(());
    }
    std::fs::create_dir_all(claude_dir).with_context(|| format!("create {}", claude_dir.display()))?;
    let tmp = claude_dir.join(format!(".{LOCAL_SETTINGS}.ninox-tmp"));
    std::fs::write(&tmp, serde_json::to_string_pretty(&settings)?).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("write {}", path.display()))
}

/// Remove the entries older ninox builds seeded into
/// `<claude_dir>/settings.json`, leaving the user's own entries in place
/// (key order preserved). The file is only rewritten when something was
/// actually removed, and deleted if nothing but ninox's entries was in it.
/// Idempotent. Returns whether the file changed.
pub fn strip_legacy(claude_dir: &Path) -> anyhow::Result<bool> {
    let path = claude_dir.join(LEGACY_SETTINGS);
    let Some(mut settings) = read_object(&path)? else {
        return Ok(false);
    };
    if !strip_ninox_entries(&mut settings) {
        return Ok(false);
    }
    if settings.is_empty() {
        std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    } else {
        std::fs::write(&path, serde_json::to_string_pretty(&settings)?)
            .with_context(|| format!("write {}", path.display()))?;
    }
    Ok(true)
}

/// Whether `<claude_dir>/settings.json` or `settings.local.json` defines a
/// `statusLine` that isn't ninox's. Claude Code lets the local file override
/// the shared one, so ninox seeding its own would silently replace the user's.
pub fn user_status_line(claude_dir: &Path) -> bool {
    [LEGACY_SETTINGS, LOCAL_SETTINGS].iter().any(|name| {
        let Ok(Some(settings)) = read_object(&claude_dir.join(name)) else { return false };
        settings.get("statusLine").is_some_and(|sl| !command_of(sl).is_some_and(is_ninox_command))
    })
}

/// Remove ninox's entries from `<claude_dir>/settings.local.json`, keeping
/// every other key. Rewrites the file only when something was removed (an
/// emptied file stays, as `{}`: it may be the user's). Returns whether it
/// changed. An unparsable file is left alone and reported.
pub fn strip_local(claude_dir: &Path) -> anyhow::Result<bool> {
    let path = claude_dir.join(LOCAL_SETTINGS);
    let Some(mut settings) = read_object(&path)? else { return Ok(false) };
    if !strip_ninox_entries(&mut settings) {
        return Ok(false);
    }
    std::fs::write(&path, serde_json::to_string_pretty(&settings)?).with_context(|| format!("write {}", path.display()))?;
    Ok(true)
}

/// Drop from ninox's `entries` every hook `<claude_dir>/settings.json`
/// already runs for the same event — matched by what the command invokes,
/// not its binary path. Used when that file is tracked (so never pruned):
/// Claude Code runs the hooks of both files, and seeding a second copy into
/// `settings.local.json` would make each fire twice. Hook groups and events
/// left empty are removed. A missing or unparsable file drops nothing.
pub fn without_legacy_duplicates(claude_dir: &Path, entries: &mut Value) {
    let Ok(Some(legacy)) = read_object(&claude_dir.join(LEGACY_SETTINGS)) else {
        return;
    };
    let Some(Value::Object(legacy_hooks)) = legacy.get("hooks") else { return };
    let Some(Value::Object(hooks)) = entries.get_mut("hooks") else { return };
    for (event, groups) in hooks.iter_mut() {
        let provided: Vec<&str> = legacy_hooks
            .get(event)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|g| g.get("hooks").and_then(Value::as_array))
            .flatten()
            .filter_map(|h| command_of(h).and_then(ninox_command_key))
            .collect();
        let Value::Array(groups) = groups else { continue };
        groups.retain_mut(|group| {
            let Some(Value::Array(cmds)) = group.get_mut("hooks") else { return true };
            cmds.retain(|h| !command_of(h).and_then(ninox_command_key).is_some_and(|k| provided.contains(&k)));
            !cmds.is_empty()
        });
    }
    hooks.retain(|_, groups| !groups.as_array().is_some_and(Vec::is_empty));
}

/// Whether `workspace`'s Claude Code settings carry a `Stop` hook whose
/// command contains `marker`. Checks `settings.local.json` (where ninox
/// seeds) and, for back-compat, `settings.json`. Missing or unparsable files
/// count as "not installed" — this backs best-effort capability probes.
pub fn stop_hook_installed(workspace: &Path, marker: &str) -> bool {
    let claude_dir = workspace.join(".claude");
    [LOCAL_SETTINGS, LEGACY_SETTINGS].iter().any(|name| {
        let Ok(Some(settings)) = read_object(&claude_dir.join(name)) else {
            return false;
        };
        settings
            .get("hooks")
            .and_then(|h| h.get("Stop"))
            .and_then(Value::as_array)
            .is_some_and(|groups| {
                groups.iter().any(|group| {
                    group.get("hooks").and_then(Value::as_array).is_some_and(|hooks| {
                        hooks.iter().any(|h| command_of(h).is_some_and(|c| c.contains(marker)))
                    })
                })
            })
    })
}

/// Set `permissions.allow`/`deny`/`ask` in the settings file at `path` to
/// exactly `rules`, so a rule removed from a worker type is revoked on the
/// next launch. Every other key is kept. A file that isn't a JSON object
/// is reported, not clobbered.
pub fn set_permission_rules(path: &Path, rules: &crate::worker_type::WorkerPermissionsConfig) -> anyhow::Result<()> {
    let mut settings = read_object(path)?.unwrap_or_default();
    let permissions = settings.entry("permissions").or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(permissions) = permissions else {
        anyhow::bail!("{}: `permissions` is not an object", path.display());
    };
    for (key, list) in [("allow", &rules.allow), ("deny", &rules.deny), ("ask", &rules.ask)] {
        permissions.insert(key.to_string(), Value::Array(list.iter().cloned().map(Value::String).collect()));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&settings)?).with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn install_local_adds_cross_session_accept_and_keeps_user_keys() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(LOCAL_SETTINGS);
        std::fs::write(&path, json!({"permissions": {"allow": ["Read"]}}).to_string()).unwrap();
        let entries = json!({"crossSessionInbound": "accept"});
        install_local(dir.path(), &entries).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["crossSessionInbound"], "accept");
        assert_eq!(v["permissions"]["allow"], json!(["Read"]));
    }

    #[test]
    fn install_local_keeps_a_user_set_cross_session_value() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(LOCAL_SETTINGS);
        std::fs::write(&path, json!({"crossSessionInbound": "hold"}).to_string()).unwrap();
        install_local(dir.path(), &json!({"crossSessionInbound": "accept"})).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["crossSessionInbound"], "hold");
    }

    #[test]
    fn set_permission_rules_replaces_the_lists_and_keeps_other_keys() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(LOCAL_SETTINGS);
        std::fs::write(&path, json!({
            "statusLine": {"type": "command", "command": "x"},
            "permissions": {"allow": ["Read", "Bash(old:*)"], "defaultMode": "default"},
        }).to_string()).unwrap();
        let rules = crate::worker_type::WorkerPermissionsConfig {
            enabled: true,
            allow: vec!["Read".into()],
            deny: vec!["Bash(rm:*)".into()],
            ask: vec!["Bash(git push:*)".into()],
        };
        set_permission_rules(&path, &rules).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["permissions"], json!({
            "allow": ["Read"], "deny": ["Bash(rm:*)"], "ask": ["Bash(git push:*)"], "defaultMode": "default",
        }));
        assert_eq!(v["statusLine"]["command"], "x");
    }

    #[test]
    fn recognises_absolute_path_guard_scripts_and_the_nx_binary() {
        assert!(is_ninox_command("node '/Users/x/Library/Application Support/o/.claude/subagent-blocker.cjs'"));
        assert!(is_ninox_command("node /root/.claude/subagent-blocker.cjs"));
        assert!(is_ninox_command("'/usr/local/bin/nx' worker-status hook-stop"));
        assert!(!is_ninox_command("node '/repo/.claude/hooks/sync.cjs'"));
        assert!(!is_ninox_command("node /repo/.claude/other.cjs"));
    }

    #[test]
    fn strip_local_removes_only_ninox_and_keeps_an_emptied_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(LOCAL_SETTINGS);
        std::fs::write(&path, json!({"statusLine": {"type": "command", "command": "'/b/ninox' statusline"}}).to_string()).unwrap();
        assert!(strip_local(dir.path()).unwrap());
        assert_eq!(serde_json::from_str::<Value>(&std::fs::read_to_string(&path).unwrap()).unwrap(), json!({}));
        assert!(!strip_local(dir.path()).unwrap());
        assert!(!strip_local(&dir.path().join("none")).unwrap());
    }

    #[test]
    fn user_status_line_looks_in_both_files_and_ignores_ninoxs_own() {
        let dir = tempdir().unwrap();
        assert!(!user_status_line(dir.path()));
        std::fs::write(dir.path().join(LOCAL_SETTINGS), json!({"statusLine": {"command": "'/b/ninox' statusline"}}).to_string()).unwrap();
        assert!(!user_status_line(dir.path()));
        std::fs::write(dir.path().join(LEGACY_SETTINGS), json!({"statusLine": {"command": "mine"}}).to_string()).unwrap();
        assert!(user_status_line(dir.path()));
    }

    #[test]
    fn recognises_only_ninox_commands() {
        for cmd in [
            "'/Users/x/Library/Application Support/ninox/bin/ninox' worker-status hook-stop",
            "'/opt/feature build/target/debug/ninox' statusline",
            "ninox inbox drain-prompt",
            "'/opt/ninox/bin/ninox' worker-status hook-needs-input",
            "'/opt/ninox/bin/ninox' worker-status hook-permission",
            "'/a/it'\\''s/ninox' inbox drain-stop",
            "node .claude/subagent-blocker.cjs",
            "node .claude/pr-watch-enforcement.cjs",
        ] {
            assert!(is_ninox_command(cmd), "{cmd}");
        }
        for cmd in [
            "lint --on-stop",
            "my-tool statusline",
            "'/usr/bin/other' worker-status hook-stop",
            "ninox worker-status hook-stop --extra",
            "node .claude/my-own-guard.cjs",
            "node .claude/subagent-blocker.cjs; rm -rf /tmp/x",
        ] {
            assert!(!is_ninox_command(cmd), "{cmd}");
        }
    }

    /// A legacy settings.json mixing ninox's seeded entries with the user's
    /// own: only ninox's go, everything else survives in its original order.
    #[test]
    fn strip_legacy_removes_only_ninox_entries() {
        let dir = tempdir().unwrap();
        let ninox = "'/Users/x/Library/Application Support/ninox/bin/ninox'";
        let original = json!({
            "permissions": {"allow": ["Bash(ls:*)"]},
            "statusLine": {"type": "command", "command": format!("{ninox} statusline"), "refreshInterval": 20},
            "hooks": {
                "Stop": [{"hooks": [
                    {"type": "command", "command": format!("{ninox} inbox drain-stop"), "timeout": 5},
                    {"type": "command", "command": "lint --on-stop"},
                    {"type": "command", "command": format!("{ninox} worker-status hook-stop"), "timeout": 5}
                ]}],
                "UserPromptSubmit": [{"hooks": [
                    {"type": "command", "command": format!("{ninox} worker-status hook-prompt"), "timeout": 5}
                ]}],
                "PreToolUse": [
                    {"matcher": "Bash", "hooks": [{"type": "command", "command": "node .claude/pr-watch-enforcement.cjs", "timeout": 5}]},
                    {"matcher": "Edit", "hooks": [{"type": "command", "command": "node .claude/my-guard.cjs"}]}
                ]
            },
            "env": {"FOO": "1"}
        });
        std::fs::write(dir.path().join(LEGACY_SETTINGS), serde_json::to_string_pretty(&original).unwrap()).unwrap();

        assert!(strip_legacy(dir.path()).unwrap());
        let after = std::fs::read_to_string(dir.path().join(LEGACY_SETTINGS)).unwrap();
        let expected = json!({
            "permissions": {"allow": ["Bash(ls:*)"]},
            "hooks": {
                "Stop": [{"hooks": [{"type": "command", "command": "lint --on-stop"}]}],
                "PreToolUse": [
                    {"matcher": "Edit", "hooks": [{"type": "command", "command": "node .claude/my-guard.cjs"}]}
                ]
            },
            "env": {"FOO": "1"}
        });
        assert_eq!(after, serde_json::to_string_pretty(&expected).unwrap(), "user entries must keep content and order");

        // Idempotent: a second pass finds nothing and leaves the bytes alone.
        assert!(!strip_legacy(dir.path()).unwrap());
        assert_eq!(std::fs::read_to_string(dir.path().join(LEGACY_SETTINGS)).unwrap(), after);
    }

    #[test]
    fn strip_legacy_leaves_files_without_ninox_entries_byte_intact() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(LEGACY_SETTINGS);
        for body in [r#"{"userCustom": true}"#, "{nope", r#"{"hooks":{"Stop":[{"hooks":[]}]}}"#] {
            std::fs::write(&path, body).unwrap();
            assert!(!strip_legacy(dir.path()).unwrap_or(false));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        }
        // Absent file: nothing to do.
        std::fs::remove_file(&path).unwrap();
        assert!(!strip_legacy(dir.path()).unwrap());
    }

    #[test]
    fn strip_legacy_deletes_a_file_that_held_only_ninox_entries() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join(LEGACY_SETTINGS),
            json!({"statusLine": {"type": "command", "command": "'/x/ninox' statusline"}}).to_string(),
        )
        .unwrap();
        assert!(strip_legacy(dir.path()).unwrap());
        assert!(!dir.path().join(LEGACY_SETTINGS).exists());
    }

    fn ninox_entries(bin: &str) -> Value {
        json!({
            "statusLine": {"type": "command", "command": format!("'{bin}' statusline")},
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": format!("'{bin}' worker-status hook-stop")}]}]}
        })
    }

    #[test]
    fn install_local_merges_with_user_content_and_replaces_stale_ninox_entries() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(LOCAL_SETTINGS);
        std::fs::write(&path, json!({
            "permissions": {"allow": ["Read"]},
            "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "lint --on-stop"}]}]}
        }).to_string()).unwrap();

        install_local(dir.path(), &ninox_entries("/old/ninox")).unwrap();
        install_local(dir.path(), &ninox_entries("/new build/ninox")).unwrap();

        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after["permissions"], json!({"allow": ["Read"]}));
        assert_eq!(after["statusLine"]["command"], "'/new build/ninox' statusline");
        let stop: Vec<_> = after["hooks"]["Stop"].as_array().unwrap().iter()
            .flat_map(|g| g["hooks"].as_array().unwrap().iter().map(|h| h["command"].as_str().unwrap()))
            .collect();
        assert_eq!(stop, ["lint --on-stop", "'/new build/ninox' worker-status hook-stop"]);
    }

    /// A tracked settings.json that already carries (some of) ninox's hooks
    /// — e.g. a repo that committed an older seed — must not get a second
    /// copy via settings.local.json, whatever binary path either one uses.
    #[test]
    fn without_legacy_duplicates_skips_hooks_settings_json_already_runs() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(LEGACY_SETTINGS), json!({"hooks": {
            "Stop": [{"hooks": [{"type": "command", "command": "'/old/ninox' worker-status hook-stop"}]}],
            "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "node .claude/pr-watch-enforcement.cjs"}]}]
        }}).to_string()).unwrap();
        let mut entries = json!({
            "statusLine": {"type": "command", "command": "'/new/ninox' statusline"},
            "hooks": {
                "Stop": [{"hooks": [
                    {"type": "command", "command": "'/new/ninox' inbox drain-stop"},
                    {"type": "command", "command": "'/new/ninox' worker-status hook-stop"}
                ]}],
                "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "'/new/ninox' worker-status hook-prompt"}]}],
                "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "node .claude/pr-watch-enforcement.cjs"}]}]
            }
        });

        without_legacy_duplicates(dir.path(), &mut entries);

        assert_eq!(entries, json!({
            "statusLine": {"type": "command", "command": "'/new/ninox' statusline"},
            "hooks": {
                "Stop": [{"hooks": [{"type": "command", "command": "'/new/ninox' inbox drain-stop"}]}],
                "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "'/new/ninox' worker-status hook-prompt"}]}]
            }
        }));
    }

    #[test]
    fn install_local_keeps_a_user_statusline_and_never_clobbers_unparsable_files() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(LOCAL_SETTINGS);
        std::fs::write(&path, r#"{"statusLine": {"type": "command", "command": "my-line"}}"#).unwrap();
        install_local(dir.path(), &ninox_entries("/x/ninox")).unwrap();
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after["statusLine"]["command"], "my-line");

        std::fs::write(&path, "{broken").unwrap();
        assert!(install_local(dir.path(), &ninox_entries("/x/ninox")).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{broken");
    }

    #[test]
    fn stop_hook_probe_reads_local_then_legacy_settings() {
        let dir = tempdir().unwrap();
        let claude = dir.path().join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        assert!(!stop_hook_installed(dir.path(), "worker-status hook-stop"));

        install_local(&claude, &ninox_entries("/x/ninox")).unwrap();
        assert!(stop_hook_installed(dir.path(), "worker-status hook-stop"));

        std::fs::remove_file(claude.join(LOCAL_SETTINGS)).unwrap();
        std::fs::write(claude.join(LEGACY_SETTINGS), ninox_entries("/x/ninox").to_string()).unwrap();
        assert!(stop_hook_installed(dir.path(), "worker-status hook-stop"));
    }
}
