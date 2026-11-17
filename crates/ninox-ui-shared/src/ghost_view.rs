//! What the GUI and TUI show for ghost plans: which plans appear where,
//! which reply-drawer turn proposed which plan, and the shared wording of
//! cards, badges and the accept confirm (`PromptReply.dc.html`).

use ninox_core::ghost_plan::{self, NodeRow, NodeState, PlanStatus, PlanWithNodes};
use ninox_core::store::Store;
use ninox_core::worker_type::PROMPT_SESSION_ID;

/// Shown under every set of ghost cards.
pub const NOTHING_SPAWNS: &str = "Nothing is spawned until you accept.";

/// Plans the UI lists: every pending plan, then decided or expired ones
/// from the last day (so an accept or expiry stays visible for a while),
/// newest first within each group. Expires due plans first.
pub fn load(store: &Store, now_ms: i64) -> Vec<PlanWithNodes> {
    let _ = store.expire_ghost_plans(now_ms);
    let mut plans = store.list_ghost_plans(false).unwrap_or_default();
    plans.retain(|p| p.plan.status == PlanStatus::Pending || now_ms - p.plan.decided_at.unwrap_or(p.plan.expires_at) < RECENT_MS);
    plans.sort_by_key(|p| (p.plan.status != PlanStatus::Pending, std::cmp::Reverse(p.plan.created_at)));
    plans
}

/// How long a decided or expired plan stays in the Plans list.
/// Arbitrary; long enough to see what happened after a break.
const RECENT_MS: i64 = 24 * 60 * 60 * 1000;

/// Pending plans that start new top-level sessions: each is drawn as
/// its own dashed ghost island in Fleet.
pub fn ghost_islands(plans: &[PlanWithNodes]) -> Vec<&PlanWithNodes> {
    plans.iter().filter(|p| p.plan.status == PlanStatus::Pending && p.plan.target_parent.is_none()).collect()
}

/// Pending plans attached under `orchestrator`: drawn as dashed ghost
/// nodes inside that orchestrator's island.
pub fn ghosts_under<'a>(plans: &'a [PlanWithNodes], orchestrator: &str) -> Vec<&'a PlanWithNodes> {
    plans
        .iter()
        .filter(|p| p.plan.status == PlanStatus::Pending && p.plan.target_parent.as_deref() == Some(orchestrator))
        .collect()
}

/// For each turn (by its `asked_at_ms`, in order), the ids of the plans the
/// prompt agent proposed during it: from the turn's start until the next
/// turn's start.
pub fn plans_per_turn(asked_at: &[i64], plans: &[PlanWithNodes]) -> Vec<Vec<String>> {
    asked_at
        .iter()
        .enumerate()
        .map(|(i, from)| {
            ghost_plan::plans_in_window(plans, PROMPT_SESSION_ID, *from, asked_at.get(i + 1).copied())
                .into_iter()
                .map(|p| p.plan.id.clone())
                .collect()
        })
        .collect()
}

/// Shown under a prompt-agent answer that talks about accepting a proposal
/// when the store holds no plan from that turn.
pub const NO_PLAN_STORED: &str = "No plan was stored during this turn — there is nothing to accept.";

/// Whether a finished prompt-agent turn's `answer` tells the user to accept
/// a proposal although the store backs none: no plan from this turn
/// (`turn_plans`) and no prompt-agent plan still pending in `plans` (which
/// an answer may legitimately point back to). Turns older than the plan
/// list's own window are never flagged, since a real plan from them may
/// have aged out of `plans`.
pub fn claims_unstored_plan(
    answer: &str,
    finished: bool,
    turn_plans: &[String],
    plans: &[PlanWithNodes],
    asked_at_ms: i64,
    now_ms: i64,
) -> bool {
    if !finished || !turn_plans.is_empty() || now_ms - asked_at_ms >= RECENT_MS {
        return false;
    }
    if plans.iter().any(|p| p.plan.proposer == PROMPT_SESSION_ID && p.plan.status == PlanStatus::Pending) {
        return false;
    }
    let answer = answer.to_lowercase();
    let words: Vec<&str> = answer.split(|c: char| !c.is_alphabetic()).collect();
    words.iter().any(|w| matches!(*w, "accept" | "accepting"))
        && words.iter().any(|w| w.starts_with("propos") || w.starts_with("plan"))
}

