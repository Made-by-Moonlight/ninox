# MLOPS-5350: v1 try-out feedback, round 1

Source: `.claude/design-ref/fleet-overview-v1/FEEDBACK-v1-tryout-round1.md`
(read `BRIEF-common.md` in the same folder first). One PR against `v1` per
item, cut from `origin/v1` once the previous item's PR has merged.

## T1 — TUI: add a remote from the Remotes tab

The Remotes tab (`crates/ninox-tui/src/tui/remotes.rs`, `state.rs`,
`view.rs`) already ships on `origin/v1` as a read-only list with up/down
selection. This item adds:

- `a` on the Remotes tab opens an `AddRemoteModal` (`Modal::AddRemote`),
  styled like the existing `Spawn` modal: Host (required) and Label
  (optional, placeholder is the host portion of `host`). Tab/Shift-Tab
  swap fields, Enter submits from either field (no "advance then submit"
  staging, unlike Spawn), Esc cancels, an empty Host sets an inline error
  and keeps the modal open.
- Submit produces `Action::AddRemote { host, label }`, handled by
  suspending the TUI (`suspended()` in `tui/mod.rs`, the same primitive
  `attach_suspended`/`edit_suspended` already use) and running
  `ninox machine add <host> [--label <label>]` as a foreground child of
  the real terminal, so its SSH/host-key/install/session-picker prompts
  work normally. The binary is resolved via `canonical_exe()`, never a
  bare `ninox` off `$PATH`; host/label are passed as separate argv
  entries, never a shell string. Waits for Enter before restoring the TUI
  so the command's final output is readable, then refreshes the machine
  list and shows a one-line status (`machine add failed (exit N)` on a
  non-zero exit).
- `d` opens a `Pending::RemoveMachine` confirm (reusing the existing
  `Modal::Confirm` y/n flow); confirming calls
  `ninox_ui_shared::machine::remove` directly (synchronous, no SSH) rather
  than shelling out — `machine.rs`'s `run_remove` is split into a silent
  `remove()` (shared) and the CLI's own printing wrapper, since the TUI
  can't have a stray `println!` land on its alternate screen outside
  ratatui's own diffing.
- Empty-state copy becomes "a add a remote machine" with
  `ninox machine add` kept as a dim secondary line; the footer hints `a
  add remote` always and `d remove` only once the list is non-empty.

Out of scope: the GUI equivalent (tracked separately), changing `machine
add`/`remove` semantics.

Tests: unit tests for the modal's field state/validation/submitted
`Action` and for Remotes-tab key routing (`a`/`d`, and that typing in the
open modal never leaks to tab-switch or list-navigation keys), plus
render tests for the empty-state copy, the populated-list footer hint,
and both modals.

## T2 — TUI: bordered ninox-prompt box

The prompt ("the talk to ninox control") was a single dim line with no
border, docked under the Fleet tab's main area (`layout.rs`'s `fleet()`,
`view.rs`'s `draw_fleet_prompt`). The user's complaint: "not very
obvious." This item overrides this module's usual "whitespace and rules,
not boxes" structure for the prompt only (per the feedback item), same as
the GUI's parallel G1 item (tracked separately, not this ticket):

- `layout.rs`: the prompt's reserved row budget goes from 1 to 3
  (`FleetLayout::prompt`); `main`'s height shrinks by the same 2 rows
  since it's computed as the remainder. Verified at both ticket
  acceptance-criterion sizes (120x40, 220x60): the box reserves exactly 3
  rows and never exceeds the body width (no horizontal scroll).
