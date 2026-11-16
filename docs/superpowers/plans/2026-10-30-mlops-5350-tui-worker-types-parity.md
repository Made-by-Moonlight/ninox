# MLOPS-5350 item 6: TUI Worker Types parity — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The TUI Worker Types form gains `can_spawn` (multi-select of type names plus `*`), `can_manage_types` and Reset for built-ins. allow/deny/ask become one rule per row (`a` adds, `x` deletes); no comma strings. The #265 rules hold: hidden types aren't listed, and only the user may change `can_manage_types`.

**Architecture:** Every TUI mutation now goes through `ninox_core::worker_type_cli` (create/edit/delete/reset/import), the same backend as the GUI and `ninox worker-type`. Its validation and authorization rules therefore apply in the TUI too. The TUI used to read and write `config.toml` itself. A save applies the change to a clone of the resolved spec, then sends only the fields that differ (`changed_fields`). The caller is the pane-resolved session this TUI runs inside; at a plain terminal that's `None`, i.e. the user. Two shared commits come first: `Fields::permissions_enabled` (the TUI's "Permissions enforced" toggle had no shared equivalent), and `toggle_can_spawn` moving from the GUI component to `worker_type_cli`.

**Tech Stack:** Rust, ratatui (`ninox-tui`), `ninox-core::worker_type_cli`.

**Spec:** `.claude/design-ref/fleet-overview-v1/BRIEF-MLOPS-5350-tui-parity.md`, Worker A item 6.

## Global Constraints

- `x` exports a type, except on a rule row, where it deletes that rule (the brief's key). `R` resets a customized built-in, behind a confirm. `D` on a built-in now refuses with the shared "use reset" message rather than silently reverting it.
- Import follows `ninox worker-type import`: an existing name is updated. The TUI used to refuse that.

## Review Focus

1. An agent driving `nx` is held to its own type's `can_manage_types`/`can_spawn` rules, and an edit that doesn't touch `can_manage_types` never names it (which the backend would refuse for an agent).
2. Emptying a rule's text removes it; removing the last rule clears the list in config.
3. `*` and named `can_spawn` entries stay mutually exclusive whichever row is toggled.
4. The hidden `prompt` type is never listed, and never offered as a `can_spawn` option.

---

### Task 1: Shared backend pieces
- [x] `Fields::permissions_enabled` with `edit_turns_permission_enforcement_on_and_off`. Commit.
- [x] Move `toggle_can_spawn` (and its test) to `worker_type_cli`; the GUI calls it there. Commit.

### Task 2: TUI form and keys
- [x] `FieldId::{Rule(RuleList, Option<usize>), CanSpawn, CanSpawnOption(String), CanManageTypes}`; `Change::{AddRule, RemoveRule}`; `Field::continued`; `WorkerTypesView::{builtin, adding_rule, resettable}`.
- [x] `save`/`new_type`/`delete_type`/`reset_type`/`import_types` over `worker_type_cli`, with `changed_fields`; `interactive_caller` in `mod.rs`.
- [x] Keys: `a`/`x` on rule rows, `R` with `Pending::ResetWorkerType`; the help rows edited in place (the help modal doesn't scroll until item 7).
- [x] Tests: module (rule rows, can-spawn exclusivity, changed fields, list/resettable, save/new/delete/reset/import through the backend); key routes; view rows. `cargo test --workspace`, clippy; commit.
