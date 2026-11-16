//! Accept, discard and spawn ghost plans (`ninox_core::ghost_plan`).
//!
//! The CLI (`ninox plan accept/discard`), the GUI and the TUI all call
//! [`accept`] / [`discard`] directly with the caller's pane-verified session
//! id (`None` for the user), and the rule in
//! `ghost_plan::authorize_decision` runs inside, so no frontend can skip it.
//! Accepted nodes spawn through the normal spawn paths
//! (`spawn_orchestrator_common_opts`, `admit_and_launch_worker`), so session
//! limits, worker-type resolution and brain sets behave exactly as they do
//! for `ninox spawn`. A node whose parent or dependencies aren't ready stays
//! `waiting`; the poller's [`plan_advancer`] spawns it later.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{bail, Context as _};
use ninox_core::config::AppConfig;
use ninox_core::ghost_plan::{self, Decider, NodeRow, NodeState, PlanAdvancer, PlanStatus, PlanWithNodes, Readiness};
use ninox_core::store::Store;
use serde::Serialize;

use crate::cli::SpawnedSession;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum NodeOutcome {
    Spawned { session_id: String },
    Queued { session_id: String, message: String },
    Waiting { reason: String },
    /// Already decided before this call: nothing was done.
    Unchanged { state: NodeState },
    Failed { error: String },
    Discarded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NodeReport {
    pub node_id: String,
    #[serde(flatten)]
    pub outcome: NodeOutcome,
    /// The spawn's [`SpawnedSession::warnings`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DecisionReport {
    pub plan_id: String,
    pub decided_by: String,
    pub status: PlanStatus,
    pub nodes: Vec<NodeReport>,
}

impl DecisionReport {
    pub fn summary(&self) -> String {
        let count = |f: fn(&NodeOutcome) -> bool| self.nodes.iter().filter(|n| f(&n.outcome)).count();
        let spawned = count(|o| matches!(o, NodeOutcome::Spawned { .. }));
        let queued = count(|o| matches!(o, NodeOutcome::Queued { .. }));
        let waiting = count(|o| matches!(o, NodeOutcome::Waiting { .. }));
        let failed = count(|o| matches!(o, NodeOutcome::Failed { .. }));
        let discarded = count(|o| matches!(o, NodeOutcome::Discarded));
        let mut parts = Vec::new();
        for (n, label) in [(spawned, "spawned"), (queued, "queued"), (waiting, "waiting"), (failed, "failed"), (discarded, "discarded")] {
            if n > 0 {
                parts.push(format!("{n} {label}"));
            }
        }
        if parts.is_empty() {
            "nothing changed".to_string()
        } else {
            parts.join(" · ")
        }
    }
}

fn load(store: &Store, plan_id: &str) -> anyhow::Result<PlanWithNodes> {
    store.get_ghost_plan(plan_id)?.with_context(|| format!("no plan '{plan_id}'"))
}

fn selection(plan: &PlanWithNodes, only: Option<&[String]>) -> anyhow::Result<Vec<String>> {
    match only {
        None => Ok(plan.nodes.iter().map(|n| n.node_id.clone()).collect()),
        Some(ids) => {
            for id in ids {
                if plan.node(id).is_none() {
                    bail!("plan {} has no node '{id}' — its nodes are: {}", plan.plan.id,
                        plan.nodes.iter().map(|n| n.node_id.as_str()).collect::<Vec<_>>().join(", "));
                }
            }
            Ok(ids.to_vec())
        }
    }
}

fn decidable(store: &Store, plan_id: &str, caller_session_id: Option<&str>, now: i64) -> anyhow::Result<(PlanWithNodes, Decider)> {
    store.expire_ghost_plans(now)?;
    let plan = load(store, plan_id)?;
    let decider = ghost_plan::authorize_decision(store, &plan.plan, caller_session_id)?;
    match plan.plan.status {
        PlanStatus::Expired => bail!("plan {plan_id} expired without being accepted — propose it again"),
        PlanStatus::Discarded => bail!("plan {plan_id} was discarded"),
        PlanStatus::Pending | PlanStatus::Accepted => {}
    }
    Ok((plan, decider))
}

/// Accepts `only` (default: every node) of `plan_id` and spawns whatever is
/// ready. Idempotent: a node that isn't a ghost any more is reported
/// `Unchanged` and never spawned twice.
pub async fn accept(
    store: &Store,
    config: &AppConfig,
    plan_id: &str,
    only: Option<&[String]>,
    caller_session_id: Option<&str>,
    now: i64,
) -> anyhow::Result<DecisionReport> {
    let (plan, decider) = decidable(store, plan_id, caller_session_id, now)?;
    let chosen = selection(&plan, only)?;
    let chosen_set: HashSet<&str> = chosen.iter().map(String::as_str).collect();
    for id in &chosen {
        let node = plan.node(id).expect("selection checked");
        if node.state != NodeState::Ghost {
            continue;
        }
        if let Some(parent) = node.parent_node_id.as_deref().and_then(|p| plan.node(p)) {
            if !chosen_set.contains(parent.node_id.as_str()) && !parent.state.is_accepted() {
                bail!("node '{id}' sits under '{}', which is {} — accept it too (add it to --nodes)", parent.node_id, parent.state.as_str());
            }
        }
    }
    let moved = store.transition_ghost_nodes(plan_id, &chosen, &[NodeState::Ghost], NodeState::Waiting, now)?;
    if !moved.is_empty() {
        store.record_ghost_decision(plan_id, decider.label(), now)?;
    }
    let progressed = advance(store, config, plan_id, now).await?;
    let plan = load(store, plan_id)?;
    let nodes = chosen
        .iter()
        .map(|id| {
            let node = plan.node(id).expect("selection checked");
            let outcome = if moved.contains(id) {
                match progressed.iter().find(|r| &r.node_id == id) {
                    Some(r) => return r.clone(),
                    None => outcome_of(store, node),
                }
            } else {
                NodeOutcome::Unchanged { state: node.state }
            };
            NodeReport { node_id: id.clone(), outcome, warnings: Vec::new() }
        })
        .collect();
    Ok(DecisionReport { plan_id: plan_id.to_string(), decided_by: decider.label().to_string(), status: plan.plan.status, nodes })
}

/// Discards `only` (default: every undecided node). Accepted nodes are
/// never touched.
pub fn discard(
    store: &Store,
    plan_id: &str,
    only: Option<&[String]>,
    caller_session_id: Option<&str>,
    now: i64,
) -> anyhow::Result<DecisionReport> {
    let (plan, decider) = decidable(store, plan_id, caller_session_id, now)?;
    let chosen = selection(&plan, only)?;
    let moved = store.transition_ghost_nodes(plan_id, &chosen, &[NodeState::Ghost], NodeState::Discarded, now)?;
    if !moved.is_empty() {
        store.record_ghost_decision(plan_id, decider.label(), now)?;
    }
    let plan = load(store, plan_id)?;
    let nodes = chosen
        .iter()
        .map(|id| NodeReport {
            node_id: id.clone(),
            warnings: Vec::new(),
            outcome: if moved.contains(id) {
                NodeOutcome::Discarded
            } else {
                NodeOutcome::Unchanged { state: plan.node(id).map_or(NodeState::Ghost, |n| n.state) }
            },
        })
        .collect();
    Ok(DecisionReport { plan_id: plan_id.to_string(), decided_by: decider.label().to_string(), status: plan.plan.status, nodes })
}

fn outcome_of(store: &Store, node: &NodeRow) -> NodeOutcome {
    match (node.state, node.session_id.clone()) {
        (NodeState::Spawned, Some(sid)) if store.is_waiting_in_queue(&sid).unwrap_or(false) => {
            NodeOutcome::Queued { session_id: sid, message: "queued under the session limit".into() }
        }
        (NodeState::Spawned, Some(sid)) => NodeOutcome::Spawned { session_id: sid },
        (NodeState::Failed, _) => NodeOutcome::Failed { error: node.error.clone().unwrap_or_default() },
        (NodeState::Discarded, _) => NodeOutcome::Discarded,
        (NodeState::Waiting | NodeState::Spawning, _) => {
            NodeOutcome::Waiting { reason: node.error.clone().unwrap_or_else(|| "waiting".into()) }
        }
        (state, _) => NodeOutcome::Unchanged { state },
    }
}

/// Spawns every accepted node of `plan_id` that is ready, top-down, until a
/// pass makes no progress (a parent spawned in one pass can unblock its
/// children in the next). Returns what happened to each node it touched.
pub async fn advance(store: &Store, config: &AppConfig, plan_id: &str, now: i64) -> anyhow::Result<Vec<NodeReport>> {
    let mut reports: Vec<NodeReport> = Vec::new();
    let mut report = |node_id: &str, outcome: NodeOutcome, warnings: Vec<String>| {
        reports.retain(|r| r.node_id != node_id);
        reports.push(NodeReport { node_id: node_id.to_string(), outcome, warnings });
    };
    // Bounded by the node count: each productive pass settles at least one.
    for _ in 0..=load(store, plan_id)?.nodes.len() {
        let plan = load(store, plan_id)?;
        let mut progressed = false;
        for node in plan.top_down() {
            if node.state != NodeState::Waiting {
                continue;
            }
            match ghost_plan::readiness(store, &plan, node)? {
                Readiness::Wait(reason) => {
                    store.note_ghost_node_wait(plan_id, &node.node_id, &reason)?;
                    report(&node.node_id, NodeOutcome::Waiting { reason }, Vec::new());
                }
                Readiness::Blocked(reason) => {
                    if store.fail_waiting_ghost_node(plan_id, &node.node_id, &reason, now)? {
                        report(&node.node_id, NodeOutcome::Failed { error: reason }, Vec::new());
                        progressed = true;
                    }
                }
                Readiness::Ready { parent_session } => {
                    // Claim stamps are wall-clock, not `now`: a long accept
                    // must not leave claims that already look stale to the
                    // poller's crash recovery.
                    if !store.claim_ghost_node(plan_id, &node.node_id, now_ms())? {
                        continue;
                    }
                    progressed = true;
                    match spawn_node(store, config, &plan, node, parent_session.as_deref()).await {
                        Ok(spawned) => {
                            store.finish_ghost_node(plan_id, &node.node_id, NodeState::Spawned, Some(&spawned.id), None, now_ms())?;
                            let outcome = match spawned.queued {
                                Some(message) => NodeOutcome::Queued { session_id: spawned.id, message },
                                None => NodeOutcome::Spawned { session_id: spawned.id },
                            };
                            report(&node.node_id, outcome, spawned.warnings);
                        }
                        Err(e) => {
                            let error = format!("{e:#}");
                            store.finish_ghost_node(plan_id, &node.node_id, NodeState::Failed, None, Some(&error), now_ms())?;
                            report(&node.node_id, NodeOutcome::Failed { error }, Vec::new());
                        }
                    }
                }
            }
        }
        if !progressed {
            break;
        }
    }
    Ok(reports)
}

/// The text a node's session starts with.
pub fn node_brief(plan: &PlanWithNodes, node: &NodeRow) -> String {
    let mut brief = node.brief.trim().to_string();
    if node.is_orchestrator() {
        if let Some(ws) = &node.workspace {
            brief.push_str(&format!("\n\nWorkspace: {ws}"));
        }
    }
    if let Some(linear) = &node.linear_ref {
        brief.push_str(&format!("\n\nLinear: {linear}"));
    }
    brief.push_str(&format!("\n\n(Started from accepted ghost plan {}, node '{}'.)", plan.plan.id, node.node_id));
    brief
}

async fn spawn_node(
    store: &Store,
    config: &AppConfig,
    plan: &PlanWithNodes,
    node: &NodeRow,
    parent_session: Option<&str>,
) -> anyhow::Result<SpawnedSession> {
    let brief = node_brief(plan, node);
    if let Some(parent) = parent_session {
        let alive = store.get_session(parent)?.is_some_and(|s| !s.status.is_terminal());
        anyhow::ensure!(alive, "parent '{parent}' ended before this node could spawn");
    }
    if node.is_orchestrator() {
        if let Some(parent) = parent_session {
            let cap = config.nested_orchestration.max_depth;
            let depth = ninox_core::orchestrator_auth::orchestrator_depth(store, parent, cap)? + 1;
            anyhow::ensure!(depth < cap, "a sub-orchestrator under '{parent}' would reach depth {depth}, at or beyond the cap ({cap})");
        }
        let agent = Some(crate::cli::resolve_orchestrator_agent(config, node.model.as_deref()));
        return crate::cli::spawn_orchestrator_common_opts(
            store, config, &node.name, Some(brief), parent_session, agent, node.brain.clone(), false,
        )
        .await;
    }

    let Some(orchestrator_id) = parent_session else {
        bail!("a '{}' node needs a parent orchestrator", node.worker_type);
    };
    let registry = config.worker_type_registry();
    let spec = crate::cli::resolve_requested_worker_type(&registry, &node.worker_type)
        .map_err(|_| anyhow::anyhow!("worker type '{}' can't be spawned as a worker", node.worker_type))?;
    let parent_type = ninox_core::orchestrator_auth::resolve_caller_worker_type(store, Some(orchestrator_id)).unwrap_or_default();
    let parent_spec = registry.get(&parent_type).cloned().unwrap_or_default();
    anyhow::ensure!(
        parent_spec.allows_spawning(&spec.name),
        "worker type '{parent_type}' is not allowed to spawn '{}'", spec.name
    );
    let agent = ninox_core::model_resolution::effective_agent(
        &ninox_core::model_resolution::SpawnModelRequest::worker(Some(&spec)).with_model(node.model.as_deref()),
        config,
    )
    .agent;
    anyhow::ensure!(
        config.registry().spec(&agent.harness).worker_args.is_some(),
        "harness '{}' has no worker mode", agent.harness
    );
    let id = node.session_slug();
    anyhow::ensure!(store.get_session(&id)?.is_none(), "a session named '{id}' already exists");
    let workspace = node.workspace.clone().context("a worker node needs a workspace")?;
    let brain = match &node.brain {
        Some(b) => b.clone(),
        None => ninox_core::brain_set::BrainSet::from_session(
            &store.get_session(orchestrator_id)?.context("parent orchestrator has no session row")?,
            config,
        ),
    };
    let launch = crate::session_queue::WorkerLaunch {
        id: id.clone(),
        workspace,
        display_name: node.name.clone(),
        summary: crate::cli::first_line(&node.brief, 140),
        prompt: spec.compose_prompt(&brief)?,
        orchestrator_id: Some(orchestrator_id.to_string()),
        agent,
        worker_type: Some(spec.name.clone()),
        success_criteria: crate::cli::resolve_success_criteria(&spec.success, &[], false),
        started_at: now_ms(),
        ninox_brain: Some(brain.write_str()),
        ninox_config: std::env::var("NINOX_CONFIG").ok(),
        brain: Some(brain),
    };
    crate::cli::admit_and_launch_worker(store, config, launch, false).await
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as i64
}

/// One poller tick: expire due plans, recover crashed claims, spawn ready
/// nodes. Errors are logged, never fatal to the poller.
pub async fn advance_all(store: &Store, config: &AppConfig, now: i64) {
    if let Err(e) = store.expire_ghost_plans(now) {
        tracing::warn!("ghost plans: expiry sweep: {e}");
    }
    if let Err(e) = store.recover_stale_ghost_claims(now) {
        tracing::warn!("ghost plans: claim recovery: {e}");
    }
    let ids = match store.ghost_plans_with_waiting() {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!("ghost plans: list waiting: {e}");
            return;
        }
    };
    for id in ids {
        if let Err(e) = advance(store, config, &id, now).await {
            tracing::warn!("ghost plans: advance {id}: {e}");
        }
    }
}

/// What `Poller::with_plan_advancer` runs each tick. Spawns can take
/// minutes (an orchestrator's brief waits for its prompt), so the work runs
/// off the poller's tick, at most one run at a time.
pub fn plan_advancer(store: Arc<Store>) -> PlanAdvancer {
    let running = Arc::new(std::sync::atomic::AtomicBool::new(false));
    Arc::new(move |now: i64| {
        let store = store.clone();
        let running = running.clone();
        Box::pin(async move {
            if running.swap(true, std::sync::atomic::Ordering::AcqRel) {
                return;
            }
            tokio::spawn(async move {
                let config = AppConfig::load_or_default();
                advance_all(&store, &config, now).await;
                running.store(false, std::sync::atomic::Ordering::Release);
            });
        })
    })
}

#[cfg(test)]
#[path = "ghost_accept_tests.rs"]
mod tests;
