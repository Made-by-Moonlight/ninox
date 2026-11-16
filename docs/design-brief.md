# Ninox — Feature Outline (design brief)

**What it is:** Ninox is a native desktop app (Rust + Iced, GPU-rendered, no Electron) for orchestrating and monitoring fleets of AI coding agents. It runs terminal sessions (via tmux) for both "orchestrator" agents (coordinators) and "worker" agents (implementers), tracks their GitHub PRs/CI status, and surfaces things that need human attention. Currently ships with a full dark theme, light theme, and a warm/amber "Ninox" theme — no branding beyond a "⬡ Ninox" wordmark.

---

## 1. Core object model
- **Orchestrator** — a top-level coordinating agent session; can spawn multiple **workers**
- **Session** (worker or standalone) — one running agent instance, tied to a repo/workspace, with a live terminal
- **PR** — linked to a session once the agent opens one; has CI status + review comments
- **Notification** — system alerts (CI failure, agent stuck, PR needs attention, merge conflict, worker done)

## 2. Primary views
1. **Fleet Board** (default/home view) — Kanban-style board, one column per status: Working → PR Open → CI Failed → Review Pending → Mergeable → Done → Terminated. Each session is a card (status dot, name, repo, live cost in $). Horizontal scroll across columns.
2. **Session Detail** — header (back nav, name, repo, status dot, cost, PR badge, Kill button) + a panel switcher with 4 modes:
   - **Terminal** — full-width live terminal (xterm-like grid canvas, custom font rendering, cursor modes, scrollback)
   - **Split** — terminal + side info panel together (default view)
   - **Info** — PR metadata, CI pass/fail counts, review comments thread, full-width
   - **Inspector** — raw session metadata (ID, agent type, orchestrator, PID, workspace path, timestamps) as a key/value list
3. **PR List** — flat table of all open PRs across the fleet: number, title, session name, CI badge; click to jump to session.

## 3. Persistent chrome
- **Sidebar** (resizable, drag handle) — header with logo, notification bell (unread count), PRs shortcut, "+ Spawn" button; tree list of orchestrators (expand/collapse to show their workers) plus standalone sessions; each row has a colored status dot + repo name + remove (×) button; footer has a theme switcher popout (Light/Dark/Ninox swatches).
- **Notification panel** — slide-down overlay from the sidebar, listing recent alerts with kind-colored tag, title, body, dismiss-one/dismiss-all, click-to-navigate.
- **Attention banner** — red bar at the top of the fleet board summarizing count of CI failures / reviews awaiting, only shown when non-zero.
- **Filter bar** — text search over session name/repo, live count ("3/12 sessions"), clear button.
- **Spawn modal** — centered dialog over a dimmed backdrop, single name field, Cancel/Spawn.

## 4. Interaction details worth flagging to a designer
- Resizable panels: sidebar width and info-panel width both have drag handles.
- Terminal is a real interactive PTY (keyboard input, scroll, copy-to-clipboard, cursor-mode-aware arrow keys) — not just a log viewer.
- Everything is live/streaming: session status, terminal output, notifications, CI status all update via an event bus, plus a 3s DB poll fallback.
- Status is color-coded consistently everywhere (dot color reused across sidebar, board cards, session header): green=working, blue=PR open, red=CI failed, yellow=review pending, purple=mergeable, grey=done/terminated.
- Cost-per-session ($ from agent usage) is shown in multiple places — it's a first-class metric, not an afterthought.

## 5. Current visual language (baseline the designer is replacing/evolving)
- Dense, developer-tool aesthetic: small type (10–16px), thin 1px borders, flat colored dots for status, no icons beyond emoji (🔔) and a hex-hexagon glyph (⬡) as the logo mark.
- Three themes already defined as full palettes (bg/surface/elevated/sidebar, text primary/secondary/muted, accent, terminal fg/bg, 6 status colors) — good reference point for a token system.
- No onboarding, empty states are minimal one-liners ("No PR yet", "No notifications", "Terminal connecting…").

## 6. Known gaps / likely design opportunities
- No visual distinction for orchestrator vs. worker beyond indentation + expand chevron in the sidebar tree.
- Kanban board can get very wide (7 fixed columns) with no way to collapse/hide empty ones.
- Spawn flow is a single "name" field — no repo picker, agent/model choice, or task prompt surfaced in UI (those exist in CLI/skills only).
- A "Brain" knowledge-base feature (searchable facts index) is speced (`docs/specs/brain.md`) but not yet in the UI — see Section 7.

## 7. Brain / Knowledge Graph (planned — not yet built)

Backing data already exists (`docs/specs/brain.md`): a Markdown knowledge store under `brain/` with fixed categories — `repos/`, `symbols/`, `concepts/`, `patterns/`, `decisions/`, `architecture/`, `relationships/`, `errors/` — each entry has YAML frontmatter (`type`, `name`, `tags`, `repos`, `updated`) and can link to other entries via `[[wikilinks]]`. A SQLite FTS index and CLI/HTTP query API (`ninox brain query`, `GET /api/brain/query`) already exist for text search, but there's no visual traversal yet.

**Two view modes, one dataset:**

- **File tree view** — mirrors the on-disk taxonomy (category → entries), for people who think in terms of "where is this fact filed." Needs: expand/collapse per category, entry count badges, filter by tag, jump-to-entry opens the rendered Markdown (reuse the Info-panel-style reading pane).
- **Force-directed graph view (primary)** — nodes = brain entries, edges = wikilinks between them (secondary/optional edges via shared `tags` or `repos`). This is the one that should carry the design weight:
  - **Node encoding:** color or shape by `entry_type` (repo/symbol/concept/pattern/decision/architecture/error) so the category taxonomy from the tree view is still legible at a glance; size by edge count or query hits (busier nodes = bigger).
  - **Growth over time:** since the ask is explicitly "see how things are developing," the graph needs a temporal control — e.g. a scrubber/timeline that fades in nodes as of their `updated`/creation date, or a "recently changed" highlight pulse, so a user can watch the brain accrete rather than only see a static snapshot.
  - **Interaction:** hover for a preview card (name, type, snippet), click to open full entry, search box that highlights/filters matching nodes and dims the rest, click-to-focus that recenters the graph on a node's immediate neighborhood (useful once the graph gets large).
  - **Clustering:** group by repo or by category as a layout option, since cross-repo relationships are one of the things this store is meant to capture.

**API implication for the designer's engineering counterpart (not now, just context):** the current server only exposes single-entry fetch and text query — a real graph view will need a "give me all entries + edges" endpoint, so the design should assume that's coming rather than being limited to what's queryable today.
