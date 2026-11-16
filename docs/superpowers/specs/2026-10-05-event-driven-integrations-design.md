# Event-Driven External System Integration (Webhooks)

**Status:** Design spec — not scheduled. Written to answer "should ninox
become the push target for external systems instead of polling them," with
the public-reachability tradeoff as the central decision for the user to
make before any implementation is planned.

**Relationship to MLOPS-4681:** MLOPS-4681 adds a "checks passed"
notification to the existing `poll_github_batched` timer tick in
`crates/ninox-core/src/lifecycle/poller.rs`. It is independent of this spec
and proceeds regardless of what's decided here — nothing below supersedes
it, and if this design is never implemented, MLOPS-4681 still stands on its
own as the near-term improvement.

## Context

Today, everything ninox knows about a GitHub PR — status, CI checks, review
threads — comes from `poller.rs` calling the GitHub API on a timer
(`poll_github_batched`, driven by `GithubBatchApi::fetch_batch`, see
§"Batched GitHub enrichment" in `poller.rs`). This is polling pushed from
agent-side (`gh pr checks` in a loop) down to one ninox-side batched tick,
but it is still ninox *pulling* on a schedule.

`crates/ninox-server/src/server.rs` binds `axum` to `127.0.0.1` only and
exposes four route groups — `/api/v1/sessions`, `/api/v1/orchestrators`,
`/api/v1/events`, `/api/brain` — all local-only, all consumed by the Iced
app and the `ninox` CLI on the same machine. `CorsLayer::permissive()` is
there to let a local browser/app origin call in; it is not a trust boundary
and was never meant to be internet-facing. There is no inbound webhook
route, no HMAC/signature verification anywhere in the codebase (confirmed:
no `hmac`/`sha2`-based request verification exists today — `sha2` appears
only in `brain_sync/manifest.rs` and `lifecycle/binary_update.rs` for
content hashing, unrelated to authenticating a caller), and no precedent
for authenticating an external, untrusted caller.

This spec asks: should ninox instead be the thing GitHub (and later Linear,
Slack) **pushes events into**? That is a materially different posture than
"poll faster" or "poll from a different process" — it requires
`ninox-server` to accept connections initiated by the public internet,
which today it structurally cannot do.

## Goals

1. Design a webhook receiver that feeds the *same* notification pipeline
   `poller.rs` already populates — not a second, parallel notification
   system with its own PR/CI/review state machine.
2. Be honest about the one hard problem: `ninox-server` is loopback-only
   and GitHub needs to reach it over the internet. Lay out the real options
   with their actual tradeoffs, not a foregone conclusion.
3. Treat "most production systems still poll as a reconciliation fallback"
   as the default assumption, not a concession — webhooks can be missed,
   delayed, or delivered out of order; this is a fast-path, not a
   replacement for `poll_github_batched`.
4. Keep the signing-secret handling compatible with the org's secrets
   policy from day one: never a plaintext secret in the repo or on disk
   unmanaged.
5. Shape the GitHub-specific pieces (route, payload model, verification)
   so that adding a second source (Linear, Slack) later is "write a new
   translator," not "redesign the receiver."

## Non-goals

- Implementing any of this. This is a design spec; MLOPS-4681 is the only
  code that ships in the near term.
- Picking the public-reachability option on the user's behalf — see
  "Open questions." This spec lays out options and their honest tradeoffs;
  it does not recommend skipping CITADEL review in favor of something
  faster to stand up.
- Full design of the Linear/Slack receivers. Sketched only, enough to
  confirm the GitHub design doesn't foreclose them.
- Changing anything about `poll_pr_reconciliation` / `poll_github_batched`
  for sessions that aren't explicitly opted into webhook delivery.

## Current state (confirmed in code)

- `ninox-server/src/server.rs`: `axum::Router` nested at `/api/v1/{sessions,
  orchestrators, events}` and `/api/brain`, bound to `SocketAddr::from(([127,
  0, 0, 1], port))`. `CorsLayer::permissive()` is the only cross-origin
  concern addressed; there is no auth middleware anywhere in
  `ninox-server`.
- `poller.rs`'s batched path (`poll_github_batched`) fetches every
  session-attached PR plus every `store::PrWatch` registry entry
  (`list_pr_watches`) in one `GithubBatchApi::fetch_batch` call per tick,
  then runs the same `ingest_ci` / `scan_reviews` / `apply_status_and_gate`
  helpers `poll_github` (the legacy per-session path) uses, and fans watch
  results out via `deliver_watch_updates`, which emits `Notification`s
  (`NotificationKind::CiFailure`, `PrNeedsAttention`, `WorkerDone`, …) —
  see `types.rs`'s `NotificationKind` enum.
