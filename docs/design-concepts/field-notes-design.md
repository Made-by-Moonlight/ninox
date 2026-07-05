# Ninox — "Field Notes" Design Specification

The chosen UI direction for the Ninox native app (Rust + Iced). Reference mockup:
`docs/design-concepts/03-field-notes.html` — a navigable prototype covering every
top-level view in both themes. This document is the implementation handoff.

**The idea:** Ninox is a genus of owl; the app is an ornithologist's *field journal*
for a fleet of agents. Warm paper, ink, ruled ledger columns, rotated rubber-stamp
statuses, dotted rules, hard offset shadows. The terminal is deliberately the one
dark object on the page. The dark theme is *the same journal read by lamplight*,
not a separate design.

---

## 1. Design tokens

All values live as CSS variables in the mockup (`:root` = light, `body.dark` = dark)
and should become the Iced theme struct. Status colors keep the existing semantic
mapping from the current app (green=working, blue=PR open, red=CI failed,
yellow=review, purple=mergeable, grey=done/terminated).

### Surfaces & ink

| Token        | Light     | Dark      | Use |
|--------------|-----------|-----------|-----|
| `paper`      | `#f5f0e4` | `#171410` | app background |
| `paper-2`    | `#efe8d8` | `#1f1b15` | sidebar, modal header, table header |
| `card`       | `#fbf7ee` | `#262119` | cards, panels, modals, reading pane |
| `ink`        | `#211d16` | `#ece3cd` | primary text, heavy borders |
| `ink-2`      | `#5b5344` | `#b5a98d` | secondary text |
| `faint`      | `#968a72` | `#83775c` | tertiary/metadata text |
| `rule`       | `#d9cfba` | `#393227` | light rules/separators |
| `rule-dark`  | `#b7ab90` | `#4e4534` | stronger rules, input underlines, card borders |
| `accent`     | `#c8451f` | `#e06038` | vermilion: active nav, stamps, attention, primary buttons, wikilinks |

### Status

| Status      | Light     | Dark      |
|-------------|-----------|-----------|
| working     | `#3e7d34` | `#7cc46a` |
| pr-open     | `#20629e` | `#5ca8e8` |
| ci-failed   | `#c8451f` | `#e86a4c` |
| review      | `#a97913` | `#d8a83c` |
| mergeable   | `#6d4fa3` | `#a184d6` |
| done        | `#8b8272` | `#7d7461` |

### Brain category colors (extends status palette)

repos → pr-open blue · symbols → working green · concepts → review gold ·
architecture → mergeable purple · patterns `#a23f8c`/`#c876b4` ·
decisions `#c86a1f`/`#e08a4a` · relationships `#2a8a80`/`#4ab0a4` ·
errors `#b3261e`/`#e0604a` (light/dark).

### Terminal (same in both themes — it is "the dark object")

- Light theme: bg `#23201a`, bar `#2c2822`, text `#ece4d0`
- Dark theme: bg `#100d09` (darker than the page, cream border), bar `#191510`
- ANSI-ish accents: ok `#8fd37f`, error `#f08a72`, agent-voice `#f0c069`, dim `#7a7260`

## 2. Typography

Three families, three jobs. (Google Fonts in the mockup; bundle as assets in the app.)

| Family | Role | Notes |
|--------|------|-------|
| **Newsreader** (serif, variable optical size) | Display: view titles, card names, column heads (italic), tabs, drawer labels, reading-pane body headings | Titles ~28–34px opsz 60–72; card names 16px; italic = "handwritten margin note" register |
| **Archivo** (sans) | UI labels, buttons, body UI text | Micro-labels: 9–10px, 700, letter-spaced 0.14–0.2em, uppercase |
| **Spline Sans Mono** | Data: repo slugs, costs, timestamps, IDs, terminal, frontmatter | 9.5–12px |

## 3. Texture & object rules

These carry the identity — apply consistently:

- **Paper grain**: full-surface SVG fractal-noise overlay at ~35% opacity (10% in dark).
- **Heavy ink borders**: structural edges are `2px solid ink` (sidebar edge, terminal,
  ledger, reading pane, modals). Cards use `1px rule-dark`.
- **Hard offset shadows**: no blur, ever. Cards `2px 3px 0`, hero objects `4px 5px 0`,
  modal `8px 10px 0` — shadow color `rgba(33,29,22,α)` light / `rgba(0,0,0,α)` dark.
  Hover = translate(-1px,-2px) + grow the shadow (the card "lifts off the pin").
- **Rubber stamps**: status chips are uppercase, 8.5px/700/0.16em, 1.5px border in the
  status color, `rotate(-2deg)`. Stamps say a *word* (Working, Failed, Awaiting, Ready,
  Filed), not the enum name.
