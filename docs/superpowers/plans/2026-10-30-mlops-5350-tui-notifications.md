# MLOPS-5350 item 5: TUI notifications list — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `N` opens a persistent list of notifications, newest first and styled by kind, with dismiss (`d`), dismiss all (`D`) and Enter to go to the notification's session. The header count stays, and new arrivals still toast.

**Architecture:** The TUI's own `Engine` hosts no poller, so it never emits the poller's `Event::Notification`. Those come from the GUI or headless daemon. `ninox_core::events_client::follow_notifications` reads that daemon's `/api/v1/events` SSE stream (its own commit), and the TUI keeps what arrives, capped at the GUI's 50. Each kind's stamp label and status tone move from the GUI panel to `ninox_overview::notifications` (own commit), so both apps colour a kind the same.

**Tech Stack:** Rust, ratatui (`ninox-tui`), `reqwest` (`Response::chunk`, no new features), `ninox-overview`.

**Spec:** `.claude/design-ref/fleet-overview-v1/BRIEF-MLOPS-5350-tui-parity.md`, Worker A item 5.

## Global Constraints

- The list is in-memory, like the GUI's: it lasts for the TUI's run, and isn't stored.
- Enter goes to the notification's session. The GUI's notification has no PR field, so there's no PR target. Notifications without a session (update, machine) stay put.

## Review Focus

1. A UTF-8 character split across SSE chunks must not corrupt a title (byte buffer, decoded per complete event).
2. No daemon reachable: the follower keeps retrying quietly, and the empty list says no daemon is reachable rather than just "nothing new".
3. Dismissing under the cursor keeps the cursor in range; dismiss-all leaves the list open and empty.
4. The selected row in the modal never paints text in its own `highlight` colour.

---

### Task 1: Follow the daemon's stream (core)
- [x] `events_client::{drain_notifications, follow_notifications}`; tests for split events and multi-byte chars across chunks, and an end-to-end read from a loopback SSE responder. Commit.

### Task 2: Shared kind label and tone (overview)
- [x] `notifications::{kind_label, kind_tone, Tone}`; the GUI panel maps `Tone` to its colours. Commit.

### Task 3: TUI list
- [x] Tests: toast by tone and the 50 cap; `N` newest-first list with `j/k`, `d`, `D`, Esc; Enter to the session (or nothing without one); view (stamps, order, header `✦ n (N)`, highlight contrast in both palettes); `Notifications` in the modal contrast walk.
- [x] `TuiState::{notifications, push_notification, notification_at}`, `Modal::Notifications`, the follower spawned in `event_loop`, the header count, and the help row (folded into `n · g · N`, since the help modal doesn't scroll yet; that's item 7).
- [x] `cargo test --workspace`, clippy; commit.
