//! The Brain tab's model: entries grouped into a foldable tree (by tag, by
//! type, or flat), cursor identity that survives reloads, and the edit /
//! new / delete jobs. Jobs write through the same functions as
//! `ninox brain add` (`write_brain_entry`), then sync a remote-backed brain
//! and rebuild the index, like `ninox brain index`.

use std::collections::{BTreeMap, HashMap, HashSet};

use ninox_core::brain::{kinds_sorted_by_count, resolve_edges, top_values, visible_entries, BrainSort};
use ninox_core::BrainEntry;
pub use ninox_ui_shared::brain_edit::{brain_job_paths, entry_from_edit, run_job, source_file, template, unqualify_job_id, Job, JobDone};

/// The group untagged entries sit under, after every tag.
pub const UNTAGGED: &str = "untagged";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Grouping {
    #[default]
    Tag,
    Type,
    Flat,
}

impl Grouping {
    pub fn next(self) -> Self {
        match self {
            Self::Tag => Self::Type,
            Self::Type => Self::Flat,
            Self::Flat => Self::Tag,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Tag => "by tag",
            Self::Type => "by type",
            Self::Flat => "flat",
        }
    }
}

/// Which graph-mode view is showing (`s` cycles). `Everything` (the GUI's
/// third scope) has no TUI equivalent — the scope bar says so instead of
/// offering it, per the brief ("Everything is GUI-only; use Clusters").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GraphScope {
    #[default]
    Neighbourhood,
    Clusters,
}

impl GraphScope {
    pub fn next(self) -> Self {
        match self {
            Self::Neighbourhood => Self::Clusters,
            Self::Clusters => Self::Neighbourhood,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Neighbourhood => "Neighbourhood",
            Self::Clusters => "Clusters",
        }
    }
}

/// A search result: `entries[idx]`, its rank score, and (rg-style) the
/// first body line containing a term, 1-based.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub idx: usize,
    pub score: u32,
    pub line: Option<(usize, String)>,
    /// Found only by the semantic search, not by the text terms.
    pub related: bool,
}

/// `full` when `term` starts a word of `text`, `part` when it appears
/// anywhere, else 0.
fn word_score(text: &str, term: &str, full: u32, part: u32) -> u32 {
    let mut best = 0;
    for (i, _) in text.match_indices(term) {
        let starts_word = text[..i].chars().next_back().is_none_or(|c| !c.is_alphanumeric());
        best = best.max(if starts_word { full } else { part });
        if best == full {
            break;
        }
    }
    best
}

/// One line of the entry list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    Group { key: String, count: usize, folded: bool },
    /// `entries[idx]`, under `group` (`None` in the flat list).
    Entry { group: Option<String>, idx: usize },
}

/// What the cursor is on, by identity rather than position: an entry with
/// several tags is listed once per tag, and a reload reorders the list.
/// `id: None` is a group header.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sel {
    pub group: Option<String>,
    pub id: Option<String>,
}

#[derive(Debug, Default)]
pub struct BrainView {
    pub entries: Vec<BrainEntry>,
    pub query: String,
    pub editing_query: bool,
    pub grouping: Grouping,
    /// Unfolded groups, client-side only — groups start folded so a big
    /// brain opens as a scannable list of tags.
    pub expanded: HashSet<(Grouping, String)>,
    /// Semantic matches for a query (ranked ids), from the background
    /// embedding search Enter starts. Shown after the text hits.
    pub semantic: Option<(String, Vec<String>)>,
    /// A semantic search is running for this query.
    pub searching: Option<String>,
    pub cursor: Sel,
    /// Where the cursor last was, for when its item disappears (deleted).
    pub last_index: usize,
    pub open: bool,
    pub scroll: u16,
    pub error: Option<String>,
    /// A background index job is running; what it is doing.
    pub indexing: Option<String>,
    /// Active Kinds-column selection (`ninox_core::brain::normalize_kind`
    /// value); `None` is "All entries". Mirrors the GUI's `kind_filter`.
    pub kind_filter: Option<String>,
    /// Active Repo chip filter, single-select.
    pub repo_filter: Option<String>,
    /// Active Tag chip filter, single-select.
    pub tag_filter: Option<String>,
    pub sort: BrainSort,
    /// `AppConfig::catalogue_options()`, snapshotted the first time the
    /// Brain tab loads and reused across reloads so cycling survives
    /// switching tabs away and back (`set_catalogues` preserves
    /// `active_catalogue` when called again with the same/updated list).
    pub catalogue_options: Vec<ninox_core::config::CatalogueRef>,
    /// Index into `catalogue_options`; `None` means "All sources" (the
    /// merged view across every enabled catalogue). `c` cycles this.
    pub active_catalogue: Option<usize>,
    /// Graph edges as undirected, deduplicated node-index pairs into
    /// `entries`, resolved from `BrainIndex::links_all()` by `set_edges`
    /// once per data load. Used by `BrainSort::MostLinked`.
    pub edges: Vec<(usize, usize)>,
    /// Entries that link to the selected entry ("Cited by"), loaded by
    /// `set_links` whenever the selection changes. Empty when nothing is
    /// selected or nothing cites it.
    pub backlinks: Vec<BrainEntry>,
    /// Entries related to the selected one (links, co-citation, shared
    /// tags — see `BrainIndex::related`), same load-on-select pattern.
    pub related: Vec<BrainEntry>,
    /// The selected entry's own link targets ("Links to"), same
    /// load-on-select pattern.
    pub outlinks: Vec<BrainEntry>,
    /// A flattened index into `backlinks ++ related ++ outlinks` (in that
    /// order) when the link panes have keyboard focus (`L` while open);
    /// `None` means the reading pane itself has focus, as normal.
    pub link_focus: Option<usize>,
    /// Whether the Brain tab shows the List or the Neighbourhood graph
    /// (`Tab` toggles).
    pub graph_mode: bool,
    /// The graph's centred entry id; `None` until something is selected.
    pub graph_focus: Option<String>,
    /// The last-built Neighbourhood flow, rebuilt by `ensure_graph_flow`
    /// whenever the focus/entries/edges change — `RefCell` because
    /// rendering only ever gets `&TuiState`, never `&mut` (same reason
    /// `FleetState::opened_flow` is one).
    graph_flow: std::cell::RefCell<Option<rataflow::Flow<super::brain_flow::BrainGraphNode, rataflow::StepEdge>>>,
    /// The focus id `graph_flow` was last built for — a change means the
    /// next `ensure_graph_flow` resets the viewport/selection instead of
    /// carrying over the previous (unrelated) neighbourhood's.
    graph_flow_focus: std::cell::RefCell<Option<String>>,
    /// Which graph-mode view is showing (`s` cycles between them).
    pub graph_scope: GraphScope,
    /// Selected row in the Clusters ranked table (`ninox_core::brain::cluster_by_repo`'s
    /// order — count descending, alphabetical ties).
    pub clusters_cursor: usize,
    /// The Sources sub-view (`S`); takes over the tab while `open`.
    pub sources: super::sources::SourcesView,
}

