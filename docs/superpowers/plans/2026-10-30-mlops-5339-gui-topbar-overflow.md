# MLOPS-5339: top bar at narrow widths

At about 745px the top bar's fixed 240px search field left no room. iced squeezed `+ Spawn` and the
settings dot to zero width, so they disappeared off the right edge. They must never leave the
window. The search field gives way first, then the tab labels.

## Tasks

1. **Layout sweep test** (`app` tests, tiny-skia layout of `shell::nav::nav`): for every width from
   480 to 1400px in 20px steps, with search closed and open, the bar ends inside the window,
   `+ Spawn` and the settings dot keep their real width, and no label wraps. It fails on the old
   bar, where `+ Spawn` measured 0px at 480px.
2. **`NavFit`** (`shell/nav.rs`), chosen from `window_width`, which is zoom-corrected since #306:
   - `Full` at 1000px and above.
   - `SearchIcon` from 760px: the field becomes a `⌕ ⌘K` button that opens search.
   - `TabNumbers` below 760px: the icon stays and the tab labels become their numbers.
   With search open on a narrow bar, the field takes the tab labels' room as `Fill`, and the tabs
   show numbers.
3. **Review fixes**:
   - `GlobalSearch::dismiss` also clears `hover_field`. On a narrow bar the field leaves the layout
     on close and never gets `on_exit`, so a stale flag swallowed click-outside dismissal.
   - The leftover-workspaces chip's estimated width comes off the width `nav_fit` sees, and in
     numbers mode the chip shows just its count. The sweep covers the chip.
   - At startup `window_width`/`window_height` start as 1024×768 divided by the saved zoom, not
     raw 1024×768.
4. Isolated window captures at 745, 900 and 1280px, plus 745px with search open.