- `PrWatchConfig` (`config.rs`) is the existing opt-in pattern: `#[serde(default)]`
  struct, defaults to `enabled: false`, declared on `AppConfig` before
  `harnesses` (TOML tables must serialize after scalars — repo convention,
  see this file's `CLAUDE.md`).
- Nothing in the dependency tree does HMAC request verification today.
  `hmac`/`sha2` (for `X-Hub-Signature-256`, which is HMAC-SHA256 over the
  raw request body) would be new, small, well-understood dependencies.

## Architecture

### 1. Webhook receiver route

A new router, `webhooks_router`, nested at `/api/v1/webhooks` alongside the
existing four groups in `server.rs`. One route per source, versioned by
path rather than header so a misconfigured delivery fails loudly (404) and
routing can be grepped straight out of the server's `Router` construction:

```
POST /api/v1/webhooks/github
```

Request handling, in order:

1. **Read the raw body** before any JSON deserialization — signature
   verification is over the exact bytes GitHub sent, not a
   re-serialization of a parsed struct. `axum::body::Bytes` as the
   extractor, not `Json<T>`.
2. **Verify `X-Hub-Signature-256`**: HMAC-SHA256 of the raw body, keyed by
   the configured webhook secret, compared to the header's `sha256=<hex>`
   value using a constant-time comparison (`subtle::ConstantTimeEq` or
   equivalent — not `==` on the hex strings, which short-circuits and leaks
   timing). Any failure — missing header, malformed hex, mismatch, no
   secret configured — is a `401` with no further processing. This is the
   entire trust boundary; get it reviewed before shipping, independent of
   whatever else this spec concludes.
3. **Dispatch on `X-GitHub-Event`** (`pull_request`, `check_run`,
   `check_suite`, `pull_request_review`; everything else is `200 OK` +
   ignored, since GitHub retries on non-2xx and an unhandled-but-known
   event type isn't an error).
4. **Translate into the existing notification pipeline.** This is the part
   that must not become a second system:
   - Look the payload's `(repo, pr_number)` up against
     `store.list_pr_watches()` and sessions' `(repo, pr_number)` — the
     exact same identity `poll_github_batched` already keys enrichment on
     (`PrKey { repo, number }`).
   - For a match, call the *same* helpers `poll_github_batched` calls per
     PR today: `ingest_ci`, `scan_reviews`, `apply_status_and_gate`,
     `handle_merge_detection`, `deliver_watch_updates`'s per-watch logic.
     Those helpers currently take a `GithubBatchApi`-shaped snapshot
     (`PrSnapshot`/`BatchResult` entries); the webhook path needs to
     produce an equivalent single-PR snapshot from the webhook payload (a
     `check_run` event carries less than a full batched fetch — e.g. no
     review-thread state — so some payloads may still need one targeted
     API call to fill in what the webhook didn't carry, rather than
     fabricating state). The important invariant: **the same functions
     decide status/gate/notification transitions regardless of whether the
     trigger was a timer tick or a webhook delivery.** No second state
     machine, no second set of `NotificationKind` emissions.
   - A payload for a `(repo, pr_number)` ninox isn't tracking (no session,
     no watch) is `200 OK` + dropped. The receiver doesn't grow its own
     registry of what it cares about — `store::PrWatch` and
     session-attached PRs stay the single source of truth for "what ninox
     watches," exactly as they are for polling today.
5. Respond `200` quickly. GitHub's own delivery timeout is short (10s);
   anything that needs an API call to fill gaps in the payload should be
   spawned rather than awaited inline, with the `200` returned as soon as
   signature verification and routing succeed.

### 2. Public reachability — the central open question

`ninox-server` binding to `127.0.0.1` is correct for everything it does
today. A webhook receiver inverts the dependency: GitHub's servers must be
able to reach this one over the internet. That is not a config flag, it is
a different deployment posture, and it is the actual hard part of this
design. Three real options:

**Option A — persistent public endpoint.**
A long-lived, internet-reachable listener (its own service, not the
per-developer `ninox-server` instance on a laptop) terminates TLS, verifies
the signature, and either re-emits into each relevant ninox instance's
local event pipeline (requiring some reachability from the public endpoint
*back* to wherever ninox is actually running — itself nontrivial if ninox
runs on laptops) or becomes the shared store of record that per-machine
ninox instances poll/subscribe against.
- *This is the option that, if pursued, must go through CITADEL*
  (https://app.notion.com/p/synthesia/CITADEL-Platform-Guide-190c16d22bf1803abfe7d4cdb28b29b9),
  not an ad-hoc tunnel. A durable internet-facing listener receiving
  unsolicited inbound traffic from GitHub is exactly the "shared service
  needing its own infrastructure" category CITADEL exists for — ownership,
  TLS, secret storage, on-call, and the "whose laptop is this running on"
  problem all need a real answer, not a developer's machine with a tunnel
  open. **This is a hard constraint on the design, not a suggestion**:
  this spec does not propose ngrok, Cloudflare Tunnel, or any other
  ad-hoc public tunnel as the recommended path, and does not propose a
  3rd-party hosting product (Vercel, Netlify, etc.) as an alternative to
  CITADEL.
- Honest cost: this is real infrastructure with an owner, a deploy
  pipeline, and an operational burden that today's "run `ninox` on your
  laptop" model doesn't have at all. It's the only option that gives true
  low-latency push, and the only one that scales past "one person's
  machine."

**Option B — poll GitHub's webhook-deliveries API as a middle ground.**
GitHub's REST API exposes delivered webhook payloads for an app/hook
(`GET /repos/{owner}/{repo}/hooks/{hook_id}/deliveries` for a
repo-level webhook, or the equivalent app-level endpoint) — a registered
webhook still requires *something* with a public URL to be the delivery
target, but that target can be minimal (even a CITADEL-hosted stub that
just persists deliveries), and ninox polls *that* for new deliveries
instead of polling GitHub's PR/CI/review REST or GraphQL endpoints
directly. This keeps today's "ninox polls on a timer" model but polls a
feed of discrete events rather than re-fetching full PR state, which is
cheaper (fewer stale re-fetches, event-shaped deltas) without requiring
ninox itself to be reachable.
- Still needs *a* public delivery target to register the webhook against
  — this doesn't eliminate Option A's CITADEL question, it shrinks the
  surface of what has to live there (a dumb persist-and-serve endpoint vs.
  a full receiver with routing/translation logic).
- Net effect vs. today: faster signal (seconds instead of a full tick
  interval) and lower GitHub API cost per observation, at the cost of
  still not being true push — ninox finds out on its next poll of the
  deliveries feed, not the instant GitHub fires the webhook.

**Option C — a relay/broker ninox instances connect out to.**
Instead of GitHub reaching ninox, flip the connection direction: a shared,
CITADEL-hosted relay receives GitHub's webhooks (it's the public endpoint,
same ownership question as Option A) and each `ninox-server` instance opens
an *outbound* long-lived connection to it (WebSocket/SSE/long-poll) to
receive forwarded events. Outbound-only from each developer's machine is
compatible with today's "runs on a laptop, no inbound ports" model.
- This is still a CITADEL-hosted shared service at its core (the relay),
  so it doesn't dodge the ownership question, but it cleanly separates
  "the thing on the public internet" (small, stable, no per-user state)
  from "the thing with all the ninox-specific logic" (stays on each
  machine, unauthenticated inbound, nothing changes about ninox's own
  threat model).
- More moving parts than B, lower latency than B, no inbound-port
  requirement unlike A.

None of these is free. The honest comparison: **A** is the only one with
true low-latency push and the most infrastructure; **B** is the cheapest
change with the least payoff (polling a smaller, cheaper, event-shaped
feed instead of full state); **C** sits in between, trading relay
complexity for keeping every `ninox-server` instance's network posture
unchanged. All three still need *something* public and CITADEL-owned —
there is no version of "GitHub pushes to ninox" that avoids a publicly
reachable endpoint somewhere in the path.

### 3. Coexistence with polling, not replacement

Webhooks get missed: GitHub's own delivery guarantees are best-effort (no
guaranteed ordering, no guaranteed delivery — their own docs recommend
reconciliation polling for exactly this reason), a receiver can be down
during a deploy, a signature-verification bug can silently drop valid
events. Whatever option above is chosen, `poll_github_batched` keeps
running as the reconciliation fallback — probably at a *longer* interval
once webhooks are the fast path (the honest reason to poll less
frequently is that webhooks cover the common case, not that polling is no
longer needed), not removed. This mirrors nearly every production webhook
consumer: webhooks for freshness, polling for correctness-over-time. Don't
let "event-driven" in the ticket title read as "polling goes away" — it
doesn't, here or anywhere.

