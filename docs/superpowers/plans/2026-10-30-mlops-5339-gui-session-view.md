# MLOPS-5339: GUI session view, v1 restyle

Spec: `Session.dc.html` plus the canvas `brainnote` "SESSION VIEW" paragraph.
One PR into `v1`.

## Decisions (orchestrator, 30 Oct)
- Restart and Kill sit behind an inline confirm (`Kill? Confirm · Cancel`).
  Esc cancels, and nothing fires without Confirm.
- The page opens with the terminal unfocused. A click on the terminal, `c`
  or Connect focuses it. While it's focused every key reaches the PTY, Esc
  included. `Ctrl+]` releases it (matching the TUI), as does a click outside
  the terminal. `Ctrl+]` is shown in the frame header and listed in the `?`
  overlay.
- Pop out runs `ninox connect <id>` in kitty when `/Applications/kitty.app`
  exists, otherwise in the default terminal (a `.command` file opened with
  `open`).

## Tasks
1. `session_detail.rs`:
   - Header: breadcrumb, name, type and harness badges, live activity, cost.
   - Actions: Connect, Send, More ▾, Restart and Kill, with the inline
     confirm and the composer.
   - Tabs: Terminal · Split · Info · Inspector · Diff, with a teal underline
     and a `+N −M` chip on Diff. Orchestrators keep Terminal · Plan.
   - The dark terminal frame, with its mono header bar and the hint footer.
   - The Split side panel: PR, Definition of done, Brief, Workspace and
     Activity.
   - Fixed row heights, so `TERM_CHROME_H` is an exact sum.
2. Pure helpers with unit tests:
   - `SessionAction` → `Message` mapping
   - More-menu items
   - visible tabs and effective panel
   - diff totals
   - workspace line
   - activity lines
   - pop-out command
3. `app.rs`:
   - Focus state, gating `terminal_owns_keys` and the key encode path.
   - The `c`, `Ctrl+]` and Esc-back keys.
   - The click-outside release, which uses the canvas click marker against
     `MousePressed` ordering.
   - The More menu, the confirms and pop out.
   - Workspace-facts and activity fetches, plus a diff fetch when the page
     opens so the tab count shows.
   - `resize_terminals` on focus, tab and composer changes.
   - `is_prompt_agent_session` in `NavigateSession`.
4. `terminal.rs`: a `focused` flag. Unfocused, the canvas ignores keys (so
   typing `c` in the prompt bar can't grab the terminal) and reports clicks.
5. `info_panel.rs` and `inspector_panel.rs`: restyle to v1 tokens. No new
   content.
6. `help_overlay.rs`: a Session scope (`c`, `Ctrl+]`, Esc). The chord gets a
   `ctrl` flag, because `cmd` is ⌘ on macOS.
7. Visual check: an isolated instance on a random port, window-ID capture
   only.
