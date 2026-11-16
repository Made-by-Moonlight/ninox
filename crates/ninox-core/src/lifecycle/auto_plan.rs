//! Auto-planning: a scoped, read-only `claude -p` subprocess turns a task
//! description into a proposed plan of ghost nodes for the fleet overview's
//! plan-review UI. A generated plan is only ever a proposal — nothing here
//! spawns anything — and the model's output is untrusted, so every plan
//! goes through [`validate_plan`] before it is returned.

use crate::worker_type::WorkerTypeRegistry;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    time::Duration,
};
use tokio::process::Command;

/// Read-only on purpose: planning must never modify the workspace it
/// explores. A tool left off `--tools` is unregistered outright, even under
/// `--dangerously-skip-permissions` (see `brain_harvest::HARVEST_TOOLS`).
const PLAN_TOOLS: &str = "Read,Grep";

/// What `claude` needs to resolve and authenticate, and nothing else this
/// process holds (GitHub tokens etc.). `USER` is required on macOS: without
/// it the keychain-stored login isn't found and `claude -p` exits with "Not
/// logged in".
const PLAN_ENV_PASSTHROUGH: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "ANTHROPIC_API_KEY",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CONFIG_DIR",
];

/// Generous for a large codebase; only here so a wedged subprocess can't
/// hang `ninox plan generate` forever. Arbitrary — raise it if real plans
/// hit it.
const PLAN_TIMEOUT: Duration = Duration::from_secs(600);

/// How much of an unparseable reply is echoed back in the error.
const MALFORMED_EXCERPT_CHARS: usize = 200;

/// The only worker type whose ghost may own children: nested children hang
/// off a sub-orchestrator. `allow_nested_spawn` is not the gate — it lets a
/// *worker* spawn a sub-orchestrator attached under that worker's own
/// parent, so a worker ghost still never owns children itself.
const ORCHESTRATOR_WORKER_TYPE: &str = "Orchestrator";

/// One proposed section. Field-for-field the same shape as
/// `ninox_overview::node::GhostNode` (which depends on this crate, so it
/// can't be imported here) — keep the two in sync so a generated plan's
/// JSON deserializes straight into the overview's plan-review state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GhostNode {
    pub id: String,
    pub parent_id: String,
    pub worker_type: Option<String>,
    pub brief: String,
    #[serde(default)]
    pub dependencies: Vec<String>,
}

/// Same shape as `ninox_overview::plan::GhostPlan`. `sections` is flat;
/// nesting is expressed by a section's `parent_id` naming another section.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GhostPlan {
    pub parent_id: String,
    pub sections: Vec<GhostNode>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanValidationError {
    EmptyPlan,
    DanglingParent { id: String, parent_id: String },
    NestedSpawnNotAllowed { id: String, worker_type: String },
    MissingWorkerType { id: String },
    UnknownWorkerType { id: String, worker_type: String },
    DuplicateSectionId { id: String },
    UnknownDependency { id: String, target: String },
    DependencyCycle { id: String },
    DependencyOnLineage { id: String, target: String },
    DepthExceeded { id: String, depth: u32, cap: u32 },
}

impl PlanValidationError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::EmptyPlan => "empty_plan",
            Self::DanglingParent { .. } => "dangling_parent",
            Self::NestedSpawnNotAllowed { .. } => "nested_spawn_not_allowed",
            Self::MissingWorkerType { .. } => "missing_worker_type",
            Self::UnknownWorkerType { .. } => "unknown_worker_type",
            Self::DuplicateSectionId { .. } => "duplicate_section_id",
            Self::UnknownDependency { .. } => "unknown_dependency",
            Self::DependencyCycle { .. } => "dependency_cycle",
            Self::DependencyOnLineage { .. } => "dependency_on_lineage",
            Self::DepthExceeded { .. } => "depth_exceeded",
        }
    }
}

