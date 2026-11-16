use anyhow::Result;
use std::io::Write as _;
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
///   NINOX_SESSION     — session ID used as metadata filename
///   NINOX_DATA_DIR    — directory where {NINOX_SESSION}.json lives
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

# Detect `pr create` regardless of preceding global flags (`-R`, `--repo`,
# `--hostname`, with or without `=`) — argv-position matching alone
# (`$1==pr && $2==create`) misses `gh -R owner/repo pr create` entirely,
# silently falling through to the real gh with no metadata capture.
_is_pr_create=false
_skip_next=false
_positional=()
for _arg in "$@"; do
    if $_skip_next; then
        _skip_next=false
        continue
    fi
    case "$_arg" in
        -R|--repo|--hostname)
            _skip_next=true
            continue
            ;;
        --repo=*|--hostname=*)
            continue
            ;;
        -R?*)
            # Cuddled short-flag form gh's parser also accepts, e.g.
            # `-Rowner/repo` (no space) — value is embedded, nothing to skip.
            continue
            ;;
    esac
    _positional+=("$_arg")
done
if [[ "${_positional[0]:-}" == "pr" && "${_positional[1]:-}" == "create" ]]; then
    _is_pr_create=true
fi

# Run the real gh and tee output so we can parse it. Retried a few times
# specifically on "Text file busy" — immediately after ninox (re)writes
# this wrapper, exec'ing it (or, as here, exec'ing whatever it finds as the
# real binary) can transiently race a filesystem that hasn't fully
# committed the write yet. `_exit=$?` after a bare `_output=$(...)` would
# never run here: under `set -e`, a failing assignment's command
# substitution aborts the script on that line before the next one — the
# `|| _exit=$?` keeps the failure inside this statement instead.
if $_is_pr_create; then
    _exit=0
    _attempt=0
    while :; do
        _output=$("$_real_gh" "$@" 2>&1) || _exit=$?
        if [[ $_exit -ne 0 && "$_output" == *"Text file busy"* && $_attempt -lt 5 ]]; then
            _attempt=$((_attempt + 1))
            _exit=0
            sleep 0.02
            continue
        fi
        break
    done
    echo "$_output"
    _nx_session="${NINOX_SESSION:-}"
    _nx_data_dir="${NINOX_DATA_DIR:-}"
    if [[ $_exit -eq 0 && -n "$_nx_session" && -n "$_nx_data_dir" ]]; then
        _pr_url=$(echo "$_output" | grep -oE 'https?://[^/]+/[^/]+/[^/]+/pull/[0-9]+' | head -1)
        if [[ -n "$_pr_url" ]]; then
            _pr_num=$(echo "$_pr_url" | grep -oE '[0-9]+$')
            _meta_file="${_nx_data_dir}/${_nx_session}.json"
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
            elif command -v node &>/dev/null; then
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
                " 2>/dev/null || echo "ninox: warning: node failed to record PR metadata for $_pr_url" >&2
            else
                echo "ninox: warning: neither jq nor node found — PR metadata for $_pr_url was not recorded" >&2
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
_nx_session="${NINOX_SESSION:-}"
_nx_data_dir="${NINOX_DATA_DIR:-}"
if [[ $_exit -eq 0 && -n "$_nx_session" && -n "$_nx_data_dir" ]]; then
    _branch=""
    if [[ "${1:-}" == "checkout" && "${2:-}" == "-b" && -n "${3:-}" ]]; then
        _branch="${3}"
    elif [[ "${1:-}" == "switch" && "${2:-}" == "-c" && -n "${3:-}" ]]; then
        _branch="${3}"
    fi
    if [[ -n "$_branch" ]]; then
        _meta_file="${_nx_data_dir}/${_nx_session}.json"
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

