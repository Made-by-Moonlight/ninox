//! Fleet island grouping: one island per root orchestrator tree, plus one
//! shared Unparented group, with per-island status summaries and machine
//! info — the data `ninox-gui`/`ninox-tui`'s Fleet tab draws from.
//!
//! An orchestrator's own `MemberStatus` is still computed and stored on its
//! [`IslandMember`] entry (so an opened island can show its own card), but
//! it is deliberately excluded from [`IslandSummary`]'s bucket counts — the
//! design (`FleetAtScale.dc.html`'s `segs`/`active`/`total`) counts workers
//! only.
//!
//! A `ninox request-work` item (`FleetSnapshot.open_requests`) goes from a
//! worker to its *orchestrator*, not to the human user, and orchestrators
//! handle these routinely — it is tracked as [`Island::pending_request`],
//! an informational flag only. It deliberately does NOT feed
//! `MemberStatus::NeedsYou`, [`IslandSummary::needs_you`], or
//! [`Island::needs_attention`] — those are reserved for the user-facing
//! causes `classify_member` already covers (blocked activity, CI failure,
//! and eventually the MLOPS-5184 "ninox user-request" feature, once it
//! ships and can feed `classify_member`'s `has_pending_user_request`).

use crate::node::NodeId;
use crate::status::{classify_member, MemberStatus};
use crate::tree::OverviewTree;
use ninox_core::fleet::snapshot::{FleetMember, FleetSnapshot, Role};
use ninox_core::types::MachineHealth;
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IslandKey { Root(NodeId), Unparented }

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IslandSummary {
    pub working: usize,
    pub needs_you: usize,
    pub waiting_on_pr: usize,
    pub done: usize,
    pub stopped: usize,
    /// Waiting for a slot under the session limit; not active yet.
    pub queued: usize,
}

impl IslandSummary {
    pub fn active(&self) -> usize { self.working + self.needs_you + self.waiting_on_pr }
    pub fn total(&self) -> usize { self.active() + self.done + self.stopped + self.queued }
    fn record(&mut self, status: MemberStatus) {
        match status {
            MemberStatus::Working => self.working += 1,
            MemberStatus::NeedsYou => self.needs_you += 1,
            MemberStatus::WaitingOnPr => self.waiting_on_pr += 1,
            MemberStatus::Done => self.done += 1,
            MemberStatus::Stopped => self.stopped += 1,
            MemberStatus::Queued => self.queued += 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct IslandMachine { pub machine_id: String, pub health: Option<MachineHealth> }

/// One session in an island, with the status a renderer needs to decide
/// whether it's visible or folds into the `▸ N done · M stopped` row —
/// without re-deriving it from the snapshot itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IslandMember { pub id: NodeId, pub status: MemberStatus }

#[derive(Debug, Clone, PartialEq)]
pub struct Island {
    pub key: IslandKey,
    pub name: String,
    pub root: Option<NodeId>,
    pub members: Vec<IslandMember>,
    pub summary: IslandSummary,
    pub machine: Option<IslandMachine>,
    /// True when an orchestrator in this island (root or a sub-orchestrator)
    /// has an unresolved `ninox request-work` item addressed to it.
    /// Informational only — routine orchestrator business, not a signal
    /// that the user needs to act. See the module doc comment.
    pub pending_request: bool,
}

impl Island {
    /// Whether this island should sort/flag as needing attention. Reads
    /// only `summary.needs_you` — `pending_request` deliberately does not
    /// contribute (see the module doc comment).
    pub fn needs_attention(&self) -> bool {
        self.summary.needs_you > 0
    }
}

pub const UNPARENTED_NAME: &str = "Unparented sessions";

/// `Root` islands sort by root id ascending; `Unparented` always sorts
/// last. Used only to give `build_islands`'s returned order a fixed,
/// reproducible basis — `HashMap` iteration order is per-instance random,
/// so without this every call could return a different order even for an
/// identical snapshot, and a stable sort downstream would faithfully
/// preserve that randomness through every tie.
fn sort_rank(key: &IslandKey) -> (u8, &str) {
    match key {
        IslandKey::Root(id) => (0, id.as_str()),
        IslandKey::Unparented => (1, ""),
    }
}

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
            key, members: Vec::new(), summary: IslandSummary::default(), machine: None, pending_request: false,
        });

