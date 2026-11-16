# MLOPS-5339 GUI parity, item 4: Restore fleet

Spec: `.claude/design-ref/fleet-overview-v1/BRIEF-MLOPS-5339-gui-parity.md` item 4. There's no board;
the modal follows the existing v1 modal look. The TUI offers a y/n restore at startup
(`pending_restore`) and runs a detached `ninox fleet restore --yes`; the CLI already takes
`--only <ids>`.

## Tasks

1. **Shared launcher** (`ninox_ui_shared::fleet::spawn_detached_restore(db, config, only)`), moved
   from the TUI.
   - The child runs against the caller's `--db`, with `NINOX_CONFIG` set to the caller's config,
     so an isolated instance can only restore its own store.
   - It's a detached process group, so a minutes-long restore outlives the window.
   - Tests check the command line (scratch `--db`, `--only` ids) and the env.
   - The TUI calls it with an empty `only`.
2. **`fleet::restore_plan(store, config)`**: the dry-run plan (snapshot + `plan_restore`). It takes
   no lease and writes nothing; tested against a seeded interrupted store.
3. **`NotificationKind::FleetRestorePending`** in core: labelled "Restore" with attention tone in
   `ninox-overview`, so both apps list it the same way.
4. **GUI** (`components/restore_modal.rs`, `app.rs`):
   - **Modal:** the plan loads off the UI thread, then lists each step (name, role, resume or
     fresh) with a checkbox, all checked by default. Sessions that can't be restored are greyed
     with their anomalies.
   - **Actions:** "Restore selected (N)" and "Restore all" lead to an inline confirm ("Restore N
     sessions? Workers first, then orchestrators."). Confirming starts the detached restore,
     `--only` the selection in plan order or nothing for all. The result shows in the modal.
   - **Esc** backs out of the confirm, then closes the modal.
   - **Startup:** when `pending_restore` is set, a "Fleet was interrupted" notification offers
     "Restore…". Dismissing it clears the flag, as the TUI's decline does. A started restore removes
     the offer.
   - **Settings:** the Fleet card has "Restore fleet…".
5. **Review fixes**:
   - `fleet::restore_running` reads the restore lease. The modal shows "A restore is already
     running" and disables its actions, and confirming re-checks the lease right before launching.
     Before, a second restore was refused by the CLI out of sight in `daemon.log` while the GUI
     showed success.
   - A selection of every step runs as a full restore: `--only` leaves undelivered briefings
     behind.
   - A plan holding only undelivered briefings can still "Restore all".
   - "Dismiss all" declines the restore offer too.
6. Isolated window captures of the modal and the offer, on a scratch store seeded with fake
   interrupted sessions. Restore is never pressed.
