# MLOPS-5350 item 7a: TUI layout fixes — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The four layout fixes queued from earlier reviews for item 7, split out from item 7's Board/Fleet parity work so each PR stays small:
1. At 120 columns the Fleet toolbar's hints run under the detail pane ("f fo"). Drop them by priority instead of clipping.
2. The prompt box's `ninox ❯` title reads as a separate line above `→ ninox ▾`. Make it one `❯ ninox ▾ ask …` line, the TUIAtScale board's single prompt line.
3. The help modal doesn't scroll and is clipped at 120x40. Make it scroll.
4. A zero or unknown `started_at` renders as `20735d`. Show `—`.

**Architecture:** View-only. `fleet_toolbar_line` fits the state fields (dropped from the right), then the key hints (dropped least important first: `/ prompt` stays longest) to the toolbar's width. The prompt box keeps its border (T2: it must read as an input box) and loses its title, with `❯` leading the target chip. `help_lines` builds the help modal's lines for both `layout` (`help_max_scroll`) and the view. The modal scrolls with `j/k`/PgUp/PgDn/`g`/`G` (the shared `scrolled`), and any other key still closes it. Its hint sits on the bottom border, so it stays visible while scrolled. `ago` renders a timestamp before Sept 2001 in ms (unset 0, or seconds stored where ms belong) as `—`.

**Tech Stack:** Rust, ratatui (`ninox-tui`).

**Spec:** Orchestrator review notes on #297/#299 (queued for item 7), `TUIAtScale.dc.html` for the prompt line.

## Review Focus

1. No toolbar width, 30–200 columns, shows a partial hint.
2. Every help group, including the last ("Mouse"), is reachable at 120x40.
3. Any non-scroll key still closes the help modal, and it reopens at the top.

---

### Task 1
- [x] Tests: `ago_formats_and_shows_a_dash_for_an_unknown_time`, `the_fleet_toolbar_drops_hints_by_priority_instead_of_clipping_one`, `the_help_modal_scrolls_when_it_is_taller_than_the_screen`, `the_help_modal_scrolls_with_j_k_and_any_other_key_still_closes_it`; the prompt tests now expect `❯ ninox ▾` and no separate title.
- [x] `ago`'s guard, `fleet_toolbar_line`, the prompt chip, `help_lines` + `Layout::help_max_scroll` + `TuiState::help_scroll`.
- [x] `cargo test --workspace`, clippy; commit.
