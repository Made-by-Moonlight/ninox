use anyhow::Result;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Metadata types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct SessionMetadata {
    pub pr_number:  Option<u64>,
    pub pr_url:     Option<String>,
    pub branch:     Option<String>,
    /// Every PR the agent has reported creating, in creation order. The
    /// scalar `pr_number`/`pr_url` fields always mirror the latest one; this
    /// list is what lets the poller see PRs beyond the first.
    pub pr_reports: Vec<PrReport>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PrReport {
    pub number: u64,
    pub url:    Option<String>,
}

/// Additional work a worker asked the orchestrator to schedule
/// (`ninox request-work`). Stored one file per request under
/// `{dir}/{session}.requests/` — never inside the shared `{session}.json`,
/// which the gh/git bash wrappers rewrite concurrently and without locking.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkRequest {
    pub id:           String,
    pub description:  String,
    pub requested_at: i64,
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

    Ok(SessionMetadata { pr_number, pr_url, branch, pr_reports })
}

/// Record a worker's request for additional work (`ninox request-work`) as
/// its own file under `{dir}/{session_id}.requests/`. One file per request
/// keeps every writer single-owner: nothing here ever read-modify-writes the
/// shared `{session_id}.json` the bash wrappers rewrite.
pub fn append_work_request(dir: &Path, session_id: &str, description: &str) -> Result<WorkRequest> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);

    let requested_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    // pid + per-process sequence make the id unique even when two workers
    // (or one worker twice) request work in the same millisecond.
    let request = WorkRequest {
        id: format!(
            "wr-{requested_at}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
        ),
        description: description.to_string(),
        requested_at,
    };

    let requests_dir = work_requests_dir(dir, session_id);
    std::fs::create_dir_all(&requests_dir)?;
    let path = requests_dir.join(format!("{}.json", request.id));
    let body = serde_json::json!({
        "id":          request.id,
        "description": request.description,
        "requestedAt": request.requested_at,
    });
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&body)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(request)
}

/// Work requests not yet delivered to the orchestrator, oldest first.
pub fn read_pending_work_requests(dir: &Path, session_id: &str) -> Result<Vec<WorkRequest>> {
    let requests_dir = work_requests_dir(dir, session_id);
    let entries = match std::fs::read_dir(&requests_dir) {
        Ok(e)  => e,
        Err(_) => return Ok(Vec::new()), // no directory → nothing pending
    };
    let mut requests: Vec<WorkRequest> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|e| {
            let raw = std::fs::read_to_string(e.path()).ok()?;
            let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
            Some(WorkRequest {
                id:           v.get("id")?.as_str()?.to_string(),
                description:  v.get("description")?.as_str()?.to_string(),
                requested_at: v.get("requestedAt").and_then(|t| t.as_i64()).unwrap_or(0),
            })
        })
        .collect();
    requests.sort_by(|a, b| a.requested_at.cmp(&b.requested_at).then(a.id.cmp(&b.id)));
    Ok(requests)
}

/// Mark work requests delivered by renaming their file out of the pending
/// set (`.json` → `.json.delivered`) — kept on disk as an audit trail.
/// Best-effort per id: one failed rename must not leave *later* ids pending
/// (they would be fully re-delivered next tick); the first error is
/// returned after every id has been attempted.
pub fn mark_work_requests_delivered(dir: &Path, session_id: &str, ids: &[String]) -> Result<()> {
    let requests_dir = work_requests_dir(dir, session_id);
    let mut first_err: Option<std::io::Error> = None;
    for id in ids {
        let path = requests_dir.join(format!("{id}.json"));
        if path.exists() {
            let delivered = requests_dir.join(format!("{id}.json.delivered"));
            if let Err(e) = std::fs::rename(&path, &delivered) {
                first_err.get_or_insert(e);
            }
        }
    }
    match first_err {
        Some(e) => Err(e.into()),
        None    => Ok(()),
    }
}

/// Delete every metadata artifact for a session: the shared JSON, the
/// work-request directory, and the notified-PRs marker. Best-effort — used
/// when a session is removed, so a later session reusing the same id
/// doesn't inherit stale suppressions or pending requests.
pub fn remove_session_artifacts(dir: &Path, session_id: &str) {
    let _ = std::fs::remove_file(metadata_path(dir, session_id));
    let _ = std::fs::remove_dir_all(work_requests_dir(dir, session_id));
    let _ = std::fs::remove_file(notified_prs_path(dir, session_id));
}

/// Extra PRs (beyond the session's tracked one) that have already been
/// notified. Poller-owned side file — deliberately not the shared
/// `{session}.json` (wrapper races) and not the `prs` table (whose ids are
/// bare PR numbers and collide across repos).
pub fn read_notified_extra_prs(dir: &Path, session_id: &str) -> Vec<u64> {
    let path = notified_prs_path(dir, session_id);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Vec<u64>>(&raw).ok())
        .unwrap_or_default()
}

