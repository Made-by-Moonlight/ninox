//! Backend for `ninox worker-type list/show/create/edit/delete/reset/
//! export/import` — `AppConfig` reads/writes over `crate::worker_type`'s
//! registry.
//!
//! `list`/`show`/`export` are read-only and open to any caller (orchestrator,
//! worker, or a bare human/CLI call — see the worker-types-backend design
//! spec and MLOPS-5416's brief: an agent needs to inspect types to know what
//! it may spawn). The mutating operations (`create`/`edit`/`delete`/`reset`/
//! `import`) are NOT open to everyone: `can_spawn` enforcement in
//! `ninox spawn` is meaningless if any caller can simply edit its own
//! type's `can_spawn` to `*` first, so [`authorize_mutation`] gates them —
//! see its doc comment.

use anyhow::{bail, Context, Result};

use crate::config::AppConfig;
use crate::orchestrator_auth::resolve_caller_worker_type;
use crate::store::Store;
use crate::worker_type::{
    canonical_name, display_name, from_toml_many, is_system_type, to_toml_many, validate_permission_rule,
    validate_type_name, WorkerTypeRegistry, WorkerTypeSpec,
};

/// What a caller is allowed to touch, resolved once by [`authorize_mutation`]
/// and threaded into every mutating call below. `None`: the caller has no
/// live session at all (the user) — fully unrestricted. `Some(spec)`: an
/// authorized AGENT (its own type carries `can_manage_types: true`),
/// further restricted by the `ensure_*` functions below so that
/// authorization to manage types at all is not the same as unlimited power
/// to do so — see each function's doc comment for the specific escalation
/// it closes.
struct MutationScope {
    caller: Option<WorkerTypeSpec>,
}

impl MutationScope {
    fn is_user(&self) -> bool {
        self.caller.is_none()
    }
}

/// Authorizes a `ninox worker-type create/edit/delete/reset/import` call.
/// `caller_session_id` MUST be pane-verified (the live tmux/ptyd pane's own
/// name — see `runtime::current_private_pane_identity` and
/// `resolve_caller_worker_type`'s doc comment), never the raw `NINOX_SESSION`
/// env var, or a worker could `env -u NINOX_SESSION` its way to looking like
/// an unrestricted direct call from the user.
///
/// A caller with NO live session at all (`None` — the user, at a terminal,
/// GUI, or TUI) is always authorized. Otherwise the caller's own resolved
/// worker type must carry `can_manage_types: true` (see that field's doc
/// comment) — refused with a message telling the caller to ask the user to
/// run this instead, since that's always the fallback path. Being granted
/// `can_manage_types` at all is necessary but not sufficient to do anything
/// an authorized agent wants: see `ensure_not_own_type`,
/// `ensure_target_not_hidden`, `ensure_can_manage_types_untouched`, and
/// `ensure_can_spawn_not_widened` for the further restrictions every
/// mutating call applies on top of this gate.
fn authorize_mutation(store: &Store, caller_session_id: Option<&str>) -> Result<MutationScope> {
    let Some(caller_type) = resolve_caller_worker_type(store, caller_session_id) else {
        return Ok(MutationScope { caller: None });
    };
    let reg = AppConfig::load()?.worker_type_registry();
    let spec = reg.get(&caller_type).cloned().unwrap_or_default();
    if !spec.can_manage_types {
        bail!(
            "session '{}' (type '{caller_type}') is not allowed to manage worker types — \
             ask the user to run this instead",
            caller_session_id.unwrap_or("?"),
        );
    }
    Ok(MutationScope { caller: Some(spec) })
}

/// Refuses an authorized caller from mutating the one type its OWN session
/// currently has — e.g. an `Implementor` session granted `can_manage_types`
/// still can't `worker-type edit Implementor --can-spawn '*'` on itself.
fn ensure_not_own_type(scope: &MutationScope, target_canonical: &str) -> Result<()> {
    if scope.caller.as_ref().is_some_and(|c| c.name == target_canonical) {
        bail!("a session cannot create, edit, delete, reset, or import over its own worker type ('{target_canonical}')");
    }
    Ok(())
}

/// Refuses an authorized AGENT (never the user) from creating, editing,
/// resetting, deleting, or importing over a hidden/system type (e.g.
/// MLOPS-5415's `prompt`) — `is_hidden` is the OR of the target's current
/// hidden flag (if it already exists) and the incoming entry's own hidden
/// flag (so an agent can't launder a hidden type through an edit that also
/// flips it to non-hidden, or vice versa create a brand-new hidden type).
/// System types are internal/ninox-governed; only a human at a terminal
/// gets to touch their definition.
fn ensure_target_not_hidden(scope: &MutationScope, is_hidden: bool, target_canonical: &str) -> Result<()> {
    if !scope.is_user() && is_hidden {
        bail!(
            "'{target_canonical}' is a hidden/system type — only the user (no calling \
             session) may create, edit, delete, reset, or import over one"
        );
    }
    Ok(())
}

/// Refuses an authorized AGENT (never the user) from setting OR clearing
/// `can_manage_types` on any type (including a brand-new one it creates, or
/// its own — though `ensure_not_own_type` already blocks that case). This
/// is the fix for the chain a review caught: without it, any agent granted
/// `can_manage_types` could mint a brand-new type with `can_manage_types:
/// true` and `can_spawn: "*"`, spawn an instance of it (if its own
/// `can_spawn` allows spawning that type), and the result is a second,
/// equally-privileged mutator with no further restriction — repeatable
/// without bound. Only a human at a terminal may ever grant this
/// capability.
fn ensure_can_manage_types_untouched(scope: &MutationScope, requested: Option<bool>) -> Result<()> {
    if !scope.is_user() && requested.is_some() {
        bail!(
            "the can_manage_types field can only be set or cleared by the user (no calling \
             session), not by an agent — ask the user to run this instead"
        );
    }
    Ok(())
}

/// Refuses an authorized AGENT (never the user) from creating or editing a
/// type whose `can_spawn` reaches further than its OWN type's `can_spawn`
/// — the other half of the chain above: even without granting
/// `can_manage_types`, an agent could otherwise mint a type with
/// `can_spawn: "*"` and hand a spawned instance of it broader reach than
/// the agent minting it ever had itself. An agent whose own `can_spawn` is
/// `"*"` is unrestricted here (it already has unlimited reach); otherwise
/// every entry in `resulting` (including a literal `"*"`) must already
/// appear in the agent's own `can_spawn`.
fn ensure_can_spawn_not_widened(scope: &MutationScope, resulting: &[String]) -> Result<()> {
    let Some(caller) = &scope.caller else { return Ok(()) };
    if caller.can_spawn.iter().any(|s| s == "*") {
        return Ok(());
    }
    for name in resulting {
        if !caller.can_spawn.contains(name) {
            bail!(
                "'{name}' is not in your own type's can_spawn ({}) — an agent cannot grant a \
                 type more spawn reach than it has itself",
                if caller.can_spawn.is_empty() { "none".to_string() } else { caller.can_spawn.join(", ") },
            );
        }
    }
    Ok(())
}