impl std::fmt::Display for PlanValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyPlan => write!(f, "plan has no sections"),
            Self::DanglingParent { id, parent_id } => {
                write!(f, "section '{id}' has parent '{parent_id}', which is neither the plan root nor another section")
            }
            Self::NestedSpawnNotAllowed { id, worker_type } => write!(
                f,
                "section '{id}' has child sections but worker type '{worker_type}' cannot own them \
                 (only '{ORCHESTRATOR_WORKER_TYPE}' sections can)"
            ),
            Self::MissingWorkerType { id } => write!(f, "section '{id}' has no worker type"),
            Self::UnknownWorkerType { id, worker_type } => {
                write!(
                    f,
                    "section '{id}' names unknown worker type '{worker_type}'"
                )
            }
            Self::DuplicateSectionId { id } => {
                write!(f, "section id '{id}' is used more than once")
            }
            Self::UnknownDependency { id, target } => {
                write!(
                    f,
                    "section '{id}' depends on '{target}', which is not a section in this plan"
                )
            }
            Self::DependencyCycle { id } => {
                write!(f, "section '{id}' is part of a dependency cycle")
            }
            Self::DependencyOnLineage { id, target } => write!(
                f,
                "section '{id}' depends on '{target}', its own ancestor or descendant, \
                 which can never complete"
            ),
            Self::DepthExceeded { id, depth, cap } => {
                write!(
                    f,
                    "section '{id}' sits at depth {depth}, beyond the depth cap of {cap}"
                )
            }
        }
    }
}

/// Every problem with `plan` (empty means valid).
/// `depth_cap` counts hops below `plan.parent_id`; a caller that is itself a
/// nested orchestrator passes the cap minus its own depth.
pub fn validate_plan(
    plan: &GhostPlan,
    registry: &WorkerTypeRegistry,
    depth_cap: u32,
) -> Vec<PlanValidationError> {
    if plan.sections.is_empty() {
        return vec![PlanValidationError::EmptyPlan];
    }

    let mut errors = Vec::new();
    let mut by_id: HashMap<&str, &GhostNode> = HashMap::new();
    let mut duplicates: HashSet<&str> = HashSet::new();
    for section in &plan.sections {
        let reused = section.id == plan.parent_id || by_id.insert(&section.id, section).is_some();
        if reused && duplicates.insert(&section.id) {
            errors.push(PlanValidationError::DuplicateSectionId {
                id: section.id.clone(),
            });
        }
    }
    let owners: HashSet<&str> = plan.sections.iter().map(|s| s.parent_id.as_str()).collect();

    for section in &plan.sections {
        let id = &section.id;
        let resolved = match &section.worker_type {
            None => {
                errors.push(PlanValidationError::MissingWorkerType { id: id.clone() });
                None
            }
            Some(name) => match registry.get(name).filter(|spec| !spec.hidden) {
                Some(spec) => Some((name, spec)),
                None => {
                    errors.push(PlanValidationError::UnknownWorkerType {
                        id: id.clone(),
                        worker_type: name.clone(),
                    });
                    None
                }
            },
        };
        if let Some((name, spec)) = resolved {
            if owners.contains(id.as_str()) && spec.name != ORCHESTRATOR_WORKER_TYPE {
                errors.push(PlanValidationError::NestedSpawnNotAllowed {
                    id: id.clone(),
                    worker_type: name.clone(),
                });
            }
        }

        if section.parent_id != plan.parent_id && !by_id.contains_key(section.parent_id.as_str()) {
            errors.push(PlanValidationError::DanglingParent {
                id: id.clone(),
                parent_id: section.parent_id.clone(),
            });
        }

        for target in &section.dependencies {
            if !by_id.contains_key(target.as_str()) {
                errors.push(PlanValidationError::UnknownDependency {
                    id: id.clone(),
                    target: target.clone(),
                });
            } else if has_ancestor(id, target, &plan.parent_id, &by_id)
                || has_ancestor(target, id, &plan.parent_id, &by_id)
            {
                errors.push(PlanValidationError::DependencyOnLineage {
                    id: id.clone(),
                    target: target.clone(),
                });
            }
        }

        // Bounded at `depth_cap + 1` hops so a parent cycle terminates.
        let mut depth = 1;
        let mut parent = section.parent_id.as_str();
        while parent != plan.parent_id && depth <= depth_cap {
            match by_id.get(parent) {
                Some(node) => {
                    depth += 1;
                    parent = &node.parent_id;
                }
                None => break,
            }
        }
        // A sub-orchestrator at section depth d is itself orchestrator depth
        // d, which spawn-sub-orchestrator only allows below the cap.
        let is_orchestrator =
            matches!(resolved, Some((_, spec)) if spec.name == ORCHESTRATOR_WORKER_TYPE);
        if depth > depth_cap || (is_orchestrator && depth >= depth_cap) {
            errors.push(PlanValidationError::DepthExceeded {
                id: id.clone(),
                depth,
                cap: depth_cap,
            });
        }
    }

    errors.extend(
        dependency_cycles(plan, &by_id)
            .into_iter()
            .map(|id| PlanValidationError::DependencyCycle { id }),
    );
    errors
}