/// `cmd` prefixed with an export that puts this instance's
/// [`crate::config::AppConfig::ninox_bin_dir`] first on PATH. Exported
/// inside the launch command rather than passed as env because login-shell
/// rc files re-prepend Homebrew/nvm dirs, which would push the wrappers and
/// the `ninox` shim behind the real `gh` and an installed `ninox`.
pub fn with_bin_dir_on_path(cmd: &str) -> String {
    let bin = crate::config::AppConfig::ninox_bin_dir().display().to_string().replace('\'', "'\\''");
    format!("export PATH='{bin}':\"$PATH\"; {cmd}")
}

/// Install wrappers to the default Ninox bin dir.
pub fn install_wrappers() -> Result<()> {
    install_wrappers_to(&crate::config::AppConfig::ninox_bin_dir())
}

/// Marker line embedding the version of the binary a self-shim forwards to.
const SHIM_VERSION_MARKER: &str = "# ninox-bin: ";

/// The shared `ninox` self-shim in the Ninox bin dir — the `ninox` agents
/// resolve through their PATH and that seeded hooks/statusLine invoke.
pub fn self_shim_path() -> PathBuf {
    crate::config::AppConfig::ninox_bin_dir().join("ninox")
}

/// What an existing self-shim says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShimInfo {
    /// The binary it `exec`s.
    pub target: PathBuf,
    /// Its `# ninox-bin:` marker; `None` for shims written before markers.
    pub version: Option<String>,
}

fn shim_script(exe: &Path, version: &str) -> String {
    let exe = exe.to_string_lossy().replace('\'', "'\\''");
    format!("#!/usr/bin/env bash\n{SHIM_VERSION_MARKER}{version}\nexec '{exe}' \"$@\"\n")
}