- `view.rs`'s `draw_fleet_prompt`: now a rounded-border `Block` titled
  " ninox ❯ " (accent, bold, always). The border itself is dim (`p.faint`)
  unfocused and turns to `p.accent` while editing — the written brief says
  "teal", but this Palette has no teal token (TUI chrome resolves only
  through existing `Palette` fields, confirmed against the Spawn modal's
  identical pattern in T1), so `accent` is the deviation, same kind as
  T1's hex-token one. The idle placeholder moved from `p.faint` to
  `p.ink_2` ("mid grey, not DarkGray" — directly addresses the "not very
  obvious" complaint, since `p.faint` literally equals `Color::DarkGray`
  in terminal-colour mode). The old `— / to ask or act` suffix is gone;
  that hint now lives right-aligned inside the box (`/ focus · ⏎ send`,
  via the existing `spread()` helper).
- No state/key-handling changes: `fleet_prompt_key`/`fleet_key`'s existing
  `prompt_editing` early-return already keeps every global shortcut
  (tab-number keys, bare letters) from firing while typing, and Esc
  already unfocuses (`typing_a_digit_into_the_fleet_prompt_does_not_switch_tabs`
  already covers this).

Deviation flagged for the orchestrator, not fixed here (out of scope for
a visual-only item): `draw_fleet` returns before drawing the tree,
toolbar, or prompt at all when the fleet has zero rows (`st.rows.is_empty()`),
so a brand-new install can't see or use the prompt until something is
spawned via `n`. The visual check below seeds two synthetic rows into a
scratch store specifically to work around this and get a representative
screenshot.

Tests: a layout test asserting the 3-row reservation at 120x40 and 220x60
with no overflow; render tests asserting the box's title/hint text and a
bordered-not-bare-line glyph; a `TestBackend`-buffer test asserting the
border's fg colour is `p.faint` unfocused and `p.accent` focused (the
plain-text `render()` helper used elsewhere can't see colour).

## Fix — render the Fleet shell on an empty fleet (found while doing T2)

`draw_fleet` (`view.rs`) returned before drawing the tree, toolbar, or the
new T2 prompt box at all once `st.rows.is_empty()` — a brand-new install
could only spawn via `n`, never see or use the prompt. Fixed by always
drawing tree/toolbar/prompt and confining the empty-state message ("No
sessions yet. n to spawn, or / to ask ninox") to the grid area (`fl.main`)
instead of the whole body.

Tests: updated `empty_fleet_and_small_terminal_do_not_panic` for the new
copy, added `fleet_prompt_box_is_present_with_zero_sessions` asserting the
prompt's title and hint render with zero rows.

## T3 — TUI: the prompt must actually route and reply

**User:** "The top level ninox prompt doesn't actually do anything." "I
asked it to spawn some orchestrators and it did not reply."

Target resolution changes: the prompt's default target is now the prompt
agent (`ninox_ui_shared::prompt_agent`, MLOPS-5415, landed in #263 while
this item was in flight), not an inferred orchestrator from the current
selection — `ctrl+t` cycles `Agent → each orchestrator (row order) →
Agent` to pick one directly instead. Opening the prompt (`/`) always
resets the target back to the agent; the old selection-inference function
(`prompt_target`, and its "ambiguous — more than one orchestrator" refusal)
is gone along with it, since the prompt must now always do *something*
rather than refuse.

- `fleet.rs`: new `PromptTarget` (`Agent` | `Orchestrator(id)`),
  `PromptStrip`/`PromptStripState` (`Sent` → `Streaming(text)` → dismissed
  or `Error`), plus `prompt_history`/`prompt_history_pos` on `FleetState`.
  `FleetState::prompt_answer`/`prompt_pending_id`/`prompt_pending_since`
  and the file-based `ninox_core::prompt_reply` polling they drove are
  gone — superseded by the strip's guaranteed pane-tail/reply-stream
  polling. (`ninox_core::prompt_reply`, `ninox prompt-answer`, and the
  `ninox-prompt-reply` skill are untouched — this only stops the Fleet
  prompt from being their one caller; flagged for the orchestrator as a
  possible follow-up cleanup, not done here.)
- `state.rs`'s `fleet_prompt_key`: `ctrl+t` cycles the target (also live
  from the bare Fleet-tab context, not just while editing); `ctrl+l`
  dispatches `Action::ClearPromptContext`; `↑`/`↓` recall
  `prompt_history`; Enter prefixes `[scope: <island>] ` when scoped and
  dispatches `Action::SubmitPrompt { target, message }`. A visible strip's
  own Esc dismisses it before island-closing gets a turn; `o` opens its
  `session_id`'s pane when one exists.