/// Append PR numbers to the session's notified-extra-PRs marker file.
/// Only the poller writes this file, so read-modify-write is race-free.
pub fn mark_extra_prs_notified(dir: &Path, session_id: &str, numbers: &[u64]) -> Result<()> {
    let path = notified_prs_path(dir, session_id);
    let mut notified = read_notified_extra_prs(dir, session_id);
    for n in numbers {
        if !notified.contains(n) {
            notified.push(*n);
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string(&notified)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn metadata_path(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(format!("{session_id}.json"))
}

fn work_requests_dir(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(format!("{session_id}.requests"))
}

fn notified_prs_path(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(format!("{session_id}.notified-prs.json"))
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
    fn append_work_request_accumulates_pending_requests() {
        let dir = tempdir().unwrap();
        let first  = append_work_request(dir.path(), "s1", "Fix flaky auth test").unwrap();
        let second = append_work_request(dir.path(), "s1", "Migrate config loader").unwrap();
        assert_ne!(first.id, second.id, "each request needs a distinct id");

        let pending = read_pending_work_requests(dir.path(), "s1").unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].description, "Fix flaky auth test");
        assert_eq!(pending[1].description, "Migrate config loader");
    }

    /// Work requests must never touch the shared `{session}.json` — the gh/git
    /// wrappers rewrite that file concurrently, and a read-modify-write from
    /// another process could erase their updates (or vice versa).
    #[test]
    fn append_work_request_leaves_shared_metadata_file_alone() {
        let dir = tempdir().unwrap();
        let shared = r#"{"agentReportedPrNumber": "42", "branch": "feat/x"}"#;
        std::fs::write(dir.path().join("s1.json"), shared).unwrap();
        append_work_request(dir.path(), "s1", "New task").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("s1.json")).unwrap(),
            shared,
            "shared metadata JSON must be byte-identical after a work request",
        );
        let m = read_session_metadata(dir.path(), "s1").unwrap();
        assert_eq!(m.pr_number, Some(42));
        assert_eq!(read_pending_work_requests(dir.path(), "s1").unwrap().len(), 1);
    }

    #[test]
    fn mark_work_requests_delivered_removes_only_named_ids_from_pending() {
        let dir = tempdir().unwrap();
        let first = append_work_request(dir.path(), "s1", "task a").unwrap();
        let second = append_work_request(dir.path(), "s1", "task b").unwrap();

        mark_work_requests_delivered(dir.path(), "s1", std::slice::from_ref(&first.id)).unwrap();

        let pending = read_pending_work_requests(dir.path(), "s1").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, second.id);
    }

    #[test]
    fn pending_work_requests_empty_when_none_recorded() {
        let dir = tempdir().unwrap();
        assert!(read_pending_work_requests(dir.path(), "s1").unwrap().is_empty());
    }

    /// Removing a session must clear every metadata artifact, or a reused
    /// session id (they're slugified human-chosen names) inherits the old
    /// session's notified-PR suppressions and stale pending requests.
    #[test]
    fn remove_session_artifacts_clears_all_files_for_that_session_only() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("s1.json"), "{}").unwrap();
        append_work_request(dir.path(), "s1", "task").unwrap();
        mark_extra_prs_notified(dir.path(), "s1", &[9]).unwrap();
        std::fs::write(dir.path().join("s2.json"), "{}").unwrap();
        append_work_request(dir.path(), "s2", "other").unwrap();

        remove_session_artifacts(dir.path(), "s1");

        assert!(!dir.path().join("s1.json").exists());
        assert!(!dir.path().join("s1.requests").exists());
        assert!(!dir.path().join("s1.notified-prs.json").exists());
        assert!(dir.path().join("s2.json").exists(), "other sessions untouched");
        assert_eq!(read_pending_work_requests(dir.path(), "s2").unwrap().len(), 1);
        // Idempotent on a session with no artifacts.
        remove_session_artifacts(dir.path(), "missing");
    }

    /// The extra-PR "already notified" marker is a poller-owned side file —
    /// never the shared `{session}.json`, and never keyed on the globally
    /// collision-prone `prs.id`.
    #[test]
    fn notified_extra_prs_round_trip_and_accumulate() {
        let dir = tempdir().unwrap();
        assert!(read_notified_extra_prs(dir.path(), "s1").is_empty());
        mark_extra_prs_notified(dir.path(), "s1", &[43, 44]).unwrap();
        mark_extra_prs_notified(dir.path(), "s1", &[45]).unwrap();
        let notified = read_notified_extra_prs(dir.path(), "s1");
        assert_eq!(notified, vec![43, 44, 45]);
        // Per-session, not global.
        assert!(read_notified_extra_prs(dir.path(), "s2").is_empty());
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