### 4. Secret management

The webhook signing secret (shared with GitHub when the webhook/App is
configured) must never be a plaintext value in this repo, in `AppConfig`'s
TOML on disk unencrypted, or in a commit. Two consistent options, matching
the org's existing split:

- **If any part of this is CITADEL-hosted** (Options A/B/C above all
  involve *something* CITADEL-hosted): the secret lives in CITADEL's own
  secrets management (dev.citadel.synthesia.io) and is injected into the
  hosted service's runtime environment — not read from a ninox config
  file at all for the hosted component.
- **If a per-machine `ninox-server` instance also needs the secret**
  (e.g. to verify signatures locally, or in Option C where the relay
  forwards raw payloads and each instance re-verifies): follow this repo's
  existing `github_token` precedent (`config.rs`: falls back to an env var
  when absent from config) and extend it with SOPS-encrypted storage
  rather than a new plaintext TOML field — never add a
  `webhook_secret: String` field that round-trips through `AppConfig`'s
  plain TOML serialization the way `github_token` currently can. This is
  worth flagging explicitly: `github_token`'s existing `Option<String>` in
  `AppConfig` is itself a plaintext-secret-in-config risk if a user puts a
  real token in their TOML instead of the env var; this design should not
  repeat that pattern for the webhook secret even though precedent for
  doing so already exists in this codebase.

