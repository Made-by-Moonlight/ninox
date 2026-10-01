# Terminal Mode (TUI + CLI-only) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Full interactive use of ninox from a terminal — a ratatui TUI plus `list` / `connect` / `orchestrate` subcommands — with the existing `--headless` process formalized as an auto-started daemon.

**Architecture:** The Iced GUI is one view over a multi-process SQLite store and a private tmux server; this adds a second, terminal view over the same seams. New subcommands follow the existing short-circuit pattern in `main.rs`. The TUI is a *client*: it never hosts the poller/server itself — it calls `ensure_daemon()` which auto-starts a detached `ninox --headless` when nothing is listening on the configured port.

**Tech Stack:** Rust workspace (ninox-core / ninox-app / ninox-server), ratatui + crossterm (new deps), clap, rusqlite store, tmux `-L ninox` private socket.

**Spec:** `docs/superpowers/specs/2026-09-14-terminal-mode-design.md`

## Global Constraints

- Never run `cargo fmt` wholesale (tree is not rustfmt-clean).
- Lint gate: `cargo clippy --workspace --all-targets`; test gate: `cargo test --workspace`.
- Store writes that span an `.await` use the read→apply→write pattern (re-read the row after awaits, apply the mutation, write that) — never write a stale snapshot.
- Hot-path subcommands short-circuit in `main.rs` **before** `tmux::write_server_config()` / `hooks::install_wrappers()` / `install_self_shim()` (the block at `main.rs:368-379`).
- No new capability-registry entries: `list`/`connect`/`orchestrate`/`tui` are human-facing.
- Conventional commits; end commit messages with `Co-Authored-By Claude <noreply@anthropic.com>`.
- `ninox list --prs` output must remain byte-identical to today.

---

### Task 1: Move orchestrator-root setup from ninox-app to ninox-core

Mechanical move; the existing test suite is the gate. `setup_orchestrator_root` and `seed_orchestrator_skills` (`crates/ninox-app/src/app.rs:3363-3507`) have no Iced dependencies and are needed by later tasks conceptually owned by core.

**Files:**
- Create: `crates/ninox-core/src/orchestrator_root.rs`
- Modify: `crates/ninox-core/src/lib.rs` (add `pub mod orchestrator_root;`)
- Modify: `crates/ninox-app/src/app.rs` (delete the two functions + their `#[cfg(test)]` seeding tests; they move wholesale)
- Modify: `crates/ninox-app/src/main.rs:1818` and `crates/ninox-app/src/main.rs:1014` (call sites)

**Interfaces:**
- Produces: `ninox_core::orchestrator_root::setup_orchestrator_root(root: &std::path::Path, config: &AppConfig, ninox_bin: &str, config_path: &str) -> anyhow::Result<()>` — signature unchanged from today's `app::setup_orchestrator_root`.
- `seed_orchestrator_skills` stays private to the new module (nothing else calls it).

- [ ] **Step 1: Locate every mover and its tests**

Run: `grep -n "setup_orchestrator_root\|seed_orchestrator_skills" -r crates/`
Expected: definitions in `app.rs`, call sites in `main.rs` (x2, in `run_tui` and `run_spawn_orchestrator`), plus the seeding test module in `app.rs` (search for `seed_orchestrator_skills` inside `#[cfg(test)]`).

- [ ] **Step 2: Move the code**

Create `crates/ninox-core/src/orchestrator_root.rs` containing, verbatim from `app.rs`: `seed_orchestrator_skills` (private) and `setup_orchestrator_root` (pub), plus their doc comments and the seeding `#[cfg(test)]` tests. Fix paths: inside core, `ninox_core::capabilities` becomes `crate::capabilities`, `AppConfig` imports from `crate::config::AppConfig`. Add `pub mod orchestrator_root;` to `crates/ninox-core/src/lib.rs`. Delete the moved code from `app.rs`. Update both call sites in `main.rs` from `app::setup_orchestrator_root(...)` to `ninox_core::orchestrator_root::setup_orchestrator_root(...)`; leave a `pub use ninox_core::orchestrator_root::setup_orchestrator_root;` re-export in `app.rs` ONLY if other app.rs internals still reference it (check with grep; if nothing does, no re-export).

- [ ] **Step 3: Verify**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets`
Expected: PASS, no new warnings. The capability-registry invariant tests (in `capabilities.rs` and the moved seeding tests) all still pass.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "refactor(core): move orchestrator-root setup out of the app crate

Co-Authored-By Claude <noreply@anthropic.com>"
```

---

### Task 2: Move dead-session reconciliation into the poller

Headless mode currently never reconciles sessions whose tmux pane died with the server (reboot) — they stick in `Working`. The sweep runs only in the GUI's `App::new` startup task (`app.rs:851-894`). Move it to `Poller::start` so every host of the poller (GUI, headless daemon) gets it.

**Files:**
- Modify: `crates/ninox-core/src/lifecycle/poller.rs` (add `reconcile_dead_sessions`, call it once before the loop in `start`; move `reconciled_status_for_dead_session` here from `app.rs:476-485` with its doc comment and unit tests)
- Modify: `crates/ninox-app/src/app.rs` (delete `reconciled_status_for_dead_session` + its tests + the startup reconciliation `Task::future` block at `app.rs:851-894`; replace the block with `let task = Task::none();`)
- Modify: `crates/ninox-core/src/types.rs` (the `Interrupted` doc comment says "only the startup reconciliation in `app.rs` assigns it" — update to "only the poller's startup reconciliation assigns it")
- Test: in `poller.rs` test module

**Interfaces:**
- Produces: `Poller::reconcile_dead_sessions(&self)` (private async method) and `pub(crate) fn reconciled_status_for_dead_session(claude_session_id: &Option<String>, has_resume_args: bool) -> SessionStatus` in `poller.rs`.

- [ ] **Step 1: Write the failing test**

In `poller.rs`'s existing `#[cfg(test)]` module (follow the file's existing test-fixture pattern for building a `Poller` over a temp `Store` — look at how existing poller tests construct one), add:

```rust
#[tokio::test]
async fn reconcile_marks_dead_session_interrupted_when_resumable() {
    // Session in Working state, a claude_session_id, and no live tmux
    // session behind it (tests never create real tmux sessions on the
    // private socket, so has_session() is false).
    let (poller, store) = test_poller(); // reuse/extract the file's fixture
    let mut s = test_session("w1");      // reuse the file's session fixture
    s.status = SessionStatus::Working;
    s.claude_session_id = Some("abc".into());
    s.agent_type = "claude-code".into(); // default harness: has resume args
    store.upsert_session(&s).unwrap();

    poller.reconcile_dead_sessions().await;

    let after = store.get_session("w1").unwrap().unwrap();
    assert_eq!(after.status, SessionStatus::Interrupted);
}

#[tokio::test]
async fn reconcile_skips_terminal_sessions() {
    let (poller, store) = test_poller();
    let mut s = test_session("w2");
    s.status = SessionStatus::Done;
    store.upsert_session(&s).unwrap();

    poller.reconcile_dead_sessions().await;

    assert_eq!(store.get_session("w2").unwrap().unwrap().status, SessionStatus::Done);
}
```

