# MLOPS-5339: clickable Brain wikilinks

`brain_panel::preprocess_wikilinks` turned `[[x]]` into `[x](ninox-brain:x)`, but iced 0.13.4's
`markdown::parse` keeps a link only when its scheme is http or https (`iced_widget`
`src/markdown.rs`, the `Tag::Link` arm). So wikilinks rendered as plain text and
`Message::BrainLinkClicked` never fired.

## Tasks

1. **`components/internal_link.rs`**: `InternalLink { Brain(id), Session(id) }`, `encode` →
   `https://ninox.invalid/<brain|session>/<id>` (every byte outside RFC 3986's unreserved set
   percent-encoded, so the id is one path segment and a valid CommonMark destination), `decode(&Url)
   -> Option<InternalLink>` (`None` for anything else). Tests: round trip with spaces, parens, `%`,
   `/` and non-ASCII; foreign, malformed and wrong-kind URLs decode to `None`; an encoded link
   survives `markdown::parse`.
2. **Use it**: `preprocess_wikilinks` emits `encode(Brain(target))` and the old ad-hoc
   percent-encode/decode pair goes. `BrainLinkClicked` decodes: a Brain link resolves and selects
   the entry, a Session link is ignored here (the reply drawer wires it up), and anything else goes
   to `OpenUrl`. Tests: a preprocessed wikilink is still a link after parse, and clicking one
   selects the linked entry.
3. One isolated live capture of the reading pane showing the wikilinks rendered as links.
