# Fleet Island Model (ninox-overview) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a renderer-agnostic "fleet island" model to the existing `ninox-overview` crate — one island per root orchestrator tree (plus one unparented group), per-island status summaries, sorting, done/stopped folding, machine grouping with connection health, and Fleet-tab navigation state (open/close island, selection, tree-collapsed) — so `ninox-gui` (MLOPS-5339) and `ninox-tui` (MLOPS-5350) can both draw the Fleet tab and the opened-island view from the same core, with zero rendering/layout code in this crate.

**Architecture:** Three new modules added to the already-merged `ninox-overview` crate (`status.rs`, `islands.rs`, `nav.rs`), following the existing phase-1 pattern of pure, renderer-free data + a `FleetSnapshot`-consuming adapter function, each independently unit-tested. No existing phase-1 type's public signature changes except `OverviewTree`, which gains one new field (`roots`) computed in its existing single walk — additive, not breaking. No `ninox-core`, `ninox-gui`, or `ninox-tui` changes.

**Tech Stack:** Rust, `ninox-core` (`Session`, `SessionStatus`, `ActivityState`, `MachineHealth`, `SessionKindData`, `fleet::snapshot::{FleetSnapshot, FleetMember, Role}`, `store::WorkRequestRow`), `serde` (workspace dep, already in `ninox-overview`'s `Cargo.toml` — no new dependencies needed).

**Spec:** `docs/superpowers/specs/2026-10-21-fleet-overview-ui-vision-design.md` (general vision — superseded on specifics by the two briefs below), `.claude/design-ref/fleet-overview-v1/BRIEF-common.md` and `BRIEF-MLOPS-5351.md` (authoritative, final 27 Oct decisions — where these differ from the vision spec, the briefs win), plus the design artboards `FleetAtScale.dc.html`, `TUIAtScale.dc.html`, `FleetFocused.dc.html`, `Legend.dc.html` and `canvas.json`'s `scaleflow` note in the same folder.

## Global Constraints

- Out of scope, per `BRIEF-MLOPS-5351.md`: any rendering, layout pixels, or column-packing visuals; changes to `ninox-core` data or the wire format; `ninox-gui`/`ninox-tui` crate changes beyond a re-export.
- One PR, landed under MLOPS-5351 against `origin/v1` (not `main`). Branch already fast-forwarded onto `origin/v1` at `b00a176` (zero divergent commits) — rebase again immediately before opening the PR since other v1 workers land in parallel.
- PR title exactly: `MLOPS-5351: feat(ninox-overview): <description>`.
- `cargo fmt` is **not** run wholesale (tree isn't rustfmt-clean) — format only the files this plan touches, if at all, via targeted edits, never a blanket `cargo fmt`.
- `cargo build`, `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings` must pass before opening the PR.
- Unit-test every rule (explicit acceptance criterion) — no rule ships without a test asserting it.
- Depth/lineage must stay consistent with `orchestrator_auth::orchestrator_depth` (#242): depth and root are always computed live from `Session.orchestrator_id` chains, never cached on `Session` (no stored depth field exists, by design).

## Review Focus

- A worker whose `orchestrator_id` names a session **not present** in the snapshot (its orchestrator was torn down) must become its own single-member island, not silently vanish or get folded into the shared Unparented group — `Role::Standalone` (genuinely parentless) and "parent went missing" are different situations and must not collapse into one bucket.
- `Session.merged_at.is_some()` must classify as Done even when `status` is not yet `Done` (a PR merged but the session kept alive, per `Session::merge_handled()`) — otherwise a merged worker would wrongly keep counting as active/waiting-on-PR forever.
- A remote island whose `machine_id` has no entry in the caller-supplied health map (health not polled yet) must report `health: None`, never default to `Unreachable` or panic — a missing entry is "unknown," not "down."
- Sorting must be deterministic and panic-free for every mode, including an empty-members island and the single Unparented group, with stable tie-breaking (no dependency on `HashMap` iteration order) so the default view doesn't reorder between otherwise-identical snapshots.
- An orchestrator with an unresolved `ninox request-work` item (`WorkRequestRow` with `resolved_at: None` addressed to it) must surface as needs-you at the island level even when every individual worker under it is plainly `Working` — the "user-request pending" cause named in the brief is a property of the *orchestrator's inbox*, not of any one worker's own activity state.

---

## Tasks

### Task 1: Member status classification

**Files:**
- Create: `crates/ninox-overview/src/status.rs`

**Interfaces:**
- Consumes: `ninox_core::types::{Session, SessionStatus, ActivityState}` (existing).
- Produces: `MemberStatus` enum and `classify_member(session: &Session, has_pending_request: bool) -> MemberStatus`, consumed by Task 3.

- [ ] **Step 1: Write the failing tests**

```rust
// crates/ninox-overview/src/status.rs
#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::types::{ActivityState, Session, SessionStatus};

    fn session(status: SessionStatus) -> Session {
        Session {
            id: "s".into(), orchestrator_id: None, name: "s-name".into(),
            repo: "o/r".into(), status, agent_type: "claude-code".into(), cost_usd: 0.0,
            started_at: 0, pr_number: None, pr_id: None,
            workspace_path: Some("/ws/s".into()), pid: None, model: None,
            context_tokens: None, catalogue_path: None, context_used_pct: None,
            context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: ActivityState::default(), activity_note: None, activity_since: None,
            machine_id: None, worker_type: None,
        }
    }

    #[test]
    fn spawning_or_working_with_non_blocked_activity_is_working() {
        assert_eq!(classify_member(&session(SessionStatus::Spawning), false), MemberStatus::Working);
        assert_eq!(classify_member(&session(SessionStatus::Working), false), MemberStatus::Working);
    }

    #[test]
    fn blocked_activity_overrides_status_to_needs_you() {
        let mut s = session(SessionStatus::Working);
        s.activity = ActivityState::Blocked;
        assert_eq!(classify_member(&s, false), MemberStatus::NeedsYou);
    }

    #[test]
    fn ci_failed_status_is_needs_you_even_if_activity_is_working() {
        let mut s = session(SessionStatus::CiFailed);
        s.activity = ActivityState::Working;
        assert_eq!(classify_member(&s, false), MemberStatus::NeedsYou);
    }

    #[test]
    fn a_pending_work_request_makes_an_otherwise_working_session_needs_you() {
        assert_eq!(classify_member(&session(SessionStatus::Working), true), MemberStatus::NeedsYou);
    }

    #[test]
    fn pr_open_review_pending_and_mergeable_are_waiting_on_pr() {
        for status in [SessionStatus::PrOpen, SessionStatus::ReviewPending, SessionStatus::Mergeable] {
            assert_eq!(classify_member(&session(status), false), MemberStatus::WaitingOnPr, "{status:?}");
        }
    }

    #[test]
    fn done_status_is_done() {
        assert_eq!(classify_member(&session(SessionStatus::Done), false), MemberStatus::Done);
    }

    #[test]
    fn merged_at_set_is_done_even_if_status_lags_behind() {
        let mut s = session(SessionStatus::Working);
        s.merged_at = Some(123);
        assert_eq!(classify_member(&s, false), MemberStatus::Done);
    }

    #[test]
    fn terminated_and_interrupted_without_a_merge_are_stopped_not_done() {
        assert_eq!(classify_member(&session(SessionStatus::Terminated), false), MemberStatus::Stopped);
        assert_eq!(classify_member(&session(SessionStatus::Interrupted), false), MemberStatus::Stopped);
    }

    #[test]
    fn terminal_status_wins_over_a_pending_request_or_blocked_activity() {
        let mut s = session(SessionStatus::Terminated);
        s.activity = ActivityState::Blocked;
        assert_eq!(classify_member(&s, true), MemberStatus::Stopped);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-overview status:: -- --list`
Expected: fails to compile (`classify_member`/`MemberStatus` not defined).

- [ ] **Step 3: Implement `MemberStatus` and `classify_member` in `crates/ninox-overview/src/status.rs`**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberStatus { Working, NeedsYou, WaitingOnPr, Done, Stopped }

pub fn classify_member(session: &Session, has_pending_request: bool) -> MemberStatus { ... }
```

Order of checks (highest priority first), matching the Review Focus and test cases above:
1. `session.status == SessionStatus::Done || session.merged_at.is_some()` → `Done`.
2. `session.is_terminal()` (the remaining `Terminated`/`Interrupted` after step 1) → `Stopped`.
3. `session.activity == ActivityState::Blocked || session.status == SessionStatus::CiFailed || has_pending_request` → `NeedsYou`.
4. `matches!(session.status, SessionStatus::PrOpen | SessionStatus::ReviewPending | SessionStatus::Mergeable)` → `WaitingOnPr`.
5. else → `Working`.

Add a one-line module doc comment stating that `has_pending_request` is computed by the caller from `FleetSnapshot.open_requests`, not derived here — this module has no knowledge of work requests (keeps the dependency one-directional, Task 3 owns that lookup).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox-overview status::`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-overview/src/status.rs
git commit -m "feat(ninox-overview): member status classification for the island model"
```

---

### Task 2: Root-of-tree lookup on `OverviewTree`

**Files:**
- Modify: `crates/ninox-overview/src/tree.rs`

**Interfaces:**
- Consumes: existing `OverviewTree::build(nodes, edges)` walk (unchanged logic, same `chain` it already computes).
- Produces: `OverviewTree.roots: HashMap<NodeId, NodeId>` — for every node id, the id of the root it descends from (a root's own entry maps to itself). Consumed by Task 3.

- [ ] **Step 1: Write the failing tests**

Add to `crates/ninox-overview/src/tree.rs`'s existing `#[cfg(test)] mod tests`:

```rust
#[test]
fn a_depth_two_chains_root_is_the_top_ancestor_for_every_node_in_it() {
    let nodes = [worker("root"), worker("mid"), worker("leaf")];
    let edges = [hierarchy("root", "mid"), hierarchy("mid", "leaf")];
    let tree = OverviewTree::build(&nodes, &edges);
    assert_eq!(tree.roots["root"], "root");
    assert_eq!(tree.roots["mid"], "root");
    assert_eq!(tree.roots["leaf"], "root");
}

#[test]
fn a_standalone_nodes_root_is_itself() {
    let nodes = [worker("standalone")];
    let tree = OverviewTree::build(&nodes, &[]);
    assert_eq!(tree.roots["standalone"], "standalone");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-overview tree::`
Expected: fails to compile (`roots` field doesn't exist).

- [ ] **Step 3: Add `roots` to `OverviewTree` and populate it in `build`**

Add `pub roots: HashMap<NodeId, NodeId>` to the `OverviewTree` struct (alongside `depth`/`breadcrumb`). In `build`'s existing per-node loop, immediately after the existing `chain.reverse();` line, insert `tree.roots.insert(node.id().to_string(), chain[0].to_string());` — `chain[0]` is already the root id at that point (the same value the existing `breadcrumb` line turns into a name one line later). No new walk, no new data dependency.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox-overview tree::`
Expected: all pass, including the two pre-existing tests (unaffected).

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-overview/src/tree.rs
git commit -m "feat(ninox-overview): expose each node's tree root on OverviewTree"
```

---

### Task 3: Island grouping, summaries, and machines

**Files:**
- Create: `crates/ninox-overview/src/islands.rs`

**Interfaces:**
- Consumes: Task 1's `classify_member`/`MemberStatus`; Task 2's `tree.roots`; `ninox_core::fleet::snapshot::{FleetSnapshot, FleetMember, Role}`; `ninox_core::store::WorkRequestRow`; `ninox_core::types::MachineHealth`; `crate::snapshot_adapter::from_snapshot`; `crate::tree::OverviewTree`; `crate::node::{NodeId, OverviewNode}`.
- Produces: `IslandKey`, `IslandSummary`, `IslandMachine`, `Island`, `build_islands(snapshot: &FleetSnapshot, machine_health: &HashMap<String, MachineHealth>) -> Vec<Island>`. Consumed by Task 4 (sorting/grouping, same file) and Task 5 (`nav.rs`).

- [ ] **Step 1: Write the failing tests**

```rust
// crates/ninox-overview/src/islands.rs
#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::MemberStatus;
    use ninox_core::fleet::snapshot::{FleetMember, FleetSnapshot, Role};
    use ninox_core::fleet::probe::WorkspaceObservation;
    use ninox_core::store::{FleetRecord, WorkRequestRow};
    use ninox_core::types::{ActivityState, MachineHealth, Session, SessionStatus};
    use std::collections::HashMap;

    fn session(id: &str, orch: Option<&str>) -> Session {
        Session {
            id: id.into(), orchestrator_id: orch.map(str::to_string), name: format!("{id}-name"),
            repo: "o/r".into(), status: SessionStatus::Working, agent_type: "claude-code".into(), cost_usd: 0.0,
            started_at: 0, pr_number: None, pr_id: None,
            workspace_path: Some(format!("/ws/{id}")), pid: None, model: None,
            context_tokens: None, catalogue_path: None, context_used_pct: None,
            context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: ActivityState::default(), activity_note: None, activity_since: None,
            machine_id: None, worker_type: None,
        }
    }

    fn member(session: Session, role: Role) -> FleetMember {
        FleetMember {
            session, role, record: FleetRecord::default(), pr: None, ci: None,
            pending_inbox: 0, can_resume: true,
            workspace: WorkspaceObservation { exists: true, is_git: true, branch: None, dirty: Some(false) },
            anomalies: Vec::new(),
        }
    }

    fn snapshot(members: Vec<FleetMember>, open_requests: Vec<WorkRequestRow>) -> FleetSnapshot {
        FleetSnapshot::assemble(members, open_requests, vec![], 0)
    }

    #[test]
    fn a_sub_orchestrator_and_its_workers_join_the_roots_island() {
        let root = member(session("root", None), Role::Orchestrator);
        let sub = member(session("sub", Some("root")), Role::Orchestrator);
        let leaf = member(session("leaf", Some("sub")), Role::Worker);
        let islands = build_islands(&snapshot(vec![root, sub, leaf], vec![]), &HashMap::new());

        assert_eq!(islands.len(), 1);
        assert_eq!(islands[0].key, IslandKey::Root("root".into()));
        let mut members = islands[0].members.clone();
        members.sort();
        assert_eq!(members, vec!["leaf".to_string(), "root".to_string(), "sub".to_string()]);
    }

    #[test]
    fn a_worker_whose_orchestrator_is_missing_from_the_snapshot_becomes_its_own_island() {
        let orphan = member(session("orphan", Some("gone")), Role::Worker);
        let islands = build_islands(&snapshot(vec![orphan], vec![]), &HashMap::new());

        assert_eq!(islands.len(), 1);
        assert_eq!(islands[0].key, IslandKey::Root("orphan".into()));
        assert_ne!(islands[0].key, IslandKey::Unparented);
    }

    #[test]
    fn standalone_sessions_join_the_single_unparented_group() {
        let a = member(session("a", None), Role::Standalone);
        let b = member(session("b", None), Role::Standalone);
        let islands = build_islands(&snapshot(vec![a, b], vec![]), &HashMap::new());

        assert_eq!(islands.len(), 1);
        assert_eq!(islands[0].key, IslandKey::Unparented);
        assert_eq!(islands[0].members.len(), 2);
    }

    #[test]
    fn summary_counts_land_in_the_right_buckets_and_done_vs_stopped_split() {
        let mut working = session("w", Some("root"));
        working.status = SessionStatus::Working;
        let mut needs_you = session("n", Some("root"));
        needs_you.activity = ActivityState::Blocked;
        let mut waiting = session("p", Some("root"));
        waiting.status = SessionStatus::PrOpen;
        let mut done = session("d", Some("root"));
        done.status = SessionStatus::Done;
        let mut stopped = session("t", Some("root"));
        stopped.status = SessionStatus::Terminated;

        let root = member(session("root", None), Role::Orchestrator);
        let members = vec![root, member(working, Role::Worker), member(needs_you, Role::Worker),
            member(waiting, Role::Worker), member(done, Role::Worker), member(stopped, Role::Worker)];
        let islands = build_islands(&snapshot(members, vec![]), &HashMap::new());

        let s = &islands[0].summary;
        // root (default session() status is Working) + the explicit `working` session = 2 working.
        assert_eq!((s.working, s.needs_you, s.waiting_on_pr, s.done, s.stopped), (2, 1, 1, 1, 1));
        assert_eq!(s.active(), 4);
        assert_eq!(s.total(), 6);
    }

    #[test]
    fn an_unresolved_work_request_for_the_root_bumps_it_to_needs_you() {
        let root = member(session("root", None), Role::Orchestrator);
        let req = WorkRequestRow {
            id: "r1".into(), from_session: "somewhere".into(), orchestrator_id: Some("root".into()),
            body: "extra scope".into(), created_at: 0, delivered_at: None, resolved_at: None,
        };
        let islands = build_islands(&snapshot(vec![root], vec![req]), &HashMap::new());

        assert_eq!(islands[0].summary.needs_you, 1);
    }

    #[test]
    fn a_resolved_work_request_does_not_count() {
        let root = member(session("root", None), Role::Orchestrator);
        let req = WorkRequestRow {
            id: "r1".into(), from_session: "somewhere".into(), orchestrator_id: Some("root".into()),
            body: "extra scope".into(), created_at: 0, delivered_at: None, resolved_at: Some(5),
        };
        let islands = build_islands(&snapshot(vec![root], vec![req]), &HashMap::new());

        assert_eq!(islands[0].summary.needs_you, 0);
    }

    #[test]
    fn a_remote_roots_island_carries_machine_id_and_known_health() {
        let mut root = session("root", None);
        root.machine_id = Some("m1".into());
        let mut health = HashMap::new();
        health.insert("m1".to_string(), MachineHealth::Reconnecting);
        let islands = build_islands(&snapshot(vec![member(root, Role::Orchestrator)], vec![]), &health);

        let machine = islands[0].machine.as_ref().expect("remote island must carry a machine");
        assert_eq!(machine.machine_id, "m1");
        assert_eq!(machine.health, Some(MachineHealth::Reconnecting));
    }

    #[test]
    fn a_remote_roots_island_with_no_health_entry_yet_is_none_not_unreachable() {
        let mut root = session("root", None);
        root.machine_id = Some("m2".into());
        let islands = build_islands(&snapshot(vec![member(root, Role::Orchestrator)], vec![]), &HashMap::new());

        assert_eq!(islands[0].machine.as_ref().unwrap().health, None);
    }

    #[test]
    fn a_local_islands_machine_is_none() {
        let root = member(session("root", None), Role::Orchestrator);
        let islands = build_islands(&snapshot(vec![root], vec![]), &HashMap::new());
        assert!(islands[0].machine.is_none());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-overview islands::`
Expected: fails to compile (module doesn't exist yet).

- [ ] **Step 3: Implement `islands.rs`**

```rust
use crate::node::NodeId;
use crate::status::{classify_member, MemberStatus};
use crate::tree::OverviewTree;
use ninox_core::fleet::snapshot::{FleetMember, FleetSnapshot, Role};
use ninox_core::types::MachineHealth;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IslandKey { Root(NodeId), Unparented }

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IslandSummary {
    pub working: usize,
    pub needs_you: usize,
    pub waiting_on_pr: usize,
    pub done: usize,
    pub stopped: usize,
}

impl IslandSummary {
    pub fn active(&self) -> usize { self.working + self.needs_you + self.waiting_on_pr }
    pub fn total(&self) -> usize { self.active() + self.done + self.stopped }
    fn record(&mut self, status: MemberStatus) {
        match status {
            MemberStatus::Working => self.working += 1,
            MemberStatus::NeedsYou => self.needs_you += 1,
            MemberStatus::WaitingOnPr => self.waiting_on_pr += 1,
            MemberStatus::Done => self.done += 1,
            MemberStatus::Stopped => self.stopped += 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct IslandMachine { pub machine_id: String, pub health: Option<MachineHealth> }

#[derive(Debug, Clone, PartialEq)]
pub struct Island {
    pub key: IslandKey,
    pub name: String,
    pub root: Option<NodeId>,
    pub members: Vec<NodeId>,
    pub summary: IslandSummary,
    pub machine: Option<IslandMachine>,
}

pub const UNPARENTED_NAME: &str = "Unparented sessions";

pub fn build_islands(snapshot: &FleetSnapshot, machine_health: &HashMap<String, MachineHealth>) -> Vec<Island> {
    let (nodes, edges) = crate::snapshot_adapter::from_snapshot(snapshot);
    let tree = OverviewTree::build(&nodes, &edges);
    let by_id: HashMap<&str, &FleetMember> = snapshot.members.iter().map(|m| (m.id(), m)).collect();

    let pending_for: HashSet<&str> = snapshot.open_requests.iter()
        .filter(|r| r.resolved_at.is_none())
        .filter_map(|r| r.orchestrator_id.as_deref())
        .collect();

    let mut by_key: HashMap<IslandKey, Island> = HashMap::new();
    for m in &snapshot.members {
        let key = if m.role == Role::Standalone {
            IslandKey::Unparented
        } else {
            IslandKey::Root(tree.roots.get(m.id()).cloned().unwrap_or_else(|| m.id().to_string()))
        };
        let island = by_key.entry(key.clone()).or_insert_with(|| Island {
            root: match &key { IslandKey::Root(id) => Some(id.clone()), IslandKey::Unparented => None },
            name: match &key {
                IslandKey::Root(id) => by_id.get(id.as_str()).map(|m| m.session.name.clone()).unwrap_or_else(|| id.clone()),
                IslandKey::Unparented => UNPARENTED_NAME.to_string(),
            },
            key, members: Vec::new(), summary: IslandSummary::default(), machine: None,
        });
        island.members.push(m.id().to_string());
        let pending = pending_for.contains(m.id());
        island.summary.record(classify_member(&m.session, pending));
    }

    // Machine is derived from the ROOT's session only, in a second pass —
    // not from whichever member the main loop happens to visit last — so a
    // worker that happens to carry its own machine_id never overwrites the
    // island's machine, and Unparented (no single root) never gets one.
    for island in by_key.values_mut() {
        let Some(root_id) = &island.root else { continue };
        let Some(root_member) = by_id.get(root_id.as_str()) else { continue };
        if let Some(machine_id) = &root_member.session.machine_id {
            island.machine = Some(IslandMachine { machine_id: machine_id.clone(), health: machine_health.get(machine_id).copied() });
        }
    }

    by_key.into_values().collect()
}
```

Notes for the implementer: `by_id` is built once over all members (not just the current island) so both the root's display name and its machine lookup resolve regardless of `snapshot.members`'s arbitrary iteration order. `nodes`/`edges`/`tree` reuse the exact phase-1 derivation (`snapshot_adapter::from_snapshot` + `OverviewTree::build`) rather than re-deriving hierarchy — single source of truth, per the Architecture note. The final `by_key.into_values().collect()` yields islands in unspecified order; Task 4's sort is what gives callers a defined order, so this function alone does not need to be deterministic in output order (tests above only assert on counts/keys/fields, never raw `Vec` order).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox-overview islands::`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-overview/src/islands.rs
git commit -m "feat(ninox-overview): build fleet islands from a snapshot with per-island summaries"
```

---

### Task 4: Sorting and machine grouping

**Files:**
- Modify: `crates/ninox-overview/src/islands.rs`

**Interfaces:**
- Consumes: Task 3's `Island`/`IslandSummary`/`IslandKey`.
- Produces: `IslandSort` enum, `sort_islands(islands: &mut [Island], mode: IslandSort)`, `group_by_machine(islands: &[Island]) -> BTreeMap<Option<String>, Vec<IslandKey>>`. Consumed by MLOPS-5339/5350 (not by any other task here).

- [ ] **Step 1: Write the failing tests**

```rust
// add to islands.rs's existing #[cfg(test)] mod tests
fn island(key: IslandKey, name: &str, summary: IslandSummary) -> Island {
    Island { key, name: name.into(), root: None, members: vec![], summary, machine: None }
}

#[test]
fn needs_you_first_sorts_any_needs_you_island_before_any_without_one() {
    let mut islands = vec![
        island(IslandKey::Root("b".into()), "Bravo", IslandSummary { working: 5, ..Default::default() }),
        island(IslandKey::Root("a".into()), "Alpha", IslandSummary { needs_you: 1, ..Default::default() }),
    ];
    sort_islands(&mut islands, IslandSort::NeedsYouFirst);
    assert_eq!(islands[0].name, "Alpha");
}

#[test]
fn needs_you_first_breaks_ties_by_most_active() {
    let mut islands = vec![
        island(IslandKey::Root("a".into()), "Alpha", IslandSummary { needs_you: 1, working: 1, ..Default::default() }),
        island(IslandKey::Root("b".into()), "Bravo", IslandSummary { needs_you: 1, working: 5, ..Default::default() }),
    ];
    sort_islands(&mut islands, IslandSort::NeedsYouFirst);
    assert_eq!(islands[0].name, "Bravo");
}

#[test]
fn most_active_orders_by_active_count_descending() {
    let mut islands = vec![
        island(IslandKey::Root("a".into()), "Alpha", IslandSummary { working: 1, ..Default::default() }),
        island(IslandKey::Root("b".into()), "Bravo", IslandSummary { working: 9, ..Default::default() }),
    ];
    sort_islands(&mut islands, IslandSort::MostActive);
    assert_eq!(islands[0].name, "Bravo");
}

#[test]
fn a_to_z_is_case_insensitive() {
    let mut islands = vec![
        island(IslandKey::Root("a".into()), "ninox", IslandSummary::default()),
        island(IslandKey::Root("b".into()), "Archipelago", IslandSummary::default()),
    ];
    sort_islands(&mut islands, IslandSort::AToZ);
    assert_eq!(islands[0].name, "Archipelago");
}

#[test]
fn sorting_is_a_panic_free_noop_on_an_empty_list() {
    let mut islands: Vec<Island> = vec![];
    sort_islands(&mut islands, IslandSort::NeedsYouFirst);
    assert!(islands.is_empty());
}

#[test]
fn group_by_machine_separates_local_from_each_remote_machine() {
    let local = island(IslandKey::Root("a".into()), "Local", IslandSummary::default());
    let mut remote = island(IslandKey::Root("b".into()), "Remote", IslandSummary::default());
    remote.machine = Some(IslandMachine { machine_id: "m1".into(), health: None });
    let groups = group_by_machine(&[local, remote]);
    assert_eq!(groups[&None], vec![IslandKey::Root("a".into())]);
    assert_eq!(groups[&Some("m1".to_string())], vec![IslandKey::Root("b".into())]);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-overview islands::`
Expected: fails to compile (`IslandSort`/`sort_islands`/`group_by_machine` undefined).

- [ ] **Step 3: Implement sorting and grouping in `islands.rs`**

```rust
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IslandSort { NeedsYouFirst, MostActive, AToZ }

pub fn sort_islands(islands: &mut [Island], mode: IslandSort) {
    match mode {
        IslandSort::NeedsYouFirst => islands.sort_by(|a, b| {
            (b.summary.needs_you > 0).cmp(&(a.summary.needs_you > 0))
                .then(b.summary.active().cmp(&a.summary.active()))
        }),
        IslandSort::MostActive => islands.sort_by(|a, b| b.summary.active().cmp(&a.summary.active())),
        IslandSort::AToZ => islands.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
    }
}

pub fn group_by_machine(islands: &[Island]) -> BTreeMap<Option<String>, Vec<IslandKey>> {
    let mut groups: BTreeMap<Option<String>, Vec<IslandKey>> = BTreeMap::new();
    for island in islands {
        let machine_id = island.machine.as_ref().map(|m| m.machine_id.clone());
        groups.entry(machine_id).or_default().push(island.key.clone());
    }
    groups
}
```

`[T]::sort_by` is a stable sort, which is what makes the tie-breaking in the Review Focus item deterministic — do not switch to `sort_unstable_by`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox-overview islands::`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-overview/src/islands.rs
git commit -m "feat(ninox-overview): island sorting (needs-you/most-active/A-Z) and machine grouping"
```

---

### Task 5: Fleet-tab navigation state

**Files:**
- Create: `crates/ninox-overview/src/nav.rs`

**Interfaces:**
- Consumes: Task 3/4's `Island`, `IslandKey`; `crate::node::NodeId`.
- Produces: `FleetNav` struct with `selected: Option<NodeId>`, `open: Option<IslandKey>`, `tree_collapsed: bool`, and methods `open_island`, `close_island`, `select`, `open_node`, `toggle_tree`; `owning_island(islands: &[Island], node_id: &str) -> Option<IslandKey>`.

- [ ] **Step 1: Write the failing tests**

```rust
// crates/ninox-overview/src/nav.rs
#[cfg(test)]
mod tests {
    use super::*;
    use crate::islands::{Island, IslandKey, IslandSummary};

    fn island(key: IslandKey, members: Vec<&str>) -> Island {
        Island { key, name: "x".into(), root: None, members: members.into_iter().map(String::from).collect(), summary: IslandSummary::default(), machine: None }
    }

    #[test]
    fn tree_starts_collapsed_by_default() {
        assert!(FleetNav::default().tree_collapsed);
    }

    #[test]
    fn toggle_tree_flips_the_flag() {
        let mut nav = FleetNav::default();
        nav.toggle_tree();
        assert!(!nav.tree_collapsed);
    }

    #[test]
    fn closing_an_island_preserves_the_current_selection() {
        let mut nav = FleetNav::default();
        nav.select("leaf");
        nav.open_island(IslandKey::Root("root".into()));
        nav.close_island();
        assert_eq!(nav.open, None);
        assert_eq!(nav.selected.as_deref(), Some("leaf"));
    }

    #[test]
    fn open_node_opens_the_owning_island_and_selects_the_node() {
        let islands = vec![island(IslandKey::Root("root".into()), vec!["root", "leaf"])];
        let mut nav = FleetNav::default();
        nav.open_node("leaf", &islands);
        assert_eq!(nav.open, Some(IslandKey::Root("root".into())));
        assert_eq!(nav.selected.as_deref(), Some("leaf"));
    }

    #[test]
    fn open_node_resolves_the_unparented_group_too() {
        let islands = vec![island(IslandKey::Unparented, vec!["solo"])];
        let mut nav = FleetNav::default();
        nav.open_node("solo", &islands);
        assert_eq!(nav.open, Some(IslandKey::Unparented));
    }

    #[test]
    fn owning_island_is_none_for_an_id_in_no_island() {
        let islands = vec![island(IslandKey::Root("root".into()), vec!["root"])];
        assert_eq!(owning_island(&islands, "nowhere"), None);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ninox-overview nav::`
Expected: fails to compile.

- [ ] **Step 3: Implement `FleetNav` and `owning_island` in `crates/ninox-overview/src/nav.rs`**

```rust
use crate::islands::{Island, IslandKey};
use crate::node::NodeId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetNav {
    pub selected: Option<NodeId>,
    pub open: Option<IslandKey>,
    pub tree_collapsed: bool,
}

impl Default for FleetNav {
    fn default() -> Self { Self { selected: None, open: None, tree_collapsed: true } }
}

impl FleetNav {
    pub fn open_island(&mut self, key: IslandKey) { self.open = Some(key); }
    pub fn close_island(&mut self) { self.open = None; }
    pub fn select(&mut self, id: impl Into<NodeId>) { self.selected = Some(id.into()); }
    pub fn toggle_tree(&mut self) { self.tree_collapsed = !self.tree_collapsed; }

    /// Composite action for "click a worker in the tree": opens the worker's
    /// owning island (if found) and selects the worker regardless.
    pub fn open_node(&mut self, id: impl Into<NodeId>, islands: &[Island]) {
        let id = id.into();
        if let Some(key) = owning_island(islands, &id) {
            self.open = Some(key);
        }
        self.selected = Some(id);
    }
}

pub fn owning_island(islands: &[Island], node_id: &str) -> Option<IslandKey> {
    islands.iter().find(|i| i.members.iter().any(|m| m == node_id)).map(|i| i.key.clone())
}
```

`FleetNav` needs `#[derive(Debug, Clone, PartialEq, Eq)]`; `IslandKey` already derives `PartialEq, Eq, Hash` from Task 3, so this composes without further changes there.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ninox-overview nav::`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/ninox-overview/src/nav.rs
git commit -m "feat(ninox-overview): Fleet-tab navigation state (open island, selection, tree-collapsed)"
```

---

### Task 6: Wire up, workspace verification, PR

**Files:**
- Modify: `crates/ninox-overview/src/lib.rs`

- [ ] **Step 1: Add the three new modules to `lib.rs`**

```rust
pub mod controls;
pub mod edge;
pub mod islands;
pub mod nav;
pub mod node;
pub mod plan;
pub mod snapshot_adapter;
pub mod state;
pub mod status;
pub mod tree;
```

(Keep the existing alphabetical ordering the file already uses.)

- [ ] **Step 2: Full workspace verification**

Run, in order, fixing anything that fails before moving on:
- `cargo build` — expected: clean.
- `cargo test --workspace` — expected: all pass, including every test from Tasks 1–5 and all pre-existing `ninox-overview`/`ninox-tui` tests unaffected.
- `cargo clippy --workspace --all-targets -- -D warnings` — expected: clean.

- [ ] **Step 3: Commit (if clippy/fmt-of-touched-lines required any fixes)**

```bash
git add -A
git commit -m "fix(ninox-overview): clippy cleanup"
```
(Skip this step and commit if there is nothing to fix.)

- [ ] **Step 4: Rebase onto the latest `origin/v1`**

```bash
git fetch origin v1
git rebase origin/v1
```
Re-run Step 2's three commands after any rebase conflict resolution.

- [ ] **Step 5: Open the PR**

Title: `MLOPS-5351: feat(ninox-overview): fleet island model`. Body sections `## Why` / `## Summary` / `## Test plan`, concise. In `## Summary`, document the public API surface precisely (every new public type/function from Tasks 1–5) so MLOPS-5339 and MLOPS-5350 can consume it without reading the source — this satisfies the ticket's "public API is documented in the PR body" acceptance criterion. Note the two documented design calls explicitly (dangling-orchestrator workers get their own singleton island rather than joining Unparented; a pending `ninox request-work` item bumps its orchestrator to needs-you) since both resolve a spec ambiguity rather than restating something explicit in the brief. If `now-playing` outputs a track, append it as its own line at the bottom.

- [ ] **Step 6: Message the orchestrator**

```bash
ninox send ninox "MLOPS-5351 PR open: <url> at <sha>. Public API: status::{MemberStatus, classify_member}, islands::{IslandKey, IslandSummary, IslandMachine, Island, build_islands, IslandSort, sort_islands, group_by_machine}, nav::{FleetNav, owning_island}, plus OverviewTree::roots. No ninox-core/GUI/TUI changes."
```