/// One entry in a `list`/`show` result: the effective spec plus whether it
/// has a compiled-in default (shown as `(built-in)` in human output,
/// `"built_in": true` in `--json`).
pub struct Entry {
    pub spec:      WorkerTypeSpec,
    pub built_in:  bool,
}

/// Whether `name` (canonical or alias) has a compiled-in default — what
/// `reset` accepts and `delete` refuses.
pub fn is_builtin(name: &str) -> bool {
    // The registry with no config overrides is exactly the compiled-in
    // builtin set — cheap to construct, no I/O.
    WorkerTypeRegistry::from_config(&[]).get(name).is_some()
}

/// `ninox worker-type list` — every user-facing (non-hidden) type, builtins
/// included, alphabetical.
pub fn list() -> Result<Vec<Entry>> {
    let cfg = AppConfig::load()?;
    let reg = cfg.worker_type_registry();
    Ok(reg.user_facing_names().into_iter().map(|name| {
        let spec = reg.get(&name).cloned().unwrap_or_default();
        Entry { built_in: is_builtin(&name), spec }
    }).collect())
}

/// `ninox worker-type list` for an agent: the user-facing types its own
/// session may spawn — its type's `can_spawn`, and `Orchestrator` only when
/// it can really start one (an orchestrator, or a type with
/// `allow_nested_spawn`; `can_spawn` alone doesn't get a worker through
/// `spawn-sub-orchestrator`). A caller with no session (the user) or one
/// that manages types (e.g. the Ninox prompt) sees every type.
pub fn list_spawnable(store: &Store, caller_session_id: Option<&str>) -> Result<Vec<Entry>> {
    let entries = list()?;
    let Some(caller_type) = resolve_caller_worker_type(store, caller_session_id) else {
        return Ok(entries);
    };
    let reg = AppConfig::load()?.worker_type_registry();
    let caller = reg.get(&caller_type).cloned().unwrap_or_default();
    if caller.can_manage_types {
        return Ok(entries);
    }
    let is_orchestrator = caller.name == "Orchestrator";
    Ok(entries.into_iter().filter(|e| {
        if e.spec.name == "Orchestrator" {
            is_orchestrator || caller.allow_nested_spawn
        } else {
            caller.allows_spawning(&e.spec.name)
        }
    }).collect())
}

/// What the GUI/TUI Worker Types editors list: [`list`] plus the system
/// types (the Ninox prompt type), which are editable but never spawnable.
pub fn editor_list() -> Result<Vec<Entry>> {
    let mut entries = list()?;
    let reg = AppConfig::load()?.worker_type_registry();
    for name in reg.names().into_iter().filter(|n| is_system_type(n)) {
        let spec = reg.get(&name).cloned().unwrap_or_default();
        entries.push(Entry { built_in: true, spec });
    }
    Ok(entries)
}

/// `ninox worker-type show <name>` — the effective (builtin-defaults +
/// config-overlay) spec for one name, alias-resolved. Showing a hidden type
/// by its exact name is allowed (an agent debugging MLOPS-5415's `prompt`
/// type still needs to inspect it) — only `list` and `ninox spawn --type`
/// hide it.
pub fn show(name: &str) -> Result<Entry> {
    let cfg = AppConfig::load()?;
    let reg = cfg.worker_type_registry();
    let spec = reg.get(name).cloned()
        .with_context(|| format!("no worker type named '{name}' — see `ninox worker-type list`"))?;
    Ok(Entry { built_in: is_builtin(&spec.name), spec })
}

/// Parses a `can_spawn` CLI value: `*` for every user-facing type, a
/// comma-separated list of names otherwise. Each named entry must already
/// resolve in the registry (built-in, or a custom type defined earlier in
/// the same session) — a typo here would otherwise silently compile into an
/// unreachable grant.
fn parse_can_spawn(raw: &str, reg: &WorkerTypeRegistry) -> Result<Vec<String>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    if raw == "*" {
        return Ok(vec!["*".to_string()]);
    }
    raw.split(',').map(str::trim).filter(|s| !s.is_empty()).map(|name| {
        let spec = reg.get(name).with_context(|| {
            format!("can_spawn entry '{name}' is not a known worker type — define it first, or use '*'")
        })?;
        Ok(spec.name.clone())
    }).collect()
}

/// Toggles `name` (or the literal `"*"`) in a `can_spawn` selection.
/// `"*"` and named entries are mutually exclusive — matches
/// [`parse_can_spawn`]'s own contract (`raw == "*"` is the
/// wildcard; anything else is a comma list of names, and `"*"` is not a
/// valid name) — so selecting one clears the other rather than producing a
/// value the backend would reject.
pub fn toggle_can_spawn(can_spawn: &mut Vec<String>, name: &str) {
    if name == "*" {
        if can_spawn.iter().any(|n| n == "*") {
            can_spawn.clear();
        } else {
            *can_spawn = vec!["*".to_string()];
        }
        return;
    }
    can_spawn.retain(|n| n != "*");
    if let Some(pos) = can_spawn.iter().position(|n| n == name) {
        can_spawn.remove(pos);
    } else {
        can_spawn.push(name.to_string());
    }
}

fn validate_use_when(use_when: &str) -> Result<()> {
    if use_when.contains('\n') {
        bail!("a use-when description is one line");
    }
    Ok(())
}

fn validate_rules(rules: &[String]) -> Result<()> {
    for rule in rules {
        validate_permission_rule(rule)?;
    }
    Ok(())
}

/// Fields accepted by both `create` and `edit` — `edit` starts from the
/// target's current effective spec and applies only the `Some`/non-empty
/// fields given; `create` starts from [`WorkerTypeSpec::default`].
#[derive(Default)]
pub struct Fields {
    pub brief:      Option<String>,
    /// `Some("")` clears the type's "use when" description.
    pub use_when:   Option<String>,
    pub harness:    Option<String>,
    pub model:      Option<String>,
    pub allow:      Vec<String>,
    pub deny:       Vec<String>,
    pub ask:        Vec<String>,
    pub clear_allow: bool,
    pub clear_deny:  bool,
    pub clear_ask:   bool,
    /// `None`: leave `can_spawn` unchanged (`edit`) or empty (`create`).
    /// `Some("")`: clear it. `Some("*")`/`Some("A,B")`: replace it.
    pub can_spawn:  Option<String>,
    /// `None`: leave `can_manage_types` unchanged (`edit`) or `false`
    /// (`create`). `Some(v)`: set it explicitly.
    pub can_manage_types: Option<bool>,
    /// `None`: leave whether the allow/deny/ask rules are enforced
    /// unchanged. `Some(v)`: turn enforcement on or off.
    pub permissions_enabled: Option<bool>,
}