        // `false`: no real "pending user request" source exists on v1 yet
        // (MLOPS-5184 hasn't shipped) — this is where it will plug in once
        // it does. The work-request queue below is a separate, orchestrator-
        // facing signal that must never reach `classify_member`.
        let status = classify_member(&m.session, false);
        island.members.push(IslandMember { id: m.id().to_string(), status });

        if m.role == Role::Orchestrator {
            if pending_for.contains(m.id()) {
                island.pending_request = true;
            }
        } else {
            island.summary.record(status);
        }
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

    let mut islands: Vec<Island> = by_key.into_values().collect();
    islands.sort_by(|a, b| sort_rank(&a.key).cmp(&sort_rank(&b.key)));
    islands
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IslandSort { NeedsYouFirst, MostActive, AToZ }

/// `[T]::sort_by`/`sort_by_key` are stable sorts, which combined with
/// `build_islands`'s now-fixed input order (see `sort_rank`) is what makes
/// every mode's tie-breaking deterministic — don't switch to an unstable
/// variant, and don't feed this function islands built in a non-fixed
/// order.
pub fn sort_islands(islands: &mut [Island], mode: IslandSort) {
    match mode {
        IslandSort::NeedsYouFirst => islands.sort_by(|a, b| {
            b.needs_attention().cmp(&a.needs_attention())
                .then(b.summary.active().cmp(&a.summary.active()))
        }),
        IslandSort::MostActive => islands.sort_by_key(|i| std::cmp::Reverse(i.summary.active())),
        IslandSort::AToZ => islands.sort_by_key(|i| i.name.to_lowercase()),
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

/// The Fleet's Machine filter (`FleetAtScale.dc.html`'s "All / Local /
/// eu1" control): every island, this machine's, or one remote machine's.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum MachineFilter {
    #[default]
    All,
    Local,
    Machine(String),
}

impl MachineFilter {
    pub fn matches(&self, island: &Island) -> bool {
        match self {
            Self::All => true,
            Self::Local => island.machine.is_none(),
            Self::Machine(id) => island.machine.as_ref().is_some_and(|m| &m.machine_id == id),
        }
    }

    /// The next filter in All → Local → each of `machines` → All.
    pub fn next(&self, machines: &[String]) -> Self {
        match self {
            Self::All => Self::Local,
            Self::Local => machines.first().map_or(Self::All, |m| Self::Machine(m.clone())),
            Self::Machine(id) => match machines.iter().position(|m| m == id) {
                Some(i) => machines.get(i + 1).map_or(Self::All, |m| Self::Machine(m.clone())),
                None => Self::All,
            },
        }
    }
}

/// The remote machines `islands` sit on, sorted and distinct.
pub fn machine_ids(islands: &[Island]) -> Vec<String> {
    let mut ids: Vec<String> = islands.iter().filter_map(|i| i.machine.as_ref().map(|m| m.machine_id.clone())).collect();
    ids.sort();
    ids.dedup();
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::fleet::probe::WorkspaceObservation;
    use ninox_core::fleet::snapshot::{FleetMember, FleetSnapshot, Role};
    use ninox_core::store::{FleetRecord, WorkRequestRow};
    use ninox_core::types::{ActivityState, MachineHealth, Session, SessionStatus};
    use std::collections::HashMap;

    fn session(id: &str, orch: Option<&str>) -> Session {
        Session {
            id: id.into(), orchestrator_id: orch.map(str::to_string), name: format!("{id}-name"),
            repo: "o/r".into(), status: SessionStatus::Working, agent_type: "claude-code".into(), cost_usd: 0.0,
            started_at: 0, pr_number: None, pr_id: None,
            workspace_path: Some(format!("/ws/{id}")), pid: None, model: None, observed_model: None,
            context_tokens: None, brain_reads: Vec::new(), brain_write: None, context_used_pct: None,
            context_total_tokens: None, context_window_size: None,
            claude_session_id: None, summary: None,
            terminal_at: None, gate_status: None, merged_at: None,
            activity: ActivityState::default(), activity_note: None, activity_since: None,
            machine_id: None, worker_type: None,
            success_criteria: Vec::new(), completion_outcome: None, self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }
    }

    fn member(session: Session, role: Role) -> FleetMember {
        FleetMember {
            session, role, record: FleetRecord::default(), pr: None, ci: None,
            pending_inbox: 0, can_resume: true,
            workspace: WorkspaceObservation { exists: true, is_git: true, branch: None, dirty: Some(false), ..Default::default() },
            anomalies: Vec::new(),
        }
    }

    fn snapshot(members: Vec<FleetMember>, open_requests: Vec<WorkRequestRow>) -> FleetSnapshot {
        FleetSnapshot::assemble(members, open_requests, vec![], 0)
    }

    fn member_ids(island: &Island) -> Vec<String> {
        let mut ids: Vec<String> = island.members.iter().map(|m| m.id.clone()).collect();
        ids.sort();
        ids
    }

    fn status_of<'a>(island: &'a Island, id: &str) -> &'a MemberStatus {
        &island.members.iter().find(|m| m.id == id).unwrap().status
    }

