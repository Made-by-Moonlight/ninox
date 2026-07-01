use anyhow::Result;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

// ---------------------------------------------------------------------------
// Metadata types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct SessionMetadata {
    pub pr_number: Option<u64>,
    pub pr_url:    Option<String>,
    pub branch:    Option<String>,
}

// ---------------------------------------------------------------------------
// Wrapper scripts
// ---------------------------------------------------------------------------

/// The `gh` wrapper script. Intercepts `gh pr create`, extracts the PR URL
/// from output, and writes it to the session metadata JSON file.
///
/// Env vars consumed at runtime (injected by athene when spawning the tmux session):
///   ATHENE_SESSION    — session ID used as metadata filename
///   ATHENE_DATA_DIR   — directory where {ATHENE_SESSION}.json lives
const GH_WRAPPER: &str = r#"#!/usr/bin/env bash
# Athene gh wrapper — intercepts gh pr create to record PR metadata.
set -euo pipefail

# Locate the real gh binary (skip ourselves).
_real_gh=""
IFS=: read -ra _path_parts <<< "$PATH"
for _dir in "${_path_parts[@]}"; do
    _candidate="$_dir/gh"
    if [[ "$_candidate" != "$0" && -x "$_candidate" ]]; then
        _real_gh="$_candidate"
        break
    fi
done
if [[ -z "$_real_gh" ]]; then
    echo "athene: gh not found in PATH (excluding wrapper)" >&2
    exit 1
fi

# Run the real gh and tee output so we can parse it.
if [[ "${1:-}" == "pr" && "${2:-}" == "create" ]]; then
    _output=$("$_real_gh" "$@" 2>&1)
    _exit=$?
    echo "$_output"
    if [[ $_exit -eq 0 && -n "${ATHENE_SESSION:-}" && -n "${ATHENE_DATA_DIR:-}" ]]; then
        _pr_url=$(echo "$_output" | grep -oE 'https?://[^/]+/[^/]+/[^/]+/pull/[0-9]+' | head -1)
        if [[ -n "$_pr_url" ]]; then
            _pr_num=$(echo "$_pr_url" | grep -oE '[0-9]+$')
            _meta_file="${ATHENE_DATA_DIR}/${ATHENE_SESSION}.json"
            mkdir -p "$(dirname "$_meta_file")"
            _tmp="${_meta_file}.tmp.$$"
            if [[ -f "$_meta_file" ]]; then
                _existing=$(cat "$_meta_file")
            else
                _existing="{}"
            fi
            if command -v jq &>/dev/null; then
                echo "$_existing" | jq \
                    --arg url "$_pr_url" \
                    --arg num "$_pr_num" \
                    '. + {"agentReportedPrUrl": $url, "agentReportedPrNumber": $num, "agentReportedState": "pr_created"}' \
                    > "$_tmp" && mv "$_tmp" "$_meta_file"
            else
                # Fallback: node (likely available alongside gh).
                # PR URL and number are passed via env vars, not interpolated into the
                # script string, to avoid shell injection from external GitHub output.
                ATHENE_PR_URL="$_pr_url" ATHENE_PR_NUM="$_pr_num" node -e "
                    const fs = require('fs');
                    const url = process.env.ATHENE_PR_URL;
                    const num = process.env.ATHENE_PR_NUM;
                    const f = '${_meta_file}';
                    const m = JSON.parse(fs.existsSync(f) ? fs.readFileSync(f,'utf8') : '{}');
                    m.agentReportedPrUrl = url;
                    m.agentReportedPrNumber = num;
                    m.agentReportedState = 'pr_created';
                    fs.writeFileSync(f + '.tmp.\$\$', JSON.stringify(m,null,2));
                    fs.renameSync(f + '.tmp.\$\$', f);
                " 2>/dev/null || true
            fi
        fi
    fi
    exit $_exit
else
    exec "$_real_gh" "$@"
fi
"#;

/// The `git` wrapper script. Intercepts branch creation to record branch name.
const GIT_WRAPPER: &str = r#"#!/usr/bin/env bash
# Athene git wrapper — records branch name on checkout -b / switch -c.
set -euo pipefail

_real_git=""
IFS=: read -ra _path_parts <<< "$PATH"
for _dir in "${_path_parts[@]}"; do
    _candidate="$_dir/git"
    if [[ "$_candidate" != "$0" && -x "$_candidate" ]]; then
        _real_git="$_candidate"
        break
    fi
done
if [[ -z "$_real_git" ]]; then
    echo "athene: git not found in PATH (excluding wrapper)" >&2
    exit 1
fi

# Run the real git command first.
"$_real_git" "$@"
_exit=$?

