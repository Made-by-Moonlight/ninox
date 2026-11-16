//! Claude Code workspace-trust seeding.
//!
//! Claude Code keys workspace trust on the exact cwd path — the
//! `projects.<abspath>.hasTrustDialogAccepted` flag in `~/.claude.json`. A
//! session launched in a directory with no such entry shows an interactive
//! "do you trust this folder?" dialog (default: No, exit) instead of an
//! input prompt. Every ninox spawn creates a brand-new directory (an
//! orchestrator workspace or a worker worktree), so a headless spawn sits
//! at that dialog until reaped and the initial brief is swallowed.
//!
//! No glob/parent-dir trust setting exists in Claude Code, so the only way
//! to spawn unattended is to seed the flag for the exact workspace path
//! before launching. Callers invoke [`seed_workspace_trust`] best-effort
//! (warn, don't fail the spawn) right before `tmux::create_session`.

use std::path::Path;

use anyhow::Context;

/// Seed `projects.<workspace>.hasTrustDialogAccepted = true` in the real
/// `~/.claude.json`.
///
/// No-op inside test binaries (same guard as `tmux::socket`): the suite
/// exercises real spawn paths against temp workspaces, and each of those
/// would otherwise deposit a junk trust entry in the developer's actual
/// Claude config.
pub fn seed_workspace_trust(workspace: &Path) -> anyhow::Result<()> {
    if crate::tmux::is_test_binary() {
        return Ok(());
    }
    let home = dirs::home_dir().context("no home directory")?;
    seed_workspace_trust_at(&home.join(".claude.json"), workspace)
}