fn apply_fields(mut spec: WorkerTypeSpec, fields: Fields, reg: &WorkerTypeRegistry) -> Result<WorkerTypeSpec> {
    let system = is_system_type(&spec.name);
    if system && (fields.can_spawn.is_some() || fields.can_manage_types.is_some() || fields.permissions_enabled.is_some()) {
        bail!(
            "'{}' is a system type: its can_spawn, can_manage_types and permission enforcement \
             are fixed — only harness, model, brief (appended to the built-in one), allow, deny \
             (added to the built-in rules) and ask can change",
            display_name(&spec.name),
        );
    }
    if system && fields.harness.as_deref().is_some_and(|h| h != AppConfig::LOCKED_HARNESS) {
        bail!(
            "'{}' runs on {} only: its permission rules are enforced through Claude Code's \
             settings.json, which other harnesses don't read",
            display_name(&spec.name), AppConfig::LOCKED_HARNESS,
        );
    }
    if let Some(brief) = fields.brief {
        let brief = if brief.is_empty() { None } else { Some(brief) };
        if system {
            spec.brief_addendum = brief;
        } else {
            spec.brief_template = brief;
        }
    }
    if let Some(use_when) = fields.use_when {
        if system {
            bail!("'{}' is never spawned, so it has no use-when description", display_name(&spec.name));
        }
        let use_when = use_when.trim().to_string();
        validate_use_when(&use_when)?;
        // Stored even when empty: an explicit clear must not fall back to a
        // built-in's description on load.
        spec.use_when = Some(use_when);
    }
    if let Some(harness) = fields.harness {
        spec.harness = harness;
    }
    if let Some(model) = fields.model {
        spec.model = if model.is_empty() { None } else { Some(model) };
    }
    if fields.clear_allow {
        spec.permissions.allow.clear();
    }
    if !fields.allow.is_empty() {
        spec.permissions.allow = fields.allow;
    }
    if fields.clear_deny {
        spec.permissions.deny.clear();
    }
    if !fields.deny.is_empty() {
        spec.permissions.deny = fields.deny;
    }
    if fields.clear_ask {
        spec.permissions.ask.clear();
    }
    if !fields.ask.is_empty() {
        spec.permissions.ask = fields.ask;
    }
    validate_rules(&spec.permissions.allow)?;
    validate_rules(&spec.permissions.deny)?;
    validate_rules(&spec.permissions.ask)?;
    if let Some(can_spawn) = fields.can_spawn {
        spec.can_spawn = parse_can_spawn(&can_spawn, reg)?;
    }
    if let Some(can_manage_types) = fields.can_manage_types {
        spec.can_manage_types = can_manage_types;
    }
    if let Some(enabled) = fields.permissions_enabled {
        spec.permissions.enabled = enabled;
    }
    Ok(spec)
}

/// `ninox worker-type create --name <n> --brief <text> ...`. Refuses a name
/// that already resolves to an existing type (built-in, alias, or custom)
/// — use `edit` to change one. `caller_session_id` must be pane-verified —
/// see [`authorize_mutation`].
pub fn create(store: &Store, caller_session_id: Option<&str>, name: &str, brief: &str, fields: Fields) -> Result<WorkerTypeSpec> {
    let scope = authorize_mutation(store, caller_session_id)?;
    validate_type_name(name)?;
    let cfg = AppConfig::load()?;
    let reg = cfg.worker_type_registry();
    if reg.get(name).is_some() {
        bail!(
            "worker type '{name}' already exists (resolved to '{}') — use `ninox worker-type edit \
             {name}` instead",
            reg.get(name).unwrap().name,
        );
    }
    let requested_can_manage_types = fields.can_manage_types;
    let base = WorkerTypeSpec {
        name: name.to_string(),
        brief_template: if brief.is_empty() { None } else { Some(brief.to_string()) },
        ..WorkerTypeSpec::default()
    };
    let spec = apply_fields(base, fields, &reg)?;
    let canonical = canonical_name(&spec.name);
    ensure_not_own_type(&scope, &canonical)?;
    ensure_target_not_hidden(&scope, spec.hidden, &canonical)?;
    ensure_can_manage_types_untouched(&scope, requested_can_manage_types)?;
    ensure_can_spawn_not_widened(&scope, &spec.can_spawn)?;
    let spec = AppConfig::update(|c| {
        c.upsert_worker_type(spec.clone());
    }).map(|_| spec)?;
    Ok(spec)
}

/// `ninox worker-type edit <name> ...` — starts from the EFFECTIVE spec
/// (builtin defaults + any existing overlay), so editing a built-in without
/// touching every field doesn't blow away its compiled-in defaults for the
/// fields left unspecified. `caller_session_id` must be pane-verified — see
/// [`authorize_mutation`].
pub fn edit(store: &Store, caller_session_id: Option<&str>, name: &str, fields: Fields) -> Result<WorkerTypeSpec> {
    let scope = authorize_mutation(store, caller_session_id)?;
    let cfg = AppConfig::load()?;
    let reg = cfg.worker_type_registry();
    let current = reg.get(name).cloned()
        .with_context(|| format!("no worker type named '{name}' — see `ninox worker-type list`"))?;
    let canonical = current.name.clone();
    ensure_not_own_type(&scope, &canonical)?;
    ensure_target_not_hidden(&scope, current.hidden, &canonical)?;
    ensure_can_manage_types_untouched(&scope, fields.can_manage_types)?;
    let spec = apply_fields(current, fields, &reg)?;
    ensure_can_spawn_not_widened(&scope, &spec.can_spawn)?;
    let (saved, ()) = AppConfig::update(|c| {
        c.upsert_worker_type(spec.clone());
    })?;
    let spec = saved.worker_type_registry().get(&canonical).cloned().unwrap_or(spec);
    debug_assert_eq!(spec.name, canonical, "edit must never change a type's name");
    Ok(spec)
}