    #[test]
    fn a_sub_orchestrator_and_its_workers_join_the_roots_island() {
        let root = member(session("root", None), Role::Orchestrator);
        let sub = member(session("sub", Some("root")), Role::Orchestrator);
        let leaf = member(session("leaf", Some("sub")), Role::Worker);
        let islands = build_islands(&snapshot(vec![root, sub, leaf], vec![]), &HashMap::new());

        assert_eq!(islands.len(), 1);
        assert_eq!(islands[0].key, IslandKey::Root("root".into()));
        assert_eq!(member_ids(&islands[0]), vec!["leaf".to_string(), "root".to_string(), "sub".to_string()]);
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
        // The root orchestrator is excluded from the summary (it's a worker-only bucket count,
        // matching FleetAtScale.dc.html's `segs`/`active`/`total`, computed from `o.workers` only).
        assert_eq!((s.working, s.needs_you, s.waiting_on_pr, s.done, s.stopped), (1, 1, 1, 1, 1));
        assert_eq!(s.active(), 3);
        assert_eq!(s.total(), 5);
    }

    #[test]
    fn an_orchestrators_own_status_is_still_tracked_on_its_member_entry_but_not_in_the_summary() {
        let root = member(session("root", None), Role::Orchestrator);
        let islands = build_islands(&snapshot(vec![root], vec![]), &HashMap::new());

        assert_eq!(*status_of(&islands[0], "root"), MemberStatus::Working);
        assert_eq!(islands[0].summary.working, 0, "orchestrators must not inflate the worker-only summary");
    }

    #[test]
    fn a_done_members_status_is_queryable_from_members_without_re_deriving_it() {
        let mut done = session("d", Some("root"));
        done.status = SessionStatus::Done;
        let root = member(session("root", None), Role::Orchestrator);
        let islands = build_islands(&snapshot(vec![root, member(done, Role::Worker)], vec![]), &HashMap::new());

        assert_eq!(*status_of(&islands[0], "d"), MemberStatus::Done);
    }

