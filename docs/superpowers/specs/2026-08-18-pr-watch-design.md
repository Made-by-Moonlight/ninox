# PR Watch: consolidated GitHub polling + agent-managed PR registry

**Date:** 2026-08-18
**Status:** Approved design, pre-implementation
**Config key:** `[pr_watch]`, opt-in, default off

## Problem

The poller makes 5 unconditional REST requests per session every 30s
(`poll_github`: PR status, check runs, reviews, review comments, issue
comments), plus 1 request per git remote per tick for sessions without a
PR (`poll_pr_reconciliation`). Terminated/Interrupted sessions with
unresolved PRs poll forever. There is no ETag caching, backoff, or
rate-limit awareness. This exhausts the user's shared 5,000 req/hr REST
budget.

Webhooks were researched and rejected: per-laptop delivery requires
testing-only tooling (`gh webhook forward`, one forwarder per repo),
unauthenticated relays (smee), or repo-admin-gated tunnels; GitHub
webhooks are at-most-once and need reconciliation polling anyway. See
brain entry `decisions/ninox-gh-rate-limit-webhook-research`.

Solution: one batched GraphQL query per tick for the whole machine
(~1–2 points against the separate GraphQL budget — see
[GraphQL rate limits](https://docs.github.com/en/graphql/overview/rate-limits-and-query-limits-for-the-graphql-api)),
plus an explicit PR watch registry agents manage via `ninox open --pr` /
`ninox close --pr`.

## Decisions (settled with Ethan, 2026-08-18)

- **Registry is additive**: the `gh pr create` wrapper shim and branch
  reconciliation keep attaching PRs to sessions; those are implicit
  watch targets. The registry adds explicit watches on top.
- **Auto-close on PR merge/close only.** Watches survive session end.
  Leaked watches from crashed agents persist until their PR closes;
  `ninox list --prs` makes them inspectable.
- **Full-parity delivery**: registered watches get the same reactions
  as auto-detected session PRs (merge, CI transitions, review
  activity), delivered to the opener session. Unowned watches (no
  `NINOX_SESSION` at open time) update store/UI events only.
- **Opt-in toggle, default off** (`[pr_watch].enabled`), mirroring
  `[inbox_messaging]`. Toggle off → legacy REST loop runs unchanged.
- **Approach**: in-poller fetcher swap (registry table in the existing
  Store, thin CLI subcommands, batched GraphQL module inside `Poller`).
  Standalone daemon and ETag-only retrofit were considered and
  rejected.

## 1. Config

`PrWatchConfig` following the `InboxMessagingConfig` pattern
(`config.rs:163-167`): `#[derive(Default)]`, `#[serde(default)] pub
enabled: bool` → default off. Field on `AppConfig` alongside
`inbox_messaging` — **before** `harnesses: BTreeMap`, which must remain
the last field or `save()` emits malformed TOML (`config.rs:270-275`).
Checkbox in `ninox-app/src/components/settings_panel.rs`. No other
knobs; the tick remains the existing 30s `github_interval`.

## 2. Registry & CLI

Store table `pr_watches`:

| column | type | notes |
|---|---|---|
| repo | TEXT | `owner/name` slug |
| pr_number | INTEGER | |
| pr_url | TEXT | as given, for display |
| opener_session_id | TEXT NULL | `$NINOX_SESSION` at open time; NULL = unowned |
| created_at | TEXT | |

Unique on `(repo, pr_number, opener_session_id)`. Rows are deleted on
close — no tombstones.

New clap subcommands, short-circuited before heavy setup like
`Statusline`/`Inbox` (`main.rs:205-216`):

- `ninox open --pr <url>` — parse
  `github.com/{owner}/{repo}/pull/{n}` (reject non-PR URLs), upsert a
  watch keyed to `$NINOX_SESSION` (or unowned if unset). Idempotent.
  If the toggle is off, still record the row but warn that pr_watch is
  disabled and the watch is inactive.
- `ninox close --pr <url>` — remove the caller's watch (matched on
  opener session id), so one session closing a PR does not remove
  another session's interest.
- `ninox list --prs` — print active watches (repo, PR, opener).

Verbs are deliberately resource-generic (`open`/`close`/`list`) so
future resource kinds can reuse them.

## 3. Fetch layer

New module `ninox-core/src/github_graphql.rs` behind its own trait
(same DI-seam style as `GithubApi`) issuing **one aliased GraphQL query
per tick** covering every deduped target. Full parity needs zero REST.
Per PR alias:

- `merged`, `state`, `mergeable`, `title`, `number`, `headRefOid`
- checks via `statusCheckRollup` on the head commit — includes commit
  statuses, which the current check-runs-only REST call misses
- `reviews`, `reviewThreads` (inline comments), issue `comments` —
  fetch `databaseId` on comments so the existing `seen_comment_ids`
  dedup keys stay compatible with REST numeric ids

Branch→PR reconciliation folds into the same query via
`pullRequests(headRefName:, states: OPEN)` aliases, replacing
`poll_pr_reconciliation`'s per-remote REST requests.
`rateLimit { cost remaining resetAt }` is piggybacked on every query.
Targets are chunked at ~50 PRs per query.

Quirk: `mergeable` returns `UNKNOWN` while GitHub computes it lazily —
map it to `None`, exactly what the legacy REST path receives (`null`
while computing), so `GateCheck::Mergeable` behaves identically on both
paths and self-corrects on the next tick.

Auth: the existing `resolve_token()` chain; POST to
`api.github.com/graphql`.

## 4. Poller integration & delivery

`poll_github` gates on the toggle: enabled → new
`poll_github_batched`, else the legacy path runs untouched.

Batched path per tick:

1. Targets = session-attached PRs (legacy semantics preserved,
   including Terminated/Interrupted sessions with unresolved PRs — now
   nearly free) ∪ registry rows. Dedupe by `(repo, number)`; one PR may
   have multiple consumers.
2. One GraphQL query (chunked).
3. Fan each result out to its consumers through the existing
   downstream functions **unchanged**: `handle_merge_detection`
   (preserves the "Done implies already-notified" invariant — see
   brain entry `errors/orchestrator-notification-gap`),
   `summarize_checks` → CI events/reactions, comment upserts →
   review reactions, `derive_session_status`/`compute_new_gate` via
   `update_live_session_row`.
4. Registry watches deliver reactions to the opener session's tmux
   exactly as a session's own PR would. Unowned watches emit events
   only.
5. After merge/close notifications fire, delete registry rows for that
   PR (auto-close).

## 5. Rate-limit compliance & error handling

Per GitHub's API guidance:

- One serial request per tick — no concurrent GitHub calls.
- Honor `Retry-After` and secondary-limit 403/429 responses with
  exponential backoff, implemented as skipped ticks (never blocking
  the poller task).
- Voluntarily pause until `resetAt` when `rateLimit.remaining` drops
  below a floor (~100 points).
- GraphQL partial errors (deleted repo, bad PR number) are handled
  per-alias: the failing target logs and flows into the existing
  deduped `notify_github_lookup_failed`; the rest of the batch
  proceeds. Whole-query failure → `tracing::warn!` and continue, same
  posture as today.

## 6. Agent-facing instructions

New capabilities reach existing installs only via always-overwritten
SKILL.md files (AGENTS.md is written only when absent — see brain
entry `architecture/ninox-orchestrator-lifecycle-cli`). Worker-side
instructions tell agents to `ninox open --pr` after creating any PR
they need watched and `ninox close --pr` when they stop caring; the
`spawn-worker` skill gains a cross-link.

## 7. Testing

- Config: default-off and missing-table tests mirroring
  `inbox_messaging_defaults_to_disabled` (`config.rs:617-633`).
- Store: registry CRUD, unique-constraint upsert idempotency,
  per-opener close.
- Poller: fake GraphQL trait impl feeding batched fixtures — merge
  detection, CI transitions, review reactions, auto-close on merge,
  `UNKNOWN` mergeable retention, rate-limit pause, per-alias error
  isolation.
- Regression: with the toggle off, every existing poller test passes
  unmodified (proof the legacy path is untouched).
- CLI: URL parsing (trailing slashes, `www.`, non-PR URLs rejected).

## Out of scope (deliberate)

- ETag/conditional REST — GraphQL covers everything on the new path;
  the legacy path is unchanged by design.
- TTL expiry on watches — merge-only auto-close was chosen;
  `ninox list --prs` covers visibility.
- Webhooks / org-hosted relay — researched and shelved; revisit only
  if sub-10s latency is ever required (GitHub App + CITADEL relay is
  the documented phase-2 shape in the brain entry).
- Tick-rate configuration — batching makes 30s cheap; tune later if
  latency matters.