impl BrainView {
    /// A semantic search finished. Searches overlap (an older, slower one
    /// can finish last), so only a result for the current query is kept.
    pub fn semantic_done(&mut self, query: String, ids: Vec<String>) {
        if self.searching.as_deref() == Some(query.as_str()) {
            self.searching = None;
        }
        if query == self.query {
            self.semantic = Some((query, ids));
        }
    }

    /// Indices into `entries`, in the order the Kinds column's filters and
    /// sort would show them; the identity sequence when none are active.
    fn visible_indices(&self) -> Vec<usize> {
        if self.kind_filter.is_none() && self.repo_filter.is_none() && self.tag_filter.is_none() && self.sort == BrainSort::Recent {
            return (0..self.entries.len()).collect();
        }
        let filtered = visible_entries(
            &self.entries,
            &self.edges,
            self.kind_filter.as_deref(),
            self.repo_filter.as_deref(),
            self.tag_filter.as_deref(),
            "",
            self.sort,
        );
        let index: HashMap<&str, usize> = self.entries.iter().enumerate().map(|(i, e)| (e.id.as_str(), i)).collect();
        filtered.into_iter().filter_map(|e| index.get(e.id.as_str()).copied()).collect()
    }

    /// Step `current` to the next (or, going backward, previous) value in
    /// `values`; wraps through `None` ("All") at both ends. Shared by the
    /// kind/repo/tag filter cycles.
    fn cycle_value(current: Option<&str>, values: &[String], forward: bool) -> Option<String> {
        if values.is_empty() {
            return None;
        }
        let len = values.len();
        let cur = current.and_then(|c| values.iter().position(|x| x == c));
        let next = match (cur, forward) {
            (None, true) => Some(0),
            (None, false) => Some(len - 1),
            (Some(i), true) => (i + 1 < len).then_some(i + 1),
            (Some(i), false) => i.checked_sub(1),
        };
        next.map(|i| values[i].clone())
    }

    /// Replace the active Kinds-column selection with the next (or, going
    /// backward, previous) kind in `kinds_sorted_by_count` order; wraps
    /// through `None` ("All entries") at both ends.
    pub fn cycle_kind_filter(&mut self, forward: bool) {
        let kinds: Vec<String> = kinds_sorted_by_count(&self.entries).into_iter().map(|(k, _, _)| k).collect();
        self.kind_filter = Self::cycle_value(self.kind_filter.as_deref(), &kinds, forward);
        self.reclamp_cursor();
    }

    /// Same cycle as `cycle_kind_filter`, over distinct `repos` values
    /// (most-frequent first, then alphabetical — `top_values`'s order).
    pub fn cycle_repo_filter(&mut self, forward: bool) {
        let repos: Vec<String> = top_values(self.entries.iter(), |e| &e.repos, usize::MAX).0.into_iter().map(|(v, _)| v).collect();
        self.repo_filter = Self::cycle_value(self.repo_filter.as_deref(), &repos, forward);
        self.reclamp_cursor();
    }

    /// Same cycle as `cycle_kind_filter`, over distinct `tags` values.
    pub fn cycle_tag_filter(&mut self, forward: bool) {
        let tags: Vec<String> = top_values(self.entries.iter(), |e| &e.tags, usize::MAX).0.into_iter().map(|(v, _)| v).collect();
        self.tag_filter = Self::cycle_value(self.tag_filter.as_deref(), &tags, forward);
        self.reclamp_cursor();
    }

    pub fn cycle_sort(&mut self) {
        self.sort = match self.sort {
            BrainSort::Recent => BrainSort::Az,
            BrainSort::Az => BrainSort::MostLinked,
            BrainSort::MostLinked => BrainSort::Recent,
        };
        self.reclamp_cursor();
    }

    pub fn clear_filters(&mut self) {
        self.kind_filter = None;
        self.repo_filter = None;
        self.tag_filter = None;
        self.reclamp_cursor();
    }

    /// Snapshot the available catalogues, preserving `active_catalogue`
    /// (a reload — e.g. switching tabs away and back — must not silently
    /// reset the cycle back to "All sources").
    pub fn set_catalogues(&mut self, options: Vec<ninox_core::config::CatalogueRef>) {
        self.catalogue_options = options;
    }

    /// Step `active_catalogue` forward: All sources -> catalogue 0 -> ... ->
    /// the last catalogue -> back to All sources.
    pub fn cycle_catalogue(&mut self) {
        if self.catalogue_options.is_empty() {
            return;
        }
        self.active_catalogue = match self.active_catalogue {
            None => Some(0),
            Some(i) if i + 1 < self.catalogue_options.len() => Some(i + 1),
            Some(_) => None,
        };
    }

    /// "All sources" or the active catalogue's own `name`.
    pub fn active_catalogue_label(&self) -> &str {
        match self.active_catalogue.and_then(|i| self.catalogue_options.get(i)) {
            Some(c) => &c.name,
            None => "All sources",
        }
    }

    /// Re-pin the cursor to a valid row after the visible item set changed
    /// underneath it (a filter/sort change) — same reclamp `cycle_grouping`
    /// already does for a grouping change.
    fn reclamp_cursor(&mut self) {
        if self.cursor.id.is_none() {
            self.last_index = 0;
        }
        let items = self.items();
        let i = self.cursor_index(&items);
        self.set_cursor(&items, i);
    }

    /// Resolve `links` (`BrainIndex::links_all()`'s id pairs) against the
    /// current `entries`, for `BrainSort::MostLinked`. Called once per data
    /// load, mirroring the GUI's `App::refresh_brain_edges`.
    pub fn set_edges(&mut self, links: &[(String, String)]) {
        self.edges = resolve_edges(&self.entries, links);
    }

    fn group_keys(&self, e: &BrainEntry) -> Vec<String> {
        match self.grouping {
            Grouping::Flat => Vec::new(),
            Grouping::Type => vec![ninox_core::brain::normalize_kind(&e.entry_type)],
            Grouping::Tag if e.tags.is_empty() => vec![UNTAGGED.to_string()],
            Grouping::Tag => {
                let mut tags: Vec<String> = Vec::new();
                for t in &e.tags {
                    if !tags.contains(t) {
                        tags.push(t.clone());
                    }
                }
                tags
            }
        }
    }