- `mod.rs`: `Action::SubmitPrompt`/`ClearPromptContext` replace the old
  `Action::SendPrompt`. Delivery is a spawned task reporting through a new
  `prompt_agent_tx`/`PromptAgentEvent` channel (`Sent`/`Cleared`/`Error`):
  the agent target calls `prompt_agent::ask`/`clear` directly (no ACP, per
  the module's own `deliver()`); a picked orchestrator keeps the existing
  `deliver_message_with_acp` path (needs `port` for its `HttpAcpRouter`,
  hence `perform`'s `too_many_arguments` allow). `poll_prompt_strip`, run
  from the existing 1s tick, is the strip's only new async machinery: for
  the agent session it diffs `prompt_agent::reply_since`'s cursor
  (cumulative); for a picked orchestrator it re-snapshots the last 6 lines
  of its pane each tick via `ninox_core::runtime::read_screen` — the same
  source `ninox read` uses — since it has no reply-stream API of its own.
- `view.rs`: the prompt box gets a target chip (`→ ninox ▾`) before the
  input/placeholder; a new `draw_fleet_prompt_strip` renders the strip in
  a conditionally-reserved row directly above the box (`layout.rs`'s
  `FleetLayout::prompt_strip: Option<Rect>`, 1 row, present only while
  `prompt_strip.is_some()` so it costs nothing otherwise).

Deviation (flagged to the orchestrator, agreed, tracked separately): a
real end-to-end screenshot of the *agent* path needs
`AppConfig::sessions_dir()`/`prompt_agent_workspace_dir()` to honor an
env override the way `config_path()` honors `NINOX_CONFIG` — neither does
today, so a test instance's `prompt_agent::ask` would write into the real
`~/.config/ninox/{sessions,prompt-agent}/` on a shared machine. The GUI's
G2 PR is adding that override; this PR's visual check instead screenshots
the strip live-streaming a real reply via the directly-picked-orchestrator
path (fully isolated already: scratch `--db`, `NINOX_TMUX_SOCKET` pointed
at a throwaway tmux socket/short `TMUX_TMPDIR`, a real tmux pane standing
in for the orchestrator). A real prompt-agent-reply screenshot follows as
a PR comment once the override lands.

Tests: target cycling (`Agent` → each orchestrator → back, live from both
the editing and non-editing context) and reset-on-open; submit builds
`Action::SubmitPrompt` with the current target and the scope prefix;
history recall (older/newer, draft preserved); `ctrl+l` dispatches clear;
strip Esc-dismiss and `o`-opens-the-pane; render tests for the chip and
the strip's text/hints.

### Review fix — a prompt-originated send must never read as unverified

PR review on #268 caught that a message typed into the ninox prompt
arrived at its target labelled `[ninox: sender identity could not be
verified]` — confirmed **real** (not an artifact of the isolated test
setup): the TUI's `Action::SubmitPrompt` orchestrator-direct path, and
`ninox_ui_shared::prompt_agent::deliver`, both resolved `from` via the same
"only a real tracked session counts" path `ninox send` uses, and a human
running the TUI/desktop app directly (the common case) has no such session
to resolve — `from` was `None`, which `messaging::annotate_with_attestation`
renders as the generic unverified line. That line is correct for "a sender
that SHOULD be a resolvable session but isn't" (a `ninox send` run outside
any session); it's the wrong framing for "a human just typed this into a
trusted local UI."

Fix: a new `ninox_core::messaging::HUMAN_SENDER` sentinel (`"ninox-user"`),
alongside `SYSTEM_SENDER` — reserved the same way (added to
`is_reserved_session_id`, so no spawned session can claim it and borrow its
trust), but a distinct category: `SYSTEM_SENDER` vouches for *content*
ninox generated itself (never defanged); `HUMAN_SENDER` vouches only for
*origin* (a human at the keyboard, not an impersonation) — the content is
still arbitrary user input and is still defanged like any other sender's.
`annotate_with_attestation` renders it as `[ninox: from the user, via the
ninox prompt]`. Both the TUI's orchestrator-direct path (via a new, unit-
tested `prompt_sender` helper) and `prompt_agent::deliver` now attribute to
`HUMAN_SENDER` instead of `None` whenever no real tracked session resolves
(a TUI/GUI running nested inside one still attributes to that session,
unaffected). The GUI's own equivalent call site (if it has one beyond
`prompt_agent::deliver`, which this PR already fixes for both callers) is
for the GUI worker to confirm.