/// Undecided nodes, in display (top-down) order.
pub fn ghost_nodes(plan: &PlanWithNodes) -> Vec<&NodeRow> {
    plan.top_down().into_iter().filter(|n| n.state == NodeState::Ghost).collect()
}

/// Indent depth of a node within its plan (0 = top section).
pub fn depth(plan: &PlanWithNodes, node: &NodeRow) -> usize {
    let mut d = 0;
    let mut cur = node.parent_node_id.as_deref();
    while let Some(p) = cur.and_then(|id| plan.node(id)) {
        d += 1;
        cur = p.parent_node_id.as_deref();
        if d > plan.nodes.len() {
            break;
        }
    }
    d
}

/// "GHOST ORCHESTRATOR", "GHOST IMPLEMENTOR".
pub fn badge(node: &NodeRow) -> String {
    format!("GHOST {}", node.worker_type.to_uppercase())
}

pub fn accept_label(plan: &PlanWithNodes) -> String {
    format!("Accept plan · spawn {}", plan.ghost_count())
}

/// The inline confirm before an accept.
pub fn confirm_text(plan: &PlanWithNodes) -> String {
    match plan.ghost_count() {
        1 => "Spawn 1 session?".to_string(),
        n => format!("Spawn {n} sessions?"),
    }
}

pub fn discard_confirm_text(plan: &PlanWithNodes) -> String {
    match plan.ghost_count() {
        1 => "Discard 1 proposed session?".to_string(),
        n => format!("Discard {n} proposed sessions?"),
    }
}

/// "pending · 3 nodes · proposed by ninox-prompt", "expired".
pub fn plan_status_line(plan: &PlanWithNodes) -> String {
    let who = if plan.plan.proposer == PROMPT_SESSION_ID { "the Ninox prompt" } else { plan.plan.proposer.as_str() };
    match plan.plan.status {
        PlanStatus::Pending => format!("pending · {} to review · proposed by {who}", plan.ghost_count()),
        PlanStatus::Accepted => format!("accepted by {}", plan.plan.decided_by.as_deref().unwrap_or("?")),
        PlanStatus::Discarded => "discarded".to_string(),
        PlanStatus::Expired => "expired — never accepted".to_string(),
    }
}

pub fn plan_title(plan: &PlanWithNodes) -> String {
    plan.plan.title.clone().unwrap_or_else(|| match plan.plan.target_parent.as_deref() {
        Some(t) => format!("Plan under {t}"),
        None => format!("{} new orchestrators", plan.nodes.len()),
    })
}

/// "After the map lands · depends on map-dashboard-queries."
pub fn dependency_line(plan: &PlanWithNodes, node: &NodeRow) -> Option<String> {
    if node.dependencies.is_empty() {
        return None;
    }
    let names: Vec<&str> = node.dependencies.iter().map(|d| plan.node(d).map_or(d.as_str(), |n| n.name.as_str())).collect();
    let verb = if node.await_done { "waits for" } else { "depends on" };
    Some(format!("{verb} {}", names.join(", ")))
}

