//! Anti-spoofing authorization for orchestrator-facing CLI commands (e.g.
//! `ninox plan register`), and the shared sender-attestation primitives
//! `messaging::deliver_message` uses to attach a daemon-verified identity to
//! every inter-session message (see
//! `docs/superpowers/specs/2026-10-21-sender-identity-attestation-design.md`).
//!
//! `NINOX_ORCHESTRATOR_ID` alone is just an env var — trivially forgeable by
//! anything running in the same shell. `authorize_orchestrator` additionally
//! cross-checks the caller's live tmux pane identity against a persisted
//! `OrchestratorRuntimeIdentity` row, self-registering that row on first use
//! (the "migration" path) so no separate registration step is needed at
//! spawn time. `attest_sender` reuses that same cross-check for orchestrator
//! senders; worker senders have no equivalent persisted record, so they are
//! instead verified against their own live pane name (see `attest_sender`'s
//! doc comment).

use crate::{runtime, store::Store, types::OrchestratorRuntimeIdentity};
use anyhow::{Context, Result};

/// The LIVE, pane-verified worker-type of the session actually running this
/// process — used by `ninox spawn`'s `can_spawn` enforcement and
/// `worker_type_cli`'s mutation gate.
///
/// `caller_session_id` MUST already be resolved from the caller's live
/// tmux/ptyd pane identity (see `runtime::current_private_pane_identity`'s
/// `physical_tmux_name`), never the raw, forgeable `NINOX_SESSION` env var
/// — a worker could otherwise `env -u NINOX_SESSION` and have its call
/// treated as an unrestricted direct call from the user. `None` means no
/// ninox-managed pane at all (a human's own shell, a bare script) — the
/// only context this returns `None` for (unrestricted).
///
/// An orchestrator session is identified via `store.is_orchestrator`,
/// never its `worker_type` column — every orchestrator predates that
/// column and has it unset, so keying this off the column would
/// misclassify the entire existing fleet as `Implementor` and lock every
/// orchestrator out of spawning (or managing types as) anything but a
/// Researcher. Any other live session defaults to `Implementor` when its
/// own `worker_type` is unset, matching an untyped `ninox spawn`'s own
/// default.
pub fn resolve_caller_worker_type(store: &Store, caller_session_id: Option<&str>) -> Option<String> {
    let sid = caller_session_id?;
    if store.is_orchestrator(sid).unwrap_or(false) {
        return Some("Orchestrator".to_string());
    }
    Some(
        store.get_session(sid).ok().flatten()
            .and_then(|s| s.worker_type)
            .unwrap_or_else(|| "Implementor".to_string()),
    )
}

/// Resolves the LIVE caller session id for `ninox spawn`'s `can_spawn`
/// enforcement and `ninox worker-type`'s mutation gate — the one function
/// both should call for this, instead of trusting the forgeable
/// `NINOX_SESSION` env var OR treating a bare `current_private_pane_
/// identity()` miss as automatically "the user."
///
/// `pane_session_id` is that pane-identity result, already resolved by the
/// caller (an async step — this function itself is sync). When it's
/// `Some`, this returns it straight through: `current_private_pane_
/// identity`'s tmux path verifies ancestry against every live tmux pane
/// directly, no env dependency, so a `Some` here is already trustworthy.
///
/// When it's `None`, this does NOT conclude "the user" outright — the
/// ptyd and ACP backends underneath `current_private_pane_identity` only
/// verify ancestry against the ONE pane a `NINOX_PANE_ID`/
/// `NINOX_ACP_PANE_ID` env var CLAIMS to be (see `ptyd_backend::PANE_ID_ENV`
/// and `acp_backend`'s mirrored check); `env -u <that var>` makes either
/// return `None` with no further check, which would otherwise look
/// identical to a genuine direct call from the user. So on a `None`, this
/// runs its OWN reverse ancestry scan — the same `runtime::process::
/// parent_map`/`descends_in` primitives those backends already use for
/// their one-pane check, just run against every LIVE (non-terminal)
/// managed session's recorded root pid instead of one claimed by an env
/// var. Only a calling process with no managed ancestor at all resolves
/// as the user.
pub fn resolve_live_caller_session_id(store: &Store, pane_session_id: Option<&str>) -> Option<String> {
    if let Some(id) = pane_session_id {
        return Some(id.to_string());
    }
    resolve_live_session_id_for_pid(store, std::process::id()).ok().flatten()
}

/// [`resolve_live_caller_session_id`]'s ancestry fallback for a process
/// other than this one — the daemon's control socket runs it against its
/// peer's kernel-reported pid, so `None` ("the user") is decided by the
/// caller's real process tree rather than anything the caller sends. Errs
/// rather than answering "the user" whenever the tree can't be read for
/// `pid` — a store error, `ps` failing, or `pid` already gone (a peer that
/// sends and exits before the check would otherwise have no ancestors).
pub fn resolve_live_session_id_for_pid(store: &Store, pid: u32) -> Result<Option<String>> {
    let sessions = store.list_sessions()?;
    let candidates: Vec<(String, u32)> = sessions.iter()
        .filter(|s| !s.status.is_terminal())
        .filter_map(|s| s.pid.map(|root| (s.id.clone(), root)))
        .collect();
    let parents = crate::runtime::process::parent_map();
    anyhow::ensure!(parents.contains_key(&pid), "pid {pid} is not in the process table");
    Ok(resolve_by_ancestry(&parents, pid, &candidates))
}

/// Pure, directly testable core of [`resolve_live_caller_session_id`]'s
/// fallback path: given an already-captured pid→parent-pid map and the
/// `(session id, root pid)` of every live managed session, returns the id
/// of whichever one the calling `pid` descends from (or is), `None` if it
/// descends from none of them.
fn resolve_by_ancestry(parents: &std::collections::HashMap<u32, u32>, pid: u32, candidates: &[(String, u32)]) -> Option<String> {
    candidates.iter()
        .find(|(_, root)| crate::runtime::process::descends_in(parents, pid, *root))
        .map(|(id, _)| id.clone())
}

