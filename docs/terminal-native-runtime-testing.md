# Testing the terminal-native runtime

Branch `terminal-native-runtime` (local only). Worktree:
`~/proj/ninox/.claude/worktrees/terminal-native-runtime`.

```sh
cd ~/proj/ninox/.claude/worktrees/terminal-native-runtime
cargo build --release
export PATH="$PWD/target/release:$PATH"   # puts ninox and its nx alias first
```

`nx` is the same binary as `ninox`. On its first run, `ninox` drops an `nx`
symlink next to itself (for `cargo install` users that's `~/.cargo/bin/nx`).
It never replaces an existing `nx` that isn't ninox's own alias.

- Bare `nx` from a terminal opens the TUI.
- Bare `ninox` behaves as it always has: it opens the desktop app when there
  is a display, and the TUI only when there isn't one.
- `nx <subcommand>` is the same as `ninox <subcommand>`.

## 0. Isolated first (no risk to your real fleet)

```sh
scripts/tnr-smoke.sh            # KEEP=1 to keep the sandbox for poking
```

Uses a throwaway `HOME`, config, db and ptyd socket plus a stand-in `sh`
harness. It covers spawn → read → send → engine restart (agents survive) →
simulated reboot → checkpoint on disk → reconcile → dry-run → restore →
idempotent re-run.

To drive the TUI in a sandbox, export the same env as the script
(`HOME`, `NINOX_CONFIG`, `NINOX_PTYD_SOCKET`) and run `nx --db <db> --port <free port>`.
For tmux-backed sessions, also export `NINOX_TMUX_SOCKET=<name>` and set
`[runtime] backend = "tmux"`. Agents then go to a throwaway `tmux -L <name>`
server, not your real `-L ninox` one. Test binaries ignore the variable.
Run the TUI inside a private outer tmux
(`tmux -L tnrtui -f /dev/null new-session -d -x 160 -y 45 '<env> nx …'`)
to script it. Drive it with `send-keys` and read it with `capture-pane -p`.
Mouse clicks can be injected as raw SGR reports:
`send-keys -l $'\e[<0;COL;ROWM\e[<0;COL;ROWm'` (1-based cells). The outer
tmux's own mouse setting doesn't matter. For brain entries, point
`NINOX_BRAIN` at the sandbox and pipe markdown into
`nx brain add notes/x.md`.

## 1. Real use

Your existing tmux sessions keep working: an operation on an existing
session goes to ptyd only if ptyd holds that pane, and everything else
falls back to tmux. New sessions use `[runtime] backend`. The default is
`tmux`, so the desktop app is unchanged. Opt in to the tmux-free runtime
with `backend = "ptyd"` under `[runtime]` in `config.toml`; the TUI renders
both kinds of session either way.

| Try | Expect |
|---|---|
| `nx` in a terminal | The engine (and ptyd, when needed) auto-start; the TUI opens on the fleet board |
| `ninox` in a terminal | The desktop app, exactly as before |
| `ninox gui` / launching the .app | The Iced app, as before. ptyd sessions render through `ninox pane attach --no-detach` |
| The fleet sidebar | Orchestrators are group headers (`▾`) with their workers nested and a rolled-up count (`◉1 ●2`). `Space` (or a click on the chevron) folds a group; a folded header shows its most urgent worker's dot. Standalone sessions sit under STANDALONE. Ended sessions stay, dimmed (`⊘ ended`, `✓ done`, `↻ interrupted`), until reaped |
| ⚑ NEEDS YOU | Pinned above the tree: blocked, CI failed, interrupted, unread messages, in review, mergeable. Click a pinned row to select the session. From the keyboard, `k` from the top of the tree walks up into it, and `!` (or `prefix !` from a pane) jumps straight to the most urgent one |
| Narrow terminal (< 64 columns) | The sidebar and the pane take turns full-width, whichever has focus |
| Colours | Your terminal's own palette: default fg/bg plus its 16 ANSI colours, so the TUI follows your terminal theme. `[tui] colors = "field-notes"` paints the desktop app's theme in RGB instead. Agent panes always keep the agent's colours |
| TUI: `Enter` on a row | Focuses the live agent pane; every key goes to the agent |
| `Enter` on a **tmux** row | The session renders *inside* the pane. The TUI runs `tmux attach` in a hidden ptyd "viewer" pane and composites it like a ptyd session: typing, paste, resize and mouse all work. If no ptyd host is running, one is started for viewers even with `backend = "tmux"` |
| `Ctrl+]` while in a pane | Always back to the sidebar, whatever the prefix; every command is then a bare key there (`Ctrl+]` then `x` kills, `?` lists keys). `prefix Ctrl+]` sends a literal `Ctrl+]` |
| The prefix | `C-\` on macOS (the system takes `Ctrl+Space` for input sources), `Ctrl+Space` elsewhere. Neither is bound by Claude Code or Codex. Change it in the Settings tab; it applies at once. On macOS with `Ctrl+Space` set explicitly, nx says once at startup that it may never arrive |
| Pane title `◀ fleet  ✕ kill  ⤢ zoom` | Click `✕ kill` to kill the agent shown (asks first; for an orchestrator the question says its workers keep running), `⤢ zoom` to zoom. Ended sessions show their report's `[ x Remove ]` instead |
| The selected sidebar row's `✕`, or a right-click on any row | Same as `x`: kill a live agent, or remove an ended one. Removing an orchestrator removes its workers too (the confirm says how many) |
| Click the sidebar, the header tabs, the footer, or the pane title buttons | Always handled by ninox, even while the agent has mouse reporting on. Only clicks inside the pane reach the agent, translated to pane cells |
| Header tabs `1 Fleet · 2 Overview · 3 PRs · 4 Brain · 5 Settings` | Click a tab, or press `1`–`5` from the sidebar (`Ctrl+]` first from a pane) |
| Settings tab | An editable form over `config.toml`, grouped like the desktop app's settings: runtime backend, prefix, colours, theme, orchestrator/worker harness and model, editor, harness on/off, send mechanism, PR watching, restore policy, auto-reap, retention days, brain harvest, port. j/k (or wheel) move, `↵`/space (or a click on the value) toggles or picks the next choice, ←/→ cycle. Text fields open an inline editor: `↵` saves, Esc cancels, `C-u` clears; a bad value is refused under the field. Each change is saved at once, re-reading the file first so nothing else in it is lost; a file that doesn't parse is never overwritten. `e` (or `[ e Open in $EDITOR ]`) suspends nx for `$VISUAL`/`$EDITOR` and reloads when it exits. The prefix, colours and restore policy apply live; the port on the next engine start |
| PRs tab | Every PR a session owns plus every `ninox open --pr` watch, one row per PR (a watched session PR is marked `watched`), grouped by repo. Each row shows the number, its state in the session's status colour (open, CI failed, in review, mergeable, merged, session ended, or watching for a watch-only PR), the CI / review / merge checks, the title, the owning session and `[ ↗ Open ]`. j/k (or the wheel) move, skipping the repo headers. `↵` or `o` (or a click on `#n` or `[ ↗ Open ]`) opens the PR with `open`/`xdg-open`. `s` (or a click on the session name) jumps to that session on the fleet tab. With `[pr_watch]` off, session PRs are still listed and the summary line says watching is off. The empty state says how PRs get there |
| Brain tab | A foldable tree grouped by tag, like the fleet sidebar: `▾ tag  (n)` headers with their entries nested, `untagged` last. An entry with several tags is listed under each one, and the cursor stays on the copy you picked. `t` switches the grouping between tag, type and flat. `Space` (or a click on a header's chevron) folds a group, `↵` on a header folds it too, and `h` goes from an entry up to its header. j/k skip folded entries. Groups start folded; `Space`/`↵` or a click unfolds one. `/` searches ripgrep-style as you type: every word must match the name, tags, type, path or body; smart case (an uppercase letter makes it case-sensitive); results become one ranked list (name > tag > type > path > body) with the first matching body line shown as `n:text`, matches highlighted. `↵` keeps the filter and adds semantic (embedding) matches from the brain's own search, marked `≈`; `Esc` clears the search. Click an entry to read it, click the doc to scroll it with j/k, click the search box to type. Wheel scrolls the list or the doc |
| Brain editing | `e` (or `[ e Edit ]` above the entry) suspends nx for `$VISUAL`/`$EDITOR` (else `vi`) on the entry's markdown. When the editor exits, the entry is reindexed in the background: "indexing…" shows by the search box, the index is rebuilt (with embeddings), a remote-backed brain is synced, then the list reloads with the cursor on the same entry. Changed tags regroup it. An entry with no markdown file (a stale index) says so instead. `a` (or `[ a New entry ]`) opens a template (`name`, `type`, `tags`) pre-filled with the type of the entry you're on and the tag of the group you're in. It is saved as `<type>/<name>.md`, never over an existing file, and indexed. Left unchanged or emptied, it is cancelled. `D` (or `[ D Delete ]`) asks in the `[ y Yes ]` / `[ n No ]` modal, then deletes the markdown file, syncs and reindexes. To try it without an editor, set `VISUAL` to a script that edits `$1` |
| Drag in a pane | Highlights and copies the selection (OSC 52) on release. Double-click copies a word. Over an agent with mouse reporting, hold Shift (your terminal may handle Shift+drag itself instead) |
| Wheel | Scrolls the pane's scrollback (or goes to the agent if it wants the mouse), the sidebar list, the brain or the settings |
| Select an ended session (done, ended, interrupted, or a ptyd pane that exited) | The pane shows its report instead of an empty pane: name, status, how it ended, start → end and duration, cost and harness. Below that: the task brief, the PR (number, title, state, CI, review, URL), what it did in its workspace (branch, commits ahead of `origin/HEAD`/main/master, files changed with +/−, uncommitted changes), its last screen when ptyd kept one, and the workspace path. The git part loads in the background and shows "loading…" until it arrives |
| Report buttons `[ x Remove ]` `[ r Resume ]` `[ O Open PR ]` | Click them, or press the key shown, bare, while the sidebar or the report has focus. Remove asks first, in a modal with clickable `[ y Yes ]` / `[ n No ]` (or press y/n), then deletes the session's record and worktree; the branch stays. Resume relaunches it under the same id with the harness's `resume_args`, the same as the desktop app's Resume. It only shows when the session has a workspace, a conversation id and a harness that can resume. Open PR (or a click on the PR lines) opens the URL with `open`/`xdg-open`. j/k or the wheel scroll the report. Esc goes back to the sidebar |
| Keys `x` / `r` / `O` / `R` | `x`: kill a live agent, or remove an ended one. Both ask first. `r`: resume. `O`: open the PR. `R` (shift): reap an orchestrator's finished workers (this was `r` before). They work bare in the sidebar and on a report. From a live agent pane, press `Ctrl+]` first (or the prefix) |
| `a` (sidebar) | Attaches the selected session full-screen, like the old behaviour. tmux: `C-b d` returns. This is also what `Enter` does on a tmux row when ptyd can't run |
| Inside a tmux viewer: `C-b d` | Closes only the view. The pane says "tmux view closed" and `Enter` reopens it |
| `?` | Help overlay: what works inside an agent pane (`Ctrl+]` first), the bare fleet-list keys, the mouse |
| `n` | Spawn modal |
| `o` / `z` / `g` | Overview grid of live previews / zoom / goto picker (Tab cycles blocked/working/idle) |
| `[` | Scroll mode (j/k, PgUp/PgDn, `y` copies via OSC 52) |
| `d` / `q` | Detach. Agents keep running; `nx` again reattaches with state intact |
| `prefix <key>` | Any of the keys above without leaving the agent pane, when the prefix reaches the TUI |
| `nx connect <id>` | Raw attach in your own terminal. For ptyd panes, detach with your `[tui] prefix` then `d` (`C-\ d` on macOS by default) |
| `nx read <id> --lines 40` | The agent's screen as text (also an orchestrator capability) |
| `nx pane list`, `nx ptyd status` | What the host holds. TUI viewer panes (`tmux-view:<tui pid>:<session>`) are hidden here and from the engine |

Things to look for in the Claude Code panes:

- No flicker or torn frames while it streams.
- Spinners and colours are right, and wide characters/emoji line up.
- Approval prompts are readable.
- Paste works, including multi-line paste.
- Mouse wheel scrolls.

## 2. Durable fleets

1. Start an orchestrator with a couple of workers.
2. Simulate a reboot: `nx ptyd stop` kills every pane, the same as a reboot. Then quit the TUI and GUI.
3. Run `nx`. The engine reconciles the dead sessions and marks them `interrupted`.
4. `nx fleet status` shows the per-session state and the restore plan.
5. `nx fleet brief <orchestrator>` prints the briefing the orchestrator will get.
6. `nx fleet restore --dry-run`, then `nx fleet restore`. Workers resume first, then orchestrators, each with the briefing as its first input.
7. The orchestrator should run `ninox fleet ack` (its `fleet-recovery` skill tells it to).

Policy goes in `config.toml`:

```toml
[fleet]
restore_policy = "prompt"   # manual (default) | prompt | auto
```

With `prompt`, the TUI opens with "Restore fleet (N workers, M orchestrators)?".

To have the engine start at login, run `nx service install` (launchd agent
`io.ninox.engine`); remove it with `nx service uninstall`.

## 3. Engine/host upgrades

- **Engine restart.** Kill the `ninox --headless` process. Agents keep running and the TUI reconnects.
- **Host upgrade without killing agents.** Build a new binary, then run `nx ptyd --takeover`. It adopts every pane over `SCM_RIGHTS`.

## Known gaps

- Unix only.
- The TUI composites panes on the client side. Server-side rendering for
  remote or multiple clients isn't built; the protocol's `Raw` streams are
  the hook for it.
- The Settings tab covers the desktop app's fields plus the TUI's own; the
  brain's settings, catalogues, harness definitions and the GitHub token are edited in
  the file (`e`).
- Viewer panes run `tmux attach`, so their scrollback is tmux's redraws,
  not the agent's history. Wheel or scroll mode over a tmux session shows
  little. Attach full-screen (`a`) and use tmux copy-mode instead.
- With `window-size latest`, the most recently active tmux client sizes the
  window. A tmux session you are viewing in the TUI takes the pane's size
  until you type into a full-screen attach.
- Viewers are killed when the TUI quits. A crashed TUI's viewers (and their
  checkpoints) are reaped the next time any TUI starts.
- Overview tiles crop rather than resize the agents.
- Replay fixtures are synthetic. A real Claude Code session did render
  correctly in ptyd during smoke testing.
- Nothing is deleted: the Iced app, `TmuxBackend` and the tmux config writer
  are all still present.