    #[test]
    fn an_unresolved_work_request_for_the_root_sets_pending_request_but_is_not_needs_attention() {
        // A `ninox request-work` item goes to the orchestrator, not the
        // user, and orchestrators handle it routinely — it must not float
        // the island to the top of the Fleet tab or badge it as needing you.
        let root = member(session("root", None), Role::Orchestrator);
        let req = WorkRequestRow {
            id: "r1".into(), from_session: "somewhere".into(), orchestrator_id: Some("root".into()),
            body: "extra scope".into(), created_at: 0, delivered_at: None, resolved_at: None,
        };
        let islands = build_islands(&snapshot(vec![root], vec![req]), &HashMap::new());

        assert!(islands[0].pending_request, "still tracked as an informational flag");
        assert!(!islands[0].needs_attention(), "but must not count as needing the user's attention");
        assert_eq!(islands[0].summary.needs_you, 0);
    }

    #[test]
    fn a_resolved_work_request_does_not_count() {
        let root = member(session("root", None), Role::Orchestrator);
        let req = WorkRequestRow {
            id: "r1".into(), from_session: "somewhere".into(), orchestrator_id: Some("root".into()),
            body: "extra scope".into(), created_at: 0, delivered_at: None, resolved_at: Some(5),
        };
        let islands = build_islands(&snapshot(vec![root], vec![req]), &HashMap::new());

        assert!(!islands[0].pending_request);
        assert!(!islands[0].needs_attention());
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

    #[test]
    fn build_islands_returns_a_deterministic_order_across_repeated_calls_on_identical_data() {
        let members: Vec<FleetMember> = (0..12)
            .map(|i| member(session(&format!("root-{i}"), None), Role::Orchestrator))
            .collect();
        let snap = snapshot(members, vec![]);

        let first: Vec<IslandKey> = build_islands(&snap, &HashMap::new()).into_iter().map(|i| i.key).collect();
        let second: Vec<IslandKey> = build_islands(&snap, &HashMap::new()).into_iter().map(|i| i.key).collect();

        assert_eq!(first, second, "build_islands must not depend on HashMap iteration order");
    }

    #[test]
    fn the_machine_filter_tells_local_from_each_remote_and_cycles_through_them() {
        let local = island(IslandKey::Root("a".into()), "a", IslandSummary::default());
        let mut remote = island(IslandKey::Root("b".into()), "b", IslandSummary::default());
        remote.machine = Some(IslandMachine { machine_id: "m1".into(), health: None });
        assert!(MachineFilter::Local.matches(&local) && !MachineFilter::Local.matches(&remote));
        assert!(MachineFilter::Machine("m1".into()).matches(&remote) && !MachineFilter::Machine("m2".into()).matches(&remote));
        assert!(MachineFilter::All.matches(&local) && MachineFilter::All.matches(&remote));
        let machines = machine_ids(&[local, remote.clone(), remote]);
        assert_eq!(machines, vec!["m1"]);
        let mut f = MachineFilter::All;
        let mut seen = Vec::new();
        for _ in 0..4 {
            f = f.next(&machines);
            seen.push(f.clone());
        }
        assert_eq!(seen, vec![MachineFilter::Local, MachineFilter::Machine("m1".into()), MachineFilter::All, MachineFilter::Local]);
        assert_eq!(MachineFilter::Machine("gone".into()).next(&machines), MachineFilter::All);
    }

    fn island(key: IslandKey, name: &str, summary: IslandSummary) -> Island {
        Island { key, name: name.into(), root: None, members: vec![], summary, machine: None, pending_request: false }
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
    fn needs_you_first_does_not_bump_a_bare_pending_request_with_no_other_needs_you_signal() {
        let mut islands = vec![
            island(IslandKey::Root("a".into()), "Alpha", IslandSummary { working: 1, ..Default::default() }),
            island(IslandKey::Root("b".into()), "Bravo", IslandSummary { working: 5, ..Default::default() }),
        ];
        islands[0].pending_request = true;
        sort_islands(&mut islands, IslandSort::NeedsYouFirst);
        assert_eq!(islands[0].name, "Bravo", "Bravo wins on active count; Alpha's pending_request must not outrank it");
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
}
