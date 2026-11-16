//! Ghost plans: proposed session trees that are stored, reviewed and only
//! spawned once someone authorized explicitly accepts them.
//!
//! `ninox plan propose` stores a pending plan (rows in `ghost_plans` /
//! `ghost_nodes`, see `store_ghost.rs`); the Fleet, the Plans tab and the
//! prompt reply drawer render it; `ninox plan accept` (or the UI) moves
//! nodes to `waiting`, and the spawn engine in `ninox-ui-shared`
//! (`ghost_accept`) starts each node once [`readiness`] says its parent and
//! dependencies allow it. Nothing here spawns.

use std::collections::{HashMap, HashSet};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::brain_set::BrainSet;
use crate::config::AppConfig;
use crate::lifecycle::auto_plan::{self, PlanValidationError};
use crate::store::Store;
use crate::types::SessionStatus;
use crate::worker_type::{WorkerTypeRegistry, PROMPT_SESSION_ID};

/// The proposer / decider recorded for a call with no managed caller.
pub const USER: &str = "user";
const ORCHESTRATOR: &str = "Orchestrator";
/// Stands in for "no parent" when a plan creates new top-level
/// orchestrators, so `auto_plan::validate_plan` has a root id to hang the
/// sections from. Not a valid slug, so no section can collide with it.
const NEW_ROOT: &str = "@top-level";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    Accepted,
    Discarded,
    Expired,
}

impl PlanStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Accepted => "accepted",
            Self::Discarded => "discarded",
            Self::Expired => "expired",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "accepted" => Self::Accepted,
            "discarded" => Self::Discarded,
            "expired" => Self::Expired,
            _ => Self::Pending,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    /// Proposed, not decided.
    Ghost,
    /// Accepted; spawns once [`readiness`] allows.
    Waiting,
    /// Claimed by one process that is running its spawn right now.
    Spawning,
    /// Has a session (running, or queued under the session limit).
    Spawned,
    Failed,
    Discarded,
    Expired,
}

impl NodeState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ghost => "ghost",
            Self::Waiting => "waiting",
            Self::Spawning => "spawning",
            Self::Spawned => "spawned",
            Self::Failed => "failed",
            Self::Discarded => "discarded",
            Self::Expired => "expired",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "waiting" => Self::Waiting,
            "spawning" => Self::Spawning,
            "spawned" => Self::Spawned,
            "failed" => Self::Failed,
            "discarded" => Self::Discarded,
            "expired" => Self::Expired,
            _ => Self::Ghost,
        }
    }

    /// Accepted at some point (whatever happened after).
    pub fn is_accepted(self) -> bool {
        matches!(self, Self::Waiting | Self::Spawning | Self::Spawned | Self::Failed)
    }

    /// Never going to spawn.
    pub fn is_dead(self) -> bool {
        matches!(self, Self::Failed | Self::Discarded | Self::Expired)
    }
}

