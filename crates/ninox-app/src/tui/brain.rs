//! The Brain tab's model: entries grouped into a foldable tree (by tag, by
//! type, or flat), cursor identity that survives reloads, and the edit /
//! new / delete jobs. Jobs write through the same functions as
//! `ninox brain add` (`write_brain_entry`), then sync a remote-backed brain
//! and rebuild the index, like `ninox brain index`.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use ninox_core::config::AppConfig;
use ninox_core::BrainEntry;

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

    fn group_keys(&self, e: &BrainEntry) -> Vec<String> {
        match self.grouping {
            Grouping::Flat => Vec::new(),
            Grouping::Type => vec![e.entry_type.clone()],
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
            let (kind, path, body) = (fold(&e.entry_type), fold(&e.id), fold(&e.body));
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
        if self.grouping == Grouping::Flat {
            return (0..self.entries.len()).map(|idx| Item::Entry { group: None, idx }).collect();
        }
        let mut groups: BTreeMap<(bool, String, String), Vec<usize>> = BTreeMap::new();
        for (idx, e) in self.entries.iter().enumerate() {
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
        if self.cursor.id.is_none() {
            self.last_index = 0;
        }
        let items = self.items();
        let i = self.cursor_index(&items);
        self.set_cursor(&items, i);
    }

    /// The new-entry template's type and tags, from where the cursor is.
    pub fn template_defaults(&self) -> (String, Vec<String>) {
        let entry_type = match (self.grouping, self.selected_item()) {
            (Grouping::Type, Some(Item::Group { key, .. })) => Some(key),
            _ => self.selected_entry().map(|e| e.entry_type.clone()),
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
}

// ── new entries ─────────────────────────────────────────────────────────────

pub fn template(entry_type: &str, tags: &[String]) -> String {
    format!("---\nname: \ntype: {entry_type}\ntags: [{}]\n---\n\n", tags.join(", "))
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.trim().chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "entry".into() } else { out }
}

/// What the editor left in a new-entry template: `Ok(None)` when it is
/// untouched or empty (cancelled), else the entry's path under the brain
/// (`<type>/<name>.md`, numbered past any existing file) and its content.
pub fn entry_from_edit(brain: &Path, template: &str, edited: &str) -> Result<Option<(String, String)>, String> {
    if edited.trim().is_empty() || edited.trim() == template.trim() {
        return Ok(None);
    }
    let heading = || {
        edited.lines().find_map(|l| l.strip_prefix("# ")).map(str::trim).filter(|h| !h.is_empty()).map(str::to_string)
    };
    let Some(name) = ninox_core::brain::frontmatter_str(edited, "name").or_else(heading) else {
        return Err("the new entry has no name: (or # heading), so it was not saved".into());
    };
    let dir = slug(&ninox_core::brain::frontmatter_str(edited, "type").unwrap_or_else(|| "note".into()));
    let base = slug(&name);
    let mut id = format!("{dir}/{base}.md");
    let mut n = 2;
    while brain.join(&id).exists() {
        id = format!("{dir}/{base}-{n}.md");
        n += 1;
    }
    let mut content = edited.to_string();
    if !content.ends_with('\n') {
        content.push('\n');
    }
    Ok(Some((id, content)))
}

/// Where an entry's markdown lives; `Err` says why it can't be edited.
pub fn source_file(brain: &Path, id: &str) -> Result<PathBuf, String> {
    let rel = Path::new(id);
    if rel.is_absolute() || rel.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err(format!("{id} is outside the brain directory"));
    }
    let path = brain.join(rel);
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!("{id} has no markdown file in {} (the index is stale: `ninox brain index` rebuilds it)", brain.display()))
    }
}

// ── index jobs ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum Job {
    /// The file was edited in place.
    Reindex { id: String },
    Write { id: String, content: String },
    Delete { id: String },
}

impl Job {
    pub fn id(&self) -> &str {
        match self {
            Self::Reindex { id } | Self::Write { id, .. } | Self::Delete { id } => id,
        }
    }

    pub fn verb(&self) -> &'static str {
        match self {
            Self::Reindex { .. } => "saved",
            Self::Write { .. } => "added",
            Self::Delete { .. } => "deleted",
        }
    }
}

#[derive(Debug)]
pub struct JobDone {
    pub job: Job,
    /// Ok: what happened, for the notice.
    pub result: Result<String, String>,
}