Also move the existing unit tests of `reconciled_status_for_dead_session` from `app.rs` into this module unchanged.

- [ ] **Step 2: Run tests to verify failure**

Run: `cargo test -p ninox-core reconcile -- --nocapture`
Expected: FAIL — `reconcile_dead_sessions` not defined.

- [ ] **Step 3: Implement**

In `poller.rs`, port the logic from `app.rs:857-894` as a method:

```rust
/// One-shot startup sweep: any non-terminal session whose tmux session no
/// longer exists lost its pane to a tmux-server death (reboot). Mark it
/// Interrupted when its harness can --resume it, Terminated otherwise.
/// Runs before the first poll_pids tick: poll_pids only checks pid
/// liveness and would mark these Terminated, destroying resumability.
async fn reconcile_dead_sessions(&self) {
    let Ok(sessions) = self.engine.store.list_sessions() else { return };
    let registry = AppConfig::load().unwrap_or_default().registry();
    for session in sessions {
        if session.status.is_terminal() {
            continue;
        }
        if !crate::tmux::has_session(&session.id).await {
            let agent = crate::config::AgentConfig {
                harness: session.agent_type.clone(),
                model:   session.model.clone(),
            };
            let has_resume = registry.resume_cmd(&agent, "placeholder").is_some();
            // Read→apply→write: re-read after the await above.
            let Ok(Some(mut live)) = self.engine.store.get_session(&session.id) else { continue };
            if live.status.is_terminal() {
                continue;
            }
            live.status = reconciled_status_for_dead_session(&live.claude_session_id, has_resume);
            if self.engine.store.upsert_session(&live).is_ok() {
                self.engine.emit(Event::SessionUpdated(live, SessionFields::STATUS));
            }
        }
    }
}
```

(Check `is_terminal()` covers exactly `Done | Terminated | Interrupted` — it does, per `types.rs:22`. Import `SessionFields` the way the rest of poller.rs does.)

At the top of `Poller::start` (`poller.rs:157`), before the interval declarations, add:

```rust
self.reconcile_dead_sessions().await;
```

Move `reconciled_status_for_dead_session` (verbatim, doc comment included) from `app.rs` into `poller.rs`. Delete the `app.rs` startup task block and the now-unused function; keep `App::new`'s return shape by substituting `Task::none()`.

- [ ] **Step 4: Run tests**

Run: `cargo test --workspace`
Expected: PASS (including the moved unit tests).

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "fix(lifecycle): reconcile dead sessions in the poller, not just GUI startup

Co-Authored-By Claude <noreply@anthropic.com>"
```

---

### Task 3: `ninox list` — session board (and keep `--prs` intact)

**Files:**
- Modify: `crates/ninox-app/src/main.rs` (extend `Command::List`, split the short-circuit guard, add `run_list_sessions`)
- Test: `main.rs` test modules (the file already hosts `#[cfg(test)]` modules, e.g. `worker_env_tests` at `main.rs:1981`)

**Interfaces:**
- Consumes: `Store::list_sessions() -> Result<Vec<Session>>`, `Store::list_orchestrators() -> Result<Vec<Orchestrator>>` (store.rs:137/607).
- Produces: `fn run_list_sessions(store: &Store, json: bool) -> anyhow::Result<String>` — later tasks (TUI) reuse its grouping helper `fn group_sessions(sessions: Vec<Session>, orchestrators: Vec<Orchestrator>) -> Vec<(Option<Orchestrator>, Vec<Session>)>`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod list_sessions_tests {
    use super::{group_sessions, render_session_board};
    use ninox_core::types::{Orchestrator, Session, SessionStatus};

    fn session(id: &str, orch: Option<&str>, status: SessionStatus) -> Session {
        Session {
            id: id.into(),
            orchestrator_id: orch.map(String::from),
            name: id.into(),
            repo: "owner/repo".into(),
            status,
            agent_type: "claude-code".into(),
            cost_usd: 1.5,
            started_at: 0,
            pr_number: Some(42),
            pr_id: None,
            workspace_path: None,
            pid: None,
            model: None,
            context_tokens: None,
            catalogue_path: None,
            context_used_pct: None,
            context_total_tokens: None,
            context_window_size: None,
            claude_session_id: None,
            summary: None,
            terminal_at: None,
            gate_status: None,
        }
    }

    #[test]
    fn workers_group_under_their_orchestrator() {
        let orch = Orchestrator { id: "boss".into(), name: "Boss".into(), created_at: 0 };
        // The orchestrator's own session row shares its id.
        let rows = group_sessions(
            vec![
                session("boss", None, SessionStatus::Working),
                session("w1", Some("boss"), SessionStatus::Working),
                session("stray", Some("gone-orch"), SessionStatus::Terminated),
            ],
            vec![orch],
        );
        assert_eq!(rows.len(), 2); // boss group + ungrouped
        assert_eq!(rows[0].0.as_ref().unwrap().id, "boss");
        assert_eq!(rows[0].1.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), vec!["boss", "w1"]);
        assert!(rows[1].0.is_none());
        assert_eq!(rows[1].1[0].id, "stray");
    }

    #[test]
    fn board_renders_status_repo_pr_and_cost() {
        let orch = Orchestrator { id: "boss".into(), name: "Boss".into(), created_at: 0 };
        let out = render_session_board(&group_sessions(
            vec![
                session("boss", None, SessionStatus::Working),
                session("w1", Some("boss"), SessionStatus::PrOpen),
            ],
            vec![orch],
        ));
        assert!(out.contains("boss"), "{out}");
        assert!(out.contains("w1"), "{out}");
        assert!(out.contains("PR #42"), "{out}");
        assert!(out.contains("$1.50"), "{out}");
        assert!(out.contains("pr-open"), "{out}");
    }

    #[test]
    fn empty_store_prints_hint() {
        let out = render_session_board(&group_sessions(vec![], vec![]));
        assert!(out.contains("no sessions"), "{out}");
        assert!(out.contains("ninox orchestrate"), "{out}");
    }
}
```

- [ ] **Step 2: Run tests to verify failure**

Run: `cargo test -p ninox-app list_sessions -- --nocapture`
Expected: FAIL — `group_sessions` / `render_session_board` not defined.

- [ ] **Step 3: Implement**

In `main.rs`:

1. Extend the variant:

```rust
    /// List sessions (default) or watched resources
    List {
        /// List active PR watches instead of sessions
        #[arg(long)]
        prs: bool,
        /// Emit JSON instead of the session board
        #[arg(long)]
        json: bool,
    },
```

2. In the `Open`/`Close`/`List` short-circuit block (`main.rs:321-341`), route `List` by flag — `--prs` keeps its exact current path and output; otherwise:

```rust
            Command::List { prs: false, json } => {
                println!("{}", run_list_sessions(&store, json)?);
                return Ok(());
            }