    pub fn is_folded(&self, key: &str) -> bool {
        self.query.is_empty() && !self.expanded.contains(&(self.grouping, key.to_string()))
    }

    /// ripgrep-style search over every entry: whitespace-separated terms
    /// must all match (name, tags, type, path or body); smart case — any
    /// uppercase letter makes the whole query case-sensitive. Ranked by
    /// where the terms hit (name > tag > type > path > body), then name;
    /// semantic-only matches from `semantic` follow, marked `related`.
    pub fn search(&self) -> Vec<Hit> {
        let terms: Vec<&str> = self.query.split_whitespace().collect();
        if terms.is_empty() {
            return Vec::new();
        }
        let sensitive = self.query.chars().any(char::is_uppercase);
        let fold = |t: &str| if sensitive { t.to_string() } else { t.to_lowercase() };
        let terms: Vec<String> = terms.iter().map(|t| fold(t)).collect();
        let mut hits: Vec<Hit> = Vec::new();
        for (idx, e) in self.entries.iter().enumerate() {
            let name = fold(&e.name);
            let tags: Vec<String> = e.tags.iter().map(|t| fold(t)).collect();
            let (kind, path, body) = (fold(&ninox_core::brain::normalize_kind(&e.entry_type)), fold(&e.id), fold(&e.body));
            let mut score = 0;
            let mut all = true;
            for t in &terms {
                let tag_score = tags
                    .iter()
                    .map(|tag| if tag == t { 7 } else if tag.starts_with(t.as_str()) { 6 } else if tag.contains(t.as_str()) { 4 } else { 0 })
                    .max()
                    .unwrap_or(0);
                let best = [word_score(&name, t, 8, 5), tag_score, if kind.contains(t.as_str()) { 3 } else { 0 }, if path.contains(t.as_str()) { 2 } else { 0 }, word_score(&body, t, 2, 1)]
                    .into_iter()
                    .max()
                    .unwrap_or(0);
                if best == 0 {
                    all = false;
                    break;
                }
                score += best;
            }
            if !all {
                continue;
            }
            if terms.len() > 1 && name.contains(&terms.join(" ")) {
                score += 5;
            }
            let line = e.body.lines().enumerate().find(|(_, l)| {
                let l = fold(l);
                terms.iter().any(|t| l.contains(t.as_str()))
            });
            hits.push(Hit { idx, score, line: line.map(|(n, l)| (n + 1, l.trim().to_string())), related: false });
        }
        hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| self.entries[a.idx].name.to_lowercase().cmp(&self.entries[b.idx].name.to_lowercase())));
        if let Some((q, ids)) = &self.semantic {
            if *q == self.query {
                for id in ids {
                    if let Some(idx) = self.entries.iter().position(|e| &e.id == id) {
                        if !hits.iter().any(|h| h.idx == idx) {
                            hits.push(Hit { idx, score: 0, line: None, related: true });
                        }
                    }
                }
            }
        }
        hits
    }

    /// The list as drawn: group headers (sorted, untagged last) with their
    /// entries in query order, folded groups' entries left out.
    pub fn items(&self) -> Vec<Item> {
        if !self.query.trim().is_empty() {
            return self.search().into_iter().map(|h| Item::Entry { group: None, idx: h.idx }).collect();
        }
        let indices = self.visible_indices();
        if self.grouping == Grouping::Flat {
            return indices.into_iter().map(|idx| Item::Entry { group: None, idx }).collect();
        }
        let mut groups: BTreeMap<(bool, String, String), Vec<usize>> = BTreeMap::new();
        for idx in indices {
            let e = &self.entries[idx];
            for key in self.group_keys(e) {
                let last = self.grouping == Grouping::Tag && key == UNTAGGED;
                groups.entry((last, key.to_lowercase(), key)).or_default().push(idx);
            }
        }
        let mut out = Vec::new();
        for ((_, _, key), members) in groups {
            let folded = self.is_folded(&key);
            out.push(Item::Group { key: key.clone(), count: members.len(), folded });
            if !folded {
                out.extend(members.into_iter().map(|idx| Item::Entry { group: Some(key.clone()), idx }));
            }
        }
        out
    }

    pub fn sel_of(&self, item: &Item) -> Sel {
        match item {
            Item::Group { key, .. } => Sel { group: Some(key.clone()), id: None },
            Item::Entry { group, idx } => Sel { group: group.clone(), id: self.entries.get(*idx).map(|e| e.id.clone()) },
        }
    }

    /// The cursor's line in `items`: the exact (group, entry), else the
    /// same entry elsewhere (its tags changed, or the grouping did), else
    /// its group's header, else where it last was.
    pub fn cursor_index(&self, items: &[Item]) -> usize {
        let sels: Vec<Sel> = items.iter().map(|i| self.sel_of(i)).collect();
        let c = &self.cursor;
        sels.iter()
            .position(|s| s == c)
            .or_else(|| c.id.as_ref().and_then(|id| sels.iter().position(|s| s.id.as_ref() == Some(id))))
            .or_else(|| c.group.as_ref().and_then(|g| sels.iter().position(|s| s.id.is_none() && s.group.as_ref() == Some(g))))
            .unwrap_or(self.last_index)
            .min(items.len().saturating_sub(1))
    }

    pub fn set_cursor(&mut self, items: &[Item], i: usize) {
        if let Some(item) = items.get(i) {
            let sel = self.sel_of(item);
            if sel.id != self.cursor.id {
                self.scroll = 0;
            }
            self.cursor = sel;
            self.last_index = i;
        }
    }

    /// Move the cursor `delta` lines; folded groups' entries aren't lines.
    pub fn move_cursor(&mut self, delta: i64) {
        let items = self.items();
        if items.is_empty() {
            return;
        }
        let i = (self.cursor_index(&items) as i64 + delta).clamp(0, items.len() as i64 - 1) as usize;
        self.set_cursor(&items, i);
    }

    pub fn selected_item(&self) -> Option<Item> {
        let items = self.items();
        let i = self.cursor_index(&items);
        items.get(i).cloned()
    }

    pub fn selected_entry(&self) -> Option<&BrainEntry> {
        match self.selected_item()? {
            Item::Entry { idx, .. } => self.entries.get(idx),
            Item::Group { .. } => None,
        }
    }

    /// The group the cursor is in (its header, or the entry's group).
    pub fn selected_group(&self) -> Option<String> {
        match self.selected_item()? {
            Item::Group { key, .. } => Some(key),
            Item::Entry { group, .. } => group,
        }
    }

    /// Fold or unfold the cursor's group; folding from an entry puts the
    /// cursor on the header, so it doesn't vanish.
    pub fn toggle_fold(&mut self) {
        if self.grouping == Grouping::Flat || !self.query.is_empty() {
            return;
        }
        let Some(key) = self.selected_group() else { return };
        let k = (self.grouping, key.clone());
        if self.expanded.remove(&k) {
            self.cursor = Sel { group: Some(key), id: None };
        } else {
            self.expanded.insert(k);
        }
    }

    /// Unfold the groups the cursor's entry now sits in, so an entry whose
    /// tags changed (edited, reloaded) stays visible under its new tag.
    pub fn reveal_cursor(&mut self) {
        let Some(id) = self.cursor.id.clone() else { return };
        let Some(e) = self.entries.iter().find(|e| e.id == id) else { return };
        let keys = self.group_keys(e);
        let grouping = self.grouping;
        if keys.iter().any(|k| self.expanded.contains(&(grouping, k.clone()))) {
            return;
        }
        if let Some(k) = keys.into_iter().next() {
            self.expanded.insert((grouping, k));
        }
    }

    pub fn cycle_grouping(&mut self) {
        self.grouping = self.grouping.next();
        // Keep the entry; its group key means something else now.
        self.cursor.group = None;
        self.reclamp_cursor();
    }

    /// The new-entry template's type and tags, from where the cursor is.
    pub fn template_defaults(&self) -> (String, Vec<String>) {
        let entry_type = match (self.grouping, self.selected_item()) {
            (Grouping::Type, Some(Item::Group { key, .. })) => Some(key),
            _ => self.selected_entry().map(|e| ninox_core::brain::normalize_kind(&e.entry_type)),
        }
        .unwrap_or_else(|| "concepts".to_string());
        let tags = match (self.grouping, self.selected_group()) {
            (Grouping::Tag, Some(t)) if t != UNTAGGED => vec![t],
            _ => Vec::new(),
        };
        (entry_type, tags)
    }

    pub fn name_of(&self, id: &str) -> String {
        self.entries.iter().find(|e| e.id == id).map(|e| e.name.clone()).unwrap_or_else(|| id.to_string())
    }

    /// Apply a `backlinks`/`related`/`outlinks` fetch for `id`, unless the
    /// selection has already moved on by the time it arrives (the fetch
    /// runs off the loop; an older, slower one can finish after a newer
    /// one, same race `semantic_done` guards against for search).
    pub fn set_links(&mut self, id: &str, backlinks: Vec<BrainEntry>, related: Vec<BrainEntry>, outlinks: Vec<BrainEntry>) {
        if self.selected_entry().is_some_and(|e| e.id == id) {
            self.backlinks = backlinks;
            self.related = related;
            self.outlinks = outlinks;
        }
    }

    pub fn has_links(&self) -> bool {
        !self.backlinks.is_empty() || !self.related.is_empty() || !self.outlinks.is_empty()
    }

    /// Flattened (section label, entry) rows in Cited-by/Related/Links-to
    /// order, skipping empty sections — what `link_focus` indexes into.
    pub(crate) fn link_rows(&self) -> Vec<(&'static str, &BrainEntry)> {
        [("Cited by", &self.backlinks), ("Related", &self.related), ("Links to", &self.outlinks)]
            .into_iter()
            .flat_map(|(label, entries)| entries.iter().map(move |e| (label, e)))
            .collect()
    }

    /// Enters link-focus mode at the first row, if there's anything to
    /// focus; a no-op otherwise (so `L` on an entry with no links does
    /// nothing rather than focusing an empty panel).
    pub fn enter_link_focus(&mut self) {
        if self.has_links() {
            self.link_focus = Some(0);
        }
    }

    pub fn exit_link_focus(&mut self) {
        self.link_focus = None;
    }

    pub fn move_link_focus(&mut self, delta: i64) {
        let len = self.link_rows().len();
        if len == 0 {
            self.link_focus = None;
            return;
        }
        let cur = self.link_focus.unwrap_or(0) as i64;
        self.link_focus = Some((cur + delta).clamp(0, len as i64 - 1) as usize);
    }

    /// Re-selects the entry the link focus currently points at. Clears any
    /// active kind/repo/tag filter first, so the jump target is never
    /// hidden behind a filter the user forgot was on — a "jump to
    /// definition" should always land somewhere visible.
    pub fn jump_to_focused_link(&mut self) {
        let Some(i) = self.link_focus else { return };
        let Some(id) = self.link_rows().get(i).map(|(_, e)| e.id.clone()) else { return };
        self.cursor = Sel { group: None, id: Some(id) };
        self.kind_filter = None;
        self.repo_filter = None;
        self.tag_filter = None;
        self.link_focus = None;
        self.scroll = 0;
        // The target may sit in a still-folded group (Tag/Type grouping
        // folds groups by default) — items() excludes a folded group's
        // entries entirely, so without this the jump silently lands
        // nowhere visible even though `cursor.id` is set correctly.
        self.reveal_cursor();
        self.reclamp_cursor();
    }

    /// Switches between the List and Neighbourhood-graph views (`Tab`).
    /// Entering graph mode with no prior focus seeds it from the entry
    /// currently selected in the list; an existing focus (from a previous
    /// visit, or a refocus) is left alone.
    pub fn toggle_graph_mode(&mut self) {
        self.graph_mode = !self.graph_mode;
        if self.graph_mode && self.graph_focus.is_none() {
            self.graph_focus = self.selected_entry().map(|e| e.id.clone());
        }
    }

    /// Rebuilds `graph_flow` for the current `graph_focus`, if any,
    /// carrying over the previous viewport/selection when the focus hasn't
    /// changed (same race-free rebuild-every-render pattern as
    /// `FleetState::ensure_opened_flow`).
    pub fn ensure_graph_flow(&self) {
        let Some(focus_id) = self.graph_focus.clone() else {
            *self.graph_flow.borrow_mut() = None;
            *self.graph_flow_focus.borrow_mut() = None;
            return;
        };
        let Some(focus_idx) = self.entries.iter().position(|e| e.id == focus_id) else {
            *self.graph_flow.borrow_mut() = None;
            return;
        };
        let Some(mut flow) = super::brain_flow::build_neighbourhood_flow(&self.entries, &self.edges, focus_idx) else {
            *self.graph_flow.borrow_mut() = None;
            *self.graph_flow_focus.borrow_mut() = Some(focus_id);
            return;
        };
        let same_focus = self.graph_flow_focus.borrow().as_deref() == Some(focus_id.as_str());
        if same_focus {
            if let Some(old) = self.graph_flow.borrow().as_ref() {
                super::island_flow::carry_over_viewport_and_selection(old, &mut flow);
            }
        }
        // A freshly built `Flow` starts with no selection at all — without
        // this, the first arrow key just establishes one instead of moving
        // it anywhere (caught live, not by `build_neighbourhood_flow`'s own
        // tests, which never exercise `apply`/selection).
        if flow.first_selected_node_id().is_none() {
            flow.select_node(&focus_id);
        }
        *self.graph_flow.borrow_mut() = Some(flow);
        *self.graph_flow_focus.borrow_mut() = Some(focus_id);
    }

    pub fn graph_flow_mut(&self) -> std::cell::RefMut<'_, Option<rataflow::Flow<super::brain_flow::BrainGraphNode, rataflow::StepEdge>>> {
        self.graph_flow.borrow_mut()
    }

    pub fn graph_selected_id(&self) -> Option<String> {
        self.graph_flow.borrow().as_ref().and_then(|f| f.first_selected_node_id())
    }

    pub fn apply_graph_action(&self, action: rataflow::FlowAction) {
        if let Some(flow) = self.graph_flow.borrow_mut().as_mut() {
            flow.apply(action);
        }
    }

    /// Re-centres the graph on `id` (Enter on a selected entry node) — a
    /// no-op for the "+N more" sentinel, which isn't a real entry.
    pub fn refocus_graph(&mut self, id: &str) {
        if id != super::brain_flow::MORE_NODE_ID {
            self.graph_focus = Some(id.to_string());
        }
    }

    /// Selects `id` directly (not via cursor movement): sets the cursor,
    /// reveals its group if still folded, and reclamps — used by "open in
    /// list" (`o` in the graph view).
    pub fn select_entry(&mut self, id: &str) {
        self.cursor = Sel { group: None, id: Some(id.to_string()) };
        self.reveal_cursor();
        self.reclamp_cursor();
    }

    pub fn cycle_graph_scope(&mut self) {
        self.graph_scope = self.graph_scope.next();
        self.clusters_cursor = 0;
    }

    /// Clusters scope's ranked table — one row per repo, count descending.
    pub fn clusters(&self) -> Vec<ninox_core::brain::ClusterBubble> {
        ninox_core::brain::cluster_by_repo(&self.entries)
    }

    pub fn move_clusters_cursor(&mut self, delta: i64) {
        let len = self.clusters().len();
        if len == 0 {
            self.clusters_cursor = 0;
            return;
        }
        self.clusters_cursor = (self.clusters_cursor as i64 + delta).clamp(0, len as i64 - 1) as usize;
    }

    /// The repo label at `clusters_cursor`, or `None` if the table is empty
    /// or the cursor has drifted past its end (e.g. the brain shrank).
    pub fn selected_cluster_repo(&self) -> Option<String> {
        self.clusters().get(self.clusters_cursor).map(|c| c.label.clone())
    }
}


