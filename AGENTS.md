# Working on ninox

Ninox is a Rust workspace (`crates/ninox-core`, `crates/ninox-app`,
`crates/ninox-server`) that orchestrates AI coding-agent sessions:
orchestrator sessions spawn worker sessions, each worker drives a PR to
merge. Build with `cargo build`, test with `cargo test --workspace`, lint
with `cargo clippy --workspace --all-targets`. Do not run `cargo fmt`
wholesale — the tree is not rustfmt-clean and a formatting pass buries real
changes in churn.

## Adding an agent-facing capability

Any feature that agents (orchestrators or workers) should know about — a
new CLI subcommand, a new workflow they can invoke — is a **capability**,
and capabilities have exactly one registration point:

1. Write the skill as a real markdown file (YAML frontmatter with `name:`
   and a one-line `description:`, then the body):
   - `crates/ninox-core/skills/orchestrator/<name>.md` for orchestrators
   - `crates/ninox-core/skills/worker/<name>.md` for workers
   - both, if the capability serves both audiences (the wording usually
     differs — write each for its reader)
2. Add one `Capability` entry to `REGISTRY` in
   `crates/ninox-core/src/capabilities.rs`: `name`, `audience`
   (`Worker` / `Orchestrator` / `Both`), the `include_str!` markdown, and
   an `enabled: fn(&AppConfig) -> bool` gate (`|_| true` for always-on;
   point it at a config toggle for opt-in features).

Everything else derives from the registry — **never hand-edit these**:

- Orchestrator-root skill seeding and the AGENTS.md "Available Skills"
  list (`seed_orchestrator_skills` / `setup_orchestrator_root` in
  `ninox-app/src/app.rs`) — descriptions come from the markdown
  frontmatter.
- Worker-worktree skill seeding (`seed_worker_skills` in
  `ninox-app/src/spawn_util.rs`) — git-excluded, gated per entry.
- The `ninox capabilities [--worker|--orchestrator] [--json]` command —
  agents run it mid-session for a live, config-aware capability list, so a
  registry entry is automatically discoverable the moment it ships.

Do not add skill content as Rust string constants in `ninox-app` — that
mechanism was removed deliberately; the registry's invariant tests (in
`capabilities.rs` and the seeding test modules) enforce that every entry
has markdown for its audience and a parseable description, and that seeded
files match the registry byte-for-byte.

Markdown bodies may use the `{{NINOX_BIN}}` and `{{CONFIG_PATH}}`
placeholders, replaced at seed time. Do not invent new placeholders without
extending the substitution and its no-`{{`-left-behind tests.

## Conventions

- Opt-in features follow the `[inbox_messaging]` / `[pr_watch]` pattern:
  a `#[serde(default)]` config struct defaulting off, declared on
  `AppConfig` **before** the `harnesses` field (TOML tables must serialize
  after scalars).
- Store writes that span an `.await` go through the read→apply→write
  pattern (see `update_live_session_row` in `lifecycle/poller.rs`) — never
  write a stale snapshot row.
- Hot-path CLI subcommands invoked by agents or hooks short-circuit in
  `main.rs` before the tmux-config/wrapper/self-shim setup (see
  `Statusline`, `Inbox`, `Open`/`Close`/`List`, `Capabilities`).
- Query the shared brain before exploring unfamiliar code and write back
  what you learn: `ninox brain query "<topic>"` / `ninox brain add`.