/// Apply `job` to the brain at `brain`, sync it with its remote if it has
/// one (failures keep the local change), and rebuild the index. Runs off
/// the UI loop: the rebuild may embed.
pub async fn run_job(brain: PathBuf, config: AppConfig, job: Job, embed: bool) -> Result<String, String> {
    if let Err(e) = ninox_core::brain_sync::ensure_sync_toml(&config, &brain) {
        tracing::warn!("brain: failed to materialize .sync.toml: {e}");
    }
    match &job {
        Job::Reindex { .. } => {}
        Job::Write { id, content } => crate::write_brain_entry(&brain, id, content).map_err(|e| format!("write {id}: {e}"))?,
        Job::Delete { id } => {
            let path = source_file(&brain, id)?;
            std::fs::remove_file(&path).map_err(|e| format!("delete {}: {e}", path.display()))?;
        }
    }
    let remote = match ninox_core::brain_sync::BrainSync::for_brain(&brain).await {
        Ok(None) => String::new(),
        Ok(Some(sync)) => match sync.sync().await {
            Ok(r) => format!(" · synced (pushed {}, pulled {})", r.pushed, r.pulled),
            Err(e) => format!(" · remote sync failed, kept local: {e}"),
        },
        Err(e) => format!(" · remote unavailable, kept local: {e}"),
    };
    let stats = tokio::task::spawn_blocking(move || -> anyhow::Result<ninox_core::brain::RebuildStats> {
        let embedder = if embed { crate::build_embedder(false) } else { None };
        ninox_core::BrainIndex::open(&brain)?.rebuild(embedder.as_deref())
    })
    .await
    .map_err(|e| format!("index task failed: {e}"))?
    .map_err(|e| format!("reindex failed: {e}"))?;
    Ok(format!("{} {} · indexed {} entries{remote}", job.verb(), job.id(), stats.indexed))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn entry(id: &str, ty: &str, tags: &[&str]) -> BrainEntry {
        BrainEntry {
            id: id.into(),
            entry_type: ty.into(),
            name: id.trim_end_matches(".md").rsplit('/').next().unwrap().into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            repos: vec![],
            updated: None,
            body: String::new(),
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
    fn an_untouched_or_empty_template_cancels() {
        let dir = tempfile::tempdir().unwrap();
        let t = template("concepts", &["tmux".into()]);
        assert_eq!(t, "---\nname: \ntype: concepts\ntags: [tmux]\n---\n\n");
        assert_eq!(entry_from_edit(dir.path(), &t, &t), Ok(None));
        assert_eq!(entry_from_edit(dir.path(), &t, "  \n"), Ok(None));
        assert!(entry_from_edit(dir.path(), &t, &format!("{t}some text")).is_err(), "no name, not saved");
    }

    #[test]
    fn a_filled_template_gets_a_path_by_type_and_name() {
        let dir = tempfile::tempdir().unwrap();
        let t = template("concepts", &[]);
        let edited = t.replace("name: ", "name: Socket Paths!").replace("tags: []", "tags: [tmux]") + "body";
        let (id, content) = entry_from_edit(dir.path(), &t, &edited).unwrap().unwrap();
        assert_eq!(id, "concepts/socket-paths.md");
        assert!(content.ends_with("body\n"));
        std::fs::create_dir_all(dir.path().join("concepts")).unwrap();
        std::fs::write(dir.path().join(&id), "x").unwrap();
        let (id, _) = entry_from_edit(dir.path(), &t, &edited).unwrap().unwrap();
        assert_eq!(id, "concepts/socket-paths-2.md", "never overwrites");
        let (id, _) = entry_from_edit(dir.path(), &t, "# From A Heading\ntext").unwrap().unwrap();
        assert_eq!(id, "note/from-a-heading.md");
        assert_eq!(slug("../../etc"), "etc");
    }

    #[test]
    fn source_files_must_exist_inside_the_brain() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "x").unwrap();
        assert!(source_file(dir.path(), "a.md").is_ok());
        assert!(source_file(dir.path(), "gone.md").unwrap_err().contains("no markdown file"));
        assert!(source_file(dir.path(), "../a.md").unwrap_err().contains("outside"));
    }

    /// write → edit in place → reindex → delete, against a temp brain, through
    /// the same path the TUI takes (`NINOX_BRAIN` resolves it).
    #[tokio::test]
    async fn edit_and_reindex_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let brain = crate::test_fixtures::with_env_override("NINOX_BRAIN", dir.path(), || AppConfig::default().resolved_brain_path());
        assert_eq!(brain, dir.path());
        let cfg = AppConfig::default();
        let query = |b: &Path| ninox_core::BrainIndex::open(b).unwrap().query("", None, Default::default()).unwrap();

        let content = "---\nname: Socket\ntags: [tmux]\n---\nbody\n".to_string();
        let msg = run_job(brain.clone(), cfg.clone(), Job::Write { id: "errors/socket.md".into(), content }, false).await.unwrap();
        assert!(msg.starts_with("added errors/socket.md · indexed 1 entries"), "{msg}");
        assert_eq!(query(&brain)[0].tags, ["tmux"]);

        let path = source_file(&brain, "errors/socket.md").unwrap();
        std::fs::write(&path, "---\nname: Socket\ntags: [tmux, ptyd]\n---\nbody\nmore\n").unwrap();
        run_job(brain.clone(), cfg.clone(), Job::Reindex { id: "errors/socket.md".into() }, false).await.unwrap();
        let e = &query(&brain)[0];
        assert_eq!(e.tags, ["tmux", "ptyd"]);
        assert!(e.body.contains("more"));

        run_job(brain.clone(), cfg.clone(), Job::Delete { id: "errors/socket.md".into() }, false).await.unwrap();
        assert!(query(&brain).is_empty());
        assert!(run_job(brain, cfg, Job::Delete { id: "errors/socket.md".into() }, false).await.is_err());
    }
}