/// `ninox worker-type delete <name>` — refuses a built-in (by canonical
/// name or alias): deleting its config overlay would only reveal the
/// compiled-in default underneath, not remove the type, so `reset` is the
/// honest operation for a built-in. `caller_session_id` must be
/// pane-verified — see [`authorize_mutation`].
pub fn delete(store: &Store, caller_session_id: Option<&str>, name: &str) -> Result<()> {
    let scope = authorize_mutation(store, caller_session_id)?;
    let canonical = canonical_name(name);
    ensure_not_own_type(&scope, &canonical)?;
    // Not explicitly listed alongside edit/reset in the review that asked
    // for this gate, but the same reasoning applies symmetrically: a custom
    // hidden type (the only kind `delete` can ever reach — a hidden
    // builtin always refuses below, same as any builtin) is still
    // ninox/user-governed, not agent-governed.
    if let Some(target) = AppConfig::load()?.worker_type_registry().get(&canonical) {
        ensure_target_not_hidden(&scope, target.hidden, &canonical)?;
    }
    if is_builtin(&canonical) {
        bail!(
            "'{name}' is a built-in worker type and can't be deleted — \
             use `ninox worker-type reset {name}` to remove its customization instead"
        );
    }
    let cfg = AppConfig::load()?;
    if !cfg.worker_types.iter().any(|s| canonical_name(&s.name) == canonical) {
        bail!("no custom worker type named '{name}' to delete");
    }
    AppConfig::update(|c| { c.remove_worker_type(&canonical); })?;
    Ok(())
}

/// `ninox worker-type reset <name>` — removes a built-in's config overlay,
/// reverting it to its compiled-in default. Refuses a name with no overlay
/// (built-in and never customized, or simply unknown) — nothing to reset.
/// `caller_session_id` must be pane-verified — see [`authorize_mutation`].
pub fn reset(store: &Store, caller_session_id: Option<&str>, name: &str) -> Result<WorkerTypeSpec> {
    let scope = authorize_mutation(store, caller_session_id)?;
    let canonical = canonical_name(name);
    ensure_not_own_type(&scope, &canonical)?;
    if !is_builtin(&canonical) {
        bail!("'{name}' is not a built-in worker type — did you mean `ninox worker-type delete {name}`?");
    }
    let builtin_hidden = WorkerTypeRegistry::from_config(&[]).get(&canonical).unwrap().hidden;
    ensure_target_not_hidden(&scope, builtin_hidden, &canonical)?;
    let cfg_before = AppConfig::load()?;
    if !cfg_before.worker_types.iter().any(|s| canonical_name(&s.name) == canonical) {
        bail!("'{name}' has no customization to reset — it's already at its compiled-in default");
    }
    AppConfig::update(|c| { c.remove_worker_type(&canonical); })?;
    Ok(WorkerTypeRegistry::from_config(&[]).get(&canonical).cloned().unwrap())
}

/// `ninox worker-type export [<name>...]` — every user-facing type with no
/// names given, else exactly the named ones (alias-resolved; an unknown
/// name is a clear error rather than a silently short export).
pub fn export(names: &[String]) -> Result<String> {
    let cfg = AppConfig::load()?;
    let reg = cfg.worker_type_registry();
    let specs: Vec<WorkerTypeSpec> = if names.is_empty() {
        reg.user_facing_names().iter().map(|n| reg.get(n).cloned().unwrap()).collect()
    } else {
        names.iter().map(|n| {
            reg.get(n).cloned().with_context(|| format!("no worker type named '{n}'"))
        }).collect::<Result<Vec<_>>>()?
    };
    to_toml_many(&specs)
}

