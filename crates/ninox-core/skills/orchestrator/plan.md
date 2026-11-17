---
name: plan
description: Track your plan doc live in the desktop app, and generate, propose, review and accept ghost plans of new sessions.
---

# Tracking Your Plan

Register a markdown file as your goals/plan doc:

    {{NINOX_BIN}} plan register /absolute/path/to/plan.md

Keep editing that same file as your plan evolves — no need to re-run
`register` unless the path changes; re-registering (or just re-editing) is
always safe, it just replaces the tracked content.

    {{NINOX_BIN}} plan show
    {{NINOX_BIN}} plan unregister

# Generating a Section Plan

To have a separate planning model split a task into sections before you
spawn anything:

    {{NINOX_BIN}} plan generate "<task description>" [--workspace /path/to/repo] [--propose]

It explores the workspace read-only and prints JSON: on success, `data`
holds `{parent_id, sections: [{id, parent_id, worker_type, brief,
dependencies}]}`. Sections whose `parent_id` is another section's id sit
under a proposed sub-orchestrator. If the plan fails validation, `ok` is
false and `error.details` lists each problem. Without `--propose` nothing is
stored or spawned; with it, the plan is stored as a pending ghost plan (see
below) and `data.plan_id` is printed.

# Ghost Plans: Propose, Review, Accept

A ghost plan is a stored proposal. It shows in the user's Fleet and Plans tab
as dashed ghost nodes, and nothing spawns until it is accepted.

    {{NINOX_BIN}} plan propose --file plan.json     # or JSON on stdin
    {{NINOX_BIN}} plan list [--pending]
    {{NINOX_BIN}} plan show <plan_id>
    {{NINOX_BIN}} plan accept <plan_id> [--nodes a,b]
    {{NINOX_BIN}} plan discard <plan_id> [--nodes a,b]

`propose` takes the `plan generate` JSON. Each section may also set `name`
(the session name; defaults to `id`), `workspace` (required for every
non-Orchestrator node), `model`, `brain_reads` / `brain_write` (catalogue
names, narrowed from your own brain set), `linear` and `await_done` (wait for
every dependency to finish, not just start). Use your own id as `parent_id`
for sessions under you; null means new top-level sessions: Orchestrators, or,
for any other worker type, a standalone session owned by the user (no
orchestrator; it reports to no one, so its PR and completion events don't reach
you). A non-Orchestrator top-level section needs a `workspace`.

Who can accept or discard: the user, or you for a plan attached under you
or one of your sub-orchestrators. A plan of new top-level sessions is
the user's to accept. Accepting spawns ready nodes through the normal spawn
path (session limits may queue them); a node whose parent or dependencies
haven't started yet waits and spawns automatically once they have. Re-running
`accept` never spawns a node twice. Pending plans expire after
`[plan_proposals] expiry_days` (default 7) and are never accepted
automatically.

Only say a plan was proposed or accepted once the command's output says so,
and quote the `plan_id` it printed.