/// Parse a shim written by [`shim_script`] (or a pre-marker one). `None` if
/// `shim` is absent or has no recognisable `exec '<path>'` line.
pub fn read_shim(shim: &Path) -> Option<ShimInfo> {
    let body = std::fs::read_to_string(shim).ok()?;
    let mut version = None;
    let mut target = None;
    for line in body.lines() {
        if let Some(v) = line.strip_prefix(SHIM_VERSION_MARKER) {
            version = Some(v.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("exec '") {
            let quoted = rest.strip_suffix(" \"$@\"")?.strip_suffix('\'')?;
            target = Some(PathBuf::from(quoted.replace("'\\''", "'")));
        }
    }
    Some(ShimInfo { target: target?, version })
}

fn is_runnable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else { return false };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    meta.is_file()
}

/// Whether `shim` exists and forwards to a binary that is still runnable.
fn shim_is_valid(shim: &Path) -> bool {
    read_shim(shim).is_some_and(|info| is_runnable(&info.target))
}

/// Install the shared `ninox` self-shim (forwarding to `current_exe`) only
/// if there is no working one: absent, unparsable, or pointing at a binary
/// that is gone or not executable. A valid shim is never replaced on
/// launch — otherwise whichever build ran last (an old install, a stale
/// `.app`) would silently redirect every worker's hooks and `ninox` calls
/// to itself. Updating a valid shim is the deliberate `ninox reset-bin`
/// ([`reset_self_shim`]). Returns whether the shim was written.
pub fn install_self_shim(current_exe: &Path, version: &str) -> Result<bool> {
    install_self_shim_at(&self_shim_path(), current_exe, version)
}

fn install_self_shim_at(shim: &Path, current_exe: &Path, version: &str) -> Result<bool> {
    if shim_is_valid(shim) {
        return Ok(false);
    }
    write_shim(shim, current_exe, version)?;
    Ok(true)
}

/// Force the shared self-shim to forward to `current_exe`, whatever it
/// pointed at before (`ninox reset-bin`).
pub fn reset_self_shim(current_exe: &Path, version: &str) -> Result<()> {
    write_shim(&self_shim_path(), current_exe, version)
}

fn write_shim(shim: &Path, exe: &Path, version: &str) -> Result<()> {
    if let Some(dir) = shim.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_executable(shim.to_path_buf(), &shim_script(exe, version))
}

/// A one-line hint when the (valid) self-shim is behind the running
/// binary: its marked version is older (or unmarked), or it forwards to a
/// different binary. Never changes the shim — the hint points at
/// `ninox reset-bin`. `None` when the shim is current, or absent/broken
/// (which [`install_self_shim`] repairs instead).
pub fn self_shim_drift_hint(current_exe: &Path, version: &str) -> Option<String> {
    self_shim_drift_hint_at(&self_shim_path(), current_exe, version)
}

fn self_shim_drift_hint_at(shim: &Path, current_exe: &Path, version: &str) -> Option<String> {
    let info = read_shim(shim).filter(|i| is_runnable(&i.target))?;
    let current = semver::Version::parse(version).ok()?;
    let older = info
        .version
        .as_deref()
        .and_then(|v| semver::Version::parse(v).ok())
        .is_none_or(|v| v < current);
    // `current_exe` is canonical; a hand-edited shim may not be.
    let elsewhere = std::fs::canonicalize(&info.target).unwrap_or(info.target.clone()) != current_exe;
    if !older && !elsewhere {
        return None;
    }
    let marked = info.version.as_deref().unwrap_or("unknown version");
    Some(format!(
        "ninox: the shared `ninox` shim ({}) points at {} ({marked}), not this ninox {version} ({}); \
         run `ninox reset-bin` to point it here",
        shim.display(),
        info.target.display(),
        current_exe.display(),
    ))
}

/// Name of the TUI alias: `nx` is the same binary, and bare `nx` opens the
/// terminal UI where bare `ninox` keeps opening the desktop app.
pub const NX_ALIAS: &str = "nx";

/// Ensure an `nx` symlink sits next to the running `ninox` executable, so
/// `cargo install` users get the alias without a second binary. `exe` must
/// already be canonical (see [`canonical_exe`]). Only created when `exe`'s
/// directory is on `path_var` and no other `nx` is reachable through it (the
/// Nx monorepo CLI is commonly installed as `nx`), and never inside a macOS
/// `.app` bundle, where an extra file breaks the code signature.
pub fn install_nx_alias(exe: &Path, path_var: Option<&std::ffi::OsStr>) -> Result<()> {
    let Some(dir) = exe.parent() else { return Ok(()) };
    let Some(exe_name) = exe.file_name() else { return Ok(()) };
    if exe_name == NX_ALIAS {
        return Ok(());
    }
    anyhow::ensure!(!inside_app_bundle(exe), "{} is inside an app bundle", exe.display());
    let path_dirs: Vec<std::path::PathBuf> = path_var.map(|p| std::env::split_paths(p).collect()).unwrap_or_default();
    let same_dir = |d: &Path| d.canonicalize().is_ok_and(|d| d == dir);
    anyhow::ensure!(path_dirs.iter().any(|d| same_dir(d)), "{} is not on PATH", dir.display());
    for candidate in path_dirs.iter().map(|d| d.join(NX_ALIAS)) {
        let Ok(target) = candidate.canonicalize() else { continue };
        anyhow::ensure!(target == exe, "{} is another program; leaving `nx` to it", candidate.display());
    }
    let link = dir.join(NX_ALIAS);
    match std::fs::symlink_metadata(&link) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
        Ok(meta) => {
            let ours = meta.file_type().is_symlink()
                && std::fs::read_link(&link).is_ok_and(|t| t == Path::new(exe_name) || t == exe);
            if ours {
                return Ok(());
            }
            anyhow::bail!("{} exists and is not ninox's alias; leaving it alone", link.display());
        }
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(exe_name, &link)?;
    Ok(())
}

/// The running executable with symlinks resolved. macOS reports the path it
/// was invoked through, so a TUI launched as `nx` would otherwise point the
/// self-shim and the alias at `nx` instead of the real `ninox`.
pub fn canonical_exe() -> std::io::Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe.canonicalize().unwrap_or(exe))
}