pub fn authorize_orchestrator(
    store: &Store,
    orchestrator_id: Option<&str>,
    caller_type: Option<&str>,
    runtime: Option<&runtime::PaneIdentity>,
) -> Result<String> {
    let orchestrator_id = orchestrator_id
        .filter(|id| !id.is_empty())
        .context("NINOX_ORCHESTRATOR_ID is not set")?;
    anyhow::ensure!(
        caller_type == Some("orchestrator"),
        "this command is only available inside the owning orchestrator session"
    );
    let runtime = runtime.context("caller is not running inside a private Ninox pane")?;
    anyhow::ensure!(
        store.is_orchestrator(orchestrator_id)?,
        "caller session is not a persisted orchestrator"
    );
    let persisted = persisted_or_migrated_identity(store, orchestrator_id, runtime)?
        .context("orchestrator has no persisted runtime identity")?;
    anyhow::ensure!(
        runtime_identity_matches(&persisted, runtime),
        "caller is not running under the immutable orchestrator runtime"
    );
    Ok(orchestrator_id.to_string())
}

/// `orchestrator_id`'s persisted runtime identity, self-registering it (the
/// "migration" path) on first use when `runtime` is a private pane literally
/// named after `orchestrator_id` and the caller genuinely descends from its
/// root process. Shared by `authorize_orchestrator` and `attest_sender` so
/// both treat a legitimate orchestrator's very first authenticated action —
/// whether that's `ninox plan register` or its first `ninox send` — the same
/// way.
fn persisted_or_migrated_identity(
    store: &Store,
    orchestrator_id: &str,
    runtime: &runtime::PaneIdentity,
) -> Result<Option<OrchestratorRuntimeIdentity>> {
    let mut persisted = store.orchestrator_runtime_identity(orchestrator_id)?;
    let stale_root = persisted.as_ref().and_then(|p| {
        (!runtime_identity_matches(p, runtime) && !crate::lifecycle::probe::is_pid_alive(p.root_pid))
            .then_some(p.root_pid)
    });
    if (persisted.is_none() || stale_root.is_some())
        && runtime.physical_tmux_name == orchestrator_id
        && runtime::caller_descends_from(runtime.pane_pid)
    {
        let registered_at = crate::lifecycle::poller::now_millis();
        let migrated = OrchestratorRuntimeIdentity {
            orchestrator_id: orchestrator_id.to_string(),
            runtime_id: format!(
                "migrated:{}:{}:{registered_at}",
                runtime.server_epoch, runtime.pane_id
            ),
            server_epoch: runtime.server_epoch.clone(),
            physical_tmux_name: runtime.physical_tmux_name.clone(),
            pane_id: runtime.pane_id.clone(),
            root_pid: runtime.pane_pid,
            root_created_at: runtime.pane_created_at,
            registered_at,
        };
        let registered = match stale_root {
            Some(dead) => store.replace_dead_orchestrator_runtime_with(&migrated, dead)?,
            None => store.register_migrated_orchestrator_runtime(&migrated)?,
        };
        if registered {
            persisted = Some(migrated);
        } else {
            // Lost the race: a concurrent call for the same orchestrator
            // (e.g. two `ninox spawn`s fired in the same turn) already won
            // the `ON CONFLICT DO NOTHING` insert. Re-read what it persisted
            // rather than treating this caller as unregistered — both calls
            // descend from the same pane root, so the match below still
            // succeeds for this one too.
            persisted = store.orchestrator_runtime_identity(orchestrator_id)?;
        }
    }
    Ok(persisted)
}

/// Whether `runtime` (a live pane) is the exact pane/process root
/// `persisted` was registered from.
fn runtime_identity_matches(persisted: &OrchestratorRuntimeIdentity, runtime: &runtime::PaneIdentity) -> bool {
    persisted.server_epoch == runtime.server_epoch
        && persisted.physical_tmux_name == runtime.physical_tmux_name
        && persisted.pane_id == runtime.pane_id
        && persisted.root_pid == runtime.pane_pid
        && (persisted.root_created_at - runtime.pane_created_at).abs() <= 2_000
        && runtime::caller_descends_from(persisted.root_pid)
}

/// Hops from `orchestrator_id` up to its root (an orchestrator whose own
/// session has no recorded parent) — a root orchestrator is depth 0.
/// Bounded by `cap`: a legitimate chain (enforced at spawn time by
/// `authorize_nested_spawn`'s callers) is never deeper than `cap`, so
/// walking more than `cap` hops means a cycle or corrupted data — this
/// errors out rather than looping further.
pub fn orchestrator_depth(store: &Store, orchestrator_id: &str, cap: u32) -> Result<u32> {
    let mut depth = 0u32;
    let mut current = orchestrator_id.to_string();
    loop {
        let session = store
            .get_session(&current)?
            .with_context(|| format!("orchestrator '{current}' has no session row"))?;
        let Some(parent) = session.orchestrator_id else { return Ok(depth) };
        depth += 1;
        anyhow::ensure!(
            depth <= cap,
            "lineage chain from '{orchestrator_id}' exceeds the configured depth cap ({cap}) \
             while walking to its root — refusing rather than walking further (possible cycle)"
        );
        current = parent;
    }
}