#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entry(id: &str, ty: &str, tags: &[&str]) -> BrainEntry {
        BrainEntry {
            id: id.into(),
            entry_type: ty.into(),
            name: id.trim_end_matches(".md").rsplit('/').next().unwrap().into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            repos: vec![],
            updated: None,
            body: String::new(),
            source: String::new(),
        }
    }

    pub(crate) fn sample() -> BrainView {
        BrainView {
            entries: vec![
                entry("concepts/ptyd.md", "concepts", &["tmux", "runtime"]),
                entry("concepts/loose.md", "concepts", &[]),
                entry("errors/socket.md", "errors", &["tmux"]),
                entry("patterns/Alpha.md", "patterns", &["Rust"]),
            ],
            ..Default::default()
        }
        .expanded_all()
    }

    impl BrainView {
        fn expanded_all(mut self) -> Self {
            for g in [Grouping::Tag, Grouping::Type] {
                let mut probe = BrainView { entries: self.entries.clone(), grouping: g, ..Default::default() };
                probe.expanded.clear();
                for item in probe.items() {
                    if let Item::Group { key, .. } = item {
                        self.expanded.insert((g, key));
                    }
                }
            }
            self
        }
    }

    fn lines(b: &BrainView) -> Vec<String> {
        b.items()
            .iter()
            .map(|i| match i {
                Item::Group { key, count, folded } => format!("{}{key} ({count})", if *folded { "▸" } else { "▾" }),
                Item::Entry { idx, .. } => format!("  {}", b.entries[*idx].name),
            })
            .collect()
    }

    #[test]
    fn groups_by_tag_with_multi_tag_entries_under_each_and_untagged_last() {
        let b = sample();
        assert_eq!(
            lines(&b),
            ["▾runtime (1)", "  ptyd", "▾Rust (1)", "  Alpha", "▾tmux (2)", "  ptyd", "  socket", "▾untagged (1)", "  loose"]
        );
    }

    #[test]
    fn groups_by_type_and_flat() {
        let mut b = sample();
        b.grouping = Grouping::Type;
        assert_eq!(lines(&b), ["▾concepts (2)", "  ptyd", "  loose", "▾errors (1)", "  socket", "▾patterns (1)", "  Alpha"]);
        b.grouping = Grouping::Flat;
        assert_eq!(lines(&b), ["  ptyd", "  loose", "  socket", "  Alpha"]);
    }

    #[test]
    fn type_grouping_merges_singular_and_plural_kind_spellings() {
        let b = BrainView {
            entries: vec![
                entry("decisions/one.md", "decision", &[]),
                entry("decisions/two.md", "decisions", &[]),
                entry("decisions/three.md", "Decision", &[]),
            ],
            grouping: Grouping::Type,
            ..Default::default()
        }
        .expanded_all();
        assert_eq!(lines(&b), ["▾decisions (3)", "  one", "  two", "  three"], "decision/decisions/Decision all land in one group");
    }

    #[test]
    fn the_cursor_keeps_its_tag_copy_and_skips_folded_entries() {
        let mut b = sample();
        b.move_cursor(4); // tmux header
        b.move_cursor(1);
        assert_eq!(b.cursor, Sel { group: Some("tmux".into()), id: Some("concepts/ptyd.md".into()) });
        assert_eq!(b.cursor_index(&b.items()), 5, "the tmux copy, not the runtime one");
        b.toggle_fold();
        assert_eq!(b.cursor, Sel { group: Some("tmux".into()), id: None }, "folding from an entry lands on its header");
        b.move_cursor(1);
        assert_eq!(b.cursor.group.as_deref(), Some(UNTAGGED), "j skips the folded entries");
        b.move_cursor(-2);
        assert_eq!(b.selected_entry().map(|e| e.name.as_str()), Some("Alpha"));
    }

    #[test]
    fn groups_start_folded_until_expanded() {
        let mut b = BrainView { entries: sample().entries, ..Default::default() };
        assert!(lines(&b).iter().all(|l| l.starts_with('▸')), "{:?}", lines(&b));
        b.move_cursor(0);
        b.toggle_fold();
        assert!(lines(&b)[0].starts_with('▾'), "Space unfolds the group under the cursor");
    }

    #[test]
    fn search_is_a_ranked_flat_list_with_word_prefixes_and_smart_case() {
        let mut b = BrainView { entries: sample().entries, ..Default::default() };
        b.entries[2].body = "first line\nthe Unix socket path is fixed\n".into();
        b.query = "sock".into();
        let hits = b.search();
        assert_eq!(b.entries[hits[0].idx].id, "errors/socket.md", "a word-prefix of the name wins");
        assert_eq!(lines(&b), ["  socket"], "searching ignores folds and grouping");
        b.query = "unix sock".into();
        let hits = b.search();
        assert_eq!(hits.len(), 1, "every term must match");
        assert_eq!(hits[0].line, Some((2, "the Unix socket path is fixed".into())), "rg-style matching line");
        b.query = "Unix".into();
        assert_eq!(b.search().len(), 1, "uppercase makes it case-sensitive");
        b.query = "UNIX".into();
        assert!(b.search().is_empty());
        b.query = "tmux".into();
        assert_eq!(b.search().len(), 2, "tags match");
    }

    #[test]
    fn semantic_matches_follow_text_hits_for_the_same_query() {
        let mut b = BrainView { entries: sample().entries, ..Default::default() };
        b.query = "socket".into();
        b.semantic = Some(("socket".into(), vec!["concepts/ptyd.md".into(), "errors/socket.md".into()]));
        let hits = b.search();
        assert_eq!(hits.iter().map(|h| (b.entries[h.idx].id.as_str(), h.related)).collect::<Vec<_>>(), [("errors/socket.md", false), ("concepts/ptyd.md", true)]);
        b.query = "sock".into();
        assert!(b.search().iter().all(|h| !h.related), "stale semantic results are ignored");
    }

    #[test]
    fn a_slow_older_semantic_search_never_replaces_the_current_one() {
        let mut b = BrainView { entries: sample().entries, ..Default::default() };
        b.query = "socket".into();
        b.searching = Some("socket".into());
        b.semantic_done("socket".into(), vec!["concepts/ptyd.md".into()]);
        assert!(b.searching.is_none());
        b.semantic_done("sock".into(), vec![]);
        assert_eq!(b.semantic, Some(("socket".into(), vec!["concepts/ptyd.md".into()])));
    }

    #[test]
    fn the_cursor_follows_an_entry_whose_tags_changed_or_vanished() {
        let mut b = sample();
        b.cursor = Sel { group: Some("Rust".into()), id: Some("patterns/Alpha.md".into()) };
        b.entries[3].tags = vec!["zig".into()];
        b.reveal_cursor();
        assert_eq!(b.selected_entry().map(|e| e.id.as_str()), Some("patterns/Alpha.md"));
        b.last_index = 3;
        b.entries.remove(3);
        assert!(b.selected_item().is_some(), "a deleted entry leaves the cursor near where it was");
        b.cycle_grouping();
        assert_eq!(b.grouping, Grouping::Type);
    }

    #[test]
    fn template_defaults_follow_the_cursor() {
        let mut b = sample();
        b.move_cursor(4);
        assert_eq!(b.template_defaults(), ("concepts".into(), vec!["tmux".into()]));
        b.move_cursor(100);
        assert_eq!(b.template_defaults(), ("concepts".into(), vec![]), "untagged is not a tag");
    }

    #[test]
    fn kind_filter_narrows_the_flat_list() {
        let mut b = sample();
        b.grouping = Grouping::Flat;
        b.kind_filter = Some("errors".to_string());
        assert_eq!(lines(&b), ["  socket"]);
    }

    #[test]
    fn repo_and_tag_filters_combine_with_kind() {
        let mut b = sample();
        b.grouping = Grouping::Flat;
        b.entries[0].repos = vec!["ninox".to_string()]; // ptyd
        b.kind_filter = Some("concepts".to_string());
        b.repo_filter = Some("ninox".to_string());
        assert_eq!(lines(&b), ["  ptyd"]);
    }

    #[test]
    fn filtering_out_the_selected_entry_leaves_a_valid_cursor() {
        let mut b = sample();
        b.grouping = Grouping::Flat;
        b.move_cursor(0); // ptyd (concepts)
        b.kind_filter = Some("errors".to_string());
        let items = b.items();
        assert!(!items.is_empty());
        assert!(b.cursor_index(&items) < items.len());
    }

    #[test]
    fn cycle_sort_wraps_recent_az_mostlinked() {
        let mut b = BrainView::default();
        assert_eq!(b.sort, BrainSort::Recent);
        b.cycle_sort();
        assert_eq!(b.sort, BrainSort::Az);
        b.cycle_sort();
        assert_eq!(b.sort, BrainSort::MostLinked);
        b.cycle_sort();
        assert_eq!(b.sort, BrainSort::Recent);
    }

    #[test]
    fn cycle_kind_filter_wraps_through_none_and_every_kind() {
        let mut b = sample(); // kinds: concepts, errors, patterns
        assert_eq!(b.kind_filter, None);
        b.cycle_kind_filter(true);
        assert_eq!(b.kind_filter.as_deref(), Some("concepts"));
        b.cycle_kind_filter(true);
        assert_eq!(b.kind_filter.as_deref(), Some("errors"));
        b.cycle_kind_filter(true);
        assert_eq!(b.kind_filter.as_deref(), Some("patterns"));
        b.cycle_kind_filter(true);
        assert_eq!(b.kind_filter, None, "wraps back to All");
    }

    #[test]
    fn cycle_kind_filter_backward_wraps_the_other_way() {
        let mut b = sample();
        b.cycle_kind_filter(false);
        assert_eq!(b.kind_filter.as_deref(), Some("patterns"), "backward from All lands on the last kind");
    }

    #[test]
    fn cycle_repo_filter_wraps_through_none_and_every_repo() {
        let mut b = sample();
        b.entries[0].repos = vec!["ninox".to_string()]; // ptyd (concepts)
        b.entries[2].repos = vec!["bellhop".to_string()]; // socket (errors)
        assert_eq!(b.repo_filter, None);
        b.cycle_repo_filter(true);
        assert_eq!(b.repo_filter.as_deref(), Some("bellhop"), "tied count, alphabetical first");
        b.cycle_repo_filter(true);
        assert_eq!(b.repo_filter.as_deref(), Some("ninox"));
        b.cycle_repo_filter(true);
        assert_eq!(b.repo_filter, None, "wraps back to All");
        b.cycle_repo_filter(false);
        assert_eq!(b.repo_filter.as_deref(), Some("ninox"), "backward from All lands on the last repo");
    }

    #[test]
    fn cycle_tag_filter_wraps_through_none_and_every_tag_by_count_then_alpha() {
        let mut b = sample(); // tags: tmux x2 (ptyd, socket), runtime x1 (ptyd), Rust x1 (Alpha)
        assert_eq!(b.tag_filter, None);
        b.cycle_tag_filter(true);
        assert_eq!(b.tag_filter.as_deref(), Some("tmux"), "highest count first");
        b.cycle_tag_filter(true);
        assert_eq!(b.tag_filter.as_deref(), Some("Rust"), "tied count, alphabetical (byte-order) next");
        b.cycle_tag_filter(true);
        assert_eq!(b.tag_filter.as_deref(), Some("runtime"));
        b.cycle_tag_filter(true);
        assert_eq!(b.tag_filter, None, "wraps back to All");
    }

    #[test]
    fn clear_filters_resets_all_three() {
        let mut b = sample();
        b.kind_filter = Some("errors".into());
        b.repo_filter = Some("x".into());
        b.tag_filter = Some("y".into());
        b.clear_filters();
        assert_eq!((b.kind_filter, b.repo_filter, b.tag_filter), (None, None, None));
    }

    fn catalogue(name: &str) -> ninox_core::config::CatalogueRef {
        ninox_core::config::CatalogueRef {
            name: name.to_string(),
            path: PathBuf::from(format!("/brains/{name}")),
            remote: None,
            endpoint: None,
            region: None,
            cache_ttl_secs: None,
            enabled: true,
            cadence: "manual".to_string(),
            access: "read_write".to_string(),
            write_rules_override: None,
        }
    }

    #[test]
    fn cycle_catalogue_wraps_through_all_sources_and_each_configured_catalogue() {
        let mut b = sample();
        b.set_catalogues(vec![catalogue("default"), catalogue("team")]);
        assert_eq!(b.active_catalogue, None, "starts on All sources");
        b.cycle_catalogue();
        assert_eq!(b.active_catalogue, Some(0));
        b.cycle_catalogue();
        assert_eq!(b.active_catalogue, Some(1));
        b.cycle_catalogue();
        assert_eq!(b.active_catalogue, None, "wraps back to All sources");
    }

    #[test]
    fn cycle_catalogue_is_a_two_state_toggle_with_only_the_default_catalogue() {
        let mut b = sample();
        b.set_catalogues(vec![catalogue("default")]);
        b.cycle_catalogue();
        assert_eq!(b.active_catalogue, Some(0));
        b.cycle_catalogue();
        assert_eq!(b.active_catalogue, None, "All sources and the lone default still alternate");
    }

    #[test]
    fn active_catalogue_label_names_all_sources_or_the_catalogues_own_name() {
        let mut b = sample();
        b.set_catalogues(vec![catalogue("default"), catalogue("team")]);
        assert_eq!(b.active_catalogue_label(), "All sources");
        b.cycle_catalogue();
        assert_eq!(b.active_catalogue_label(), "default");
        b.cycle_catalogue();
        assert_eq!(b.active_catalogue_label(), "team");
    }

    #[test]
    fn set_catalogues_preserves_the_active_selection_across_a_reload() {
        let mut b = sample();
        b.set_catalogues(vec![catalogue("default"), catalogue("team")]);
        b.cycle_catalogue();
        assert_eq!(b.active_catalogue, Some(0));
        b.set_catalogues(vec![catalogue("default"), catalogue("team")]);
        assert_eq!(b.active_catalogue, Some(0), "a reload (e.g. switching tabs away and back) doesn't reset the cycle");
    }

    #[test]
    fn set_edges_resolves_links_against_entries() {
        let mut b = sample();
        let links = vec![("concepts/ptyd.md".to_string(), "errors/socket.md".to_string())];
        b.set_edges(&links);
        assert_eq!(b.edges, vec![(0, 2)]);
    }

    fn with_entry(id: &str) -> BrainEntry {
        entry(id, "concepts", &[])
    }

    #[test]
    fn set_links_is_dropped_when_the_selection_moved_on() {
        let mut b = sample();
        b.cursor = Sel { group: Some("runtime".into()), id: Some("concepts/ptyd.md".into()) };
        b.set_links("concepts/ptyd.md", vec![with_entry("a.md")], vec![], vec![]);
        assert_eq!(b.backlinks.len(), 1);
        b.set_links("errors/socket.md", vec![with_entry("stale.md")], vec![], vec![]);
        assert_eq!(b.backlinks.len(), 1, "a fetch for an id that isn't selected anymore is dropped");
    }

    #[test]
    fn enter_link_focus_is_a_noop_with_no_links() {
        let mut b = sample();
        b.enter_link_focus();
        assert_eq!(b.link_focus, None);
    }

    #[test]
    fn link_focus_cycles_across_all_three_sections_in_order() {
        let mut b = sample();
        b.cursor = Sel { group: Some("runtime".into()), id: Some("concepts/ptyd.md".into()) };
        b.set_links("concepts/ptyd.md", vec![with_entry("back.md")], vec![with_entry("rel1.md"), with_entry("rel2.md")], vec![with_entry("out.md")]);
        b.enter_link_focus();
        assert_eq!(b.link_focus, Some(0));
        b.move_link_focus(1);
        assert_eq!(b.link_focus, Some(1));
        b.move_link_focus(1);
        assert_eq!(b.link_focus, Some(2));
        b.move_link_focus(1);
        assert_eq!(b.link_focus, Some(3), "clamped at the last row (back, rel1, rel2, out)");
        b.move_link_focus(-10);
        assert_eq!(b.link_focus, Some(0), "clamped at the first row");
    }

    #[test]
    fn jump_to_focused_link_selects_that_entry_and_clears_filters() {
        let mut b = sample();
        b.cursor = Sel { group: Some("runtime".into()), id: Some("concepts/ptyd.md".into()) };
        b.set_links("concepts/ptyd.md", vec![], vec![], vec![with_entry("patterns/Alpha.md")]);
        b.enter_link_focus();
        b.kind_filter = Some("errors".to_string()); // simulates a filter active when the jump fires
        b.jump_to_focused_link();
        assert_eq!(b.selected_entry().map(|e| e.id.as_str()), Some("patterns/Alpha.md"));
        assert_eq!(b.kind_filter, None, "jump never lands behind a stale filter");
        assert_eq!(b.link_focus, None);
    }

    #[test]
    fn jump_to_focused_link_reveals_the_target_even_in_a_still_folded_group() {
        // Groups start folded by default (sample()'s own fixture pre-expands
        // everything, which is exactly how this bug slipped past the other
        // jump test) — a real brain only expands what the user opened.
        let mut b = BrainView { entries: sample().entries, grouping: Grouping::Type, ..Default::default() };
        b.expanded.insert((Grouping::Type, "concepts".to_string()));
        b.cursor = Sel { group: Some("concepts".into()), id: Some("concepts/ptyd.md".into()) };
        b.set_links("concepts/ptyd.md", vec![with_entry("errors/socket.md")], vec![], vec![]);
        b.enter_link_focus();
        b.jump_to_focused_link();
        assert_eq!(
            b.selected_entry().map(|e| e.id.as_str()),
            Some("errors/socket.md"),
            "the jump reveals \"errors\", not just \"concepts\" (already open)"
        );
    }

    #[test]
    fn exit_link_focus_clears_it_without_changing_selection() {
        let mut b = sample();
        b.set_links("concepts/ptyd.md", vec![with_entry("back.md")], vec![], vec![]);
        b.enter_link_focus();
        b.exit_link_focus();
        assert_eq!(b.link_focus, None);
    }

    #[test]
    fn toggle_graph_mode_seeds_focus_from_the_current_selection() {
        let mut b = sample();
        b.cursor = Sel { group: Some("runtime".into()), id: Some("concepts/ptyd.md".into()) };
        assert!(!b.graph_mode);
        b.toggle_graph_mode();
        assert!(b.graph_mode);
        assert_eq!(b.graph_focus.as_deref(), Some("concepts/ptyd.md"));
        b.toggle_graph_mode();
        assert!(!b.graph_mode);
        assert_eq!(b.graph_focus.as_deref(), Some("concepts/ptyd.md"), "re-entering keeps the last focus, doesn't reset it");
    }

    #[test]
    fn toggle_graph_mode_keeps_an_existing_focus_instead_of_resetting_it() {
        let mut b = sample();
        b.graph_focus = Some("errors/socket.md".into());
        b.cursor = Sel { group: Some("runtime".into()), id: Some("concepts/ptyd.md".into()) };
        b.toggle_graph_mode();
        assert_eq!(b.graph_focus.as_deref(), Some("errors/socket.md"), "an already-set focus isn't clobbered by the current list selection");
    }

    #[test]
    fn ensure_graph_flow_builds_from_the_focus_entry() {
        let mut b = sample();
        b.graph_focus = Some("concepts/ptyd.md".into());
        b.set_edges(&[("concepts/ptyd.md".to_string(), "errors/socket.md".to_string())]);
        b.ensure_graph_flow();
        assert!(b.graph_flow_mut().as_ref().unwrap().node("concepts/ptyd.md").is_some());
        assert!(b.graph_flow_mut().as_ref().unwrap().node("errors/socket.md").is_some());
    }

    #[test]
    fn ensure_graph_flow_selects_the_focus_by_default_so_arrows_navigate_immediately() {
        // Regression (caught live, not by a unit test): a freshly built
        // flow starts with no selection at all, so the first arrow key
        // just established a selection instead of moving it — the focus
        // must be pre-selected the moment the flow is built.
        let mut b = sample();
        b.graph_focus = Some("concepts/ptyd.md".into());
        b.ensure_graph_flow();
        assert_eq!(b.graph_flow_mut().as_ref().unwrap().first_selected_node_id().as_deref(), Some("concepts/ptyd.md"));
    }

    #[test]
    fn ensure_graph_flow_does_not_clobber_a_carried_over_selection() {
        let mut b = sample();
        b.graph_focus = Some("concepts/ptyd.md".into());
        b.set_edges(&[("concepts/ptyd.md".to_string(), "errors/socket.md".to_string())]);
        b.ensure_graph_flow();
        b.graph_flow_mut().as_mut().unwrap().select_node("errors/socket.md");
        b.ensure_graph_flow(); // same focus: rebuild should carry the selection over, not reset it to ptyd
        assert_eq!(b.graph_flow_mut().as_ref().unwrap().first_selected_node_id().as_deref(), Some("errors/socket.md"));
    }

    #[test]
    fn ensure_graph_flow_clears_when_there_is_no_focus() {
        let mut b = sample();
        b.graph_focus = None;
        b.ensure_graph_flow();
        assert!(b.graph_flow_mut().is_none());
    }

    #[test]
    fn refocus_graph_moves_the_focus_but_ignores_the_more_sentinel() {
        let mut b = sample();
        b.graph_focus = Some("concepts/ptyd.md".into());
        b.refocus_graph("errors/socket.md");
        assert_eq!(b.graph_focus.as_deref(), Some("errors/socket.md"));
        b.refocus_graph(super::super::brain_flow::MORE_NODE_ID);
        assert_eq!(b.graph_focus.as_deref(), Some("errors/socket.md"), "the +N more row isn't a real entry to focus on");
    }

    #[test]
    fn cycle_graph_scope_toggles_neighbourhood_and_clusters() {
        let mut b = sample();
        assert_eq!(b.graph_scope, GraphScope::Neighbourhood);
        b.cycle_graph_scope();
        assert_eq!(b.graph_scope, GraphScope::Clusters);
        b.cycle_graph_scope();
        assert_eq!(b.graph_scope, GraphScope::Neighbourhood);
    }

    #[test]
    fn clusters_ranks_by_repo_count() {
        let mut b = sample();
        b.entries[0].repos = vec!["ninox".to_string()];
        b.entries[1].repos = vec!["ninox".to_string()];
        b.entries[2].repos = vec!["bellhop".to_string()];
        let clusters = b.clusters();
        assert_eq!(clusters[0].label, "ninox");
        assert_eq!(clusters[0].count, 2);
    }

    #[test]
    fn move_clusters_cursor_clamps_within_bounds() {
        let mut b = sample();
        b.entries[0].repos = vec!["ninox".to_string()];
        b.entries[2].repos = vec!["bellhop".to_string()];
        let len = b.clusters().len();
        assert_eq!(b.clusters_cursor, 0);
        b.move_clusters_cursor(-1);
        assert_eq!(b.clusters_cursor, 0, "doesn't go below zero");
        for _ in 0..10 {
            b.move_clusters_cursor(1);
        }
        assert_eq!(b.clusters_cursor, len - 1, "clamps at the last row");
    }

    #[test]
    fn selected_cluster_repo_returns_the_label_at_the_cursor() {
        let mut b = sample();
        b.entries[0].repos = vec!["ninox".to_string()];
        b.entries[1].repos = vec!["ninox".to_string()];
        b.entries[2].repos = vec!["bellhop".to_string()];
        b.entries[3].repos = vec!["archipelago".to_string()];
        assert_eq!(b.selected_cluster_repo().as_deref(), Some("ninox"), "highest count first");
        b.clusters_cursor = 10;
        assert_eq!(b.selected_cluster_repo(), None);
    }
}