/// Testable core of [`seed_workspace_trust`]: seed the trust flag into the
/// given Claude config file.
///
/// Read-modify-write with an atomic same-directory rename. Claude Code
/// itself rewrites this file constantly while sessions run, so a torn write
/// must never be observable; the remaining lost-update race (a concurrent
/// writer between our read and rename) is the same one the pre-existing
/// `trust-path` script lived with, and its worst case is the dialog
/// reappearing once, not corruption.
fn seed_workspace_trust_at(claude_json: &Path, workspace: &Path) -> anyhow::Result<()> {
    // Dotfiles setups symlink ~/.claude.json elsewhere; renaming onto the
    // link would replace the link itself with a regular file, severing the
    // dotfiles management and stranding this (and every future) trust entry
    // in a file Claude Code no longer reads. Rewrite the resolved target
    // instead. A missing file can't be resolved (or be a symlink) — keep
    // the literal path and create it there.
    let claude_json = claude_json
        .canonicalize()
        .unwrap_or_else(|_| claude_json.to_path_buf());
    let claude_json = claude_json.as_path();

    // Claude Code keys trust on the resolved path (symlinks followed), so
    // an entry for a symlinked spelling of the workspace would not match.
    let workspace = workspace.canonicalize().unwrap_or_else(|e| {
        // Seed the literal spelling anyway, but loudly: if it differs from
        // what Claude resolves the cwd to, the dialog this exists to prevent
        // comes back with no other trace.
        tracing::warn!("could not canonicalize {} ({e}) — seeding the literal path", workspace.display());
        workspace.to_path_buf()
    });
    let key = workspace.to_string_lossy().to_string();

    let mut data: serde_json::Value = match std::fs::read_to_string(claude_json) {
        Ok(raw) => serde_json::from_str(&raw)
            .with_context(|| format!("{} is not valid JSON — refusing to rewrite it", claude_json.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(e).with_context(|| format!("read {}", claude_json.display())),
    };
    let root = data
        .as_object_mut()
        .with_context(|| format!("{} is not a JSON object — refusing to rewrite it", claude_json.display()))?;
    let projects = root
        .entry("projects")
        .or_insert_with(|| serde_json::json!({}));
    let entry = projects
        .as_object_mut()
        .context("'projects' is not a JSON object — refusing to rewrite it")?
        .entry(key)
        .or_insert_with(|| serde_json::json!({}));
    entry
        .as_object_mut()
        .context("project entry is not a JSON object — refusing to rewrite it")?
        .insert("hasTrustDialogAccepted".to_string(), serde_json::Value::Bool(true));

    let dir = claude_json.parent().context("claude.json has no parent directory")?;
    // pid + per-process sequence, mirroring `hooks::append_work_request`:
    // the app seeds from concurrent tokio tasks, so a pid-only name would
    // let two in-flight seeds interleave writes into one temp file and
    // rename the mangled result over the real config.
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let tmp = dir.join(format!(
        ".claude.json.ninox-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));

    // The file holds Claude account/auth state and ships 0600 — a rename
    // must not swap in a umask-default (world-readable) replacement.
    let mode = std::fs::metadata(claude_json)
        .map(|m| m.permissions())
        .unwrap_or_else(|_| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::Permissions::from_mode(0o600)
        });

    let result = (|| -> anyhow::Result<()> {
        use std::io::Write as _;
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        f.set_permissions(mode)?;
        f.write_all(serde_json::to_string(&data)?.as_bytes())?;
        // Rename-before-data on a crash would leave the config empty; flush
        // the bytes to disk first so the rename only ever installs a
        // complete file.
        f.sync_all()?;
        std::fs::rename(&tmp, claude_json)
            .with_context(|| format!("rename {} over {}", tmp.display(), claude_json.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_claude_json_with_trust_entry_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".claude.json");
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();

        seed_workspace_trust_at(&cfg, &ws).unwrap();

        let data: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let key = ws.canonicalize().unwrap().to_string_lossy().to_string();
        assert_eq!(data["projects"][&key]["hasTrustDialogAccepted"], true);
    }

    #[test]
    fn preserves_unrelated_state_and_other_projects() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".claude.json");
        std::fs::write(
            &cfg,
            r#"{"numStartups": 42, "projects": {"/existing": {"hasTrustDialogAccepted": true, "history": ["x"]}}}"#,
        )
        .unwrap();
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();

        seed_workspace_trust_at(&cfg, &ws).unwrap();

        let data: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(data["numStartups"], 42);
        assert_eq!(data["projects"]["/existing"]["hasTrustDialogAccepted"], true);
        assert_eq!(data["projects"]["/existing"]["history"][0], "x");
        let key = ws.canonicalize().unwrap().to_string_lossy().to_string();
        assert_eq!(data["projects"][&key]["hasTrustDialogAccepted"], true);
    }

    #[test]
    fn sets_flag_on_existing_project_entry_without_dropping_fields() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".claude.json");
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        let key = ws.canonicalize().unwrap().to_string_lossy().to_string();
        std::fs::write(
            &cfg,
            serde_json::json!({
                "projects": { &key: { "hasTrustDialogAccepted": false, "allowedTools": ["Bash"] } }
            })
            .to_string(),
        )
        .unwrap();

        seed_workspace_trust_at(&cfg, &ws).unwrap();

        let data: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(data["projects"][&key]["hasTrustDialogAccepted"], true);
        assert_eq!(data["projects"][&key]["allowedTools"][0], "Bash");
    }

    #[test]
    fn keys_the_entry_by_the_canonical_path_behind_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".claude.json");
        let real = dir.path().join("real-workspace");
        std::fs::create_dir_all(&real).unwrap();
        let link = dir.path().join("link-workspace");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        seed_workspace_trust_at(&cfg, &link).unwrap();

        let data: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let key = real.canonicalize().unwrap().to_string_lossy().to_string();
        assert_eq!(data["projects"][&key]["hasTrustDialogAccepted"], true);
    }

    #[test]
    fn refuses_to_clobber_an_unparseable_claude_json() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".claude.json");
        std::fs::write(&cfg, "{not json").unwrap();
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();

        assert!(seed_workspace_trust_at(&cfg, &ws).is_err());
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "{not json");
    }

    #[test]
    fn preserves_the_files_restrictive_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".claude.json");
        std::fs::write(&cfg, "{}").unwrap();
        std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o600)).unwrap();
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();

        seed_workspace_trust_at(&cfg, &ws).unwrap();

        let mode = std::fs::metadata(&cfg).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "rewrite must not loosen the file's permissions");
    }

    #[test]
    fn creates_a_missing_file_owner_readable_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".claude.json");
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();

        seed_workspace_trust_at(&cfg, &ws).unwrap();

        let mode = std::fs::metadata(&cfg).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "claude.json holds account state — never world-readable");
    }

    #[test]
    fn concurrent_seeds_from_one_process_never_leave_the_file_unparseable() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".claude.json");
        std::fs::write(&cfg, "{}").unwrap();
        let workspaces: Vec<_> = (0..8)
            .map(|i| {
                let ws = dir.path().join(format!("ws-{i}"));
                std::fs::create_dir_all(&ws).unwrap();
                ws
            })
            .collect();

        std::thread::scope(|s| {
            for ws in &workspaces {
                s.spawn(|| seed_workspace_trust_at(&cfg, ws).unwrap());
            }
        });

        // Lost updates are tolerated (last rename wins); a torn or
        // interleaved write is not.
        let data: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert!(data["projects"].is_object());
    }

    #[test]
    fn rewrites_through_a_symlinked_claude_json_without_replacing_the_link() {
        // Dotfiles setups symlink ~/.claude.json into a managed repo; a
        // rename onto the link itself would sever it and strand future
        // trust entries in a file Claude Code no longer reads.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles-claude.json");
        std::fs::write(&real, "{}").unwrap();
        let link = dir.path().join(".claude.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let ws = dir.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();

        seed_workspace_trust_at(&link, &ws).unwrap();

        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        let data: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&real).unwrap()).unwrap();
        let key = ws.canonicalize().unwrap().to_string_lossy().to_string();
        assert_eq!(data["projects"][&key]["hasTrustDialogAccepted"], true);
    }

    #[test]
    fn public_entry_point_is_a_noop_inside_test_binaries() {
        // Would otherwise write a junk entry for this temp path into the
        // developer's real ~/.claude.json.
        let dir = tempfile::tempdir().unwrap();
        seed_workspace_trust(dir.path()).unwrap();
    }
}