- **Dotted/dashed rules** for soft separations (card footers, comment threads, backlinks).
- **Corner radius**: 2–3px everywhere. Nothing pill-shaped except tag chips (14px).
- Emoji-free; the only glyphs are ⬡ (logo), ⚑ (attention), ⌕ (search), ✦/☰ (brain modes).

## 4. Structure & navigation

Fixed left sidebar (258px, resizable) + main content, one view at a time.

**Sidebar, top→bottom:**
1. Masthead: "Nin*ox* ⬡" + "FLEET FIELD JOURNAL" microlabel.
2. **Table-of-contents nav** — the app's top-level navigation, styled as a journal TOC
   with roman numerals and dotted leaders: `I. Fleet board · II. Pull requests ·
   III. Brain`. No "Session" entry — the session tree below IS the session
   navigation. Active item: vermilion 3px left bar + red numeral.
   Keyboard 1–3 switches views.
3. Action row: `Alerts (badge) · + Spawn`.
4. Session tree: orchestrators (bold) with indented workers, standalone sessions below;
   status dot + name + repo slug; active = card bg + vermilion left bar. Click → Session view.
5. Footer: theme dots (light/dark/ninox) — selected dot ringed in accent. `t` toggles.

## 5. Views

### I. Fleet board
- Folio header: big serif title ("Morning *observations*"), volume/date microlabel,
  underlined filter field, live count ("8/8 sessions").
- **Attention banner** (only when non-zero): 1.5px vermilion border, ⚑, bold counts,
  "See marked entries →".
- **Ledger board**: kanban columns separated by vertical rules (not boxes); column head =
  italic serif name + `№ n` count over a 2px ink rule. Cards: serif name, mono repo·branch,
  dotted rule, stamp + cost. Card click → Session.

### II. Session detail
- Header: back button, status dot, big serif session name, mono repo/branch/orchestrator
  line, PR stamp, cost, Kill button (outline → fills vermilion on hover).
- **Panel switcher** as italic-serif tabs over a 2px ink rule, vermilion underline on the
  active tab: `Terminal · Split (default) · Info · Inspector`.
- Split = terminal (~62%) + info column (CI checks panel, "Marginalia" review-comments
  panel). Info mode = panels reflow full-width. Inspector = key/value sheet
  (uppercase micro-labels + mono values).
- Terminal: dark inset object, 2px ink border, offset shadow; title bar with tmux
  pane/size/scrollback; blinking block cursor; agent commentary prefixed `✦ agent │`.

### III. Pull requests
- One ledger table in a heavy 2px ink frame: № (mono, vermilion) · Title (serif) ·
  Session (dot + name) · Repo (mono) · CI (stamp) · Cost (mono). Row click → Session.

### IV. Brain — catalogues of specimens, two modes (toggle in the folio bar)

**Multiple catalogues:** the app can hold several brains ("catalogues"), configured in
`config.toml` and viewed one at a time. The switcher is a **volume plate** at the head
of the left rail (Pinboard) / drawers cabinet (Catalogue mode): a paper-2 strip with a
CATALOGUE micro-label and the mono name (`⌂ default ▾`) — like choosing which volume of
the journal is open on the desk. It must NOT live in the folio bar (crowds the mode
toggle). Switching reopens the index for that catalogue and reloads the view. Sessions
choose their catalogue at spawn time (see Spawn modal).

**Adding a catalogue:** a small `+` at the right edge of the volume plate opens a
journal-entry modal ("File a new *catalogue*"): Name (serif underlined input) and
Path (mono underlined input, `~` expanded; the directory is created and the brain
index initialized if missing). Confirming appends a `[[brain.catalogues]]` entry to
`config.toml`, saves it, and switches the view to the new catalogue. Refusals
(duplicate name, empty fields, unwritable path) surface inline as a vermilion ⚑
line, exactly like the spawn modal's guards.

```toml
# config.toml
[[brain.catalogues]]
name = "default"          # implicit; resolves to brain.path / the standard location
[[brain.catalogues]]
name = "ninox-dev"
path = "~/proj/ninox/.brain"
```

- **✦ Pinboard** (graph): specimen board — nodes are ink-outlined dots colored by
  category, wikilink edges are dashed threads; search hits get a vermilion ring;
  preview card is a tilted (1deg) paper slip. Left rail lists the taxonomy w/ counts.
  Planned (per brief §7, not in mockup): temporal scrubber, cluster-by-repo,
  click-to-focus neighborhood.
- **☰ Catalogue** (directory view — the on-disk taxonomy): left = **card-catalogue
  drawers**, one per category (colored dot, serif label, count, drawer-pull handle);
  expand to list entries (mono, updated-date right-aligned; selected = vermilion left
  bar). Right = **reading pane**: mono breadcrumb (`brain / symbols / …`), serif title +
  type stamp, frontmatter as a ruled dl (type/tags/repos/updated), rendered Markdown
  body (65ch measure, 1.75 line-height), `[[wikilinks]]` in dotted-underline vermilion,
  "Cited by" backlink chips at the foot.