Also checked: the reported missing space/line-break between the
attestation line and the message. Reproduced the exact `tmux send-keys -l`
call in isolation with an embedded `\n\n` — it arrives at the receiving
shell as three distinct lines, separator intact. The flattened output seen
during the PR's own visual check was an artifact of the ad-hoc test
orchestrator (a one-line `read` loop script, not a real agent), not a
product bug; no further change made.

Tests: `human_sender_messages_are_labelled_from_the_user_not_unverified` and
`human_sender_content_is_still_defanged` (`messaging.rs`), reserved-id
coverage extended to `HUMAN_SENDER`, and `prompt_sender`'s two branches
(`mod.rs`).

### Review follow-up — HUMAN_SENDER must not be forgeable

A pane-identity miss alone was too weak a bar for `HUMAN_SENDER`: a worker
could `env -u <its pane-id var> ninox prompt ask …` and have its own send
stamped as the trusted user origin. Once `orchestrator_auth::resolve_live_
caller_session_id` landed (#265, MLOPS-5416), both call sites switched to
it: it prefers a resolved pane identity, else falls back to a process-
ancestry scan against every live managed session's recorded root `pid`
(`resolve_by_ancestry`). Only `None` — no managed ancestor at all — maps to
`HUMAN_SENDER`; any resolved session sends as that session through the
normal attestation path instead.

Tests: `deliver_attributes_a_caller_with_no_managed_ancestor_to_the_human_
sender` and `deliver_does_not_stamp_human_sender_when_a_live_session_is_an_
ancestor` (`prompt_agent.rs`).

### T4 — Brain tab adopts shared kind normalization and readable titles

Round-2 feedback item: the TUI's Brain tab grouped by the raw
`BrainEntry::entry_type` string, so a frontmatter `type: decision` and a
directory-derived `decisions` showed as two separate groups, and row/doc
titles always showed the frontmatter `name`/filename stem even when the
body opened with its own `# H1` — duplicating the title when that H1 was
then also shown as the first line of the body.

`ninox_core::brain::{normalize_kind, kind_label, display_title,
strip_leading_h1}` were already the GUI's shared source of truth for this
(the first two pre-existing, `display_title`'s family moved from
`ninox-gui/src/components/brain_panel.rs` into core as a prerequisite, the
GUI re-exporting it so its own call sites are unaffected). This item wires
the same four into the TUI:

- `BrainView::group_keys`'s `Type` arm, `template_defaults`, and the search
  ranking's kind match now go through `normalize_kind`, so `decision` and
  `decisions` (any case) land in one group and match each other in search.
- The Type-grouping header, the per-entry kind badge (Tag/Flat views), and
  the group-detail pane's "of type …" line use `kind_label` for the
  display text, while the underlying group key stays the normalized form
  used for folding/filtering (the detail pane's membership filter was
  switched from a raw `entry_type == key` check to
  `normalize_kind(entry_type) == key` to match).
- Row titles, the doc header, and group-detail bullets use `display_title`
  instead of raw `e.name`; the doc body is rendered through
  `strip_leading_h1` so a body opening with its own H1 isn't shown twice.

Verified visually in an isolated kitty window (scratch `--db`, a 3-entry
scratch brain seeded with `decision`/`decisions` singular+plural types and
one H1-bearing body, NINOX_CONFIG/NINOX_BRAIN both pointed at scratch
paths — every ambient `NINOX_*` var this worker's shell already carries
must be `env -u`'d first, since `resolved_brain_path()` checks
`NINOX_BRAIN` before a loaded config's `brain.path`, see BRIEF-common.md
§7): by-type grouping showed one "Decisions (2)" group for the merged
kinds, the H1-bearing entry's row/doc title read "The PTY Daemon" (not its
frontmatter name "ptyd"), and the doc body showed only the body text below
the heading, not the heading again.