/// `ninox worker-type import <file>` — parses one or more `[[worker_types]]`
/// entries from `src` and upserts each (validated the same way `create`
/// validates a hand-built one). Returns the imported (canonical) names.
/// `caller_session_id` must be pane-verified — see [`authorize_mutation`].
pub fn import(store: &Store, caller_session_id: Option<&str>, src: &str) -> Result<Vec<String>> {
    let scope = authorize_mutation(store, caller_session_id)?;
    let specs = from_toml_many(src)?;
    let reg = AppConfig::load()?.worker_type_registry();
    for spec in &specs {
        validate_type_name(&spec.name)?;
        validate_use_when(spec.use_when.as_deref().unwrap_or_default())?;
        validate_rules(&spec.permissions.allow)?;
        validate_rules(&spec.permissions.deny)?;
        validate_rules(&spec.permissions.ask)?;
        let canonical = canonical_name(&spec.name);
        ensure_not_own_type(&scope, &canonical)?;
        let existing_hidden = reg.get(&canonical).is_some_and(|s| s.hidden);
        ensure_target_not_hidden(&scope, existing_hidden || spec.hidden, &canonical)?;
        if !scope.is_user() && spec.can_manage_types {
            bail!(
                "the can_manage_types field can only be set by the user (no calling session), \
                 not by an agent — '{canonical}' sets it in this import"
            );
        }
        ensure_can_spawn_not_widened(&scope, &spec.can_spawn)?;
    }
    let mut imported = Vec::with_capacity(specs.len());
    AppConfig::update(|c| {
        for spec in &specs {
            c.upsert_worker_type(spec.clone());
        }
    })?;
    for spec in specs {
        imported.push(canonical_name(&spec.name));
    }
    Ok(imported)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn in_scratch_config<T>(f: impl FnOnce() -> T) -> T {
        let _iso = crate::test_isolation::isolate();
        f()
    }

    fn store() -> Store {
        Store::open(tempfile::tempdir().unwrap().keep().join("t.db")).unwrap()
    }

    /// `None` throughout this module's existing tests stands for "no
    /// calling session at all" (the user, at a terminal/GUI/TUI) — always
    /// authorized. The mutation-gate tests below exercise the other cases.
    #[test]
    fn list_includes_builtins_marked_as_such() {
        in_scratch_config(|| {
            let entries = list().unwrap();
            let implementor = entries.iter().find(|e| e.spec.name == "Implementor").unwrap();
            assert!(implementor.built_in);
        });
    }

    #[test]
    fn list_excludes_hidden_types() {
        in_scratch_config(|| {
            import(&store(), None, "[[worker_types]]\nname = \"prompt\"\nhidden = true\n").unwrap();
            assert!(!list().unwrap().iter().any(|e| e.spec.name == "prompt"));
            assert!(show("prompt").unwrap().spec.hidden);
        });
    }

    #[test]
    fn create_then_show_round_trips_and_rejects_a_duplicate_name() {
        in_scratch_config(|| {
            let spec = create(&store(), None, "SecurityReviewer", "Review for security issues: {prompt}", Fields {
                allow: vec!["Read".into(), "Grep".into()],
                can_spawn: Some("Researcher".into()),
                ..Default::default()
            }).unwrap();
            assert_eq!(spec.name, "SecurityReviewer");
            assert_eq!(spec.can_spawn, vec!["Researcher".to_string()]);

            let shown = show("SecurityReviewer").unwrap();
            assert!(!shown.built_in);
            assert_eq!(shown.spec.permissions.allow, vec!["Read".to_string(), "Grep".to_string()]);

            let err = create(&store(), None, "SecurityReviewer", "again", Fields::default()).unwrap_err().to_string();
            assert!(err.contains("already exists"), "{err}");
        });
    }

    #[test]
    fn create_rejects_an_invalid_name_or_rule() {
        in_scratch_config(|| {
            assert!(create(&store(), None, "has space", "x", Fields::default()).is_err());
            let err = create(&store(), None, "Ok_Name", "x", Fields {
                allow: vec!["(bad".into()],
                ..Default::default()
            }).unwrap_err().to_string();
            assert!(err.contains("permission rule"), "{err}");
        });
    }

    #[test]
    fn edit_applies_only_the_given_fields_and_keeps_the_rest() {
        in_scratch_config(|| {
            create(&store(), None, "Tester", "Test: {prompt}", Fields { harness: Some("codex".into()), ..Default::default() }).unwrap();
            let edited = edit(&store(), None, "Tester", Fields { model: Some("gpt-5".into()), ..Default::default() }).unwrap();
            assert_eq!(edited.harness, "codex", "an untouched field must survive the edit");
            assert_eq!(edited.model.as_deref(), Some("gpt-5"));
            assert_eq!(edited.brief_template.as_deref(), Some("Test: {prompt}"));
        });
    }

    #[test]
    fn toggle_can_spawn_wildcard_and_names_are_mutually_exclusive() {
        let mut can_spawn = Vec::new();
        toggle_can_spawn(&mut can_spawn, "Researcher");
        assert_eq!(can_spawn, vec!["Researcher".to_string()]);
        toggle_can_spawn(&mut can_spawn, "*");
        assert_eq!(can_spawn, vec!["*".to_string()], "selecting * must replace any named entries");
        toggle_can_spawn(&mut can_spawn, "Evaluator");
        assert_eq!(can_spawn, vec!["Evaluator".to_string()], "selecting a name must clear the wildcard");
        toggle_can_spawn(&mut can_spawn, "Evaluator");
        assert!(can_spawn.is_empty(), "toggling the same name off again must clear it");
    }

    #[test]
    fn edit_turns_permission_enforcement_on_and_off() {
        in_scratch_config(|| {
            create(&store(), None, "Scoped", "x: {prompt}", Fields::default()).unwrap();
            let on = edit(&store(), None, "Scoped", Fields { permissions_enabled: Some(true), ..Default::default() }).unwrap();
            assert!(on.permissions.enabled);
            let kept = edit(&store(), None, "Scoped", Fields { model: Some("m".into()), ..Default::default() }).unwrap();
            assert!(kept.permissions.enabled, "None leaves it as it was");
            let off = edit(&store(), None, "Scoped", Fields { permissions_enabled: Some(false), ..Default::default() }).unwrap();
            assert!(!off.permissions.enabled);
        });
    }

    #[test]
    fn edit_can_clear_a_rule_list_explicitly() {
        in_scratch_config(|| {
            create(&store(), None, "Tester2", "x: {prompt}", Fields { deny: vec!["Bash(rm:*)".into()], ..Default::default() }).unwrap();
            let edited = edit(&store(), None, "Tester2", Fields { clear_deny: true, ..Default::default() }).unwrap();
            assert!(edited.permissions.deny.is_empty());
        });
    }

    #[test]
    fn edit_a_builtin_without_touching_other_fields_preserves_its_compiled_in_defaults() {
        in_scratch_config(|| {
            let edited = edit(&store(), None, "Implementor", Fields { harness: Some("codex".into()), ..Default::default() }).unwrap();
            assert_eq!(edited.harness, "codex");
            assert!(edited.brief_template.unwrap().contains("Implement"), "compiled-in brief template must survive");
            assert_eq!(edited.can_spawn, vec!["Researcher".to_string()], "compiled-in can_spawn must survive");
        });
    }

    #[test]
    fn delete_refuses_a_builtin_by_canonical_name_or_alias() {
        in_scratch_config(|| {
            for name in ["Implementor", "Implementation", "Worker", "Researcher"] {
                let err = delete(&store(), None, name).unwrap_err().to_string();
                assert!(err.contains("reset"), "{name}: {err}");
            }
        });
    }

    #[test]
    fn delete_removes_a_custom_type_and_refuses_a_second_delete() {
        in_scratch_config(|| {
            create(&store(), None, "Throwaway", "x: {prompt}", Fields::default()).unwrap();
            delete(&store(), None, "Throwaway").unwrap();
            assert!(show("Throwaway").is_err());
            assert!(delete(&store(), None, "Throwaway").is_err());
        });
    }

    #[test]
    fn reset_reverts_a_customized_builtin_and_refuses_an_uncustomized_one() {
        in_scratch_config(|| {
            edit(&store(), None, "Evaluator", Fields { harness: Some("codex".into()), ..Default::default() }).unwrap();
            let reset_spec = reset(&store(), None, "Evaluator").unwrap();
            assert_eq!(reset_spec.harness, "claude-code");

            let err = reset(&store(), None, "Researcher").unwrap_err().to_string();
            assert!(err.contains("no customization"), "{err}");

            let err2 = reset(&store(), None, "NotARealType").unwrap_err().to_string();
            assert!(err2.contains("not a built-in"), "{err2}");
        });
    }

    #[test]
    fn export_then_import_round_trips_a_custom_type() {
        in_scratch_config(|| {
            create(&store(), None, "Exportable", "Do it: {prompt}", Fields {
                allow: vec!["Read".into()], can_spawn: Some("*".into()), ..Default::default()
            }).unwrap();
            let snippet = export(&["Exportable".to_string()]).unwrap();
            delete(&store(), None, "Exportable").unwrap();
            assert!(show("Exportable").is_err());

            let imported = import(&store(), None, &snippet).unwrap();
            assert_eq!(imported, vec!["Exportable".to_string()]);
            let spec = show("Exportable").unwrap().spec;
            assert_eq!(spec.permissions.allow, vec!["Read".to_string()]);
            assert_eq!(spec.can_spawn, vec!["*".to_string()]);
        });
    }

    #[test]
    fn export_with_no_names_includes_every_user_facing_type() {
        in_scratch_config(|| {
            let snippet = export(&[]).unwrap();
            for builtin in ["Orchestrator", "Implementor", "Researcher", "Evaluator"] {
                assert!(snippet.contains(builtin), "{builtin} missing from {snippet}");
            }
        });
    }

    #[test]
    fn export_rejects_an_unknown_name() {
        in_scratch_config(|| {
            assert!(export(&["NotARealType".to_string()]).is_err());
        });
    }

    #[test]
    fn can_spawn_rejects_an_unknown_type_name() {
        in_scratch_config(|| {
            let err = create(&store(), None, "Bad", "x: {prompt}", Fields { can_spawn: Some("NotReal".into()), ..Default::default() })
                .unwrap_err().to_string();
            assert!(err.contains("NotReal"), "{err}");
        });
    }

    #[test]
    fn can_manage_types_is_settable_via_create_and_edit_fields() {
        in_scratch_config(|| {
            let created = create(&store(), None, "Admin2", "x: {prompt}", Fields {
                can_manage_types: Some(true), ..Default::default()
            }).unwrap();
            assert!(created.can_manage_types);

            let edited = edit(&store(), None, "Admin2", Fields { can_manage_types: Some(false), ..Default::default() }).unwrap();
            assert!(!edited.can_manage_types);

            let unchanged = edit(&store(), None, "Admin2", Fields { model: Some("gpt-5".into()), ..Default::default() }).unwrap();
            assert!(!unchanged.can_manage_types, "omitting the field in a later edit must leave it unchanged");
        });
    }

    // --- Mutation-gate tests (the `can_manage_types` matrix) ---

    fn session_with_type(store: &Store, id: &str, worker_type: Option<&str>) {
        use crate::types::{Session, SessionStatus};
        store.upsert_session(&Session {
            id: id.into(), orchestrator_id: None, name: id.into(), repo: String::new(),
            status: SessionStatus::Working, agent_type: "claude-code".into(), cost_usd: 0.0,
            started_at: 0, pr_number: None, pr_id: None, workspace_path: None, pid: None,
            model: None, context_tokens: None, brain_reads: Vec::new(), brain_write: None, context_used_pct: None,
            context_total_tokens: None, context_window_size: None, claude_session_id: None,
            summary: None, terminal_at: None, gate_status: None, merged_at: None,
            activity: Default::default(), activity_note: None, activity_since: None,
            machine_id: None, worker_type: worker_type.map(String::from),
            success_criteria: Vec::new(), completion_outcome: None, self_completed_at: None,
            cleanup_result: None, cleanup_at: None,
        }).unwrap();
    }

    #[test]
    fn an_implementor_session_is_refused_by_default() {
        in_scratch_config(|| {
            let store = store();
            session_with_type(&store, "impl-1", Some("Implementor"));
            let err = create(&store, Some("impl-1"), "X", "x: {prompt}", Fields::default()).unwrap_err().to_string();
            assert!(err.contains("not allowed to manage worker types"), "{err}");
            assert!(show("X").is_err(), "the unauthorized call must not have written anything");
        });
    }

    #[test]
    fn an_orchestrator_session_cannot_manage_types_by_default() {
        in_scratch_config(|| {
            let store = store();
            store.upsert_orchestrator(&crate::types::Orchestrator {
                id: "orch-1".into(), name: "orch-1".into(), created_at: 0,
            }).unwrap();
            session_with_type(&store, "orch-1", None);
            let err = create(&store, Some("orch-1"), "X", "x: {prompt}", Fields::default()).unwrap_err().to_string();
            assert!(err.contains("not allowed to manage worker types"), "{err}");
        });
    }

    #[test]
    fn a_type_explicitly_granted_can_manage_types_is_authorized() {
        in_scratch_config(|| {
            let store = store();
            create(&store, None, "Admin", "x: {prompt}", Fields {
                can_manage_types: Some(true), can_spawn: Some("*".into()), ..Default::default()
            }).unwrap();
            session_with_type(&store, "admin-1", Some("Admin"));

            create(&store, Some("admin-1"), "Created-By-Admin", "x: {prompt}", Fields::default()).unwrap();
            assert!(show("Created-By-Admin").is_ok());
        });
    }

    #[test]
    fn an_authorized_caller_still_cannot_touch_its_own_type() {
        in_scratch_config(|| {
            let store = store();
            create(&store, None, "Admin", "x: {prompt}", Fields { can_manage_types: Some(true), ..Default::default() }).unwrap();
            session_with_type(&store, "admin-1", Some("Admin"));

            let err = edit(&store, Some("admin-1"), "Admin", Fields { harness: Some("codex".into()), ..Default::default() })
                .unwrap_err().to_string();
            assert!(err.contains("own worker type"), "{err}");

            let err = delete(&store, Some("admin-1"), "Admin").unwrap_err().to_string();
            assert!(err.contains("own worker type"), "{err}");
        });
    }

    #[test]
    fn import_refuses_a_document_containing_the_callers_own_type() {
        in_scratch_config(|| {
            let store = store();
            create(&store, None, "Admin", "x: {prompt}", Fields { can_manage_types: Some(true), ..Default::default() }).unwrap();
            session_with_type(&store, "admin-1", Some("Admin"));

            let snippet = "[[worker_types]]\nname = \"Admin\"\nharness = \"codex\"\n";
            let err = import(&store, Some("admin-1"), snippet).unwrap_err().to_string();
            assert!(err.contains("own worker type"), "{err}");
        });
    }

    // --- The escalation chain a second review pass caught ---

    #[test]
    fn an_orchestrator_cannot_create_a_can_manage_types_type_even_though_it_can_spawn_anything() {
        in_scratch_config(|| {
            let store = store();
            store.upsert_orchestrator(&crate::types::Orchestrator {
                id: "orch-boot".into(), name: "orch-boot".into(), created_at: 0,
            }).unwrap();
            session_with_type(&store, "orch-boot", None);
            // Blocked at the very first gate (Orchestrator doesn't manage
            // types by default) — never even reaches the can_manage_types
            // field check.
            let err = create(&store, Some("orch-boot"), "Bootstrap", "x: {prompt}", Fields {
                can_manage_types: Some(true), can_spawn: Some("*".into()), ..Default::default()
            }).unwrap_err().to_string();
            assert!(err.contains("not allowed to manage worker types"), "{err}");
            assert!(show("Bootstrap").is_err());
        });
    }

    #[test]
    fn the_prompt_agent_cannot_edit_itself_or_grant_can_manage_types_to_another_type() {
        use crate::worker_type::PROMPT_WORKER_TYPE;
        in_scratch_config(|| {
            let store = store();
            session_with_type(&store, "ninox-prompt", Some(PROMPT_WORKER_TYPE));

            // Can't edit its own (hidden) type — caught by the self-type
            // guard before the hidden-type guard even runs.
            let err = edit(&store, Some("ninox-prompt"), PROMPT_WORKER_TYPE, Fields {
                harness: Some("codex".into()), ..Default::default()
            }).unwrap_err().to_string();
            assert!(err.contains("own worker type"), "{err}");

            // Can't grant can_manage_types to some OTHER type either — this
            // is the exact bootstrap move the escalation chain relies on
            // (mint a second equally-privileged mutator).
            create(&store, None, "Decoy", "x: {prompt}", Fields::default()).unwrap();
            let err = edit(&store, Some("ninox-prompt"), "Decoy", Fields {
                can_manage_types: Some(true), ..Default::default()
            }).unwrap_err().to_string();
            assert!(err.contains("can_manage_types"), "{err}");
            assert!(!show("Decoy").unwrap().spec.can_manage_types);
        });
    }

    #[test]
    fn the_user_edits_ninox_by_its_display_name_and_locked_parts_hold() {
        use crate::worker_type::{PROMPT_DISPLAY_NAME, PROMPT_WORKER_TYPE};
        in_scratch_config(|| {
            let store = store();
            let spec = edit(&store, None, PROMPT_DISPLAY_NAME, Fields {
                model: Some("claude-haiku-4-5-20251001".into()),
                brief: Some("Answer in one line.".into()),
                deny: vec!["Bash(rm:*)".into()],
                allow: vec!["Bash(ninox list:*)".into()],
                ..Default::default()
            }).unwrap();
            assert_eq!(spec.name, PROMPT_WORKER_TYPE);
            assert_eq!(spec.model.as_deref(), Some("claude-haiku-4-5-20251001"));
            let brief = spec.brief_template.unwrap();
            assert!(brief.contains("Never: answer a `ninox user-request`") && brief.contains("Answer in one line."), "{brief}");
            assert!(brief.ends_with("Your first question from the user: {prompt}"));
            let builtin = WorkerTypeRegistry::from_config(&[]).get(PROMPT_WORKER_TYPE).unwrap().clone();
            for rule in &builtin.permissions.deny {
                assert!(spec.permissions.deny.contains(rule), "built-in deny {rule} dropped");
            }
            assert!(spec.permissions.deny.iter().any(|r| r == "Bash(rm:*)"));
            assert!(spec.hidden && spec.can_spawn.is_empty() && spec.can_manage_types);

            let stored = AppConfig::load().unwrap().worker_types;
            let stored = stored.iter().find(|s| s.name == PROMPT_WORKER_TYPE).unwrap();
            assert!(stored.brief_template.is_none(), "the built-in brief is never copied into config");
            assert_eq!(stored.permissions.deny, vec!["Bash(rm:*)".to_string()]);

            // Clearing the deny list only clears the extras.
            let spec = edit(&store, None, PROMPT_DISPLAY_NAME, Fields { clear_deny: true, ..Default::default() }).unwrap();
            assert_eq!(spec.permissions.deny, builtin.permissions.deny);
        });
    }

    #[test]
    fn ninox_locked_fields_cannot_be_edited_even_by_the_user() {
        use crate::worker_type::PROMPT_DISPLAY_NAME;
        in_scratch_config(|| {
            let store = store();
            for fields in [
                Fields { can_spawn: Some("*".into()), ..Default::default() },
                Fields { can_manage_types: Some(false), ..Default::default() },
                Fields { permissions_enabled: Some(false), ..Default::default() },
            ] {
                let err = edit(&store, None, PROMPT_DISPLAY_NAME, fields).unwrap_err().to_string();
                assert!(err.contains("system type"), "{err}");
            }
            let err = delete(&store, None, PROMPT_DISPLAY_NAME).unwrap_err().to_string();
            assert!(err.contains("can't be deleted"), "{err}");
            edit(&store, None, PROMPT_DISPLAY_NAME, Fields { model: Some("m".into()), ..Default::default() }).unwrap();
            assert!(reset(&store, None, PROMPT_DISPLAY_NAME).unwrap().model.is_none());
        });
    }

    #[test]
    fn ninox_harness_is_claude_code_only_and_a_lookalike_name_is_taken() {
        use crate::worker_type::PROMPT_DISPLAY_NAME;
        in_scratch_config(|| {
            let store = store();
            let err = edit(&store, None, PROMPT_DISPLAY_NAME, Fields { harness: Some("codex".into()), ..Default::default() })
                .unwrap_err().to_string();
            assert!(err.contains("claude-code only"), "{err}");
            for name in ["ninox", "NINOX", "Prompt"] {
                assert!(create(&store, None, name, "x: {prompt}", Fields::default()).is_err(), "{name}");
            }
            // Built-in allows are never copied into config, so later
            // additions to the built-in list still reach this user.
            edit(&store, None, PROMPT_DISPLAY_NAME, Fields { allow: vec!["Bash(ninox list:*)".into(), "Bash(ls:*)".into()], ..Default::default() }).unwrap();
            let stored = AppConfig::load().unwrap().worker_types;
            assert_eq!(stored[0].permissions.allow, vec!["Bash(ls:*)".to_string()]);
        });
    }

    #[test]
    fn use_when_is_set_by_create_and_edit_and_cleared_by_an_empty_edit() {
        in_scratch_config(|| {
            let store = store();
            create(&store, None, "Docs", "Document: {prompt}", Fields { use_when: Some("Write or update documentation.".into()), ..Default::default() }).unwrap();
            assert_eq!(show("Docs").unwrap().spec.use_when.as_deref(), Some("Write or update documentation."));
            edit(&store, None, "Researcher", Fields { use_when: Some("Dig into one question.".into()), ..Default::default() }).unwrap();
            assert_eq!(show("Researcher").unwrap().spec.use_when.as_deref(), Some("Dig into one question."));
            assert!(edit(&store, None, "Docs", Fields { use_when: Some("two\nlines".into()), ..Default::default() }).is_err());
            let exported = export(&["Researcher".into()]).unwrap();
            assert!(exported.contains("use_when = \"Dig into one question.\""), "{exported}");
            edit(&store, None, "Docs", Fields { use_when: Some(String::new()), ..Default::default() }).unwrap();
            assert!(show("Docs").unwrap().spec.use_when_line().starts_with("(no description"));
            // Clearing a built-in's description sticks; it doesn't fall back.
            edit(&store, None, "Researcher", Fields { use_when: Some(String::new()), ..Default::default() }).unwrap();
            assert!(show("Researcher").unwrap().spec.use_when_line().starts_with("(no description"));
            assert!(edit(&store, None, "Ninox", Fields { use_when: Some("x".into()), ..Default::default() }).is_err());
            assert!(import(&store, None, "[[worker_types]]\nname = \"Multi\"\nuse_when = \"a\\nb\"\n").is_err());
        });
    }

    #[test]
    fn list_spawnable_shows_only_what_the_caller_may_spawn() {
        in_scratch_config(|| {
            let store = store();
            let names = |entries: Vec<Entry>| entries.into_iter().map(|e| e.spec.name).collect::<Vec<_>>();
            let all = names(list_spawnable(&store, None).unwrap());
            assert!(all.contains(&"Implementor".to_string()) && all.contains(&"Orchestrator".to_string()));

            session_with_type(&store, "impl-1", Some("Implementor"));
            assert_eq!(names(list_spawnable(&store, Some("impl-1")).unwrap()), vec!["Researcher".to_string()]);

            session_with_type(&store, "res-1", Some("Researcher"));
            assert!(list_spawnable(&store, Some("res-1")).unwrap().is_empty());

            create(&store, None, "Lead", "l: {prompt}", Fields { can_spawn: Some("Evaluator".into()), ..Default::default() }).unwrap();
            AppConfig::update(|c| {
                let mut lead = c.worker_type_registry().get("Lead").unwrap().clone();
                lead.allow_nested_spawn = true;
                c.upsert_worker_type(lead);
            }).unwrap();
            session_with_type(&store, "lead-1", Some("Lead"));
            assert_eq!(names(list_spawnable(&store, Some("lead-1")).unwrap()), vec!["Evaluator".to_string(), "Orchestrator".to_string()]);

            // `can_spawn = *` alone can't start a sub-orchestrator.
            create(&store, None, "Star", "s: {prompt}", Fields { can_spawn: Some("*".into()), ..Default::default() }).unwrap();
            session_with_type(&store, "star-1", Some("Star"));
            assert!(!names(list_spawnable(&store, Some("star-1")).unwrap()).contains(&"Orchestrator".to_string()));

            // The Ninox prompt manages types, so it sees them all.
            session_with_type(&store, "ninox-prompt", Some(crate::worker_type::PROMPT_WORKER_TYPE));
            assert_eq!(names(list_spawnable(&store, Some("ninox-prompt")).unwrap()), names(list_spawnable(&store, None).unwrap()));
        });
    }

    #[test]
    fn no_session_can_edit_ninox_by_either_name() {
        use crate::worker_type::{PROMPT_DISPLAY_NAME, PROMPT_WORKER_TYPE};
        in_scratch_config(|| {
            let store = store();
            session_with_type(&store, "ninox-prompt", Some(PROMPT_WORKER_TYPE));
            for name in [PROMPT_DISPLAY_NAME, PROMPT_WORKER_TYPE] {
                let err = edit(&store, Some("ninox-prompt"), name, Fields {
                    allow: vec!["Bash(rm:*)".into()], ..Default::default()
                }).unwrap_err().to_string();
                assert!(err.contains("own worker type"), "{err}");
            }
            create(&store, None, "Manager", "m: {prompt}", Fields { can_manage_types: Some(true), can_spawn: Some("*".into()), ..Default::default() }).unwrap();
            session_with_type(&store, "mgr", Some("Manager"));
            let err = edit(&store, Some("mgr"), PROMPT_DISPLAY_NAME, Fields { model: Some("x".into()), ..Default::default() })
                .unwrap_err().to_string();
            assert!(err.contains("hidden/system type"), "{err}");
        });
    }

    #[test]
    fn editor_list_shows_ninox_but_list_and_spawn_lists_do_not() {
        use crate::worker_type::PROMPT_WORKER_TYPE;
        in_scratch_config(|| {
            assert!(editor_list().unwrap().iter().any(|e| e.spec.name == PROMPT_WORKER_TYPE));
            assert!(!list().unwrap().iter().any(|e| e.spec.name == PROMPT_WORKER_TYPE));
            let reg = AppConfig::load().unwrap().worker_type_registry();
            assert!(!reg.user_facing_names().iter().any(|n| n == PROMPT_WORKER_TYPE));
        });
    }

    #[test]
    fn an_authorized_agent_cannot_widen_can_spawn_beyond_its_own() {
        in_scratch_config(|| {
            let store = store();
            // Granted management rights but only Researcher spawn reach —
            // mirrors Implementor's own can_spawn shape.
            create(&store, None, "Limited", "x: {prompt}", Fields {
                can_manage_types: Some(true), can_spawn: Some("Researcher".into()), ..Default::default()
            }).unwrap();
            session_with_type(&store, "limited-1", Some("Limited"));

            let err = create(&store, Some("limited-1"), "Overreach", "x: {prompt}", Fields {
                can_spawn: Some("Evaluator".into()), ..Default::default()
            }).unwrap_err().to_string();
            assert!(err.contains("more spawn reach"), "{err}");
            assert!(show("Overreach").is_err());

            let err2 = create(&store, Some("limited-1"), "Overreach2", "x: {prompt}", Fields {
                can_spawn: Some("*".into()), ..Default::default()
            }).unwrap_err().to_string();
            assert!(err2.contains("more spawn reach"), "{err2}");

            // Granting exactly what it already has (or a subset) is fine.
            create(&store, Some("limited-1"), "Fine", "x: {prompt}", Fields {
                can_spawn: Some("Researcher".into()), ..Default::default()
            }).unwrap();
            assert!(show("Fine").is_ok());
        });
    }

    #[test]
    fn a_wildcard_can_spawn_agent_is_unrestricted_by_the_widening_check() {
        in_scratch_config(|| {
            let store = store();
            create(&store, None, "SuperAdmin", "x: {prompt}", Fields {
                can_manage_types: Some(true), can_spawn: Some("*".into()), ..Default::default()
            }).unwrap();
            session_with_type(&store, "super-1", Some("SuperAdmin"));
            create(&store, Some("super-1"), "AnythingGoes", "x: {prompt}", Fields {
                can_spawn: Some("*".into()), ..Default::default()
            }).unwrap();
            assert!(show("AnythingGoes").is_ok());
        });
    }

    #[test]
    fn an_authorized_agent_cannot_touch_a_hidden_type_other_than_via_the_user() {
        use crate::worker_type::PROMPT_WORKER_TYPE;
        in_scratch_config(|| {
            let store = store();
            create(&store, None, "Admin2", "x: {prompt}", Fields {
                can_manage_types: Some(true), can_spawn: Some("*".into()), ..Default::default()
            }).unwrap();
            session_with_type(&store, "admin2-1", Some("Admin2"));

            // Not its own type, but still a hidden/system type — refused.
            let err = edit(&store, Some("admin2-1"), PROMPT_WORKER_TYPE, Fields {
                harness: Some("codex".into()), ..Default::default()
            }).unwrap_err().to_string();
            assert!(err.contains("hidden/system type"), "{err}");

            let err = reset(&store, Some("admin2-1"), PROMPT_WORKER_TYPE).unwrap_err().to_string();
            assert!(err.contains("hidden/system type"), "{err}");

            // The user (no session) can still do all of this.
            edit(&store, None, PROMPT_WORKER_TYPE, Fields { model: Some("claude-haiku-4-5-20251001".into()), ..Default::default() }).unwrap();
            reset(&store, None, PROMPT_WORKER_TYPE).unwrap();
        });
    }
}
