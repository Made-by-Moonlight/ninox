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
You are almost certainly an `Implementor` unless you were spawned with a
different `--type`.

## Can you spawn anything?

`ninox spawn` is normally an orchestrator's job (see the `spawn-worker`
skill, which you don't have) — but if your own worker type's `can_spawn`
grants it, you may spawn a sub-session of a type it lists. Check your own
profile:

```bash
ninox worker-type show Implementor   # or whatever `ninox whoami` reports as your type
```

An attempt to spawn a type not in your `can_spawn` is refused with a clear
error naming what you're allowed to spawn instead. A spawn you make is
parented to you (not to your own orchestrator), so it shows up as your
sub-session.

## Defining a custom type

If asked to define a new kind of worker for the user — a name, a brief
template, and optionally harness/model/permissions/`can_spawn` — use this
command rather than editing `config.toml` by hand:

```bash
ninox worker-type create \
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
it for "none" (a new custom type can't spawn anything until granted).

**`create`/`edit`/`delete`/`reset`/`import` are gated** — unlike `list`/
`show`/`export`, which any session can run. Your own worker type needs
`can_manage_types: true` (check with `ninox worker-type show <your-type>`);
without it these commands are refused with a message telling you to ask the
user to run them instead. If that happens, tell the user what you were
trying to define and have them run the command themselves — don't treat the
refusal as a bug to work around, and don't ask the user to grant your type
the flag either: only the user can set `can_manage_types` at all, on any
type, including yours.

Even a granted type can never: mutate the type its own session currently
has, touch the `can_manage_types` field on any type, create/edit/delete/
reset/import over a hidden/system type, or give a type more `can_spawn`
reach than its own type has (e.g. a type whose own `can_spawn` is
`Researcher` can't create or edit a type with `can_spawn: "*"` or any name
beyond `Researcher`). All of these are refused the same way — ask the user.

Other commands: `ninox worker-type list` / `show <name>` to inspect,
`edit <name> ...` to change one field at a time, `delete <name>` for a
custom type (refused on a built-in — use `reset <name>` for those), and
`export`/`import` to share a type as a TOML file. Run any subcommand with
`--help` for the full flag reference.