# On success, capture branch name for checkout -b / switch -c.
if [[ $_exit -eq 0 && -n "${ATHENE_SESSION:-}" && -n "${ATHENE_DATA_DIR:-}" ]]; then
    _branch=""
    if [[ "${1:-}" == "checkout" && "${2:-}" == "-b" && -n "${3:-}" ]]; then
        _branch="${3}"
    elif [[ "${1:-}" == "switch" && "${2:-}" == "-c" && -n "${3:-}" ]]; then
        _branch="${3}"
    fi
    if [[ -n "$_branch" ]]; then
        _meta_file="${ATHENE_DATA_DIR}/${ATHENE_SESSION}.json"
        mkdir -p "$(dirname "$_meta_file")"
        if command -v jq &>/dev/null; then
            _tmp="${_meta_file}.tmp.$$"
            _existing=$([ -f "$_meta_file" ] && cat "$_meta_file" || echo "{}")
            echo "$_existing" | jq --arg b "$_branch" '. + {"branch": $b}' \
                > "$_tmp" && mv "$_tmp" "$_meta_file"
        fi
    fi
fi

exit $_exit
"#;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Install `gh` and `git` wrapper scripts to the given directory.
/// Called with the Athene bin dir (`~/.config/athene/bin/`) in production.
pub fn install_wrappers_to(bin_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(bin_dir)?;
    write_executable(bin_dir.join("gh"),  GH_WRAPPER)?;
    write_executable(bin_dir.join("git"), GIT_WRAPPER)?;
    Ok(())
}

/// Install wrappers to the default Athene bin dir.
pub fn install_wrappers() -> Result<()> {
    install_wrappers_to(&crate::config::AppConfig::athene_bin_dir())
}

/// Write a thin `athene` shim to the Athene bin dir that forwards all arguments
/// to the currently-running executable. This ensures that when an orchestrator
/// runs `athene spawn` (and `~/.config/athene/bin` is first in PATH), it always
/// invokes the same build that is currently running — not a stale system install.
pub fn install_self_shim(current_exe: &Path) -> Result<()> {
    let bin_dir = crate::config::AppConfig::athene_bin_dir();
    std::fs::create_dir_all(&bin_dir)?;
    let exe = current_exe.to_string_lossy().replace('\'', "'\\''");
    let script = format!(
        "#!/usr/bin/env bash\nexec '{}' \"$@\"\n",
        exe
    );
    write_executable(bin_dir.join("athene"), &script)?;
    Ok(())
}

/// Read session metadata from `{dir}/{session_id}.json`.
/// Returns empty `SessionMetadata` if the file does not exist or is malformed.
pub fn read_session_metadata(dir: &Path, session_id: &str) -> Result<SessionMetadata> {
    let path = dir.join(format!("{session_id}.json"));
    if !path.exists() {
        return Ok(SessionMetadata::default());
    }
    let raw = std::fs::read_to_string(&path)?;
    let map: HashMap<String, serde_json::Value> = match serde_json::from_str(&raw) {
        Ok(m)  => m,
        Err(_) => return Ok(SessionMetadata::default()),
    };
    let pr_number = map.get("agentReportedPrNumber")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<u64>().ok());
    let pr_url = map.get("agentReportedPrUrl")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let branch = map.get("branch")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Ok(SessionMetadata { pr_number, pr_url, branch })
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn write_executable(path: PathBuf, content: &str) -> Result<()> {
    // Atomic write: write to temp, then rename.
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn install_wrappers_creates_executables() {
        let dir = tempdir().unwrap();
        install_wrappers_to(dir.path()).unwrap();
        assert!(dir.path().join("gh").exists());
        assert!(dir.path().join("git").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let gh_mode = std::fs::metadata(dir.path().join("gh"))
                .unwrap().permissions().mode();
            assert!(gh_mode & 0o111 != 0, "gh wrapper should be executable");
        }
    }

    #[test]
    fn read_session_metadata_parses_pr_number() {
        let dir = tempdir().unwrap();
        let metadata = serde_json::json!({
            "agentReportedPrNumber": "42",
            "agentReportedPrUrl": "https://github.com/org/repo/pull/42",
            "branch": "feat/my-fix"
        });
        std::fs::write(
            dir.path().join("s1.json"),
            serde_json::to_string(&metadata).unwrap(),
        ).unwrap();
        let m = read_session_metadata(dir.path(), "s1").unwrap();
        assert_eq!(m.pr_number, Some(42));
        assert_eq!(m.branch.as_deref(), Some("feat/my-fix"));
    }

    #[test]
    fn read_session_metadata_returns_default_on_missing_file() {
        let dir = tempdir().unwrap();
        let m = read_session_metadata(dir.path(), "nonexistent").unwrap();
        assert_eq!(m.pr_number, None);
        assert_eq!(m.branch, None);
    }

    #[test]
    fn read_session_metadata_handles_malformed_json() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("bad.json"), "not json").unwrap();
        let m = read_session_metadata(dir.path(), "bad").unwrap();
        assert_eq!(m.pr_number, None);
    }
}