pub fn inside_app_bundle(path: &Path) -> bool {
    let parts: Vec<_> = path.components().map(|c| c.as_os_str()).collect();
    parts.windows(2).any(|w| w[0].to_string_lossy().ends_with(".app") && w[1] == "Contents")
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
    // `install_wrappers_to` calls this with the same static content on
    // nearly every `ninox` invocation; skipping the write entirely when the
    // file already matches avoids both the fsync cost below and, more
    // importantly, the write+rename-then-exec window itself on the common
    // path, which is the only way to shrink the ETXTBSY race in production
    // (nothing here owns the later exec, done by whatever process resolves
    // `gh`/`git`/`ninox` off `PATH`, so there's no call site to retry).
    if is_runnable(&path) && std::fs::read_to_string(&path).is_ok_and(|existing| existing == content) {
        return Ok(());
    }

    // pid + per-process sequence, mirroring `trust::seed_workspace_trust_at`:
    // `install_wrappers_to` runs against this one shared bin dir on nearly
    // every `ninox` invocation, so a shared `<name>.tmp` path would let two
    // concurrent writers collide — one truncating the file the other is
    // still writing — and whichever renames second installs that mangled
    // result.
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let tmp_name = format!(
        "{}.tmp.{}-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("write_executable"),
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    );
    let tmp = path.with_file_name(tmp_name);

    // Atomic write: write to temp, then rename. `sync_all` on the file and
    // its directory before returning closes the window where a filesystem
    // (overlayfs on CI, notably) hasn't yet fully committed the rename,
    // which can otherwise manifest to a caller that execs `path` moments
    // later as a transient ETXTBSY. Clean up the temp file on any
    // failure — its name is unique per call, so unlike a shared `<name>.tmp`
    // path, nothing else will overwrite (and thereby incidentally clean up)
    // an orphaned one later.
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?;
        file.write_all(content.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o755))?;
        }
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &path)?;
        #[cfg(unix)]
        if let Some(dir) = path.parent() {
            if let Ok(dir_file) = std::fs::File::open(dir) {
                let _ = dir_file.sync_all();
            }
        }
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
    use tempfile::tempdir;

    fn canonical_tempdir() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap();
        (dir, path)
    }

    fn fake_exe(dir: &Path) -> std::path::PathBuf {
        let exe = dir.join("ninox");
        std::fs::write(&exe, "bin").unwrap();
        exe
    }

    fn path_of(dirs: &[&Path]) -> std::ffi::OsString {
        std::env::join_paths(dirs).unwrap()
    }

    fn runnable_exe(dir: &Path, name: &str) -> std::path::PathBuf {
        let exe = dir.join(name);
        write_executable(exe.clone(), "#!/bin/sh\n").unwrap();
        exe
    }

    #[test]
    fn self_shim_round_trips_target_and_version_marker() {
        let (_d, dir) = canonical_tempdir();
        let shim = dir.join("bin").join("ninox");
        let exe = dir.join("it's a build").join("ninox");
        write_shim(&shim, &exe, "1.2.3").unwrap();
        assert_eq!(read_shim(&shim), Some(ShimInfo { target: exe, version: Some("1.2.3".into()) }));
    }

    #[test]
    fn install_self_shim_leaves_a_valid_shim_untouched() {
        let (_d, dir) = canonical_tempdir();
        let shim = dir.join("bin").join("ninox");
        let newer = runnable_exe(&dir, "ninox-new");
        let older = runnable_exe(&dir, "ninox-old");
        assert!(install_self_shim_at(&shim, &newer, "0.40.0").unwrap(), "absent shim is written");
        let before = std::fs::read_to_string(&shim).unwrap();

        // An older build launching later must not clobber it.
        assert!(!install_self_shim_at(&shim, &older, "0.30.0").unwrap());
        assert_eq!(std::fs::read_to_string(&shim).unwrap(), before);
    }

    #[test]
    fn install_self_shim_heals_a_shim_whose_target_is_gone_or_not_executable() {
        let (_d, dir) = canonical_tempdir();
        let shim = dir.join("bin").join("ninox");
        let exe = runnable_exe(&dir, "ninox");

        write_shim(&shim, &dir.join("deleted-build").join("ninox"), "0.40.0").unwrap();
        assert!(install_self_shim_at(&shim, &exe, "0.33.0").unwrap());
        assert_eq!(read_shim(&shim).unwrap().target, exe);

        let not_exec = dir.join("plain-file");
        std::fs::write(&not_exec, "x").unwrap();
        write_shim(&shim, &not_exec, "0.40.0").unwrap();
        assert!(install_self_shim_at(&shim, &exe, "0.33.0").unwrap());
        assert_eq!(read_shim(&shim).unwrap().target, exe);

        // Garbage (or a pre-shim file) is replaced too.
        std::fs::write(&shim, "not a shim").unwrap();
        assert!(install_self_shim_at(&shim, &exe, "0.33.0").unwrap());
    }

    #[test]
    fn force_write_updates_target_and_marker() {
        let (_d, dir) = canonical_tempdir();
        let shim = dir.join("bin").join("ninox");
        let old = runnable_exe(&dir, "ninox-old");
        let new = runnable_exe(&dir, "ninox-new");
        install_self_shim_at(&shim, &old, "0.30.0").unwrap();
        // `reset_self_shim` is exactly this write against the real shim path.
        write_shim(&shim, &new, "0.33.0").unwrap();
        assert_eq!(read_shim(&shim), Some(ShimInfo { target: new, version: Some("0.33.0".into()) }));
    }

    #[test]
    fn drift_hint_fires_only_when_shim_is_behind_or_elsewhere() {
        let (_d, dir) = canonical_tempdir();
        let shim = dir.join("bin").join("ninox");
        let exe = runnable_exe(&dir, "ninox");
        let other = runnable_exe(&dir, "ninox-other");

        // Absent shim: nothing to hint about (install heals it).
        assert_eq!(self_shim_drift_hint_at(&shim, &exe, "0.33.0"), None);

        write_shim(&shim, &exe, "0.33.0").unwrap();
        assert_eq!(self_shim_drift_hint_at(&shim, &exe, "0.33.0"), None, "current shim is silent");
        write_shim(&shim, &exe, "0.34.0").unwrap();
        assert_eq!(self_shim_drift_hint_at(&shim, &exe, "0.33.0"), None, "newer shim is silent");

        write_shim(&shim, &exe, "0.32.9").unwrap();
        let hint = self_shim_drift_hint_at(&shim, &exe, "0.33.0").expect("older marker hints");
        assert!(hint.contains("ninox reset-bin") && !hint.contains('\n'), "{hint}");

        write_shim(&shim, &other, "0.33.0").unwrap();
        assert!(self_shim_drift_hint_at(&shim, &exe, "0.33.0").is_some(), "different target hints");

        std::fs::write(&shim, format!("#!/usr/bin/env bash\nexec '{}' \"$@\"\n", exe.display())).unwrap();
        assert!(self_shim_drift_hint_at(&shim, &exe, "0.33.0").is_some(), "pre-marker shim hints");
    }

    #[test]
    fn nx_alias_is_a_relative_symlink_and_never_clobbers() {
        let (_d, dir) = canonical_tempdir();
        let exe = fake_exe(&dir);
        let path = path_of(&[&dir]);
        install_nx_alias(&exe, Some(&path)).unwrap();
        let link = dir.join("nx");
        assert_eq!(std::fs::read_link(&link).unwrap(), Path::new("ninox"));
        install_nx_alias(&exe, Some(&path)).unwrap();

        let (_o, other) = canonical_tempdir();
        let exe2 = fake_exe(&other);
        std::fs::write(other.join("nx"), "someone else's nx").unwrap();
        assert!(install_nx_alias(&exe2, Some(&path_of(&[&other]))).is_err());
        assert_eq!(std::fs::read_to_string(other.join("nx")).unwrap(), "someone else's nx");
    }

    #[test]
    fn nx_alias_skipped_when_exe_dir_is_not_on_path() {
        let (_d, dir) = canonical_tempdir();
        let (_e, elsewhere) = canonical_tempdir();
        let exe = fake_exe(&dir);
        assert!(install_nx_alias(&exe, Some(&path_of(&[&elsewhere]))).is_err());
        assert!(install_nx_alias(&exe, None).is_err());
        assert!(std::fs::symlink_metadata(dir.join("nx")).is_err());
    }

    #[test]
    fn nx_alias_never_shadows_another_nx_on_path() {
        let (_d, dir) = canonical_tempdir();
        let (_n, node_bin) = canonical_tempdir();
        let exe = fake_exe(&dir);
        std::fs::write(node_bin.join("nx"), "nx monorepo cli").unwrap();
        for order in [[&node_bin, &dir], [&dir, &node_bin]] {
            let path = path_of(&[order[0].as_path(), order[1].as_path()]);
            assert!(install_nx_alias(&exe, Some(&path)).is_err());
        }
        assert!(std::fs::symlink_metadata(dir.join("nx")).is_err());
    }

    #[test]
    fn nx_alias_never_written_inside_an_app_bundle() {
        let (_d, root) = canonical_tempdir();
        let macos = root.join("Ninox.app/Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        let exe = fake_exe(&macos);
        assert!(install_nx_alias(&exe, Some(&path_of(&[&macos]))).is_err());
        assert!(std::fs::symlink_metadata(macos.join("nx")).is_err());
    }

    /// `install_wrappers_to` writes into one shared bin dir on nearly every
    /// `ninox` invocation, so concurrent processes/threads can call
    /// `write_executable` against the same target at once. Before the
    /// pid+sequence-unique temp name, every writer shared the same
    /// `<name>.tmp` path: one could truncate it mid-write by another,
    /// corrupting whichever one renamed second. Each writer here gets
    /// distinctly-sized content, so a torn/interleaved write would produce a
    /// length or byte pattern matching none of the inputs.
    #[test]
    fn concurrent_writes_to_one_target_never_corrupt_the_result() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("gh");

        let contents: Vec<String> = (0..16)
            .map(|i| format!("#!/usr/bin/env bash\n# writer {i}\n{}\n", "x".repeat(i * 37)))
            .collect();

        std::thread::scope(|s| {
            for content in &contents {
                let target = target.clone();
                s.spawn(move || write_executable(target, content).unwrap());
            }
        });

        let result = std::fs::read_to_string(&target).unwrap();
        assert!(
            contents.iter().any(|c| c == &result),
            "final content must exactly match one writer's full output, not a torn mix: {result:?}",
        );

        // No writer's unique temp file should be left behind, whether it
        // lost the rename race or won it.
        let leftover_tmp: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name != "gh")
            .collect();
        assert!(leftover_tmp.is_empty(), "leftover temp files: {leftover_tmp:?}");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target).unwrap().permissions().mode();
            assert!(mode & 0o111 != 0, "final file must still be executable");
        }
    }

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

    /// The wrapper scripts read only the NINOX_* env names — the legacy
    /// ATHENE_* transition fallbacks are gone.
    #[test]
    fn wrapper_scripts_read_only_ninox_env() {
        for wrapper in [GH_WRAPPER, GIT_WRAPPER] {
            assert!(wrapper.contains("${NINOX_SESSION:-}"));
            assert!(wrapper.contains("${NINOX_DATA_DIR:-}"));
            assert!(!wrapper.contains("ATHENE"));
        }
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

    /// A missing jq *and* node must be diagnosable, not a silent no-op —
    /// the wrapper previously dropped the write with `|| true` and no trace.
    #[test]
    fn gh_wrapper_warns_when_neither_jq_nor_node_available() {
        assert!(GH_WRAPPER.contains("neither jq nor node"));
    }

    /// Raw Unix ETXTBSY ("Text file busy").
    const ETXTBSY: i32 = 26;

    fn is_etxtbsy(err: &std::io::Error) -> bool {
        err.raw_os_error() == Some(ETXTBSY) || err.kind() == std::io::ErrorKind::ExecutableFileBusy
    }

    /// Run `f` (exec of a path `write_executable` may have just written),
    /// retrying a bounded number of times with a short sleep when it fails
    /// with ETXTBSY. Any other error is returned immediately. This only
    /// covers ETXTBSY raised by `f`'s own exec — GH_WRAPPER's *internal*
    /// invocation of `$_real_gh` has its own retry loop, directly in the
    /// script, since under `set -e` a failing `$_real_gh` surfaces to this
    /// process as a plain non-zero exit with no distinguishable `io::Error`
    /// to retry on here.
    fn exec_with_etxtbsy_retry<T>(mut f: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
        const MAX_ATTEMPTS: u32 = 5;
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(20);
        let mut attempt = 0;
        loop {
            match f() {
                Err(e) if is_etxtbsy(&e) && attempt + 1 < MAX_ATTEMPTS => {
                    attempt += 1;
                    std::thread::sleep(RETRY_DELAY);
                }
                result => return result,
            }
        }
    }

    /// Runs the real `GH_WRAPPER` script as a subprocess against a fake
    /// "real gh" that just echoes a PR URL, proving the argv scan actually
    /// detects `pr create` end to end — not just via string matching on the
    /// script source.
    fn run_gh_wrapper(args: &[&str], path_extra: &std::path::Path, session: &str, data_dir: &std::path::Path)
        -> std::process::Output
    {
        let wrapper_dir = tempdir().unwrap();
        let wrapper_path = wrapper_dir.path().join("gh");
        write_executable(wrapper_path.clone(), GH_WRAPPER).unwrap();
        let path_env = format!(
            "{}:{}",
            path_extra.display(),
            std::env::var("PATH").unwrap_or_default(),
        );
        exec_with_etxtbsy_retry(|| {
            std::process::Command::new(&wrapper_path)
                .args(args)
                .env("PATH", &path_env)
                .env("NINOX_SESSION", session)
                .env("NINOX_DATA_DIR", data_dir.to_string_lossy().to_string())
                .output()
        })
        .expect("failed to run gh wrapper")
    }

    fn fake_real_gh(pr_url: &str) -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        write_executable(
            dir.path().join("gh"),
            &format!("#!/usr/bin/env bash\necho 'Created pull request {pr_url}'\n"),
        ).unwrap();
        dir
    }

    /// `gh -R owner/repo pr create ...` and `gh --repo owner/repo pr create
    /// ...` must both be detected — a global flag before the subcommand
    /// previously made the wrapper fall straight through to real gh with no
    /// metadata capture at all.
    #[test]
    fn gh_wrapper_detects_pr_create_behind_global_repo_flags() {
        for extra_args in [
            vec!["-R", "org/repo"],
            vec!["--repo", "org/repo"],
            vec!["--repo=org/repo"],
            vec!["-Rorg/repo"],
        ] {
            let real_gh_dir = fake_real_gh("https://github.com/org/repo/pull/99");
            let data_dir = tempdir().unwrap();

            let mut args = extra_args.clone();
            args.extend(["pr", "create", "--title", "t", "--body", "b"]);

            let output = run_gh_wrapper(&args, real_gh_dir.path(), "s1", data_dir.path());
            assert!(
                output.status.success(),
                "wrapper failed for {extra_args:?}: {}",
                String::from_utf8_lossy(&output.stderr),
            );
            let meta = read_session_metadata(data_dir.path(), "s1").unwrap();
            assert_eq!(
                meta.pr_number, Some(99),
                "flags {extra_args:?} should still be detected as pr create",
            );
        }
    }

    /// Regression guard: non-`pr create` invocations (even with the same
    /// global flags) must still pass straight through with no metadata
    /// write — the argv scan must not become overly eager.
    #[test]
    fn gh_wrapper_does_not_intercept_non_pr_create_invocations() {
        let real_gh_dir = fake_real_gh("https://github.com/org/repo/pull/99");
        let data_dir = tempdir().unwrap();

        let output = run_gh_wrapper(
            &["-R", "org/repo", "pr", "list"],
            real_gh_dir.path(), "s1", data_dir.path(),
        );
        assert!(output.status.success());
        assert!(!data_dir.path().join("s1.json").exists());
    }
}