/// Whether `ancestor` is on `id`'s parent chain. Bounded by the section count
/// so a parent cycle terminates.
fn has_ancestor<'a>(
    id: &'a str,
    ancestor: &str,
    root: &str,
    by_id: &HashMap<&'a str, &'a GhostNode>,
) -> bool {
    let mut current = id;
    for _ in 0..by_id.len() {
        match by_id.get(current).copied() {
            Some(node) if node.parent_id == ancestor => return true,
            Some(node) if node.parent_id != root => current = &node.parent_id,
            _ => return false,
        }
    }
    false
}

/// Ids at which a depth-first walk of `dependencies` closed a cycle — one
/// per cycle found.
fn dependency_cycles(plan: &GhostPlan, by_id: &HashMap<&str, &GhostNode>) -> Vec<String> {
    #[derive(PartialEq)]
    enum Mark {
        Visiting,
        Done,
    }

    fn visit<'a>(
        id: &'a str,
        by_id: &HashMap<&'a str, &'a GhostNode>,
        marks: &mut HashMap<&'a str, Mark>,
        cycles: &mut Vec<String>,
    ) {
        match marks.get(id) {
            Some(Mark::Visiting) => return cycles.push(id.to_string()),
            Some(Mark::Done) => return,
            None => {}
        }
        marks.insert(id, Mark::Visiting);
        if let Some(node) = by_id.get(id) {
            for target in &node.dependencies {
                if by_id.contains_key(target.as_str()) {
                    visit(target, by_id, marks, cycles);
                }
            }
        }
        marks.insert(id, Mark::Done);
    }

    let mut marks = HashMap::new();
    let mut cycles = Vec::new();
    for section in &plan.sections {
        visit(&section.id, by_id, &mut marks, &mut cycles);
    }
    cycles
}

#[derive(Debug)]
pub enum AutoPlanError {
    UnsupportedHarness(String),
    SubprocessFailed(String),
    MalformedOutput(String),
    ValidationFailed(Vec<PlanValidationError>),
}

impl AutoPlanError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnsupportedHarness(_) => "unsupported_harness",
            Self::SubprocessFailed(_) => "subprocess_failed",
            Self::MalformedOutput(_) => "malformed_output",
            Self::ValidationFailed(_) => "validation_failed",
        }
    }
}

impl std::fmt::Display for AutoPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedHarness(h) => {
                write!(
                    f,
                    "planning harness '{h}' is not supported (only 'claude-code' is)"
                )
            }
            Self::SubprocessFailed(e) => write!(f, "planning subprocess failed: {e}"),
            Self::MalformedOutput(e) => write!(f, "planner output was not a valid plan: {e}"),
            Self::ValidationFailed(errs) => write!(
                f,
                "generated plan failed validation ({} problems)",
                errs.len()
            ),
        }
    }
}

impl std::error::Error for AutoPlanError {}