### 5. Generalizing beyond GitHub

Sketch, not a full design — enough to confirm the GitHub receiver doesn't
bake in assumptions that block Linear or Slack later:

- **Route per source**, not a shared catch-all: `/api/v1/webhooks/github`,
  `/api/v1/webhooks/linear`, `/api/v1/webhooks/slack`. Each source has its
  own signature scheme (GitHub: `X-Hub-Signature-256`; Linear: its own
  HMAC header; Slack: signing secret + timestamp to prevent replay) and
  its own event taxonomy — trying to unify verification behind one code
  path before there's a second real source would be speculative
  abstraction.
- **Shared downstream shape**: whatever a source's translator produces
  should resolve to the same kind of thing `poll_github_batched` already
  produces — an identity to match against existing store state (PR, Linear
  issue, Slack thread — whatever ninox already tracks) and a transition to
  apply via existing helpers. The receiver's job per source is "verify,
  parse, identify, call existing logic" — never "verify, parse, decide,
  notify" with its own parallel rules.
- **The public-reachability question in §2 is shared infrastructure**,
  not per-source. Whichever option (A/B/C) is chosen for GitHub is the
  same endpoint/relay a Linear or Slack receiver would register against
  later — this is actually a reason to resolve §2 deliberately rather than
  bolt on a GitHub-specific tunnel now and rebuild it for the next source.

## Phasing (if pursued)

Each phase is independently shippable; nothing below is scheduled.

1. Resolve the public-reachability decision (§2) with the user and
   whoever owns CITADEL intake — this blocks everything else and is an
   infrastructure/ownership decision, not an engineering one.
2. Webhook receiver route + signature verification + translation into
   existing `poller.rs` helpers, tested against recorded GitHub payloads,
   running against whatever endpoint §1 produces.
3. Config: opt-in flag (`[webhooks] enabled`, following the `PrWatchConfig`
   pattern) plus secret wiring per §4.
4. Lengthen the `poll_github_batched` interval once webhooks are live and
   its fallback role is confirmed working — never removed outright.
5. Second source (Linear or Slack) as a proof that §5's shape holds.

## Open questions

These are genuine infrastructure/ownership decisions for the user to make
— not engineering choices this spec should pre-empt.

- **Does this warrant a CITADEL app at all**, or is "shorten
  `poll_github_batched`'s interval and ship MLOPS-4681's checks-passed
  notification" sufficient for the actual pain being felt today? The
  operational cost of any of Options A/B/C (owning a public listener,
  even a minimal one) is real and ongoing; the benefit is latency
  (seconds vs. a tick interval) and a smaller GitHub API footprint. Is
  that worth a CITADEL app's lifecycle (intake, ownership, on-call) right
  now, or later once/if polling intervals become a measured problem
  (rate limits, noticeable staleness)?
- **If pursued, which of A/B/C**, and who owns the resulting CITADEL
  app/service day to day? None of them are "ninox's problem alone" in the
  way today's per-laptop polling is — all three introduce a
  multi-user-owned service where there wasn't one.
- **Scope of the first cut**: org-wide (one shared CITADEL app receiving
  webhooks for every repo anyone's ninox instance cares about) vs.
  per-repo/per-team? This changes both the CITADEL intake conversation and
  the identity-matching logic in §1 step 4.
- **Who registers the GitHub webhook/App** and against which repos —
  a personal GitHub App, an org-level webhook, or per-repo webhooks? This
  determines who can see/rotate the signing secret and who's on the hook
  when GitHub's delivery dashboard shows failures.