### Spawn modal (enriched — closes the brief's "name-only" gap)
Centered over a dimmed blurred backdrop: journal-entry header ("Spawn a *session*",
`ENTRY № n`). First field is an **Entry type** selector (joined ink-fill segments):
`⬡ Orchestrator · Standalone` — humans spawn orchestrators or standalone sessions;
workers are spawned BY orchestrators themselves (via `ninox spawn` / NINOX_BIN), never
from this modal.

- **Orchestrator** (default): Name · Catalogue picker · Agent/Model chips.
  No repository field, no attachment — orchestrators own their workspace.
- **Standalone**: Name · Workspace (mono path input — the directory it works in; its
  repo derives from that directory's git remote; no orchestrator attachment, no
  report-back instructions injected) · Catalogue picker · Agent/Model chips.

The **Catalogue** picker (both kinds) chooses which brain the session thinks with,
from `[[brain.catalogues]]`, exported to the session as `NINOX_BRAIN`. There is **no
Task field** — sessions are interactive; spawning drops you into the session terminal.

Fields as underlined serif inputs; cost estimate, Cancel ghost + vermilion "Spawn ⬡"
with offset shadow. Esc closes.

### V. Settings — the appendix
Opened from the sidebar footer (which becomes a `Settings ▸` row — the theme dots MOVE
here). Folio: "The *appendix*" / SETTINGS. A single narrow column (~720px) of cards:

- **Theme**: the light/dark/ninox dots (relocated from the footer) + a mono pointer to
  the active theme file (`themes/field-notes.toml`).
- **Harnesses**: one row per agent harness — ink-fill toggle, serif name, mono binary,
  underlined mono default-model input (disabled until enabled). `claude-code` is the
  locked-on DEFAULT; `codex`, `opencode`, `aider` and custom names (e.g. `freebuff` —
  unknown harnesses run their name verbatim as the binary) are **off by default**.
  Enabled harnesses appear as agent chips in the Spawn modal.
- **Assignments**: which harness+model new Orchestrators and Workers use unless the
  spawn entry overrides it (maps to config `[orchestrator]`/`[worker]`).

**Backend (registry, not enum):** harness definitions are DATA, not code — adding a
future harness must require zero Rust changes. Each harness is a spec:

```toml
[harnesses.freebuff]            # any name; binary defaults to the name
enabled = true
binary  = "freebuff"
model   = "fb-large"
interactive_args = ["--model", "{model}"]
worker_args      = ["--model", "{model}", "-p", "{prompt}"]
```

The four known harnesses ship as compiled-in default specs (claude-code enabled,
exact current launch shapes preserved); config entries override or extend the
registry. Template vars: `{model}`, `{prompt}` — an arg element containing
`{model}` is dropped entirely when no model is set. `AgentConfig { harness, model }`
stays as the per-role/per-spawn pointer into the registry, and the existing
`interactive_cmd`/`worker_cmd` call sites resolve through it. Serde-defaulted
throughout — existing configs keep parsing unchanged.

## 6. Interaction inventory

- Keys: `1–3` views · `t` theme · `Esc` closes modal. Mockup deep links:
  `#dark`, `#brain`, `#catalogue` (comma-separable).
- Hovers: cards lift; tree/TOC rows tint to card bg; × remove buttons appear on row hover.
- All live data (status dots, costs, CI stamps, counts) updates in place via the
  existing event bus; no skeletons — values just change.

## 7. Not yet designed (follow the language)

- Notification slide-down panel (style as journal margin: kind-stamped slips, dismiss-all).
- Empty states (one italic serif line, e.g. "Nothing pinned tonight.").
- Orchestrator vs worker visual distinction beyond bold+indent (consider a small ⬡ badge).
- Collapsed/empty kanban columns; sidebar & info-panel drag handles; "ninox" third theme.
- Brain temporal scrubber in Field Notes language (suggestion: a ruled timeline with a
  brass slider, "as of 14 Jun" in mono).

## 8. Iced implementation notes

- Tokens → one `Theme` struct with `light()` / `dark()` constructors; everything above
  is a color/spacing constant, no per-component hardcoding.
- Bundle Newsreader (variable), Archivo, Spline Sans Mono via `iced::font::load`.
- Hard offset shadows: Iced shadows support offset + 0 blur; otherwise a layered quad
  behind the card (container with translated backdrop) gives the exact effect.
- Stamp rotation: Iced lacks arbitrary rotation on widgets — acceptable fallback is an
  unrotated stamp; keep the border/typography treatment. (Canvas-drawn stamps possible
  where cheap.)
- Paper grain: optional; a subtle tiled texture via `image` widget or custom shader —
  ship without it first, it's polish not structure.
- The brain Pinboard graph is a `canvas` widget (the mockup's force layout is ~60 lines).