/// `ninox plan propose` input: the `ninox plan generate` JSON shape
/// (`parent_id` + flat `sections`) plus the optional per-section fields
/// below. A null/absent `parent_id` proposes new top-level orchestrators.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Proposal {
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    pub sections: Vec<ProposalSection>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProposalSection {
    pub id: String,
    /// The plan's `parent_id` (or absent) for a top section, else another
    /// section's id.
    #[serde(default)]
    pub parent_id: Option<String>,
    pub worker_type: Option<String>,
    /// Session name; defaults to `id`.
    #[serde(default)]
    pub name: Option<String>,
    pub brief: String,
    #[serde(default)]
    pub dependencies: Vec<String>,
    /// Wait for every dependency to finish (`Done`), not just to start.
    #[serde(default)]
    pub await_done: bool,
    /// Repository a worker runs in; folded into an orchestrator's brief.
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Catalogue names or paths, narrowed from the proposer's own set.
    #[serde(default)]
    pub brain_reads: Vec<String>,
    #[serde(default)]
    pub brain_write: Option<String>,
    #[serde(default)]
    pub linear: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanRow {
    pub id: String,
    pub proposer: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub status: PlanStatus,
    /// Existing orchestrator the nodes attach under; `None` = new top-level
    /// orchestrators.
    pub target_parent: Option<String>,
    pub title: Option<String>,
    pub decided_by: Option<String>,
    pub decided_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NodeRow {
    pub plan_id: String,
    pub node_id: String,
    pub parent_node_id: Option<String>,
    pub worker_type: String,
    pub name: String,
    pub brief: String,
    pub workspace: Option<String>,
    pub model: Option<String>,
    pub brain: Option<BrainSet>,
    pub dependencies: Vec<String>,
    pub await_done: bool,
    pub linear_ref: Option<String>,
    pub state: NodeState,
    pub session_id: Option<String>,
    pub error: Option<String>,
    pub updated_at: i64,
    pub seq: i64,
}

impl NodeRow {
    pub fn is_orchestrator(&self) -> bool {
        self.worker_type == ORCHESTRATOR
    }

    /// The id the node's session will get.
    pub fn session_slug(&self) -> String {
        crate::slugify(&self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanWithNodes {
    pub plan: PlanRow,
    pub nodes: Vec<NodeRow>,
}

impl PlanWithNodes {
    pub fn node(&self, id: &str) -> Option<&NodeRow> {
        self.nodes.iter().find(|n| n.node_id == id)
    }

    pub fn ghost_count(&self) -> usize {
        self.nodes.iter().filter(|n| n.state == NodeState::Ghost).count()
    }

    /// Parents before children, then proposal order.
    pub fn top_down(&self) -> Vec<&NodeRow> {
        let mut out: Vec<&NodeRow> = Vec::with_capacity(self.nodes.len());
        let mut placed: HashSet<&str> = HashSet::new();
        while out.len() < self.nodes.len() {
            let before = out.len();
            for n in &self.nodes {
                if placed.contains(n.node_id.as_str()) {
                    continue;
                }
                let parent_placed = n.parent_node_id.as_deref().is_none_or(|p| placed.contains(p) || self.node(p).is_none());
                if parent_placed {
                    placed.insert(&n.node_id);
                    out.push(n);
                }
            }
            if out.len() == before {
                out.extend(self.nodes.iter().filter(|n| !placed.contains(n.node_id.as_str())));
                break;
            }
        }
        out
    }
}

/// What a validated proposal becomes before it is stored.
#[derive(Debug, Clone)]
pub struct ValidatedProposal {
    pub target_parent: Option<String>,
    pub title: Option<String>,
    pub nodes: Vec<NodeRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposalError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for ProposalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

fn perr(code: &'static str, message: impl Into<String>) -> ProposalError {
    ProposalError { code, message: message.into() }
}

impl From<PlanValidationError> for ProposalError {
    fn from(e: PlanValidationError) -> Self {
        perr(e.code(), e.to_string())
    }
}

/// Everything wrong with `proposal` (empty = valid), without touching the
/// store. `target_depth` is the orchestrator depth of `parent_id` (0 for a
/// root orchestrator); ignored for a top-level plan.
pub fn validate_proposal(
    proposal: &Proposal,
    registry: &WorkerTypeRegistry,
    depth_cap: u32,
    target_depth: u32,
) -> Vec<ProposalError> {
    let root = proposal.parent_id.clone().filter(|p| !p.trim().is_empty());
    let root_id = root.clone().unwrap_or_else(|| NEW_ROOT.to_string());
    let plan = auto_plan::GhostPlan {
        parent_id: root_id.clone(),
        sections: proposal
            .sections
            .iter()
            .map(|s| auto_plan::GhostNode {
                id: s.id.clone(),
                parent_id: s.parent_id.clone().filter(|p| !p.is_empty()).unwrap_or_else(|| root_id.clone()),
                worker_type: s.worker_type.clone(),
                brief: s.brief.clone(),
                dependencies: s.dependencies.clone(),
            })
            .collect(),
    };
    // A new top-level orchestrator sits at depth 0, one above where
    // `validate_plan` puts a top section.
    let cap = match &root {
        Some(_) => depth_cap.saturating_sub(target_depth),
        None => depth_cap + 1,
    };
    let mut errors: Vec<ProposalError> = auto_plan::validate_plan(&plan, registry, cap).into_iter().map(Into::into).collect();

    let mut slugs: HashMap<String, &str> = HashMap::new();
    for (section, ghost) in proposal.sections.iter().zip(&plan.sections) {
        let name = section.name.as_deref().unwrap_or(&section.id);
        let slug = crate::slugify(name);
        if slug.is_empty() {
            errors.push(perr("invalid_name", format!("section '{}' has a name with no alphanumeric characters", section.id)));
        } else if crate::messaging::is_reserved_session_id(&slug) {
            errors.push(perr("invalid_name", format!("section '{}' would be named '{slug}', which is reserved for ninox", section.id)));
        } else if let Some(other) = slugs.insert(slug.clone(), &section.id) {
            errors.push(perr(
                "duplicate_name",
                format!("sections '{other}' and '{}' would both become session '{slug}'", section.id),
            ));
        }
        if section.brief.trim().is_empty() {
            errors.push(perr("empty_brief", format!("section '{}' has an empty brief", section.id)));
        }
        let Some(ty) = section.worker_type.as_deref() else { continue };
        let is_top = ghost.parent_id == root_id;
        if root.is_none() && is_top && ty != ORCHESTRATOR {
            errors.push(perr(
                "top_level_not_orchestrator",
                format!("section '{}' is a top-level '{ty}', but a plan with no parent_id can only start Orchestrators", section.id),
            ));
        }
        if ty != ORCHESTRATOR && section.workspace.as_deref().is_none_or(|w| w.trim().is_empty()) {
            errors.push(perr("missing_workspace", format!("section '{}' is a '{ty}' and needs a workspace", section.id)));
        }
        if !is_top {
            if let Some(parent_ty) = proposal.sections.iter().find(|p| p.id == ghost.parent_id).and_then(|p| p.worker_type.as_deref()) {
                if registry.get(parent_ty).is_some_and(|spec| !spec.allows_spawning(ty)) {
                    errors.push(perr(
                        "can_spawn_denied",
                        format!("section '{}' is a '{ty}', which its parent's type '{parent_ty}' may not spawn", section.id),
                    ));
                }
            }
        }
    }
    if !errors.iter().any(|e| e.code == "dependency_cycle") {
        for id in wait_cycles(&plan) {
            errors.push(perr("dependency_cycle", format!(
                "section '{id}' waits on itself through its dependencies and its parents' dependencies"
            )));
        }
    }
    errors
}

/// Sections on a cycle of "waits for" edges, where a section waits for each
/// dependency and for its parent section (it can't spawn before either).
/// `validate_plan` only checks dependency edges.
fn wait_cycles(plan: &auto_plan::GhostPlan) -> Vec<String> {
    let ids: HashSet<&str> = plan.sections.iter().map(|s| s.id.as_str()).collect();
    let edges: HashMap<&str, Vec<&str>> = plan
        .sections
        .iter()
        .map(|s| {
            let mut out: Vec<&str> = s.dependencies.iter().map(String::as_str).filter(|d| ids.contains(d)).collect();
            if ids.contains(s.parent_id.as_str()) {
                out.push(&s.parent_id);
            }
            (s.id.as_str(), out)
        })
        .collect();
    let mut on_cycle = Vec::new();
    for start in plan.sections.iter().map(|s| s.id.as_str()) {
        let mut stack: Vec<&str> = edges.get(start).cloned().unwrap_or_default();
        let mut seen: HashSet<&str> = HashSet::new();
        while let Some(n) = stack.pop() {
            if n == start {
                on_cycle.push(start.to_string());
                break;
            }
            if seen.insert(n) {
                stack.extend(edges.get(n).into_iter().flatten().copied());
            }
        }
    }
    on_cycle
}

/// Validates `proposal` against the live store and config and resolves each
/// node's brain set, narrowed from the proposer's own set
/// (`proposer_session`: pane-verified, `None` for the user).
pub fn prepare_proposal(
    store: &Store,
    config: &AppConfig,
    proposal: &Proposal,
    proposer_session: Option<&str>,
) -> std::result::Result<ValidatedProposal, Vec<ProposalError>> {
    let target = proposal.parent_id.clone().filter(|p| !p.trim().is_empty());
    let cap = config.nested_orchestration.max_depth;
    let mut target_depth = 0;
    if let Some(t) = &target {
        if !store.is_orchestrator(t).unwrap_or(false) {
            return Err(vec![perr("unknown_parent", format!("'{t}' is not an orchestrator"))]);
        }
        match crate::orchestrator_auth::orchestrator_depth(store, t, cap) {
            Ok(d) => target_depth = d,
            Err(e) => return Err(vec![perr("unknown_parent", e.to_string())]),
        }
    }
    let errors = validate_proposal(proposal, &config.worker_type_registry(), cap, target_depth);
    if !errors.is_empty() {
        return Err(errors);
    }

    // A session proposer's nodes never default to more than its own set.
    let root_set = match (proposer_session, &target) {
        (Some(_), _) => None,
        (None, Some(t)) => store.get_session(t).ok().flatten().map(|s| BrainSet::from_session(&s, config)),
        (None, None) => None,
    };
    let mut resolved: HashMap<String, BrainSet> = HashMap::new();
    let mut nodes = Vec::with_capacity(proposal.sections.len());
    // `validate_plan` has ruled out parent cycles, so a top-down pass
    // always resolves each parent's set before its children.
    let order = top_down_sections(proposal);
    for section in order {
        let parent = section.parent_id.clone().filter(|p| !p.is_empty() && Some(p) != target.as_ref());
        let explicit = !section.brain_reads.is_empty() || section.brain_write.is_some();
        let brain = if explicit {
            crate::brain_set::resolve_child(store, proposer_session, config, &section.brain_reads, section.brain_write.as_deref())
                .map_err(|e| vec![perr("invalid_brain", format!("section '{}': {e}", section.id))])?
        } else if let Some(p) = parent.as_ref().and_then(|p| resolved.get(p)) {
            p.clone()
        } else if let Some(r) = &root_set {
            r.clone()
        } else {
            crate::brain_set::resolve_child(store, proposer_session, config, &[], None)
                .map_err(|e| vec![perr("invalid_brain", format!("section '{}': {e}", section.id))])?
        };
        resolved.insert(section.id.clone(), brain.clone());
        nodes.push((section, parent, brain));
    }
    let seq_of: HashMap<&str, usize> = proposal.sections.iter().enumerate().map(|(i, s)| (s.id.as_str(), i)).collect();
    let mut rows: Vec<NodeRow> = nodes
        .into_iter()
        .map(|(s, parent, brain)| NodeRow {
            plan_id: String::new(),
            node_id: s.id.clone(),
            parent_node_id: parent,
            worker_type: s.worker_type.clone().unwrap_or_default(),
            name: s.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| s.id.clone()),
            brief: s.brief.clone(),
            workspace: s.workspace.clone().filter(|w| !w.trim().is_empty()),
            model: s.model.clone().filter(|m| !m.trim().is_empty()),
            brain: Some(brain),
            dependencies: s.dependencies.clone(),
            await_done: s.await_done,
            linear_ref: s.linear.clone().filter(|l| !l.trim().is_empty()),
            state: NodeState::Ghost,
            session_id: None,
            error: None,
            updated_at: 0,
            seq: seq_of[s.id.as_str()] as i64,
        })
        .collect();
    rows.sort_by_key(|n| n.seq);
    Ok(ValidatedProposal { target_parent: target, title: proposal.title.clone().filter(|t| !t.trim().is_empty()), nodes: rows })
}

fn top_down_sections(proposal: &Proposal) -> Vec<&ProposalSection> {
    let ids: HashSet<&str> = proposal.sections.iter().map(|s| s.id.as_str()).collect();
    let mut placed: HashSet<&str> = HashSet::new();
    let mut out = Vec::new();
    while out.len() < proposal.sections.len() {
        let before = out.len();
        for s in &proposal.sections {
            if placed.contains(s.id.as_str()) {
                continue;
            }
            let ready = s.parent_id.as_deref().is_none_or(|p| !ids.contains(p) || placed.contains(p));
            if ready {
                placed.insert(&s.id);
                out.push(s);
            }
        }
        if out.len() == before {
            break;
        }
    }
    out
}

/// Who is accepting or discarding a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decider {
    User,
    Orchestrator(String),
}

impl Decider {
    pub fn label(&self) -> &str {
        match self {
            Self::User => USER,
            Self::Orchestrator(id) => id,
        }
    }
}

/// The accept/discard rule. `caller_session_id` must come from
/// `orchestrator_auth::resolve_live_caller_session_id` (pane-verified, with
/// its ancestry scan), never an env var. Allowed: the user (no managed
/// caller) for any plan; an orchestrator only for a plan whose target parent
/// is itself or one of its descendants. Never the prompt agent, never a
/// worker.
pub fn authorize_decision(store: &Store, plan: &PlanRow, caller_session_id: Option<&str>) -> Result<Decider> {
    let Some(caller) = caller_session_id else { return Ok(Decider::User) };
    if caller == PROMPT_SESSION_ID {
        bail!(
            "the Ninox prompt agent can propose plans but never accept or discard them — \
             ask the user to accept {} in the UI or with `ninox plan accept {}`",
            plan.id, plan.id
        );
    }
    if !store.is_orchestrator(caller)? {
        bail!("only the user, or an orchestrator for plans under itself, can accept or discard plans — '{caller}' is neither");
    }
    let Some(target) = plan.target_parent.as_deref() else {
        bail!("plan {} starts new top-level orchestrators — only the user can accept or discard it", plan.id);
    };
    if is_self_or_descendant(store, target, caller)? {
        Ok(Decider::Orchestrator(caller.to_string()))
    } else {
        bail!("plan {} attaches under '{target}', which is not '{caller}' or one of its sub-orchestrators", plan.id)
    }
}

fn is_self_or_descendant(store: &Store, node: &str, ancestor: &str) -> Result<bool> {
    let mut current = node.to_string();
    for _ in 0..64 {
        if current == ancestor {
            return Ok(true);
        }
        match store.get_session(&current)?.and_then(|s| s.orchestrator_id) {
            Some(parent) => current = parent,
            None => return Ok(false),
        }
    }
    Ok(false)
}

/// Expires due plans and spawns accepted nodes that became ready. Installed
/// by the UI layer (which owns the spawn code) via
/// `Poller::with_plan_advancer`; takes the current time in ms.
pub type PlanAdvancer =
    std::sync::Arc<dyn Fn(i64) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync>;

/// Whether an accepted (`Waiting`) node may spawn now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
    /// Spawn it under this orchestrator session (`None` = top-level).
    Ready { parent_session: Option<String> },
    Wait(String),
    /// Can never spawn; the node should fail with this reason.
    Blocked(String),
}

enum Started {
    /// Queued under the session limit, or admitted and still launching.
    NotYet,
    Running(Box<crate::types::Session>),
    /// No session row and no queue entry: removed, cancelled, purged or
    /// never launched. It will never start.
    Gone,
}

fn started(store: &Store, id: &str) -> Result<Started> {
    if store.queue_entry(id)?.is_some() {
        return Ok(Started::NotYet);
    }
    Ok(match store.get_session(id)? {
        Some(s) => Started::Running(Box::new(s)),
        None => Started::Gone,
    })
}

/// "Finished" for `await_done`: `Done`, or merged / self-completed while
/// kept alive (`[auto_reap]` off leaves a merged worker non-terminal).
fn finished(s: &crate::types::Session) -> bool {
    s.status == SessionStatus::Done || s.merged_at.is_some() || s.self_completed_at.is_some()
}

pub fn readiness(store: &Store, plan: &PlanWithNodes, node: &NodeRow) -> Result<Readiness> {
    let parent_session = match node.parent_node_id.as_deref() {
        Some(pid) => {
            let Some(parent) = plan.node(pid) else {
                return Ok(Readiness::Blocked(format!("parent node '{pid}' is missing")));
            };
            match (parent.state, parent.session_id.as_deref()) {
                (NodeState::Spawned, Some(sid)) => match started(store, sid)? {
                    Started::NotYet => return Ok(Readiness::Wait(format!("waiting for parent '{sid}' to start"))),
                    Started::Gone => return Ok(Readiness::Blocked(format!("parent '{sid}' no longer exists"))),
                    Started::Running(s) if s.status.is_terminal() => {
                        return Ok(Readiness::Blocked(format!("parent '{sid}' ended before this node spawned")))
                    }
                    Started::Running(_) => Some(sid.to_string()),
                },
                (s, _) if s.is_dead() => {
                    return Ok(Readiness::Blocked(format!("parent node '{pid}' is {}", s.as_str())))
                }
                (s, _) => return Ok(Readiness::Wait(format!("waiting for parent node '{pid}' ({})", s.as_str()))),
            }
        }
        None => match plan.plan.target_parent.as_deref() {
            Some(t) => match started(store, t)? {
                Started::Running(s) if !s.status.is_terminal() => Some(t.to_string()),
                Started::Running(_) => return Ok(Readiness::Blocked(format!("target orchestrator '{t}' has ended"))),
                Started::Gone => return Ok(Readiness::Blocked(format!("target orchestrator '{t}' no longer exists"))),
                Started::NotYet => return Ok(Readiness::Wait(format!("waiting for '{t}' to start"))),
            },
            None => None,
        },
    };
    for dep_id in &node.dependencies {
        let Some(dep) = plan.node(dep_id) else {
            return Ok(Readiness::Blocked(format!("dependency '{dep_id}' is missing")));
        };
        match (dep.state, dep.session_id.as_deref()) {
            (NodeState::Spawned, Some(sid)) => match started(store, sid)? {
                Started::NotYet => return Ok(Readiness::Wait(format!("waiting for dependency '{dep_id}' to start"))),
                Started::Gone => return Ok(Readiness::Blocked(format!("dependency '{dep_id}' ('{sid}') no longer exists"))),
                Started::Running(s) if node.await_done && finished(&s) => {}
                Started::Running(s) if node.await_done && s.status.is_terminal() => {
                    return Ok(Readiness::Blocked(format!("dependency '{dep_id}' ended without finishing")))
                }
                Started::Running(_) if node.await_done => {
                    return Ok(Readiness::Wait(format!("waiting for dependency '{dep_id}' to finish")))
                }
                Started::Running(_) => {}
            },
            (s, _) if s.is_dead() => {
                return Ok(Readiness::Blocked(format!("dependency '{dep_id}' is {}", s.as_str())))
            }
            (s, _) => return Ok(Readiness::Wait(format!("waiting for dependency '{dep_id}' ({})", s.as_str()))),
        }
    }
    Ok(Readiness::Ready { parent_session })
}

/// The caller id for accepting, discarding or proposing, failing closed:
/// `pane_session_id` (already pane-verified) wins; otherwise the live
/// process-ancestry scan, whose errors refuse rather than mean "the user".
/// A process that claims a session through `NINOX_*` env but matches none
/// is refused too: only a process with no session claim and no managed
/// ancestor counts as the user.
pub fn resolve_decider_caller(store: &Store, pane_session_id: Option<&str>) -> Result<Option<String>> {
    if let Some(id) = pane_session_id {
        return Ok(Some(id.to_string()));
    }
    let found = crate::orchestrator_auth::resolve_live_session_id_for_pid(store, std::process::id())
        .map_err(|e| anyhow::anyhow!("can't verify who is calling ({e}); refusing"))?;
    if found.is_some() {
        return Ok(found);
    }
    for var in SESSION_CLAIM_VARS {
        if std::env::var(var).is_ok_and(|v| !v.is_empty()) {
            bail!("this process claims a ninox session ({var} is set) but can't be verified as running inside one; refusing");
        }
    }
    Ok(None)
}

const SESSION_CLAIM_VARS: &[&str] =
    &["NINOX_SESSION", "NINOX_PANE_ID", "NINOX_ACP_PANE_ID", "NINOX_ORCHESTRATOR_ID", "NINOX_CALLER_TYPE"];

/// The plans a prompt-drawer turn produced: proposed by `proposer` at or
/// after `from_ms` and before `until_ms` (the next turn's start; `None` for
/// the latest turn).
pub fn plans_in_window<'a>(plans: &'a [PlanWithNodes], proposer: &str, from_ms: i64, until_ms: Option<i64>) -> Vec<&'a PlanWithNodes> {
    plans
        .iter()
        .filter(|p| p.plan.proposer == proposer && p.plan.created_at >= from_ms && until_ms.is_none_or(|u| p.plan.created_at < u))
        .collect()
}

pub fn new_plan_id(now_ms: i64) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or_default();
    let mix = (now_ms as u64).wrapping_mul(6_364_136_223_846_793_005) ^ u64::from(nanos) ^ u64::from(std::process::id());
    format!("gp-{:06x}", mix & 0xff_ffff)
}

#[cfg(test)]
#[path = "ghost_plan_tests.rs"]
mod tests;