Tests: `type_grouping_merges_singular_and_plural_kind_spellings`
(`brain.rs`) and `brain_prefers_the_bodys_first_h1_as_title_and_shows_the_
normalized_kind_label` (`view.rs`).

### T5 — the Fleet tab must never surface the ninox-prompt singleton

Round-2 item: the GUI worker's G2 fix (#273) found the ninox-prompt agent
session (`PROMPT_SESSION_ID`/`worker_type: "prompt"`) leaking into the
GUI's islands/Board/counts, since nothing excluded it from the raw session
list those are built from — and landed the shared predicate for it,
`ninox_core::worker_type::is_prompt_agent_session`. The TUI has the
identical gap.

The TUI funnels every session into one place before anything downstream
(islands via `fleet_snapshot`, Board via `kanban::column_rows`, the tree,
every `rows`-derived count, selection, and connect/attach) ever sees it:
`build_rows` (`mod.rs`), fed by `state::Row`. Filtering there — rather than
at each of those call sites separately, the way the GUI had to (it has no
single row-construction choke point) — means the singleton can never reach
any of them, by construction, with no second site to keep in sync if a new
one is added later. (#273 landed mid-item; a local mirror of the predicate
was used until then and removed once the real `ninox_core` function was
available, so both apps share the exact same check.)

Filtering it out of `rows` doesn't cover the prompt strip, though — that's
driven by a direct `PROMPT_SESSION_ID` call (`prompt_agent::ask`), never a
`Row`. A closer look found the actual bug: `PromptAgentEvent::Sent` always
carried a `session_id` (it's `ask`'s success value, always `PROMPT_SESSION_
ID` itself), which the strip used for its `'o' open pane` key — so sending
to the agent made `'o'` try to attach to it. `poll_prompt_strip` also keys
off that same `session_id == PROMPT_SESSION_ID` to pick its live-reply poll
path (`prompt_agent::reply_since` vs. a plain `read_screen` snapshot), so
the field couldn't just be nulled out for the Agent target without
silently breaking T3's live-streaming reply. Fixed at the two read sites
instead, both now excluding the reserved id specifically rather than
`session_id.is_some()`: the `'o'` key handler (`state.rs`) and its footer
hint (`view.rs`).

Verified visually in an isolated kitty window, a scratch store seeded with
one orchestrator, one ordinary worker, and the `ninox-prompt` singleton
(`worker_type: "prompt"`): the Fleet tree/islands showed only the
orchestrator and its one worker (counts: "0 active · 0 done · 1 stopped"),
and the Board's columns showed only those two — the singleton appeared in
neither.

Tests: `o_never_opens_the_prompt_agent_singleton` (`state.rs`);
`rows_never_include_the_prompt_agent_singleton`, `an_ordinary_spawn_type_
prompt_session_is_not_mistaken_for_the_singleton` (`mod.rs`); `fleet_
prompt_strip_never_offers_to_open_the_prompt_agent_singleton` (`view.rs`)
— `is_prompt_agent_session` itself is covered by `ninox_core`'s own tests.
