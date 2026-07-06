use anyhow::Result;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Metadata types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct SessionMetadata {
    pub pr_number:     Option<u64>,
    pub pr_url:        Option<String>,
    pub branch:        Option<String>,
    /// Every PR the agent has reported creating, in creation order. The
    /// scalar `pr_number`/`pr_url` fields always mirror the latest one; this
    /// list is what lets the poller see PRs beyond the first.
    pub pr_reports:    Vec<PrReport>,
    /// Additional work the agent asked the orchestrator to schedule
    /// (`ninox request-work`), oldest first.
    pub work_requests: Vec<WorkRequest>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PrReport {
    pub number: u64,
    pub url:    Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkRequest {
    pub id:           String,
    pub description:  String,
    pub requested_at: i64,
    /// Set once the poller has forwarded the request to the orchestrator so
    /// it is never delivered twice.
    pub delivered:    bool,
}

// ---------------------------------------------------------------------------
// Wrapper scripts
// ---------------------------------------------------------------------------

/// The `gh` wrapper script. Intercepts `gh pr create`, extracts the PR URL
/// from output, and writes it to the session metadata JSON file.
///
/// Env vars consumed at runtime (injected by ninox when spawning the tmux session):
///   ATHENE_SESSION    — session ID used as metadata filename
///   ATHENE_DATA_DIR   — directory where {ATHENE_SESSION}.json lives
const GH_WRAPPER: &str = r#"#!/usr/bin/env bash
# Ninox gh wrapper — intercepts gh pr create to record PR metadata.
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
    echo "ninox: gh not found in PATH (excluding wrapper)" >&2
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
                    '. + {"agentReportedPrUrl": $url, "agentReportedPrNumber": $num, "agentReportedState": "pr_created"}
                     | .agentReportedPrs = ((.agentReportedPrs // []) + [{"number": $num, "url": $url}])' \
                    > "$_tmp" && mv "$_tmp" "$_meta_file"
            else
                # Fallback: node (likely available alongside gh).
                # PR URL and number are passed via env vars, not interpolated into the
                # script string, to avoid shell injection from external GitHub output.
                NINOX_PR_URL="$_pr_url" NINOX_PR_NUM="$_pr_num" node -e "
                    const fs = require('fs');
                    const url = process.env.NINOX_PR_URL;
                    const num = process.env.NINOX_PR_NUM;
                    const f = '${_meta_file}';
                    const m = JSON.parse(fs.existsSync(f) ? fs.readFileSync(f,'utf8') : '{}');
                    m.agentReportedPrUrl = url;
                    m.agentReportedPrNumber = num;
                    m.agentReportedState = 'pr_created';
                    m.agentReportedPrs = (Array.isArray(m.agentReportedPrs) ? m.agentReportedPrs : [])
                        .concat([{ number: num, url: url }]);
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
# Ninox git wrapper — records branch name on checkout -b / switch -c.
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
    echo "ninox: git not found in PATH (excluding wrapper)" >&2
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
/// Called with the Ninox bin dir (`~/.config/ninox/bin/`) in production.
pub fn install_wrappers_to(bin_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(bin_dir)?;
    write_executable(bin_dir.join("gh"),  GH_WRAPPER)?;
    write_executable(bin_dir.join("git"), GIT_WRAPPER)?;
    Ok(())
}

/// Install wrappers to the default Ninox bin dir.
pub fn install_wrappers() -> Result<()> {
    install_wrappers_to(&crate::config::AppConfig::ninox_bin_dir())
}

/// Write a thin `ninox` shim to the Ninox bin dir that forwards all arguments
/// to the currently-running executable. This ensures that when an orchestrator
/// runs `ninox spawn` (and `~/.config/ninox/bin` is first in PATH), it always
/// invokes the same build that is currently running — not a stale system install.
pub fn install_self_shim(current_exe: &Path) -> Result<()> {
    let bin_dir = crate::config::AppConfig::ninox_bin_dir();
    std::fs::create_dir_all(&bin_dir)?;
    let exe = current_exe.to_string_lossy().replace('\'', "'\\''");
    let script = format!(
        "#!/usr/bin/env bash\nexec '{}' \"$@\"\n",
        exe
    );
    write_executable(bin_dir.join("ninox"), &script)?;
    Ok(())
}

/// Read session metadata from `{dir}/{session_id}.json`.
/// Returns empty `SessionMetadata` if the file does not exist or is malformed.
pub fn read_session_metadata(dir: &Path, session_id: &str) -> Result<SessionMetadata> {
    let map = match read_metadata_map(&metadata_path(dir, session_id))? {
        Some(m) => m,
        None    => return Ok(SessionMetadata::default()),
    };
    let pr_number = map.get("agentReportedPrNumber").and_then(as_u64);
    let pr_url = map.get("agentReportedPrUrl")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let branch = map.get("branch")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let mut pr_reports: Vec<PrReport> = map.get("agentReportedPrs")
        .and_then(|v| v.as_array())
        .map(|prs| {
            prs.iter()
                .filter_map(|p| {
                    Some(PrReport {
                        number: as_u64(p.get("number")?)?,
                        url:    p.get("url").and_then(|u| u.as_str()).map(String::from),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    // Metadata written before `agentReportedPrs` existed only carries the
    // scalar keys — surface that PR in the list view too.
    if pr_reports.is_empty() {
        if let Some(number) = pr_number {
            pr_reports.push(PrReport { number, url: pr_url.clone() });
        }
    }

    let work_requests: Vec<WorkRequest> = map.get("workRequests")
        .and_then(|v| v.as_array())
        .map(|reqs| {
            reqs.iter()
                .filter_map(|r| {
                    Some(WorkRequest {
                        id:           r.get("id")?.as_str()?.to_string(),
                        description:  r.get("description")?.as_str()?.to_string(),
                        requested_at: r.get("requestedAt").and_then(|v| v.as_i64()).unwrap_or(0),
                        delivered:    r.get("delivered").and_then(|v| v.as_bool()).unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(SessionMetadata { pr_number, pr_url, branch, pr_reports, work_requests })
}

/// Record a worker's request for additional work in its session metadata
/// file (`ninox request-work`). The poller delivers undelivered requests to
/// the orchestrator and marks them via [`mark_work_requests_delivered`].
pub fn append_work_request(dir: &Path, session_id: &str, description: &str) -> Result<WorkRequest> {
    let path = metadata_path(dir, session_id);
    let mut map = read_metadata_map(&path)?.unwrap_or_default();

    let existing = map.get("workRequests").and_then(|v| v.as_array()).map_or(0, |a| a.len());
    let requested_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let request = WorkRequest {
        id: format!("wr-{session_id}-{requested_at}-{existing}"),
        description: description.to_string(),
        requested_at,
        delivered: false,
    };

    let entry = serde_json::json!({
        "id":          request.id,
        "description": request.description,
        "requestedAt": request.requested_at,
        "delivered":   request.delivered,
    });
    match map.entry("workRequests".to_string())
        .or_insert_with(|| serde_json::Value::Array(vec![]))
    {
        serde_json::Value::Array(arr) => arr.push(entry),
        other => *other = serde_json::Value::Array(vec![entry]),
    }

    write_metadata_map(&path, &map)?;
    Ok(request)
}

/// Flag the given work-request ids as delivered so the poller never forwards
/// them to the orchestrator twice.
pub fn mark_work_requests_delivered(dir: &Path, session_id: &str, ids: &[String]) -> Result<()> {
    let path = metadata_path(dir, session_id);
    let Some(mut map) = read_metadata_map(&path)? else { return Ok(()) };
    if let Some(serde_json::Value::Array(arr)) = map.get_mut("workRequests") {
        for entry in arr.iter_mut() {
            let matches = entry.get("id")
                .and_then(|v| v.as_str())
                .is_some_and(|id| ids.iter().any(|i| i == id));
            if matches {
                entry["delivered"] = serde_json::Value::Bool(true);
            }
        }
    }
    write_metadata_map(&path, &map)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn metadata_path(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(format!("{session_id}.json"))
}

/// Load the raw metadata JSON object. `Ok(None)` when the file is missing or
/// malformed — callers treat both as "no metadata yet".
fn read_metadata_map(path: &Path) -> Result<Option<serde_json::Map<String, serde_json::Value>>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&raw).ok())
}

/// Atomic write (temp + rename), preserving keys we don't model — the same
/// discipline the bash wrappers follow.
fn write_metadata_map(path: &Path, map: &serde_json::Map<String, serde_json::Value>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(map)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// The wrappers write numbers as JSON strings (jq `--arg`); accept both.
fn as_u64(v: &serde_json::Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

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

    #[test]
    fn append_work_request_accumulates_undelivered_requests() {
        let dir = tempdir().unwrap();
        let first  = append_work_request(dir.path(), "s1", "Fix flaky auth test").unwrap();
        let second = append_work_request(dir.path(), "s1", "Migrate config loader").unwrap();
        assert_ne!(first.id, second.id, "each request needs a distinct id");

        let m = read_session_metadata(dir.path(), "s1").unwrap();
        assert_eq!(m.work_requests.len(), 2);
        assert_eq!(m.work_requests[0].description, "Fix flaky auth test");
        assert_eq!(m.work_requests[1].description, "Migrate config loader");
        assert!(m.work_requests.iter().all(|r| !r.delivered));
    }

    #[test]
    fn append_work_request_preserves_existing_metadata_keys() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("s1.json"),
            r#"{"agentReportedPrNumber": "42", "branch": "feat/x"}"#,
        ).unwrap();
        append_work_request(dir.path(), "s1", "New task").unwrap();
        let m = read_session_metadata(dir.path(), "s1").unwrap();
        assert_eq!(m.pr_number, Some(42));
        assert_eq!(m.branch.as_deref(), Some("feat/x"));
        assert_eq!(m.work_requests.len(), 1);
    }

    #[test]
    fn mark_work_requests_delivered_flags_only_named_ids() {
        let dir = tempdir().unwrap();
        let first = append_work_request(dir.path(), "s1", "task a").unwrap();
        append_work_request(dir.path(), "s1", "task b").unwrap();

        mark_work_requests_delivered(dir.path(), "s1", &[first.id.clone()]).unwrap();

        let m = read_session_metadata(dir.path(), "s1").unwrap();
        assert!(m.work_requests.iter().find(|r| r.id == first.id).unwrap().delivered);
        assert_eq!(m.work_requests.iter().filter(|r| !r.delivered).count(), 1);
    }

    #[test]
    fn read_session_metadata_parses_reported_pr_list() {
        let dir = tempdir().unwrap();
        let metadata = serde_json::json!({
            "agentReportedPrNumber": "44",
            "agentReportedPrUrl": "https://github.com/org/repo/pull/44",
            "agentReportedPrs": [
                {"number": "42", "url": "https://github.com/org/repo/pull/42"},
                {"number": "43", "url": "https://github.com/org/repo/pull/43"},
                {"number": "44", "url": "https://github.com/org/repo/pull/44"},
            ],
        });
        std::fs::write(
            dir.path().join("s1.json"),
            serde_json::to_string(&metadata).unwrap(),
        ).unwrap();
        let m = read_session_metadata(dir.path(), "s1").unwrap();
        let numbers: Vec<u64> = m.pr_reports.iter().map(|p| p.number).collect();
        assert_eq!(numbers, vec![42, 43, 44]);
        assert_eq!(
            m.pr_reports[0].url.as_deref(),
            Some("https://github.com/org/repo/pull/42"),
        );
    }

    /// Metadata written before `agentReportedPrs` existed only has the scalar
    /// keys — the list view must still surface that PR so extra-PR detection
    /// and first-PR detection see the same universe.
    #[test]
    fn read_session_metadata_synthesizes_pr_list_from_legacy_scalars() {
        let dir = tempdir().unwrap();
        let metadata = serde_json::json!({
            "agentReportedPrNumber": "42",
            "agentReportedPrUrl": "https://github.com/org/repo/pull/42",
        });
        std::fs::write(
            dir.path().join("s1.json"),
            serde_json::to_string(&metadata).unwrap(),
        ).unwrap();
        let m = read_session_metadata(dir.path(), "s1").unwrap();
        assert_eq!(m.pr_reports.len(), 1);
        assert_eq!(m.pr_reports[0].number, 42);
    }

    /// The gh wrapper must append every created PR to `agentReportedPrs`, not
    /// just overwrite the scalar keys — otherwise a worker that opens N PRs
    /// only ever exposes the latest one to the poller.
    #[test]
    fn gh_wrapper_appends_to_pr_list() {
        assert!(GH_WRAPPER.contains("agentReportedPrs"));
    }
}
