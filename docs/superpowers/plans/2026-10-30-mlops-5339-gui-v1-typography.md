# MLOPS-5339: v1 typography — Inter and system mono, real bold

The GUI still bundled the Field Notes fonts (Archivo, Newsreader, Spline Sans Mono), and none of
them rendered as intended. cosmic-text 0.12 (iced 0.13's text stack) only takes a face of exactly
the requested weight and style before falling back to system fonts; monospace is the one exception.
The variable Archivo registered as a single weight-600 face, so:

- `SANS` (400) rendered in San Francisco on macOS and DejaVu Sans on Linux.
- `SANS_BOLD` rendered in Menlo-Bold.

The v1 boards (`Main.dc.html`) specify `-apple-system, 'Segoe UI', Inter, system-ui, sans-serif`
for UI text and `ui-monospace, 'SF Mono', Menlo, Consolas` for mono.

## Tasks

1. **Inter**: bundle static Inter 4.1 Regular, SemiBold, Bold and Italic (OFL, no Reserved Font
   Name; `OFL-Inter.txt`). Hinting is stripped and the full character set kept (Latin, Greek,
   Cyrillic, symbols); about 290 KB each. Constants:
   - `SANS` is Inter 400, `SANS_SEMIBOLD` 600 (the boards' 600), `SANS_BOLD` 700, and
     `SANS_ITALIC` 400 italic.
   - The leftover `SERIF*` constants become aliases of the Inter constants.
   - `style::use_v1_generic_families()` points the generic sans-serif family at Inter, so iced's
     markdown bold and italic, which ask by weight and style rather than name, resolve to Inter.
   - The default font is `SANS`.
2. **Mono**: `MONO` is Menlo on macOS, which is in the board's stack (SF Mono can't be loaded by
   name), and the generic monospace family elsewhere. On macOS the generic monospace family also
   points at Menlo, so markdown code spans match. No mono is bundled: cosmic-text's monospace
   exception covers weights Menlo lacks. The terminal keeps its own JetBrains Mono.
3. **Remove** the Archivo, Newsreader and Spline Sans Mono files, their licences, and the comments
   that described them.
4. **Glyphs**: iced's `text` shapes Basic by default, which has no per-glyph fallback, so a symbol
   the base font lacks renders as a box. An audit of every non-ASCII literal against Inter's and
   Menlo's coverage found these:
   - Inter covers `·×–—•…›←↑→↓↗↵⇧−⌘⏎▶▼○●✓✗`.
   - The sites rendering `▸ ✕ ⚒ ◌` in sans text switch to `Shaping::Advanced`: the `fleet_islands`
     fold chevron, the Brain repo chip, the `filter_bar` clear button, the worker-type rule chips
     and toggles, the `fleet_focus` tool note, and the `info_panel` CI glyph.
   - The other symbols Inter lacks already go through `GLYPH`, `GLYPH_ALT` or `NERD`.
5. **Tests** (`style`), shaping through iced's own `to_attributes`:
   - each Inter constant, plus markdown bold and italic, shapes in Inter at its weight and style;
   - `MONO` and markdown code shape in Menlo on macOS;
   - every `BASIC_SYMBOLS` glyph is a real Inter glyph under Basic shaping;
   - every `ADVANCED_SYMBOLS` glyph gets a real fallback glyph under Advanced shaping.

   The same resolution was checked on Linux with a cosmic-text 0.12.1 reproduction in a Debian
   container with fontconfig and DejaVu, as CI has: Inter at 400, 600 and 700, Inter Italic, and
   DejaVu fallbacks for the Advanced symbols.
6. **Review fixes**:
   - Off macOS, `GLYPH` and `GLYPH_ALT` name DejaVu Sans. The generic default they used to fall to
     is now Inter, which lacks every dingbat.
   - `MONO_MEDIUM` is Bold: neither mono has a 500 face, and a missing weight leaves the family.
   - Mono off macOS is the bundled JetBrains Mono, both as `MONO` and as the generic monospace
     family.
   - `SERIF_MEDIUM_ITALIC` keeps its weight (SemiBold) so the active state still differs.
   - Headless layout tests call `style::load_bundled_fonts()` first.
   - Tests: every mono weight resolves to the platform mono, and every `glyph_font_for` dingbat is a
     real glyph under Basic shaping.
7. **Linux CI fix (dingbats)**: DejaVu Sans on the runner has no `⌕` (U+2315), so `GLYPH` can't
   lean on a system font.
   - `GLYPH` is now a bundled "Ninox Symbols" on every platform (137 KB, OFL-NinoxSymbols.txt). It
     merges Noto Sans Symbols (instanced at 400) and Noto Sans Symbols 2 (neither declares a
     Reserved Font Name), subset to the Arrows, Misc Technical, Geometric Shapes, Misc Symbols,
     Dingbats and Misc Symbols & Arrows blocks.
   - `GLYPH_ALT` is Inter, and `glyph_font_for(text)` keeps text whose characters are all ASCII or
     in `INTER_SYMBOLS` in Inter ("⌘K" in one font), otherwise the symbol font.
   - `glyph_font_text_renders_as_real_glyphs` checks every glyph of each dingbat and keycap string.
   - A Linux cosmic-text reproduction shapes all of them with no .notdef.
8. Before and after window captures of Fleet, Brain and the reply drawer.