```

(Restructure the block so `Open`/`Close`/`List{prs:true}` still flow into `run_pr_watch` exactly as before. The `anyhow::ensure!(prs, ...)` guard is deleted — bare `list` is now the session board.)

3. Implement the functions:

```rust
fn group_sessions(
    sessions: Vec<Session>,
    orchestrators: Vec<Orchestrator>,
) -> Vec<(Option<Orchestrator>, Vec<Session>)> {
    let mut groups: Vec<(Option<Orchestrator>, Vec<Session>)> = orchestrators
        .into_iter()
        .map(|o| (Some(o), Vec::new()))
        .collect();
    let mut ungrouped: Vec<Session> = Vec::new();
    for s in sessions {
        // An orchestrator's own session row shares its id; a worker points
        // at its orchestrator via orchestrator_id.
        let owner = s.orchestrator_id.as_deref().unwrap_or(&s.id).to_string();
        match groups.iter_mut().find(|(o, _)| o.as_ref().is_some_and(|o| o.id == owner)) {
            Some((_, members)) => members.push(s),
            None => ungrouped.push(s),
        }
    }
    // Orchestrator's own row first within its group.
    for (o, members) in &mut groups {
        let oid = o.as_ref().map(|o| o.id.clone()).unwrap_or_default();
        members.sort_by_key(|s| (s.id != oid, s.started_at));
    }
    if !ungrouped.is_empty() {
        groups.push((None, ungrouped));
    }
    groups
}

fn render_session_board(groups: &[(Option<Orchestrator>, Vec<Session>)]) -> String {
    if groups.iter().all(|(_, m)| m.is_empty()) {
        return "no sessions — start one with `ninox orchestrate <name>`".to_string();
    }
    let mut out = String::new();
    for (orch, members) in groups {
        match orch {
            Some(o) => out.push_str(&format!("{} ({})\n", o.name, o.id)),
            None    => out.push_str("(no orchestrator)\n"),
        }
        for s in members {
            let pr = s.pr_number.map(|n| format!("PR #{n}")).unwrap_or_default();
            out.push_str(&format!(
                "  {:<24} {:<10} {:<20} {:<8} ${:.2}\n",
                s.id, status_slug(&s.status), s.repo, pr, s.cost_usd,
            ));
        }
    }
    out
}

fn status_slug(s: &SessionStatus) -> &'static str {
    match s {
        SessionStatus::Spawning      => "spawning",
        SessionStatus::Working       => "working",
        SessionStatus::PrOpen        => "pr-open",
        SessionStatus::CiFailed      => "ci-failed",
        SessionStatus::ReviewPending => "review",
        SessionStatus::Mergeable     => "mergeable",
        SessionStatus::Done          => "done",
        SessionStatus::Terminated    => "terminated",
        SessionStatus::Interrupted   => "interrupted",
    }
}

fn run_list_sessions(store: &Store, json: bool) -> anyhow::Result<String> {
    let sessions = store.list_sessions()?;
    let orchestrators = store.list_orchestrators()?;
    if json {
        return Ok(serde_json::to_string_pretty(&serde_json::json!({
            "orchestrators": orchestrators,
            "sessions": sessions,
        }))?);
    }
    Ok(render_session_board(&group_sessions(sessions, orchestrators)))
}
```

(If `SessionStatus` already has a display/serde slug, use that instead of `status_slug` and adjust the test's expected string to match — do not invent a second spelling of an existing name.)

- [ ] **Step 4: Run tests**

Run: `cargo test -p ninox-app list_sessions` then `cargo test --workspace`
Expected: PASS. Manually verify: `cargo run -- list` on a machine with sessions shows the board; `cargo run -- list --prs` output unchanged.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(cli): ninox list shows the session board (--prs unchanged)

Co-Authored-By Claude <noreply@anthropic.com>"
```

---

### Task 4: `ninox connect <session-id>`