/// One-line node state for an accepted plan's rows ("spawned → map-queries").
pub fn node_state_line(node: &NodeRow) -> String {
    match (node.state, node.session_id.as_deref()) {
        (NodeState::Spawned, Some(sid)) => format!("spawned → {sid}"),
        (NodeState::Failed, _) => format!("failed: {}", node.error.as_deref().unwrap_or("?")),
        (NodeState::Waiting, _) => node.error.clone().unwrap_or_else(|| "waiting".into()),
        (s, _) => s.as_str().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ninox_core::ghost_plan::{NodeState, PlanRow};

    fn node(id: &str, parent: Option<&str>, ty: &str, deps: &[&str], state: NodeState) -> NodeRow {
        NodeRow {
            plan_id: "gp-1".into(),
            node_id: id.into(),
            parent_node_id: parent.map(str::to_string),
            worker_type: ty.into(),
            name: id.into(),
            brief: format!("brief {id}"),
            workspace: None,
            model: None,
            brain: None,
            dependencies: deps.iter().map(|d| d.to_string()).collect(),
            await_done: false,
            linear_ref: None,
            state,
            session_id: None,
            error: None,
            updated_at: 0,
            seq: 0,
        }
    }

    fn plan(id: &str, proposer: &str, created_at: i64, target: Option<&str>, status: PlanStatus, nodes: Vec<NodeRow>) -> PlanWithNodes {
        PlanWithNodes {
            plan: PlanRow {
                id: id.into(),
                proposer: proposer.into(),
                created_at,
                expires_at: created_at + 1000,
                status,
                target_parent: target.map(str::to_string),
                title: None,
                decided_by: None,
                decided_at: None,
            },
            nodes,
        }
    }

    #[test]
    fn drawer_turns_get_the_prompt_agents_plans_from_their_window() {
        let plans = vec![
            plan("a", PROMPT_SESSION_ID, 150, None, PlanStatus::Pending, vec![]),
            plan("user", "user", 160, None, PlanStatus::Pending, vec![]),
            plan("b", PROMPT_SESSION_ID, 260, None, PlanStatus::Pending, vec![]),
            plan("c", PROMPT_SESSION_ID, 400, None, PlanStatus::Accepted, vec![]),
        ];
        let per_turn = plans_per_turn(&[100, 200, 300], &plans);
        assert_eq!(per_turn, vec![vec!["a".to_string()], vec!["b".to_string()], vec!["c".to_string()]]);
        assert!(plans_per_turn(&[500], &plans)[0].is_empty());
    }

    #[test]
    fn fleet_placement_splits_top_level_from_attached_plans() {
        let plans = vec![
            plan("top", "user", 1, None, PlanStatus::Pending, vec![]),
            plan("under", "user", 2, Some("ninox"), PlanStatus::Pending, vec![]),
            plan("old", "user", 3, None, PlanStatus::Expired, vec![]),
        ];
        assert_eq!(ghost_islands(&plans).iter().map(|p| p.plan.id.as_str()).collect::<Vec<_>>(), ["top"]);
        assert_eq!(ghosts_under(&plans, "ninox").len(), 1);
        assert!(ghosts_under(&plans, "other").is_empty());
    }

    #[test]
    fn card_wording_matches_the_board() {
        let p = plan("p", PROMPT_SESSION_ID, 1, None, PlanStatus::Pending, vec![
            node("lead", None, "Orchestrator", &[], NodeState::Ghost),
            node("map", Some("lead"), "Researcher", &[], NodeState::Ghost),
            node("port", Some("lead"), "Implementor", &["map"], NodeState::Ghost),
            node("gone", Some("lead"), "Researcher", &[], NodeState::Discarded),
        ]);
        assert_eq!(accept_label(&p), "Accept plan · spawn 3");
        assert_eq!(confirm_text(&p), "Spawn 3 sessions?");
        assert_eq!(badge(p.node("lead").unwrap()), "GHOST ORCHESTRATOR");
        assert_eq!(dependency_line(&p, p.node("port").unwrap()).as_deref(), Some("depends on map"));
        assert_eq!(depth(&p, p.node("port").unwrap()), 1);
        assert_eq!(ghost_nodes(&p).len(), 3);
        assert!(plan_status_line(&p).contains("the Ninox prompt"));
    }

    #[test]
    fn an_answer_saying_accept_with_no_stored_plan_is_flagged() {
        let lie = "I proposed 3 orchestrators. Accept them in the UI to start them.";
        assert!(claims_unstored_plan(lie, true, &[], &[], 0, 10));
        assert!(!claims_unstored_plan(lie, true, &["gp-1".into()], &[], 0, 10), "a stored plan backs it");
        assert!(!claims_unstored_plan(lie, false, &[], &[], 0, 10), "still answering");
        assert!(!claims_unstored_plan("CI is green on #12.", true, &[], &[], 0, 10));
        assert!(!claims_unstored_plan("The plan's acceptance criteria are met.", true, &[], &[], 0, 10));
        assert!(!claims_unstored_plan(lie, true, &[], &[], 0, RECENT_MS), "older than the plan list keeps");
    }

    #[test]
    fn pointing_back_at_an_earlier_pending_plan_is_not_flagged() {
        let answer = "Accept the plan in the Plans tab.";
        let pending = plan("gp-1", PROMPT_SESSION_ID, 0, None, PlanStatus::Pending, vec![]);
        assert!(!claims_unstored_plan(answer, true, &[], &[pending], 0, 10));
        let discarded = plan("gp-1", PROMPT_SESSION_ID, 0, None, PlanStatus::Discarded, vec![]);
        assert!(claims_unstored_plan(answer, true, &[], &[discarded], 0, 10));
    }
}