/// Verifies the caller of `ninox spawn-sub-orchestrator` and resolves the
/// orchestrator id the new sub-orchestrator should attach under (its
/// parent). Two caller shapes are accepted, matching the nested-orchestration
/// design spec's permission-gating section:
///
/// - An already-verified orchestrator (checked exactly like
///   [`authorize_orchestrator`]) has nested-spawn permission by default —
///   its own verified id becomes the new sub-orchestrator's parent.
/// - A verified worker (its own live pane matches its claimed
///   `NINOX_SESSION`) whose recorded `worker_type` resolves in
///   `worker_type_registry` to a spec with `allow_nested_spawn: true` — its
///   own recorded parent orchestrator becomes the new sub-orchestrator's
///   parent.
///
/// `caller_type == Some("orchestrator")` takes the first branch; anything
/// else — `None`, `Some("worker")`, or any other value — takes the second.
/// This matches `caller_is_orchestrator`'s existing convention elsewhere in
/// this codebase: a worker's env never carries `NINOX_CALLER_TYPE` at all,
/// so its *absence* is the "not an orchestrator" signal, not a distinct
/// `"worker"` string nothing actually sets. The real authentication is the
/// pane-identity match inside [`authorize_nested_spawn_as_worker`], not this
/// hint — so routing every non-orchestrator caller there is safe.
pub fn authorize_nested_spawn(
    store: &Store,
    claimed_orchestrator_id: Option<&str>,
    caller_type: Option<&str>,
    caller_session_id: Option<&str>,
    runtime: Option<&runtime::PaneIdentity>,
    worker_type_registry: &crate::worker_type::WorkerTypeRegistry,
) -> Result<String> {
    if caller_type == Some("orchestrator") {
        authorize_orchestrator(store, claimed_orchestrator_id, caller_type, runtime)
    } else {
        authorize_nested_spawn_as_worker(store, caller_session_id, runtime, worker_type_registry)
    }
}

/// The worker-caller branch of [`authorize_nested_spawn`]: verifies the
/// caller's live pane genuinely is the worker it claims to be
/// (`caller_session_id`), then requires that worker's recorded
/// `worker_type` to resolve to a spec with `allow_nested_spawn: true`.
fn authorize_nested_spawn_as_worker(
    store: &Store,
    caller_session_id: Option<&str>,
    runtime: Option<&runtime::PaneIdentity>,
    worker_type_registry: &crate::worker_type::WorkerTypeRegistry,
) -> Result<String> {
    let worker_id = caller_session_id.context("NINOX_SESSION is not set")?;
    let pane = runtime.context("caller is not running inside a private Ninox pane")?;
    anyhow::ensure!(
        pane.physical_tmux_name == worker_id,
        "caller's live pane does not match its claimed session '{worker_id}'"
    );
    let session = store
        .get_session(worker_id)?
        .with_context(|| format!("no session recorded for '{worker_id}'"))?;
    let type_name = session.worker_type.context(
        "nested-spawn requires a worker type that explicitly grants it, and this worker has no worker type"
    )?;
    let spec = worker_type_registry
        .get(&type_name)
        .with_context(|| format!("unknown worker type '{type_name}'"))?;
    anyhow::ensure!(
        spec.allow_nested_spawn,
        "worker type '{type_name}' does not grant nested-spawn permission"
    );
    session.orchestrator_id.context(
        "worker has no recorded parent orchestrator to attach a sub-orchestrator under"
    )
}

/// A claimed sender's relationship to the session it's delivering a message
/// to, as attached to every delivered inter-session message. See the design
/// spec's "Relationship categories" section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SenderRelationship {
    /// Sender is the receiver's recorded parent/spawner.
    DirectOrchestrator,
    /// Same orchestrator, different worker.
    Sibling,
    /// The reverse direction: a worker messaging back up to its orchestrator.
    Descendant,
    /// No recorded lineage relationship (includes legitimate cross-tree
    /// messaging, and a claimed identity that failed to verify).
    Unrelated,
    /// The attestation check itself could not be run (e.g. no live pane
    /// identity was available to check the claim against).
    Unverifiable,
}

/// The result of [`attest_sender`]: a claimed sender identity, whether it
/// was actually verified, and its relationship to the message's receiver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SenderAttestation {
    pub claimed_session_id: Option<String>,
    pub relationship: SenderRelationship,
    pub verified: bool,
    /// What check produced `verified: true`; only meaningful then, folded
    /// into `render`'s wording.
    basis: &'static str,
}

impl SenderAttestation {
    /// The plain-text verification line `messaging::deliver_message`
    /// prepends to the delivered content (see the design spec's "Delivered
    /// message format"). Never discloses the claimed identity or
    /// relationship when `verified` is false — a failed or impossible check
    /// must never be dressed up as a specific (if cautious) relationship.
    pub fn render(&self) -> String {
        let Some(id) = self.claimed_session_id.as_deref().filter(|_| self.verified) else {
            return "[ninox: sender identity could not be verified]".to_string();
        };
        match self.relationship {
            SenderRelationship::DirectOrchestrator => format!(
                "[ninox: sender session {id} verified as your direct spawning orchestrator, \
                 confirmed via {}]",
                self.basis
            ),
            SenderRelationship::Sibling => format!(
                "[ninox: sender session {id} verified as a sibling session under the same \
                 orchestrator, confirmed via {}]",
                self.basis
            ),
            SenderRelationship::Descendant => format!(
                "[ninox: sender session {id} verified as a descendant session (one you spawned), \
                 confirmed via {}]",
                self.basis
            ),
            SenderRelationship::Unrelated => format!(
                "[ninox: sender session {id} verified as an unrelated session — no lineage \
                 relationship recorded]"
            ),
            SenderRelationship::Unverifiable => "[ninox: sender identity could not be verified]".to_string(),
        }
    }
}