**Files:**
- Create: `crates/ninox-app/src/connect.rs` (`ConnectPlan`, `connect_preflight`, `exec_attach` — a module from the start; `main.rs` is already 2500 lines and Task 8's TUI consumes this too)
- Modify: `crates/ninox-app/src/main.rs` (add `mod connect;`, new `Command::Connect`, short-circuit arm, `run_connect`)
- Test: `connect.rs` test module

**Interfaces:**
- Consumes: `tmux::has_session(id) -> bool`, `tmux::attach_args(id) -> Vec<String>` (tmux.rs:334/390); `Store::get_session` / `upsert_session` / `list_sessions`.
- Produces: `enum ConnectPlan { Attach(Vec<String>), DeadMarked(String), NotFound { suggestions: Vec<String> } }` and `async fn connect_preflight(store: &Store, id: &str) -> anyhow::Result<ConnectPlan>` — Task 8 (TUI connect) reuses `connect_preflight`.

- [ ] **Step 1: Write the failing tests**

`connect_preflight` holds all the logic; the `exec` wrapper stays untested (it replaces the process).

```rust
#[cfg(test)]
mod connect_tests {
    use super::{connect_preflight, ConnectPlan};
    use ninox_core::{store::Store, types::SessionStatus};

    fn temp_store() -> Store {
        let dir = std::env::temp_dir().join(format!("ninox-connect-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Store::open(dir.join(format!("t-{}.db", rand_suffix()))).unwrap()
    }
    fn rand_suffix() -> u128 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    }

    #[tokio::test]
    async fn unknown_id_suggests_prefix_matches() {
        let store = temp_store();
        store.upsert_session(&crate::test_fixtures::session("fix-login-flow", None, SessionStatus::Working)).unwrap();
        let ConnectPlan::NotFound { suggestions } = connect_preflight(&store, "fix").await.unwrap() else {
            panic!("expected NotFound");
        };
        assert_eq!(suggestions, vec!["fix-login-flow"]);
    }

    #[tokio::test]
    async fn dead_session_is_marked_terminated() {
        let store = temp_store();
        // No tmux session exists on the private socket for this id, so
        // has_session() is false and connect must mark it Terminated.
        let s = crate::test_fixtures::session("ghost", None, SessionStatus::Working);
        store.upsert_session(&s).unwrap();
        let ConnectPlan::DeadMarked(id) = connect_preflight(&store, "ghost").await.unwrap() else {
            panic!("expected DeadMarked");
        };
        assert_eq!(id, "ghost");
        assert_eq!(store.get_session("ghost").unwrap().unwrap().status, SessionStatus::Terminated);
    }
}
```

(Extract Task 3's `session(...)` builder into a shared `#[cfg(test)] pub(crate) mod test_fixtures` in `main.rs` and have both test modules use it — do not copy the 20-field struct literal twice. Task 3's fixture sets `claude_session_id: None`, which is what "not resumable → Terminated" relies on here.)

- [ ] **Step 2: Run tests to verify failure**

Run: `cargo test -p ninox-app connect_tests`
Expected: FAIL — `connect_preflight` not defined.

- [ ] **Step 3: Implement**

Add the variant:

```rust
    /// Attach your terminal to a running session (worker or orchestrator).
    /// Detach with tmux's detach key (default: C-b d) to return to your shell.
    Connect {
        /// Session ID (see `ninox list`)
        session_id: String,
    },
```

Short-circuit before the heavy setup (needs only `Store` + tmux, exactly like Open/Close/List — extend that guard's `matches!` and dispatch):

```rust
async fn connect_preflight(store: &Store, id: &str) -> anyhow::Result<ConnectPlan> {
    let Some(_) = store.get_session(id)? else {
        let suggestions = store
            .list_sessions()?
            .into_iter()
            .filter(|s| s.id.starts_with(id))
            .map(|s| s.id)
            .collect();
        return Ok(ConnectPlan::NotFound { suggestions });
    };
    if !tmux::has_session(id).await {
        // Read→apply→write across the await above; never clobber terminal.
        if let Some(mut live) = store.get_session(id)? {
            if !live.status.is_terminal() {
                live.status = SessionStatus::Terminated;
                store.upsert_session(&live)?;
            }
        }
        return Ok(ConnectPlan::DeadMarked(id.to_string()));
    }
    Ok(ConnectPlan::Attach(tmux::attach_args(id).await))
}

async fn run_connect(store: &Store, id: &str) -> anyhow::Result<()> {
    match connect_preflight(store, id).await? {
        ConnectPlan::Attach(argv) => exec_attach(argv),
        ConnectPlan::DeadMarked(id) => {
            anyhow::bail!("session {id} has no live tmux session — marked terminated")
        }
        ConnectPlan::NotFound { suggestions } if suggestions.is_empty() => {
            anyhow::bail!("no session named {id} — run `ninox list`")
        }
        ConnectPlan::NotFound { suggestions } => {
            anyhow::bail!("no session named {id} — did you mean: {}", suggestions.join(", "))
        }
    }
}

fn exec_attach(argv: Vec<String>) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(&argv[0]).args(&argv[1..]).exec();
        Err(anyhow::anyhow!("failed to exec {}: {err}", argv[0]))
    }
    #[cfg(not(unix))]
    {
        let status = std::process::Command::new(&argv[0]).args(&argv[1..]).status()?;
        anyhow::ensure!(status.success(), "tmux attach exited with {status}");
        Ok(())
    }
}
```

Note the terminal-status handling difference from the GUI's NavigateSession: we intentionally do **not** resurrect anything, and we never overwrite a status that is already terminal.

- [ ] **Step 4: Run tests + manual check**

Run: `cargo test -p ninox-app connect_tests && cargo test --workspace`
Expected: PASS. Manual: `cargo run -- connect <live-session>` lands in tmux; detach returns to shell; `cargo run -- connect nope` prints the suggestion line and exits non-zero.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(cli): ninox connect attaches the terminal to a session

Co-Authored-By Claude <noreply@anthropic.com>"
```

---

### Task 5: `ensure_daemon()` in ninox-core

**Files:**
- Create: `crates/ninox-core/src/daemon.rs`
- Modify: `crates/ninox-core/src/lib.rs` (add `pub mod daemon;`)
- Test: inside `daemon.rs`

**Interfaces:**
- Produces:
  - `pub async fn port_in_use(port: u16) -> bool`
  - `pub enum DaemonStatus { AlreadyRunning, Started, Failed(String) }`
  - `pub async fn ensure_daemon(port: u16, ninox_bin: &std::path::Path) -> DaemonStatus`
- Consumed by: Task 6 (`orchestrate`) and Task 7 (TUI startup).

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::port_in_use;

    #[tokio::test]
    async fn detects_listener() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(port_in_use(port).await);
    }

    #[tokio::test]
    async fn detects_free_port() {
        // Bind then drop to get a port that was just proven free.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(!port_in_use(port).await);
    }
}
```

- [ ] **Step 2: Run tests to verify failure**

Run: `cargo test -p ninox-core daemon`
Expected: FAIL — module doesn't exist.

- [ ] **Step 3: Implement**

```rust
//! Auto-start for the background services. The process running
//! `ninox --headless` (poller + ninox-server) is the daemon; the TCP bind
//! on the configured port is both the liveness probe and the lock — the
//! GUI holds it too, so "something is listening" always means "the
//! services are running" regardless of which frontend hosts them.

use std::path::Path;
use std::time::Duration;

pub enum DaemonStatus {
    AlreadyRunning,
    Started,
    /// Spawning or the post-spawn port wait failed; callers warn and
    /// continue (store reads still work, statuses may go stale).
    Failed(String),
}

pub async fn port_in_use(port: u16) -> bool {
    matches!(
        tokio::time::timeout(
            Duration::from_millis(500),
            tokio::net::TcpStream::connect(("127.0.0.1", port)),
        )
        .await,
        Ok(Ok(_))
    )
}

pub async fn ensure_daemon(port: u16, ninox_bin: &Path) -> DaemonStatus {
    if port_in_use(port).await {
        return DaemonStatus::AlreadyRunning;
    }
    let log_dir = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("ninox");
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        return DaemonStatus::Failed(format!("create log dir: {e}"));
    }
    let log = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("daemon.log"))
    {
        Ok(f) => f,
        Err(e) => return DaemonStatus::Failed(format!("open daemon.log: {e}")),
    };
    let mut cmd = std::process::Command::new(ninox_bin);
    cmd.arg("--headless")
        .arg("--port").arg(port.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().map(std::process::Stdio::from).unwrap_or_else(|_| std::process::Stdio::null()))
        .stderr(std::process::Stdio::from(log));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0); // survive the parent's terminal/session
    }
    if let Err(e) = cmd.spawn() {
        return DaemonStatus::Failed(format!("spawn ninox --headless: {e}"));
    }
    // ninox --headless installs wrappers and seeds the orchestrator root
    // before binding, so give it a generous window.
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if port_in_use(port).await {
            return DaemonStatus::Started;
        }
    }
    DaemonStatus::Failed("daemon did not start listening within 10s".into())
}
```

(`ensure_daemon`'s spawn path is deliberately not unit-tested — forking real daemons in tests is worse than the risk; `port_in_use` carries the logic. Manual verification in Step 4.)

- [ ] **Step 4: Run tests + manual check**

Run: `cargo test -p ninox-core daemon && cargo clippy --workspace --all-targets`
Expected: PASS. Manual: with nothing running, a small `main`-less check isn't possible — verified end-to-end in Task 6/7.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(core): ensure_daemon auto-starts the headless services

Co-Authored-By Claude <noreply@anthropic.com>"
```

---

### Task 6: `ninox orchestrate` — shared spawn body + attach

**Files:**
- Modify: `crates/ninox-app/src/main.rs` (extract `spawn_orchestrator_common` from `run_spawn_orchestrator:974-1114`, add `Command::Orchestrate` + `run_orchestrate`)
- Test: `main.rs` (extraction is verified by existing behavior; new logic is thin)

**Interfaces:**
- Consumes: `ensure_daemon` (Task 5), `exec_attach` (Task 4), `ninox_core::orchestrator_root::setup_orchestrator_root` (Task 1).
- Produces: `struct SpawnedOrchestrator { id: String, workspace: String }` and `async fn spawn_orchestrator_common(store: &Store, config: &AppConfig, name: &str, prompt: Option<String>) -> anyhow::Result<SpawnedOrchestrator>` — Task 9's TUI spawn reuses it.

- [ ] **Step 1: Extract the shared body**

Refactor `run_spawn_orchestrator` so everything between the duplicate-name guard and the brief delivery (inclusive) lives in `spawn_orchestrator_common`: slugify + duplicate guard, `setup_orchestrator_root`, workspace `create_dir_all`, both store rows, `tmux::create_session` with `orchestrator_env_vars`, the rollback-on-tmux-failure (delete both rows), `wait_for_input_prompt` + `deliver_message` for the brief when `prompt.is_some()`. Return `SpawnedOrchestrator { id, workspace }`. `run_spawn_orchestrator` becomes: check `--user-requested`, call common, print the exact same messages as today (`spawned orchestrator {id}` / `send it a brief with: ...`).

Behavior note: today the "spawned orchestrator {id}" line prints *before* brief delivery; keep the printing in the callers (print after `spawn_orchestrator_common` returns) — the observable difference is line ordering relative to the brief-delivery warnings, which is acceptable; the warning text itself must not change.

- [ ] **Step 2: Verify the extraction is behavior-neutral**

Run: `cargo test --workspace`
Expected: PASS. Manual: `cargo run -- spawn-orchestrator --name test-extract --user-requested` still spawns and prints the same lines; clean up with the GUI or `tmux -L ninox kill-session -t test-extract` plus store rows via `ninox list` sanity check.

- [ ] **Step 3: Add the user-facing command**

```rust
    /// Start a new orchestrator and attach your terminal to it.
    Orchestrate {
        /// Display name; slugified into the session ID
        name: String,
        /// Initial brief, delivered before attaching
        #[arg(long, short)]
        prompt: Option<String>,
        /// Print the workspace directory and a connect hint instead of attaching
        #[arg(long)]
        no_attach: bool,
    },
```

Dispatch in the full-setup `match` (it needs the wrapper/shim setup exactly like `SpawnOrchestrator`):

```rust
        Some(Command::Orchestrate { name, prompt, no_attach }) => {
            let config = AppConfig::load().unwrap_or_default();
            run_orchestrate(store, config, name, prompt, no_attach).await
        }
```

```rust
async fn run_orchestrate(
    store: Arc<Store>,
    config: AppConfig,
    name: String,
    prompt: Option<String>,
    no_attach: bool,
) -> anyhow::Result<()> {
    // Sessions outlive this command; make sure the poller/services do too.
    if let Ok(exe) = std::env::current_exe() {
        match ninox_core::daemon::ensure_daemon(config.port, &exe).await {
            ninox_core::daemon::DaemonStatus::Failed(e) => {
                eprintln!("warning: background services not running ({e}) — statuses may go stale");
            }
            _ => {}
        }
    }
    let spawned = spawn_orchestrator_common(&store, &config, &name, prompt).await?;
    println!("spawned orchestrator {}", spawned.id);
    if no_attach {
        println!("dir: {}", spawned.workspace);
        println!("connect with: ninox connect {}", spawned.id);
        return Ok(());
    }
    exec_attach(tmux::attach_args(&spawned.id).await)
}
```

- [ ] **Step 4: Verify**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets`
Manual: `cargo run -- orchestrate demo-cli --no-attach` prints dir + hint; `cargo run -- orchestrate demo-cli2` lands inside the new session (detach with `C-b d`); a `ninox --headless` process appears when nothing was listening (`ps aux | grep 'ninox --headless'`, `lsof -i :<port>`).

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(cli): ninox orchestrate spawns and attaches an orchestrator

Co-Authored-By Claude <noreply@anthropic.com>"
```

---

### Task 7: TUI scaffolding — deps, state model, event loop, session board

**Files:**
- Modify: `Cargo.toml` (workspace deps: `ratatui = "0.29"`, `crossterm = "0.28"`)
- Modify: `crates/ninox-app/Cargo.toml` (add both, `workspace = true`)
- Create: `crates/ninox-app/src/tui/mod.rs` (terminal setup/teardown, run loop, draw)
- Create: `crates/ninox-app/src/tui/state.rs` (pure state + key handling — all the testable logic)
- Modify: `crates/ninox-app/src/main.rs` (add `mod tui;`)
- Test: `tui/state.rs`

**Interfaces:**
- Consumes: `group_sessions` from Task 3 (move it — and `status_slug`, `render_session_board` — into a `board.rs` shared location or make them `pub(crate)` in `main.rs`; the TUI needs `group_sessions` + `status_slug` only).
- Produces:
  - `tui::run(store: Arc<Store>, port: u16) -> anyhow::Result<()>` — Task 10 wires it to `ninox tui`.
  - `state::TuiState`, `state::Action`, `state::handle_key(&mut TuiState, KeyEvent) -> Action`.

- [ ] **Step 1: Write the failing state tests**

```rust
// tui/state.rs
#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(c: char) -> KeyEvent { KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE) }
    fn state_with_rows(n: usize) -> TuiState {
        let mut st = TuiState::default();
        st.rows = (0..n).map(|i| Row::test_row(&format!("s{i}"))).collect();
        st
    }

    #[test]
    fn j_and_k_move_selection_within_bounds() {
        let mut st = state_with_rows(3);
        assert_eq!(st.selected, 0);
        handle_key(&mut st, key('j'));
        assert_eq!(st.selected, 1);
        handle_key(&mut st, key('k'));
        handle_key(&mut st, key('k')); // clamped at 0
        assert_eq!(st.selected, 0);
    }

    #[test]
    fn enter_on_a_row_requests_connect() {
        let mut st = state_with_rows(2);
        st.selected = 1;
        let a = handle_key(&mut st, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(a, Action::Connect("s1".into()));
    }

    #[test]
    fn kill_requires_confirmation() {
        // 'k' is vim-up, so kill is bound to 'd' — this test pins that map.
        let mut st = state_with_rows(1);
        let a = handle_key(&mut st, key('d'));
        assert_eq!(a, Action::None);
        assert!(matches!(st.confirm, Some(Pending::Kill(ref id)) if id == "s0"));
        let a = handle_key(&mut st, key('y'));
        assert_eq!(a, Action::Kill("s0".into()));
        assert!(st.confirm.is_none());
    }

    #[test]
    fn q_quits_unless_confirming() {
        let mut st = state_with_rows(1);
        handle_key(&mut st, key('d')); // open confirm
        assert_eq!(handle_key(&mut st, key('n')), Action::None); // declines
        assert!(st.confirm.is_none());
        assert_eq!(handle_key(&mut st, key('q')), Action::Quit);
    }
}
```

**Keymap (final — the spec's `k` for kill collides with vim-style up; use `d`):** `j`/`↓` down, `k`/`↑` up, `Enter`/`c` connect, `n` new orchestrator, `d` kill (confirm `y`/`n`), `x` reap finished workers of selected orchestrator (confirm), `p` toggle PR-watches view, `q`/`Esc` quit (Esc first closes any open confirm/modal).

- [ ] **Step 2: Run tests to verify failure**

Run: `cargo test -p ninox-app tui::state`
Expected: FAIL — module doesn't exist (after adding deps + `mod tui;`, compile error first, then test failures).

- [ ] **Step 3: Implement state.rs**

```rust
use crossterm::event::{KeyCode, KeyEvent};

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub id: String,
    pub is_orchestrator: bool,
    pub name: String,
    pub status: &'static str,
    pub repo: String,
    pub pr: Option<u64>,
    pub cost_usd: f64,
    pub context_used_pct: Option<f64>,
}

impl Row {
    #[cfg(test)]
    pub fn test_row(id: &str) -> Self {
        Self { id: id.into(), is_orchestrator: false, name: id.into(), status: "working",
               repo: String::new(), pr: None, cost_usd: 0.0, context_used_pct: None }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Pending { Kill(String), Reap(String) }

#[derive(Clone, Debug, PartialEq)]
pub enum View { Board, PrWatches }

#[derive(Default)]
pub struct TuiState {
    pub rows: Vec<Row>,
    pub selected: usize,
    pub confirm: Option<Pending>,
    pub view: View,          // impl Default → Board
    pub spawn_modal: Option<SpawnModal>, // Task 9
    pub daemon_up: bool,
    pub status_line: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    None,
    Quit,
    Connect(String),
    Kill(String),
    Reap(String),
    OpenSpawnModal,
    Spawn { name: String, prompt: Option<String> }, // emitted by modal (Task 9)
}

pub fn handle_key(st: &mut TuiState, key: KeyEvent) -> Action {
    // Modal captures input first (Task 9 fills this in).
    if st.spawn_modal.is_some() {
        return handle_spawn_modal_key(st, key);
    }
    if let Some(pending) = st.confirm.clone() {
        return match key.code {
            KeyCode::Char('y') => {
                st.confirm = None;
                match pending { Pending::Kill(id) => Action::Kill(id), Pending::Reap(id) => Action::Reap(id) }
            }
            KeyCode::Char('n') | KeyCode::Esc => { st.confirm = None; Action::None }
            _ => Action::None,
        };
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
        KeyCode::Char('j') | KeyCode::Down => { st.selected = (st.selected + 1).min(st.rows.len().saturating_sub(1)); Action::None }
        KeyCode::Char('k') | KeyCode::Up   => { st.selected = st.selected.saturating_sub(1); Action::None }
        KeyCode::Enter | KeyCode::Char('c') => selected_id(st).map(Action::Connect).unwrap_or(Action::None),
        KeyCode::Char('d') => { if let Some(id) = selected_id(st) { st.confirm = Some(Pending::Kill(id)); } Action::None }
        KeyCode::Char('x') => { if let Some(id) = selected_orchestrator_id(st) { st.confirm = Some(Pending::Reap(id)); } Action::None }
        KeyCode::Char('n') => Action::OpenSpawnModal,
        KeyCode::Char('p') => { st.view = if st.view == View::PrWatches { View::Board } else { View::PrWatches }; Action::None }
        _ => Action::None,
    }
}

fn selected_id(st: &TuiState) -> Option<String> {
    st.rows.get(st.selected).map(|r| r.id.clone())
}

/// The orchestrator that owns the selected row (itself, if it is one).
fn selected_orchestrator_id(st: &TuiState) -> Option<String> {
    // rows are ordered orchestrator-then-workers (from group_sessions);
    // walk backwards to the nearest orchestrator row.
    let idx = st.selected.min(st.rows.len().checked_sub(1)?);
    st.rows[..=idx].iter().rev().find(|r| r.is_orchestrator).map(|r| r.id.clone())
}
```

(`View` needs a manual `Default` impl returning `Board`. `handle_spawn_modal_key` is a stub returning `Action::None` until Task 9 — with a `// filled in by the spawn-modal task` note only if the stub would otherwise be confusing; prefer landing the real signature here.)

- [ ] **Step 4: Implement mod.rs — terminal lifecycle + loop + draw**

```rust
// tui/mod.rs
pub mod state;

use state::{Action, Row, TuiState, View};
use std::sync::Arc;
use ninox_core::{events::Engine, store::Store};

pub async fn run(store: Arc<Store>, port: u16) -> anyhow::Result<()> {
    let engine = Engine::new(Arc::clone(&store));
    let mut st = TuiState::default();
    st.daemon_up = match ninox_core::daemon::ensure_daemon(
        port,
        &std::env::current_exe()?,
    ).await {
        ninox_core::daemon::DaemonStatus::Failed(e) => { st.status_line = Some(format!("daemon down: {e}")); false }
        _ => true,
    };

    let mut terminal = enter_terminal()?;
    let res = event_loop(&mut terminal, &mut st, &store, &engine).await;
    leave_terminal(&mut terminal)?;
    res
}

fn enter_terminal() -> anyhow::Result<ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>> {
    use crossterm::{execute, terminal::{enable_raw_mode, EnterAlternateScreen}};
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    // A panic must never leave the user's terminal in raw mode.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
        hook(info);
    }));
    Ok(ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(stdout))?)
}

fn leave_terminal(terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>) -> anyhow::Result<()> {
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), crossterm::terminal::LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

async fn event_loop(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    st: &mut TuiState,
    store: &Arc<Store>,
    engine: &Arc<Engine>,
) -> anyhow::Result<()> {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
    let mut events = crossterm::event::EventStream::new();
    use futures_util::StreamExt;
    refresh(st, store);
    loop {
        terminal.draw(|f| draw(f, st))?;
        tokio::select! {
            _ = tick.tick() => refresh(st, store),
            ev = events.next() => {
                let Some(Ok(crossterm::event::Event::Key(key))) = ev else { continue };
                if key.kind != crossterm::event::KeyEventKind::Press { continue; }
                match state::handle_key(st, key) {
                    Action::Quit => return Ok(()),
                    Action::None => {}
                    action => perform(action, terminal, st, store, engine).await?,
                }
            }
        }
    }
}

fn refresh(st: &mut TuiState, store: &Store) {
    let (Ok(sessions), Ok(orchs)) = (store.list_sessions(), store.list_orchestrators()) else { return };
    let groups = crate::group_sessions(sessions, orchs);
    let mut rows = Vec::new();
    for (orch, members) in &groups {
        for s in members {
            rows.push(Row {
                id: s.id.clone(),
                is_orchestrator: orch.as_ref().is_some_and(|o| o.id == s.id),
                name: s.name.clone(),
                status: crate::status_slug(&s.status),
                repo: s.repo.clone(),
                pr: s.pr_number,
                cost_usd: s.cost_usd,
                context_used_pct: s.context_used_pct,
            });
        }
    }
    st.rows = rows;
    st.selected = st.selected.min(st.rows.len().saturating_sub(1));
}
```

`draw` renders: a title bar (`ninox — <n> sessions`, red `daemon down` tag when `!st.daemon_up`, `st.status_line` if set), a `ratatui::widgets::Table` of rows (orchestrator rows bold, workers indented two spaces; columns: id, status, repo, PR, cost, ctx%), a confirm popup (`kill <id>? y/n`) when `st.confirm.is_some()`, and a one-line key legend (`enter connect · n new · d kill · x reap · p prs · q quit`). PR-watches view (`st.view == View::PrWatches`): a table over `store.list_pr_watches()` (repo#n, opener, url). `perform` is a stub in this task: only `Action::Connect` → Task 8, others → Task 9; until then it sets `st.status_line = Some(format!("not implemented: {action:?}"))`.

Add `futures-util = "0.3"` to `ninox-app` deps for `EventStream` (crossterm's `event-stream` feature must be enabled: `crossterm = { workspace = true, features = ["event-stream"] }`).

Rendering smoke test with `TestBackend`:

```rust
#[cfg(test)]
mod render_tests {
    #[test]
    fn draws_without_panicking() {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut st = super::state::TuiState::default();
        st.rows = vec![super::state::Row::test_row("a"), super::state::Row::test_row("b")];
        terminal.draw(|f| super::draw(f, &st)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains('a'));
    }
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p ninox-app tui && cargo clippy --workspace --all-targets`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(tui): ratatui session board with state-driven keymap

Co-Authored-By Claude <noreply@anthropic.com>"
```

---

### Task 8: TUI connect — suspend, attach, resume

**Files:**
- Modify: `crates/ninox-app/src/tui/mod.rs` (`perform`'s `Action::Connect` arm + `attach_suspended`)
- Test: manual (child-process + terminal-mode churn isn't unit-testable; the preflight logic is already covered by Task 4's tests)

**Interfaces:**
- Consumes: `crate::connect::{connect_preflight, ConnectPlan}` from Task 4.

- [ ] **Step 1: Implement**

```rust
async fn perform(/* as in Task 7 */) -> anyhow::Result<()> {
    match action {
        Action::Connect(id) => {
            match crate::connect::connect_preflight(store, &id).await? {
                crate::connect::ConnectPlan::Attach(argv) => attach_suspended(terminal, argv)?,
                crate::connect::ConnectPlan::DeadMarked(id) => {
                    st.status_line = Some(format!("{id} is dead — marked terminated"));
                }
                crate::connect::ConnectPlan::NotFound { .. } => {
                    st.status_line = Some(format!("{id} not found"));
                }
            }
            refresh(st, store);
        }
        // ... Task 9 arms
    }
    Ok(())
}

/// Leave the TUI's terminal modes, run tmux attach as a *child* (not exec —
/// we come back), and restore. The child inherits the real stdio.
fn attach_suspended(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    argv: Vec<String>,
) -> anyhow::Result<()> {
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), crossterm::terminal::LeaveAlternateScreen)?;
    let status = std::process::Command::new(&argv[0]).args(&argv[1..]).status();
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), crossterm::terminal::EnterAlternateScreen)?;
    terminal.clear()?;
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => { tracing::warn!("tmux attach exited {s}"); Ok(()) }
        Err(e) => Err(anyhow::anyhow!("spawn tmux attach: {e}")),
    }
}
```

Note the crossterm `EventStream` from Task 7 stays alive across the suspend — verify manually that keys typed inside tmux do not leak into the TUI after detach (they go to the child because it owns the foreground; if leakage is observed, drain pending events after re-entering with a `while crossterm::event::poll(Duration::ZERO)? { crossterm::event::read()?; }` loop).

- [ ] **Step 2: Manual verification**

Run: `cargo run -- tui` won't exist until Task 10 — for now add a temporary invocation or test via Task 10 done first? No: wire a minimal hidden entry now (`ninox tui` subcommand added in this step, full display-fallback wiring stays in Task 10):

```rust
    /// Open the terminal UI (session board, connect, spawn).
    Tui,
```

Dispatch in the full-setup match:

```rust
        Some(Command::Tui) => {
            let config = AppConfig::load().unwrap_or_default();
            let port = args.port.unwrap_or(config.port);
            tui::run(store, port).await
        }
```

Then manually: `cargo run -- tui`; Enter on a live session lands in tmux; `C-b d` returns to a intact, refreshed TUI; kill the tmux session out from under it and Enter shows the dead-marked status line.

- [ ] **Step 3: Run checks + commit**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets`

```bash
git add -A
git commit -m "feat(tui): connect to sessions via suspend + tmux attach

Co-Authored-By Claude <noreply@anthropic.com>"
```

---

### Task 9: TUI spawn / kill / reap / PR view

**Files:**
- Modify: `crates/ninox-app/src/tui/state.rs` (SpawnModal + `handle_spawn_modal_key` for real)
- Modify: `crates/ninox-app/src/tui/mod.rs` (perform arms, modal + PR-view rendering)
- Test: `tui/state.rs`

**Interfaces:**
- Consumes: `spawn_orchestrator_common` (Task 6 — make it `pub(crate)`), `Engine::terminate_session(&self, session_id: &str)`, `Engine::reap_workers(&self, orchestrator_id: &str, selection: ReapSelection<'_>, force: bool)` (events.rs:461/241 — use `ReapSelection::Finished` or the file's equivalent bulk "finished only" selector; read the enum before coding), `Store::list_pr_watches()`.

- [ ] **Step 1: Write the failing modal tests**

```rust
    #[test]
    fn spawn_modal_collects_name_then_prompt_then_spawns() {
        let mut st = state_with_rows(0);
        handle_key(&mut st, key('n'));      // Action::OpenSpawnModal handled by loop;
        st.spawn_modal = Some(SpawnModal::default()); // loop does this — test simulates
        for c in "demo".chars() { handle_key(&mut st, key(c)); }
        handle_key(&mut st, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)); // name → prompt field
        for c in "do x".chars() { handle_key(&mut st, key(c)); }
        let a = handle_key(&mut st, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(a, Action::Spawn { name: "demo".into(), prompt: Some("do x".into()) });
        assert!(st.spawn_modal.is_none());
    }

    #[test]
    fn spawn_modal_esc_cancels() {
        let mut st = state_with_rows(0);
        st.spawn_modal = Some(SpawnModal::default());
        let a = handle_key(&mut st, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(a, Action::None);
        assert!(st.spawn_modal.is_none());
    }

    #[test]
    fn empty_prompt_becomes_none() {
        let mut st = state_with_rows(0);
        st.spawn_modal = Some(SpawnModal::default());
        for c in "demo".chars() { handle_key(&mut st, key(c)); }
        handle_key(&mut st, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let a = handle_key(&mut st, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(a, Action::Spawn { name: "demo".into(), prompt: None });
    }
```

- [ ] **Step 2: Run to verify failure, then implement state**

```rust
#[derive(Default, Clone, Debug, PartialEq)]
pub struct SpawnModal {
    pub name: String,
    pub prompt: String,
    pub on_prompt_field: bool,
}

pub(super) fn handle_spawn_modal_key(st: &mut TuiState, key: KeyEvent) -> Action {
    let Some(modal) = st.spawn_modal.as_mut() else { return Action::None };
    match key.code {
        KeyCode::Esc => { st.spawn_modal = None; Action::None }
        KeyCode::Enter if !modal.on_prompt_field => { modal.on_prompt_field = true; Action::None }
        KeyCode::Enter => {
            let modal = st.spawn_modal.take().unwrap();
            if modal.name.trim().is_empty() { return Action::None }
            Action::Spawn {
                name: modal.name,
                prompt: Some(modal.prompt).filter(|p| !p.trim().is_empty()),
            }
        }
        KeyCode::Backspace => {
            if modal.on_prompt_field { modal.prompt.pop(); } else { modal.name.pop(); }
            Action::None
        }
        KeyCode::Char(c) => {
            if modal.on_prompt_field { modal.prompt.push(c) } else { modal.name.push(c) }
            Action::None
        }
        _ => Action::None,
    }
}
```

- [ ] **Step 3: Implement the perform arms**

```rust
        Action::OpenSpawnModal => { st.spawn_modal = Some(state::SpawnModal::default()); }
        Action::Spawn { name, prompt } => {
            let config = AppConfig::load().unwrap_or_default();
            st.status_line = Some(format!("spawning {name}…"));
            match crate::spawn_orchestrator_common(store, &config, &name, prompt).await {
                Ok(spawned) => { st.status_line = Some(format!("spawned {}", spawned.id)); refresh(st, store); }
                Err(e) => st.status_line = Some(format!("spawn failed: {e}")),
            }
        }
        Action::Kill(id) => {
            if let Err(e) = engine.terminate_session(&id).await {
                st.status_line = Some(format!("kill failed: {e}"));
            }
            refresh(st, store);
        }
        Action::Reap(orch_id) => {
            match engine.reap_workers(&orch_id, ninox_core::events::ReapSelection::Finished, false).await {
                Ok(outcomes) => st.status_line = Some(format!("reaped {} workers", outcomes.len())),
                Err(e) => st.status_line = Some(format!("reap failed: {e}")),
            }
            refresh(st, store);
        }
```

Rendering: spawn modal as a centered popup with two labeled fields (name / brief), the active one highlighted; PR view already scaffolded in Task 7.

Note `spawn_orchestrator_common` blocks up to 90s on `wait_for_input_prompt` when a brief is given, and the TUI must not freeze for that long: run it via `tokio::spawn`, keep the `JoinHandle` in a `Vec<tokio::task::JoinHandle<anyhow::Result<SpawnedOrchestrator>>>` owned by the event loop, and drain finished handles in the tick arm, setting `st.status_line` from each result.

- [ ] **Step 4: Run tests + manual pass**

Run: `cargo test -p ninox-app tui && cargo test --workspace && cargo clippy --workspace --all-targets`
Manual: `cargo run -- tui`; `n` → type name, Enter, Enter → new orchestrator appears on the board within a tick; `d`+`y` kills it; `x`+`y` on an orchestrator with finished workers reaps them; `p` toggles the PR view.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(tui): spawn, kill, reap, and PR-watch views

Co-Authored-By Claude <noreply@anthropic.com>"
```

---

### Task 10: Entry wiring — display fallback + docs

**Files:**
- Modify: `crates/ninox-app/src/main.rs` (`run_tui` display fallback; `Tui` subcommand landed in Task 8)
- Modify: `README.md` (terminal-mode section)
- Modify: `CLAUDE.md` (hot-path short-circuit list: add `List` sessions / `Connect`)

**Interfaces:**
- Consumes: `tui::run(store, port)` (Task 7), `has_display()` (`main.rs:1974`).

- [ ] **Step 1: Implement the fallback**

In `run_tui` (`main.rs:1805`), before `setup_orchestrator_root` and the service spawns, add:

```rust
    use std::io::IsTerminal;
    // No display but an interactive terminal: open the TUI as a *client*
    // (ensure_daemon inside tui::run starts the services out-of-process, so
    // quitting the TUI doesn't take the poller down with it). Explicit
    // --headless still means "be the daemon" and never opens the TUI.
    if !headless && !has_display() && std::io::stdout().is_terminal() {
        return crate::tui::run(store, port).await;
    }
```

(`port` is already computed at the top of `run_tui`. The existing `if headless || !has_display()` Ctrl-C branch at `main.rs:1862` stays for the non-TTY case and `--headless`.)

- [ ] **Step 2: Verify all entry combinations**

- `cargo run -- tui` → TUI (any platform).
- `cargo run -- --headless` → services + Ctrl-C wait, no TUI (check with `lsof -i :<port>`).
- Linux/SSH (`DISPLAY` unset, TTY): `cargo run` → TUI. Non-TTY (`cargo run < /dev/null | cat`) → headless wait. (macOS `has_display()` is hardcoded `true`, so bare `ninox` keeps opening the GUI there — `ninox tui` is the terminal path on macOS; say so in the README.)
- Run: `cargo test --workspace && cargo clippy --workspace --all-targets`

- [ ] **Step 3: Docs**

README: a "Terminal mode" section — `ninox tui`, `ninox list [--json]`, `ninox connect <id>`, `ninox orchestrate <name> [--prompt] [--no-attach]`, the auto-started daemon (`daemon.log` location), and the macOS bare-`ninox` caveat. CLAUDE.md: extend the hot-path subcommand list ("`Statusline`, `Inbox`, `Open`/`Close`/`List`, `Capabilities`") with `Connect` and note bare `List` now reads sessions.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "feat(app): ninox tui entry + no-display terminal fallback, docs

Co-Authored-By Claude <noreply@anthropic.com>"
```

---

### Task 11: Brain write-back + PR

- [ ] **Step 1: Record learnings in the brain**

Per project convention, write back what this feature established:

```bash
ninox brain add patterns/terminal-mode.md --content '---
title: Terminal mode (TUI + CLI)
tags: [ninox, tui, cli, daemon]
---
`ninox tui` / bare `ninox` without a display opens a ratatui client over the store; `ensure_daemon()` (ninox-core/src/daemon.rs) auto-starts `ninox --headless` when nothing is listening on the configured port — the TCP bind is the liveness probe and the lock. `ninox list` = session board (`--prs` still PR watches), `ninox connect <id>` = exec tmux attach on the private socket, `ninox orchestrate <name>` = user-facing spawn-orchestrator + attach. Dead-session reconciliation now lives in Poller::start, not GUI startup.'
```

- [ ] **Step 2: Final verification**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets`
Manual end-to-end on a clean port: `ninox orchestrate e2e-check --no-attach` (daemon auto-starts), `ninox list` shows it, `ninox connect e2e-check` attaches, detach, `ninox tui` shows the board; clean up the test orchestrator via the TUI (`d`+`y`).

- [ ] **Step 3: Open the PR**

Per the feature workflow: push `mlops-3737-cli-only-mode`, open a PR titled with `MLOPS-3737` (copied from Linear), body with `## Why` / `## Summary` / `## Test plan`, run `now-playing` and append its track line if any, end the body with the managed attribution line, spawn a reviewer, link the PR on the Linear ticket, and move the ticket to In Review.
