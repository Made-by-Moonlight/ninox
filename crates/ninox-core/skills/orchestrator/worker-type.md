---
name: worker-type
description: Define and manage named worker types (spawn profiles) with `ninox worker-type`, instead of hand-editing config.toml.
---

# Worker Types

A **worker type** is a named, reusable spawn profile: a brief template, a
default harness/model, a permission profile, and a `can_spawn` list of
other types it may itself pass to `ninox spawn --type`. `ninox spawn --type
<name>` applies one.

Built-ins always exist: `Orchestrator`, `Implementor` (plain implementation
work — also resolves `Implementation`/`Worker`), `Researcher`, `Evaluator`.
`Orchestrator` is never spawned through `ninox spawn --type` — it only
describes the orchestrator role for `can_spawn` purposes; use `{{NINOX_BIN}}
spawn-orchestrator` or `{{NINOX_BIN}} spawn-sub-orchestrator` for that.

## Inspecting types

```bash
{{NINOX_BIN}} worker-type list
{{NINOX_BIN}} worker-type show Researcher
```

`show`'s `can_spawn` tells you what a session of that type may itself spawn
— useful before telling a worker to delegate further. `show`'s
`can_manage_types` tells you whether a session of that type may itself run
`create`/`edit`/`delete`/`reset`/`import` (see below).

## Defining a custom type

`create`/`edit`/`delete`/`reset`/`import` are gated — **your own
orchestrator session is NOT exempt**: unless the user has granted your type
`can_manage_types` (via a `[[worker_types]]` entry in `config.toml`, or a
prior session that already had the right to grant it), these commands
refuse you and tell you to ask the user to run them instead. Only a bare
call with no live session at all — the user, at a terminal, in the GUI, or
in the TUI — is always authorized. `list`/`show`/`export` are open to
everyone.

Even an authorized session (one whose type does carry `can_manage_types`)
cannot: touch the `can_manage_types` field on any type (only the user can
grant or revoke it), create/edit/delete/reset/import over a hidden/system
type, mutate the type its own session currently has, or give a type more
`can_spawn` reach than its own type has. If you hit one of these refusals,
tell the user what you were trying to do and have them run it themselves.

```bash
{{NINOX_BIN}} worker-type create \
  --name SecurityReviewer \
  --brief "Review the following for security issues and report findings: {prompt}" \
  --use-when "Audit a change or area for security problems; reports findings, no fixes." \
  --allow Read --allow Grep \
  --can-spawn Researcher
```

`--brief` MUST contain the literal `{prompt}` placeholder (or pass
`@path/to/file.txt` to read it from a file). Always give a new type a
one-line `--use-when`: it's what `worker-type list` shows orchestrators,
the planner and the Ninox prompt when they choose a type (change it later
with `worker-type edit <name> --use-when "..."`). `--can-spawn` is a
comma-separated list of type names, or `*` for every user-facing type; omit
it for "none" (the default — a new custom type can't spawn anything until
you grant it).

Change one later with `edit` (only the flags you pass are touched):

```bash
{{NINOX_BIN}} worker-type edit SecurityReviewer --model claude-opus-5-5
```

Editing a built-in writes a config overlay; `{{NINOX_BIN}} worker-type reset
<name>` removes it and reverts to the compiled-in default. `delete` only
works on a custom type — it refuses a built-in and tells you to `reset`
instead.

## Sharing a type

```bash
{{NINOX_BIN}} worker-type export SecurityReviewer > security-reviewer.toml
{{NINOX_BIN}} worker-type import security-reviewer.toml
```

Run any subcommand with `--help` for the full flag reference — it's written
to be learned from standalone.