/// Generalizes `authorize_orchestrator`'s anti-spoofing cross-check so
/// `messaging::deliver_message` can attach a daemon-verified sender identity
/// to every inter-session message, not just orchestrator-facing commands.
///
/// `claimed_sender` is the `from` a caller resolved server-side (never a
/// bare CLI argument). `caller_pane` is that SAME caller's own live pane
/// identity, captured synchronously in the process that is actually sending
/// the message (e.g. the `ninox send` CLI invocation) — `None` means no such
/// identity could be resolved at all (the check cannot even be attempted).
///
/// Verification strength differs by sender kind, both real but not equally
/// strong:
/// - An orchestrator sender is checked against its persisted
///   `OrchestratorRuntimeIdentity` exactly as `authorize_orchestrator` does
///   (process-ancestry + tmux-pane + server-epoch cross-check against an
///   immutable, self-registering record) — the full strength the design
///   spec asks for.
/// - A worker sender has no equivalent persisted record, so it is verified
///   by comparing `caller_pane.physical_tmux_name` against the claim: every
///   `PaneIdentity` this crate ever resolves (tmux or ptyd backend) already
///   only exists because the backend confirmed the calling process
///   genuinely descends from that pane's live root — so a name match is
///   still real process-ancestry proof, just without a persisted baseline
///   that survives a pane being killed and respawned under the same name.
pub fn attest_sender(
    store: &Store,
    claimed_sender: Option<&str>,
    caller_pane: Option<&runtime::PaneIdentity>,
    receiver_session_id: &str,
) -> SenderAttestation {
    let Some(claimed) = claimed_sender.filter(|s| !s.is_empty()) else {
        return SenderAttestation {
            claimed_session_id: None,
            relationship: SenderRelationship::Unverifiable,
            verified: false,
            basis: "",
        };
    };
    let Some(pane) = caller_pane else {
        return SenderAttestation {
            claimed_session_id: Some(claimed.to_string()),
            relationship: SenderRelationship::Unverifiable,
            verified: false,
            basis: "",
        };
    };
    let is_orchestrator = store.is_orchestrator(claimed).unwrap_or_else(|e| {
        // Fail closed (treat as not-an-orchestrator) either way, but a
        // transient store error must not be indistinguishable from a
        // genuine "this isn't an orchestrator" when someone's debugging a
        // downgraded trust classification later.
        tracing::warn!("is_orchestrator check failed for {claimed}, treating as non-orchestrator: {e}");
        false
    });
    let (verified, basis) = if is_orchestrator {
        let verified = persisted_or_migrated_identity(store, claimed, pane)
            .ok()
            .flatten()
            .is_some_and(|persisted| runtime_identity_matches(&persisted, pane));
        (verified, "process-ancestry + tmux-pane cross-check")
    } else {
        (pane.physical_tmux_name == claimed, "live session-pane identity check")
    };
    if !verified {
        return SenderAttestation {
            claimed_session_id: Some(claimed.to_string()),
            relationship: SenderRelationship::Unrelated,
            verified: false,
            basis: "",
        };
    }
    SenderAttestation {
        claimed_session_id: Some(claimed.to_string()),
        relationship: relationship_of(store, claimed, receiver_session_id, is_orchestrator),
        verified: true,
        basis,
    }
}