/// Runs the planning subprocess and returns its stdout. Production code uses
/// [`ClaudePlanRunner`]; tests inject canned output.
pub trait PlanRunner: Send + Sync {
    fn run(
        &self,
        prompt: String,
        workspace: PathBuf,
        model: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send>>;
}

/// One-shot `claude -p` restricted to [`PLAN_TOOLS`].
/// `--dangerously-skip-permissions` for the same reason as
/// `brain_harvest::ClaudeHarvestRunner`: a headless `-p` process has no TTY
/// to approve even reads.
pub struct ClaudePlanRunner;

fn build_claude_command(prompt: &str, workspace: &Path, model: Option<&str>) -> Command {
    let mut cmd = Command::new("claude");
    // `--tools` only restricts built-in tools; without `--strict-mcp-config`
    // the user's and workspace's MCP servers would load and run unprompted.
    cmd.args([
        "--dangerously-skip-permissions",
        "--strict-mcp-config",
        "--tools",
        PLAN_TOOLS,
    ]);
    if let Some(model) = model {
        cmd.args(["--model", model]);
    }
    cmd.args(["-p", prompt]).current_dir(workspace);
    cmd.env_clear();
    for var in PLAN_ENV_PASSTHROUGH {
        if let Ok(value) = std::env::var(var) {
            cmd.env(var, value);
        }
    }
    cmd
}

async fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<String> {
    cmd.kill_on_drop(true);
    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| anyhow::anyhow!("timed out after {}s", timeout.as_secs_f32()))??;
    if !output.status.success() {
        // `claude -p` prints some failures (e.g. "Not logged in") on stdout.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = if stderr.trim().is_empty() {
            String::from_utf8_lossy(&output.stdout)
        } else {
            stderr
        };
        anyhow::bail!("exited with {}: {}", output.status, detail.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

impl PlanRunner for ClaudePlanRunner {
    fn run(
        &self,
        prompt: String,
        workspace: PathBuf,
        model: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send>> {
        Box::pin(async move {
            run_with_timeout(
                build_claude_command(&prompt, &workspace, model.as_deref()),
                PLAN_TIMEOUT,
            )
            .await
        })
    }
}

/// The planner's instructions. `task` is the requesting orchestrator's own
/// text, not third-party content, so it is inlined as-is.
/// `type_guide` is `worker_type::type_guide`'s `- Name: use when` lines for
/// the same types, so the planner chooses a type the way orchestrators do.
pub fn build_plan_prompt(
    task: &str,
    parent_id: &str,
    worker_type_names: &[String],
    type_guide: &str,
    depth_cap: u32,
) -> String {
    let types = worker_type_names.join(", ");
    let default_type = crate::worker_type::DEFAULT_SPAWN_TYPE;
    format!(
        "You are planning how to split a task into sections, each handled by one agent session. \
Explore the current directory with Read and Grep only to understand the codebase; never modify anything.

Task:
{task}

Rules for the plan:
- Each section has a unique short \"id\", a \"brief\" (self-contained instructions for the agent that runs it), \
a \"worker_type\" (exactly one of: {types} — pick the one whose description below fits the section, \
{default_type} when none fits better), and \"dependencies\" (ids of sections that must finish first; \
no cycles, and never a section's own ancestor or descendant).
- Worker types and when to use each:
{type_guide}
- A top-level section's \"parent_id\" is \"{parent_id}\".
- A section may own child sections (children set \"parent_id\" to its id) only if its worker_type is \
\"{ORCHESTRATOR_WORKER_TYPE}\" — that proposes a sub-orchestrator that runs those children.
- The depth cap is {depth_cap}: a top-level section is depth 1, its children depth 2, and so on; no section may \
be deeper than {depth_cap}, and a \"{ORCHESTRATOR_WORKER_TYPE}\" section must be shallower than {depth_cap}. Prefer a flat plan unless nesting clearly helps.

Reply with only one ```json fenced code block containing an object of this shape, and nothing else:
{{\"sections\": [{{\"id\": \"...\", \"parent_id\": \"...\", \"worker_type\": \"...\", \"brief\": \"...\", \"dependencies\": []}}]}}"
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GhostPlanWire {
    sections: Vec<GhostNode>,
}

/// Parses the planner's reply: bare JSON, or the first fenced code block.
/// `parent_id` becomes the plan's root — the model never chooses it.
pub fn parse_plan_output(raw: &str, parent_id: &str) -> Result<GhostPlan, AutoPlanError> {
    let raw = raw.trim();
    let wire = serde_json::from_str::<GhostPlanWire>(raw).or_else(|bare_err| {
        let fenced = fenced_block(raw).ok_or_else(|| bare_err.to_string())?;
        serde_json::from_str::<GhostPlanWire>(fenced).map_err(|e| e.to_string())
    });
    match wire {
        Ok(wire) => Ok(GhostPlan {
            parent_id: parent_id.to_string(),
            sections: wire.sections,
        }),
        Err(e) => {
            let excerpt: String = raw.chars().take(MALFORMED_EXCERPT_CHARS).collect();
            Err(AutoPlanError::MalformedOutput(format!(
                "{e}; output began: {excerpt:?}"
            )))
        }
    }
}

/// The body of the first ``` fence, skipping an info string like `json`.
fn fenced_block(text: &str) -> Option<&str> {
    let after_open = &text[text.find("```")? + 3..];
    let body = &after_open[after_open.find('\n')? + 1..];
    Some(&body[..body.find("```")?])
}

/// Runs the planner for `task` and returns its plan only if it validates.
/// `parent_id` is the requesting orchestrator — every top-level section's
/// parent.
#[allow(clippy::too_many_arguments)]
pub async fn generate_plan(
    runner: &dyn PlanRunner,
    registry: &WorkerTypeRegistry,
    task: &str,
    workspace: PathBuf,
    parent_id: &str,
    harness: &str,
    model: Option<String>,
    depth_cap: u32,
) -> Result<GhostPlan, AutoPlanError> {
    if harness != "claude-code" {
        return Err(AutoPlanError::UnsupportedHarness(harness.to_string()));
    }
    let names = registry.user_facing_names();
    let guide = crate::worker_type::type_guide(registry, &names);
    let prompt = build_plan_prompt(task, parent_id, &names, &guide, depth_cap);
    let raw = runner
        .run(prompt, workspace, model)
        .await
        .map_err(|e| AutoPlanError::SubprocessFailed(e.to_string()))?;
    let plan = parse_plan_output(&raw, parent_id)?;
    let errors = validate_plan(&plan, registry, depth_cap);
    if errors.is_empty() {
        Ok(plan)
    } else {
        Err(AutoPlanError::ValidationFailed(errors))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> WorkerTypeRegistry {
        WorkerTypeRegistry::from_config(&[])
    }

    fn leaf(id: &str, worker_type: &str, deps: &[&str]) -> GhostNode {
        GhostNode {
            id: id.into(),
            parent_id: "root".into(),
            worker_type: Some(worker_type.into()),
            brief: "do it".into(),
            dependencies: deps.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn plan(sections: Vec<GhostNode>) -> GhostPlan {
        GhostPlan {
            parent_id: "root".into(),
            sections,
        }
    }

    #[test]
    fn valid_flat_plan_passes() {
        let p = plan(vec![
            leaf("a", "Implementor", &[]),
            leaf("b", "Researcher", &["a"]),
        ]);
        assert!(validate_plan(&p, &registry(), 3).is_empty());
    }

    #[test]
    fn nested_sub_orchestrator_within_depth_cap_passes() {
        let mut child = leaf("child", "Implementor", &[]);
        child.parent_id = "sub".into();
        let p = plan(vec![leaf("sub", "Orchestrator", &[]), child]);
        assert!(validate_plan(&p, &registry(), 3).is_empty());
    }

    #[test]
    fn child_under_a_non_orchestrator_worker_type_is_rejected() {
        let mut child = leaf("child", "Implementor", &[]);
        child.parent_id = "r".into();
        let errs = validate_plan(
            &plan(vec![leaf("r", "Researcher", &[]), child]),
            &registry(),
            3,
        );
        assert!(errs.contains(&PlanValidationError::NestedSpawnNotAllowed {
            id: "r".into(),
            worker_type: "Researcher".into(),
        }));
    }

    #[test]
    fn dangling_parent_is_rejected() {
        let mut s = leaf("a", "Implementor", &[]);
        s.parent_id = "nowhere".into();
        let errs = validate_plan(&plan(vec![s]), &registry(), 3);
        assert!(errs.contains(&PlanValidationError::DanglingParent {
            id: "a".into(),
            parent_id: "nowhere".into(),
        }));
    }

    #[test]
    fn duplicate_section_id_is_rejected() {
        let p = plan(vec![
            leaf("a", "Implementor", &[]),
            leaf("a", "Researcher", &[]),
        ]);
        assert!(validate_plan(&p, &registry(), 3)
            .contains(&PlanValidationError::DuplicateSectionId { id: "a".into() }));
    }

    #[test]
    fn section_reusing_the_plan_root_id_is_rejected() {
        let errs = validate_plan(
            &plan(vec![leaf("root", "Implementor", &[])]),
            &registry(),
            3,
        );
        assert!(errs.contains(&PlanValidationError::DuplicateSectionId { id: "root".into() }));
    }

    #[test]
    fn empty_plan_is_rejected() {
        assert_eq!(
            validate_plan(&plan(vec![]), &registry(), 3),
            vec![PlanValidationError::EmptyPlan]
        );
    }

    #[test]
    fn unknown_worker_type_is_rejected() {
        let errs = validate_plan(&plan(vec![leaf("a", "ghost-type", &[])]), &registry(), 3);
        assert!(errs.contains(&PlanValidationError::UnknownWorkerType {
            id: "a".into(),
            worker_type: "ghost-type".into(),
        }));
    }

    #[test]
    fn hidden_system_worker_type_is_rejected_as_unknown() {
        let errs = validate_plan(
            &plan(vec![leaf("a", crate::worker_type::PROMPT_WORKER_TYPE, &[])]),
            &registry(),
            3,
        );
        assert!(errs.contains(&PlanValidationError::UnknownWorkerType {
            id: "a".into(),
            worker_type: crate::worker_type::PROMPT_WORKER_TYPE.into(),
        }));
    }

    #[test]
    fn worker_type_alias_resolves_through_the_registry() {
        // "Implementation" is a documented legacy alias for "Implementor".
        let p = plan(vec![leaf("a", "Implementation", &[])]);
        assert!(validate_plan(&p, &registry(), 3).is_empty());
    }

    #[test]
    fn missing_worker_type_is_rejected() {
        let mut s = leaf("a", "Implementor", &[]);
        s.worker_type = None;
        let errs = validate_plan(&plan(vec![s]), &registry(), 3);
        assert!(errs.contains(&PlanValidationError::MissingWorkerType { id: "a".into() }));
    }

    #[test]
    fn dependency_on_unknown_section_is_rejected() {
        let errs = validate_plan(
            &plan(vec![leaf("a", "Implementor", &["missing"])]),
            &registry(),
            3,
        );
        assert!(errs.contains(&PlanValidationError::UnknownDependency {
            id: "a".into(),
            target: "missing".into(),
        }));
    }

    #[test]
    fn dependency_on_the_plan_root_is_rejected_as_unknown() {
        let errs = validate_plan(
            &plan(vec![leaf("a", "Implementor", &["root"])]),
            &registry(),
            3,
        );
        assert!(errs.contains(&PlanValidationError::UnknownDependency {
            id: "a".into(),
            target: "root".into(),
        }));
    }

    #[test]
    fn self_dependency_is_a_cycle() {
        let errs = validate_plan(
            &plan(vec![leaf("a", "Implementor", &["a"])]),
            &registry(),
            3,
        );
        assert!(errs
            .iter()
            .any(|e| matches!(e, PlanValidationError::DependencyCycle { .. })));
    }

    #[test]
    fn two_node_dependency_cycle_is_rejected() {
        let p = plan(vec![
            leaf("a", "Implementor", &["b"]),
            leaf("b", "Implementor", &["a"]),
        ]);
        assert!(validate_plan(&p, &registry(), 3)
            .iter()
            .any(|e| matches!(e, PlanValidationError::DependencyCycle { .. })));
    }

    #[test]
    fn tree_exactly_at_depth_cap_passes_and_one_over_is_rejected() {
        // root (depth 0) -> a (1) -> b (2) -> c (3): at cap 3, passes.
        let a = leaf("a", "Orchestrator", &[]);
        let mut b = leaf("b", "Orchestrator", &[]);
        b.parent_id = "a".into();
        let mut c = leaf("c", "Implementor", &[]);
        c.parent_id = "b".into();
        let p = plan(vec![a.clone(), b.clone(), c.clone()]);
        assert!(validate_plan(&p, &registry(), 3).is_empty());

        // root -> a -> b -> c -> d: depth 4, rejected.
        let mut d = leaf("d", "Implementor", &[]);
        d.parent_id = "c".into();
        c.worker_type = Some("Orchestrator".into());
        let errs = validate_plan(&plan(vec![a, b, c, d]), &registry(), 3);
        assert!(errs.contains(&PlanValidationError::DepthExceeded {
            id: "d".into(),
            depth: 4,
            cap: 3
        }));
    }

    #[test]
    fn childless_orchestrator_at_the_depth_cap_is_rejected() {
        // A sub-orchestrator at depth 3 would itself be orchestrator depth 3,
        // which `spawn-sub-orchestrator` refuses under cap 3.
        let a = leaf("a", "Orchestrator", &[]);
        let mut b = leaf("b", "Orchestrator", &[]);
        b.parent_id = "a".into();
        let mut c = leaf("c", "Orchestrator", &[]);
        c.parent_id = "b".into();
        let errs = validate_plan(&plan(vec![a, b, c]), &registry(), 3);
        assert_eq!(
            errs,
            vec![PlanValidationError::DepthExceeded {
                id: "c".into(),
                depth: 3,
                cap: 3
            }]
        );
    }

    #[test]
    fn dependency_on_own_ancestor_is_rejected() {
        let mut child = leaf("child", "Implementor", &["sub"]);
        child.parent_id = "sub".into();
        let errs = validate_plan(
            &plan(vec![leaf("sub", "Orchestrator", &[]), child]),
            &registry(),
            3,
        );
        assert!(errs.contains(&PlanValidationError::DependencyOnLineage {
            id: "child".into(),
            target: "sub".into(),
        }));
    }

    #[test]
    fn dependency_on_own_descendant_is_rejected() {
        let mut child = leaf("child", "Implementor", &[]);
        child.parent_id = "sub".into();
        let errs = validate_plan(
            &plan(vec![leaf("sub", "Orchestrator", &["child"]), child]),
            &registry(),
            3,
        );
        assert!(errs.contains(&PlanValidationError::DependencyOnLineage {
            id: "sub".into(),
            target: "child".into(),
        }));
    }

    #[test]
    fn parent_cycle_terminates_and_is_rejected() {
        let mut a = leaf("a", "Orchestrator", &[]);
        a.parent_id = "b".into();
        let mut b = leaf("b", "Orchestrator", &[]);
        b.parent_id = "a".into();
        let errs = validate_plan(&plan(vec![a, b]), &registry(), 3);
        assert!(errs
            .iter()
            .any(|e| matches!(e, PlanValidationError::DepthExceeded { .. })));
    }

    #[test]
    fn every_error_is_reported_not_just_the_first() {
        let mut s = leaf("a", "ghost-type", &["missing"]);
        s.parent_id = "nowhere".into();
        assert_eq!(validate_plan(&plan(vec![s]), &registry(), 3).len(), 3);
    }

    struct FakeRunner(String);

    impl PlanRunner for FakeRunner {
        fn run(
            &self,
            _: String,
            _: PathBuf,
            _: Option<String>,
        ) -> Pin<Box<dyn Future<Output = Result<String>> + Send>> {
            let out = self.0.clone();
            Box::pin(async move { Ok(out) })
        }
    }

    const ONE_SECTION: &str = r#"{"sections":[{"id":"a","parent_id":"root","brief":"b","worker_type":"Implementor","dependencies":[]}]}"#;

    #[test]
    fn parse_plan_output_accepts_bare_json() {
        let parsed = parse_plan_output(ONE_SECTION, "root").unwrap();
        assert_eq!(parsed.sections.len(), 1);
        assert_eq!(parsed.parent_id, "root");
    }

    #[test]
    fn parse_plan_output_extracts_fenced_json_block() {
        let raw = format!(
            "Here is the plan:\n```json\n{ONE_SECTION}\n```\nLet me know if you'd like changes."
        );
        assert_eq!(parse_plan_output(&raw, "root").unwrap().sections[0].id, "a");
    }

    #[test]
    fn parse_plan_output_extracts_unlabelled_fence() {
        let raw = format!("```\n{ONE_SECTION}\n```");
        assert!(parse_plan_output(&raw, "root").is_ok());
    }

    #[test]
    fn parse_plan_output_rejects_non_json_cleanly() {
        let err = parse_plan_output("I couldn't find anything to plan.", "root").unwrap_err();
        assert!(matches!(err, AutoPlanError::MalformedOutput(_)));
    }

    #[test]
    fn parse_plan_output_rejects_unknown_fields() {
        let raw = r#"{"sections":[{"id":"a","parent_id":"root","brief":"b","worker_type":"Implementor","priority":1}]}"#;
        assert!(matches!(
            parse_plan_output(raw, "root"),
            Err(AutoPlanError::MalformedOutput(_))
        ));
    }

    #[test]
    fn build_plan_prompt_includes_task_worker_types_and_depth_cap() {
        let prompt = build_plan_prompt(
            "add dark mode",
            "orch-1",
            &["Implementor".into(), "Researcher".into()],
            "- Implementor: build things\n- Researcher: look things up",
            3,
        );
        assert!(prompt.contains("- Researcher: look things up"), "the planner sees each type's use-when: {prompt}");
        assert!(prompt.contains("Implementor when none fits better"));
        assert!(prompt.contains("add dark mode"));
        assert!(prompt.contains("Implementor"));
        assert!(prompt.contains("Researcher"));
        assert!(prompt.contains("orch-1"));
        assert!(prompt.contains("depth cap is 3"));
    }

    #[test]
    fn planning_command_is_read_only_and_scrubs_the_environment() {
        let cmd = build_claude_command("plan this", Path::new("."), Some("claude-opus-4-5"));
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let tools = args
            .iter()
            .position(|a| a == "--tools")
            .map(|i| args[i + 1].as_str());
        assert_eq!(tools, Some("Read,Grep"));
        assert!(args.windows(2).any(|w| w == ["--model", "claude-opus-4-5"]));
        assert!(args.iter().any(|a| a == "--strict-mcp-config"));
        for (key, _) in cmd.as_std().get_envs() {
            assert!(
                PLAN_ENV_PASSTHROUGH.contains(&key.to_str().unwrap()),
                "{key:?} leaked"
            );
        }
    }

    #[tokio::test]
    async fn subprocess_that_overruns_its_timeout_is_killed_and_reported() {
        let mut cmd = Command::new("sleep");
        cmd.arg("5");
        let err = run_with_timeout(cmd, Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
    }

    #[tokio::test]
    async fn failure_reported_only_on_stdout_is_surfaced() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo 'Not logged in'; exit 1"]);
        let err = run_with_timeout(cmd, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Not logged in"), "{err}");
    }

    #[test]
    fn env_passthrough_keeps_user_for_keychain_auth() {
        assert!(PLAN_ENV_PASSTHROUGH.contains(&"USER"));
    }

    #[tokio::test]
    async fn generate_plan_returns_validated_plan_on_good_output() {
        let got = generate_plan(
            &FakeRunner(ONE_SECTION.into()),
            &registry(),
            "task",
            PathBuf::from("."),
            "root",
            "claude-code",
            None,
            3,
        )
        .await
        .unwrap();
        assert_eq!(got.sections[0].id, "a");
    }

    #[tokio::test]
    async fn generate_plan_surfaces_validation_errors_without_returning_a_plan() {
        let runner = FakeRunner(ONE_SECTION.replace("Implementor", "ghost-type"));
        let err = generate_plan(
            &runner,
            &registry(),
            "task",
            PathBuf::from("."),
            "root",
            "claude-code",
            None,
            3,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AutoPlanError::ValidationFailed(errs) if !errs.is_empty()));
    }

    #[tokio::test]
    async fn generate_plan_rejects_unsupported_harness_before_running_anything() {
        let runner = FakeRunner("should never be read".into());
        let err = generate_plan(
            &runner,
            &registry(),
            "task",
            PathBuf::from("."),
            "root",
            "codex",
            None,
            3,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AutoPlanError::UnsupportedHarness(h) if h == "codex"));
    }

    /// Needs a real, authenticated `claude` on PATH — run with `--ignored`.
    #[tokio::test]
    #[ignore]
    async fn generate_plan_end_to_end_against_a_real_subprocess() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "# fixture\n").unwrap();
        let got = generate_plan(
            &ClaudePlanRunner,
            &registry(),
            "Add a one-line CHANGELOG entry",
            dir.path().to_path_buf(),
            "root",
            "claude-code",
            None,
            3,
        )
        .await
        .expect("a well-formed, valid plan");
        assert!(!got.sections.is_empty());
    }

    #[test]
    fn ghost_plan_json_matches_the_overview_ghost_node_shape() {
        let value = serde_json::to_value(plan(vec![leaf("a", "Implementor", &["b"])])).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "parent_id": "root",
                "sections": [{
                    "id": "a", "parent_id": "root", "worker_type": "Implementor",
                    "brief": "do it", "dependencies": ["b"],
                }],
            })
        );
    }
}