/// The lineage relationship between a verified `sender` and `receiver`, per
/// their recorded `Session::orchestrator_id`. Only meaningful once the
/// sender's identity is already verified — this never runs for a claim that
/// failed to check out.
fn relationship_of(store: &Store, sender: &str, receiver: &str, sender_is_orchestrator: bool) -> SenderRelationship {
    // A queued receiver has no sessions row yet; its queue entry carries
    // the row it will be written as.
    let receiver_session = store.get_session(receiver).ok().flatten()
        .or_else(|| store.queue_entry(receiver).ok().flatten().map(|e| e.session));
    let receiver_parent = receiver_session.as_ref().and_then(|s| s.orchestrator_id.as_deref());
    if sender_is_orchestrator && receiver_parent == Some(sender) {
        return SenderRelationship::DirectOrchestrator;
    }
    if !sender_is_orchestrator {
        let sender_session = store.get_session(sender).ok().flatten();
        let sender_parent = sender_session.as_ref().and_then(|s| s.orchestrator_id.as_deref());
        if let Some(parent) = sender_parent {
            if receiver_parent.is_some() && sender_parent == receiver_parent {
                return SenderRelationship::Sibling;
            }
            if parent == receiver {
                return SenderRelationship::Descendant;
            }
        }
    }
    SenderRelationship::Unrelated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Orchestrator, Session, SessionStatus};
    use tempfile::tempdir;

    fn open_store() -> Store {
        Store::open(tempdir().unwrap().keep().join("t.db")).unwrap()
    }

    fn pane(name: &str) -> runtime::PaneIdentity {
        runtime::PaneIdentity {
            physical_tmux_name: name.to_string(),
            pane_id: format!("%{name}"),
            pane_pid: std::process::id(),
            pane_created_at: 0,
            server_epoch: "test-epoch".to_string(),
        }
    }

    fn session_with(id: &str, orchestrator_id: Option<&str>) -> Session {
        Session {
            id: id.to_string(), orchestrator_id: orchestrator_id.map(str::to_string),
            name: id.to_string(), repo: String::new(), status: SessionStatus::Working,
            agent_type: "claude-code".to_string(), cost_usd: 0.0, started_at: 0,
            pr_number: None, pr_id: None, workspace_path: None, pid: Some(std::process::id()),
            model: None, observed_model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None,
            context_used_pct: None, context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None, terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None,
            worker_type: None,
            success_criteria: Vec::new(),
            completion_outcome: None,
            self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }
    }

    #[test]
    fn resolve_live_caller_session_id_prefers_the_pane_fast_path() {
        let store = open_store();
        // No matching session in the store at all — proves the pane value
        // is returned directly, not merely consistent with a lookup.
        assert_eq!(resolve_live_caller_session_id(&store, Some("from-pane")).as_deref(), Some("from-pane"));
    }

    /// The exact scenario a review asked for: `NINOX_PANE_ID`/
    /// `NINOX_ACP_PANE_ID` stripped (so the pane-identity fast path misses,
    /// `pane_session_id: None`), but the calling process genuinely
    /// descends from a live managed session's recorded root pid — this
    /// must still resolve to that session, not fall through to "the user."
    /// Uses a literal mocked parent map (`resolve_by_ancestry`, the pure
    /// core), not a real spawned process tree.
    #[test]
    fn resolve_by_ancestry_finds_a_session_whose_root_pid_is_an_ancestor() {
        // 300 (the "calling pid") -> 200 -> 100 (a live session's root pid).
        let parents = std::collections::HashMap::from([(300u32, 200u32), (200u32, 100u32)]);
        let candidates = vec![("other-session".to_string(), 999u32), ("the-session".to_string(), 100u32)];
        assert_eq!(resolve_by_ancestry(&parents, 300, &candidates).as_deref(), Some("the-session"));
    }

    #[test]
    fn resolve_by_ancestry_returns_none_with_no_managed_ancestor() {
        let parents = std::collections::HashMap::from([(300u32, 200u32), (200u32, 1u32)]);
        let candidates = vec![("unrelated".to_string(), 999u32)];
        assert_eq!(resolve_by_ancestry(&parents, 300, &candidates), None);
    }

    #[test]
    fn resolve_live_caller_session_id_falls_back_to_ancestry_when_pane_identity_is_none() {
        let store = open_store();
        // `pid: Some(std::process::id())` (via `session_with`) makes this
        // test process trivially its own root pid — exercises the full
        // `resolve_live_caller_session_id` path (including the real
        // `process::parent_map()` call) without needing a spawned child.
        store.upsert_session(&session_with("live-worker", None)).unwrap();
        assert_eq!(resolve_live_caller_session_id(&store, None).as_deref(), Some("live-worker"));
    }

    #[test]
    fn resolve_live_caller_session_id_ignores_terminal_sessions() {
        let store = open_store();
        let mut terminal = session_with("done-worker", None);
        terminal.status = SessionStatus::Done;
        store.upsert_session(&terminal).unwrap();
        assert_eq!(resolve_live_caller_session_id(&store, None), None);
    }

    #[test]
    fn a_pid_that_is_gone_is_never_the_user() {
        let store = open_store();
        let gone = (1u32 << 22) + 12_345;
        assert!(resolve_live_session_id_for_pid(&store, gone).is_err());
        assert_eq!(resolve_live_session_id_for_pid(&store, std::process::id()).unwrap(), None);
    }

    #[test]
    fn resolve_live_caller_session_id_is_the_user_with_no_pane_and_no_managed_session() {
        let store = open_store();
        assert_eq!(resolve_live_caller_session_id(&store, None), None);
    }

    /// Persists `id` as an orchestrator with a runtime identity that exactly
    /// matches `pane` (so a caller presenting that same pane verifies).
    fn register_orchestrator(store: &Store, id: &str, pane: &runtime::PaneIdentity) {
        store.upsert_orchestrator(&Orchestrator { id: id.to_string(), name: id.to_string(), created_at: 0 }).unwrap();
        store.upsert_session(&session_with(id, None)).unwrap();
        store.register_orchestrator_runtime(&OrchestratorRuntimeIdentity {
            orchestrator_id: id.to_string(),
            runtime_id: format!("test:{id}"),
            server_epoch: pane.server_epoch.clone(),
            physical_tmux_name: pane.physical_tmux_name.clone(),
            pane_id: pane.pane_id.clone(),
            root_pid: pane.pane_pid,
            root_created_at: pane.pane_created_at,
            registered_at: 0,
        }).unwrap();
    }

    const DEAD_PID: u32 = (1 << 22) + 54_321;

    /// An orchestrator row whose persisted identity names `old_root` under a
    /// previous host epoch, while its session row records this process (the
    /// live pane root) as its pid.
    fn orchestrator_with_stale_identity(store: &Store, id: &str, old_root: u32) {
        store.upsert_orchestrator(&Orchestrator { id: id.to_string(), name: id.to_string(), created_at: 0 }).unwrap();
        store.upsert_session(&session_with(id, None)).unwrap();
        store.register_orchestrator_runtime(&OrchestratorRuntimeIdentity {
            orchestrator_id: id.to_string(),
            runtime_id: "old".into(),
            server_epoch: "old-epoch".into(),
            physical_tmux_name: id.to_string(),
            pane_id: format!("ptyd:{id}"),
            root_pid: old_root,
            root_created_at: 0,
            registered_at: 0,
        }).unwrap();
    }

    #[test]
    fn an_orchestrator_with_a_null_pid_is_adopted_from_its_live_pane() {
        let _iso = crate::test_isolation::isolate();
        let store = open_store();
        store.upsert_orchestrator(&Orchestrator { id: "orch".into(), name: "orch".into(), created_at: 0 }).unwrap();
        let mut row = session_with("orch", None);
        row.pid = None;
        store.upsert_session(&row).unwrap();
        let live = pane("orch");

        assert!(authorize_orchestrator(&store, Some("orch"), Some("orchestrator"), Some(&live)).is_err());

        assert!(crate::lifecycle::pane_pid::adopt_pane_pid(&store, &row, Some(live.pane_pid)));
        assert_eq!(authorize_orchestrator(&store, Some("orch"), Some("orchestrator"), Some(&live)).unwrap(), "orch");
    }

    #[test]
    fn a_dead_root_identity_is_replaced_by_the_live_pane() {
        let store = open_store();
        orchestrator_with_stale_identity(&store, "orch", DEAD_PID);
        let live = pane("orch");

        assert_eq!(authorize_orchestrator(&store, Some("orch"), Some("orchestrator"), Some(&live)).unwrap(), "orch");
        let now = store.orchestrator_runtime_identity("orch").unwrap().unwrap();
        assert_eq!((now.root_pid, now.server_epoch.as_str()), (live.pane_pid, "test-epoch"));
    }

    #[test]
    fn a_live_old_root_that_is_not_the_pane_root_is_never_replaced() {
        let store = open_store();
        // pid 1 is always alive: stands in for a recycled pid.
        orchestrator_with_stale_identity(&store, "orch", 1);

        let err = authorize_orchestrator(&store, Some("orch"), Some("orchestrator"), Some(&pane("orch"))).unwrap_err();
        assert!(err.to_string().contains("immutable orchestrator runtime"), "{err}");
        assert_eq!(store.orchestrator_runtime_identity("orch").unwrap().unwrap().root_pid, 1);
    }

    #[test]
    fn a_dead_root_is_not_replaced_from_a_pane_named_for_another_session() {
        let store = open_store();
        orchestrator_with_stale_identity(&store, "orch", DEAD_PID);

        assert!(authorize_orchestrator(&store, Some("orch"), Some("orchestrator"), Some(&pane("other"))).is_err());
        assert_eq!(store.orchestrator_runtime_identity("orch").unwrap().unwrap().root_pid, DEAD_PID);
    }

    #[test]
    fn a_dead_root_is_not_replaced_unless_the_session_row_records_the_live_root() {
        let store = open_store();
        orchestrator_with_stale_identity(&store, "orch", DEAD_PID);
        let mut row = session_with("orch", None);
        row.pid = None;
        store.upsert_session(&row).unwrap();

        assert!(authorize_orchestrator(&store, Some("orch"), Some("orchestrator"), Some(&pane("orch"))).is_err());
        assert_eq!(store.orchestrator_runtime_identity("orch").unwrap().unwrap().root_pid, DEAD_PID);
    }

    #[test]
    fn orchestrator_depth_is_zero_for_a_root() {
        let store = open_store();
        register_orchestrator(&store, "root", &pane("root"));
        assert_eq!(orchestrator_depth(&store, "root", 3).unwrap(), 0);
    }

    #[test]
    fn orchestrator_depth_counts_hops_to_root() {
        let store = open_store();
        register_orchestrator(&store, "root", &pane("root"));
        store.upsert_orchestrator(&Orchestrator { id: "sub1".into(), name: "sub1".into(), created_at: 0 }).unwrap();
        store.upsert_session(&session_with("sub1", Some("root"))).unwrap();
        store.upsert_orchestrator(&Orchestrator { id: "sub2".into(), name: "sub2".into(), created_at: 0 }).unwrap();
        store.upsert_session(&session_with("sub2", Some("sub1"))).unwrap();

        assert_eq!(orchestrator_depth(&store, "sub1", 3).unwrap(), 1);
        assert_eq!(orchestrator_depth(&store, "sub2", 3).unwrap(), 2);
    }

    #[test]
    fn orchestrator_depth_errors_instead_of_looping_on_a_cycle() {
        let store = open_store();
        store.upsert_orchestrator(&Orchestrator { id: "a".into(), name: "a".into(), created_at: 0 }).unwrap();
        store.upsert_session(&session_with("a", Some("b"))).unwrap();
        store.upsert_orchestrator(&Orchestrator { id: "b".into(), name: "b".into(), created_at: 0 }).unwrap();
        store.upsert_session(&session_with("b", Some("a"))).unwrap();

        assert!(orchestrator_depth(&store, "a", 3).is_err());
    }

    #[test]
    fn authorize_nested_spawn_accepts_a_verified_orchestrator() {
        let store = open_store();
        register_orchestrator(&store, "orch-1", &pane("orch-1"));
        let registry = crate::worker_type::WorkerTypeRegistry::from_config(&[]);

        let parent = authorize_nested_spawn(
            &store, Some("orch-1"), Some("orchestrator"), None, Some(&pane("orch-1")), &registry,
        ).unwrap();
        assert_eq!(parent, "orch-1");
    }

    #[test]
    fn authorize_nested_spawn_rejects_a_forged_orchestrator_claim() {
        let store = open_store();
        let registry = crate::worker_type::WorkerTypeRegistry::from_config(&[]);
        let err = authorize_nested_spawn(
            &store, Some("some-real-orchestrator"), Some("orchestrator"), None, None, &registry,
        ).unwrap_err();
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn authorize_nested_spawn_rejects_a_worker_with_no_worker_type() {
        let store = open_store();
        register_orchestrator(&store, "orch-1", &pane("orch-1"));
        store.upsert_session(&session_with("worker-a", Some("orch-1"))).unwrap();
        let registry = crate::worker_type::WorkerTypeRegistry::from_config(&[]);

        let err = authorize_nested_spawn(
            &store, None, Some("worker"), Some("worker-a"), Some(&pane("worker-a")), &registry,
        ).unwrap_err();
        assert!(err.to_string().contains("worker type"), "{err}");
    }

    #[test]
    fn authorize_nested_spawn_rejects_a_worker_type_without_the_permission() {
        let store = open_store();
        register_orchestrator(&store, "orch-1", &pane("orch-1"));
        let mut w = session_with("worker-a", Some("orch-1"));
        w.worker_type = Some("Implementation".into());
        store.upsert_session(&w).unwrap();
        let registry = crate::worker_type::WorkerTypeRegistry::from_config(&[]);

        let err = authorize_nested_spawn(
            &store, None, Some("worker"), Some("worker-a"), Some(&pane("worker-a")), &registry,
        ).unwrap_err();
        assert!(err.to_string().contains("does not grant"), "{err}");
    }

    #[test]
    fn authorize_nested_spawn_accepts_a_worker_type_with_the_permission() {
        let store = open_store();
        register_orchestrator(&store, "orch-1", &pane("orch-1"));
        let mut w = session_with("worker-a", Some("orch-1"));
        w.worker_type = Some("Delegator".into());
        store.upsert_session(&w).unwrap();
        let registry = crate::worker_type::WorkerTypeRegistry::from_config(&[crate::worker_type::WorkerTypeSpec {
            name: "Delegator".into(),
            allow_nested_spawn: true,
            ..crate::worker_type::WorkerTypeSpec::default()
        }]);

        let parent = authorize_nested_spawn(
            &store, None, Some("worker"), Some("worker-a"), Some(&pane("worker-a")), &registry,
        ).unwrap();
        assert_eq!(parent, "orch-1");
    }

    /// Production reality: `worker_env_vars` (`cli.rs`) never sets
    /// `NINOX_CALLER_TYPE` at all for a spawned worker — its *absence* is
    /// this codebase's existing "not an orchestrator" signal (see
    /// `caller_is_orchestrator`'s doc comment). A permitted worker-type
    /// grant must still work with `caller_type: None`, not just the
    /// test-convenience `Some("worker")` shape above — otherwise the
    /// worker-permission path is unreachable by any real spawned worker.
    #[test]
    fn authorize_nested_spawn_accepts_a_permitted_worker_with_no_caller_type_env() {
        let store = open_store();
        register_orchestrator(&store, "orch-1", &pane("orch-1"));
        let mut w = session_with("worker-a", Some("orch-1"));
        w.worker_type = Some("Delegator".into());
        store.upsert_session(&w).unwrap();
        let registry = crate::worker_type::WorkerTypeRegistry::from_config(&[crate::worker_type::WorkerTypeSpec {
            name: "Delegator".into(),
            allow_nested_spawn: true,
            ..crate::worker_type::WorkerTypeSpec::default()
        }]);

        let parent = authorize_nested_spawn(
            &store, None, None, Some("worker-a"), Some(&pane("worker-a")), &registry,
        ).unwrap();
        assert_eq!(parent, "orch-1");
    }

    /// A caller with no session context at all (no `NINOX_SESSION`, no live
    /// pane) falls through to the worker branch (anything that isn't
    /// `Some("orchestrator")` does) and is rejected there for lacking an
    /// identifiable session — not a special "unrecognized caller type" path.
    #[test]
    fn authorize_nested_spawn_rejects_a_caller_with_no_session_context() {
        let store = open_store();
        let registry = crate::worker_type::WorkerTypeRegistry::from_config(&[]);
        assert!(authorize_nested_spawn(&store, None, None, None, None, &registry).is_err());
    }

    #[test]
    fn direct_orchestrator_when_sender_is_receivers_recorded_parent() {
        let store = open_store();
        register_orchestrator(&store, "orch-1", &pane("orch-1"));
        store.upsert_session(&session_with("worker-a", Some("orch-1"))).unwrap();

        let a = attest_sender(&store, Some("orch-1"), Some(&pane("orch-1")), "worker-a");
        assert_eq!(a.relationship, SenderRelationship::DirectOrchestrator);
        assert!(a.verified);
    }

    #[test]
    fn sibling_when_both_share_the_same_orchestrator() {
        let store = open_store();
        register_orchestrator(&store, "orch-1", &pane("orch-1"));
        store.upsert_session(&session_with("worker-a", Some("orch-1"))).unwrap();
        store.upsert_session(&session_with("worker-b", Some("orch-1"))).unwrap();

        let a = attest_sender(&store, Some("worker-a"), Some(&pane("worker-a")), "worker-b");
        assert_eq!(a.relationship, SenderRelationship::Sibling);
        assert!(a.verified);
    }

    #[test]
    fn descendant_when_a_worker_messages_its_own_orchestrator() {
        let store = open_store();
        register_orchestrator(&store, "orch-1", &pane("orch-1"));
        store.upsert_session(&session_with("worker-a", Some("orch-1"))).unwrap();

        let a = attest_sender(&store, Some("worker-a"), Some(&pane("worker-a")), "orch-1");
        assert_eq!(a.relationship, SenderRelationship::Descendant);
        assert!(a.verified);
    }

    #[test]
    fn unrelated_when_verified_but_no_lineage_recorded() {
        let store = open_store();
        register_orchestrator(&store, "orch-1", &pane("orch-1"));
        register_orchestrator(&store, "orch-2", &pane("orch-2"));
        store.upsert_session(&session_with("worker-a", Some("orch-1"))).unwrap();

        let a = attest_sender(&store, Some("orch-2"), Some(&pane("orch-2")), "worker-a");
        assert_eq!(a.relationship, SenderRelationship::Unrelated);
        assert!(a.verified);
    }

    #[test]
    fn unrelated_and_unverified_when_claimed_pane_does_not_match() {
        let store = open_store();
        register_orchestrator(&store, "orch-1", &pane("orch-1"));
        store.upsert_session(&session_with("worker-a", Some("orch-1"))).unwrap();
        store.upsert_session(&session_with("worker-b", Some("orch-1"))).unwrap();

        // Caller's real pane is "worker-b", but claims to be "worker-a".
        let a = attest_sender(&store, Some("worker-a"), Some(&pane("worker-b")), "orch-1");
        assert_eq!(a.relationship, SenderRelationship::Unrelated);
        assert!(!a.verified);
    }

    #[test]
    fn unverifiable_when_no_caller_pane_is_available() {
        let store = open_store();
        store.upsert_session(&session_with("worker-a", None)).unwrap();

        let a = attest_sender(&store, Some("worker-a"), None, "orch-1");
        assert_eq!(a.relationship, SenderRelationship::Unverifiable);
        assert!(!a.verified);
    }

    #[test]
    fn unverifiable_when_no_sender_is_claimed_at_all() {
        let store = open_store();
        let a = attest_sender(&store, None, Some(&pane("worker-a")), "orch-1");
        assert_eq!(a.relationship, SenderRelationship::Unverifiable);
        assert!(!a.verified);
        assert_eq!(a.claimed_session_id, None);
    }

    #[test]
    fn orchestrator_self_registers_on_first_attested_send() {
        let store = open_store();
        store.upsert_orchestrator(&Orchestrator { id: "orch-2".into(), name: "orch-2".into(), created_at: 0 }).unwrap();
        store.upsert_session(&session_with("orch-2", None)).unwrap();
        assert!(store.orchestrator_runtime_identity("orch-2").unwrap().is_none());

        let a = attest_sender(&store, Some("orch-2"), Some(&pane("orch-2")), "some-receiver");
        assert!(a.verified, "a genuine first-use orchestrator must self-register and verify");
        assert!(store.orchestrator_runtime_identity("orch-2").unwrap().is_some());
    }

    #[test]
    fn render_matches_spec_examples_for_direct_orchestrator_and_unrelated() {
        let store = open_store();
        register_orchestrator(&store, "orch-1", &pane("orch-1"));
        register_orchestrator(&store, "orch-2", &pane("orch-2"));
        store.upsert_session(&session_with("worker-a", Some("orch-1"))).unwrap();

        let direct = attest_sender(&store, Some("orch-1"), Some(&pane("orch-1")), "worker-a");
        assert_eq!(
            direct.render(),
            "[ninox: sender session orch-1 verified as your direct spawning orchestrator, \
             confirmed via process-ancestry + tmux-pane cross-check]"
        );

        let unrelated = attest_sender(&store, Some("orch-2"), Some(&pane("orch-2")), "worker-a");
        assert_eq!(
            unrelated.render(),
            "[ninox: sender session orch-2 verified as an unrelated session — \
             no lineage relationship recorded]"
        );
    }

    #[test]
    fn render_never_overclaims_descendant_as_transitive_when_only_a_direct_parent_check_runs() {
        let store = open_store();
        register_orchestrator(&store, "orch-1", &pane("orch-1"));
        store.upsert_session(&session_with("worker-a", Some("orch-1"))).unwrap();

        let descendant = attest_sender(&store, Some("worker-a"), Some(&pane("worker-a")), "orch-1");
        assert_eq!(
            descendant.render(),
            "[ninox: sender session worker-a verified as a descendant session (one you spawned), \
             confirmed via live session-pane identity check]",
            "relationship_of only ever checks a direct parent link, so the wording must not claim \
             transitive descent it never verified"
        );
    }

    #[test]
    fn authorize_orchestrator_mismatch_never_clears_the_persisted_row() {
        let store = open_store();
        register_orchestrator(&store, "orch-1", &pane("orch-1"));
        let before = store.orchestrator_runtime_identity("orch-1").unwrap().unwrap();

        let mismatched = pane("someone-else");
        let result = authorize_orchestrator(&store, Some("orch-1"), Some("orchestrator"), Some(&mismatched));

        assert!(result.is_err());
        assert_eq!(store.orchestrator_runtime_identity("orch-1").unwrap().unwrap(), before);
    }

    #[test]
    fn attest_sender_mismatch_never_clears_the_persisted_row() {
        let store = open_store();
        register_orchestrator(&store, "orch-1", &pane("orch-1"));
        store.upsert_session(&session_with("worker-a", Some("orch-1"))).unwrap();
        let before = store.orchestrator_runtime_identity("orch-1").unwrap().unwrap();

        let a = attest_sender(&store, Some("orch-1"), Some(&pane("someone-else")), "worker-a");

        assert!(!a.verified);
        assert_eq!(store.orchestrator_runtime_identity("orch-1").unwrap().unwrap(), before);
    }

    #[test]
    fn render_is_generic_and_omits_the_claim_whenever_verified_is_false() {
        let store = open_store();
        store.upsert_session(&session_with("worker-a", None)).unwrap();

        let unverifiable = attest_sender(&store, Some("worker-a"), None, "orch-1");
        assert_eq!(unverifiable.render(), "[ninox: sender identity could not be verified]");

        store.upsert_session(&session_with("worker-b", None)).unwrap();
        let tampered = attest_sender(&store, Some("worker-a"), Some(&pane("worker-b")), "orch-1");
        assert_eq!(tampered.render(), "[ninox: sender identity could not be verified]");
    }

    /// Captures every `tracing` event's formatted `message` field so tests
    /// can assert on log output without a real logging backend.
    #[derive(Clone, Default)]
    struct CapturedLogs(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    impl CapturedLogs {
        fn contains(&self, needle: &str) -> bool {
            self.0.lock().unwrap().iter().any(|m| m.contains(needle))
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CapturedLogs {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
            struct Visitor(String);
            impl tracing::field::Visit for Visitor {
                fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                    if field.name() == "message" {
                        self.0 = format!("{value:?}");
                    }
                }
            }
            let mut visitor = Visitor(String::new());
            event.record(&mut visitor);
            self.0.lock().unwrap().push(visitor.0);
        }
    }

    #[test]
    fn attest_sender_logs_only_when_the_orchestrator_check_actually_errors() {
        use tracing_subscriber::{layer::SubscriberExt, Registry};

        let db_path = tempdir().unwrap().keep().join("t.db");
        let store = Store::open(&db_path).unwrap();
        let caller_pane = pane("claimed-id");

        let logs = CapturedLogs::default();
        let _log_guard = tracing::subscriber::set_default(Registry::default().with(logs.clone()));

        // Ordinary `Ok(false)` path (claimed id just isn't an orchestrator) —
        // already-handled, must not log.
        attest_sender(&store, Some("claimed-id"), Some(&caller_pane), "receiver");
        assert!(!logs.contains("is_orchestrator check failed"), "the ordinary Ok(false) path must not log");

        // Force a genuine store error out from under the Store's own
        // connection, via a second raw connection to the same file.
        rusqlite::Connection::open(&db_path).unwrap().execute("DROP TABLE orchestrators", []).unwrap();

        attest_sender(&store, Some("claimed-id"), Some(&caller_pane), "receiver");
        assert!(
            logs.contains("is_orchestrator check failed"),
            "a transient store error must be logged, not silently downgraded with no signal"
        );
    }
}
