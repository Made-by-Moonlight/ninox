//! Brain entry writes shared by the TUI and the GUI: new-entry templates,
//! the add/edit/delete/reindex jobs, and resolving which catalogue a job
//! runs against.

use std::path::{Path, PathBuf};

use ninox_core::config::{AppConfig, CatalogueRef};

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
    /// A full, on-demand rebuild (`R`) — not tied to one entry, so it also
    /// picks up changes made outside the TUI (an external editor, a synced
    /// remote) between automatic reindexes.
    ReindexAll,
    Write { id: String, content: String },
    /// An existing entry rewritten in place by an in-app editor; written
    /// exactly like `Write`, reported as "saved" rather than "added".
    Save { id: String, content: String },
    Delete { id: String },
}

impl Job {
    pub fn id(&self) -> &str {
        match self {
            Self::Reindex { id } | Self::Write { id, .. } | Self::Save { id, .. } | Self::Delete { id } => id,
            Self::ReindexAll => "",
        }
    }

    pub fn verb(&self) -> &'static str {
        match self {
            Self::Reindex { .. } | Self::Save { .. } => "saved",
            Self::ReindexAll => "reindexed",
            Self::Write { .. } => "added",
            Self::Delete { .. } => "deleted",
        }
    }

    /// The job-done notice, before any remote-sync suffix is appended.
    pub fn summary(&self, indexed: usize) -> String {
        match self {
            Self::ReindexAll => format!("reindexed the brain · indexed {indexed} entries"),
            _ => format!("{} {} · indexed {indexed} entries", self.verb(), self.id()),
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
        Job::Reindex { .. } | Job::ReindexAll => {}
        Job::Save { id, content } => crate::cli::write_brain_entry(&brain, id, content).map_err(|e| format!("write {id}: {e}"))?,
        // A new entry: never replace a file that appeared since its path was picked.
        Job::Write { id, content } => write_new(&brain, id, content).map_err(|e| format!("write {id}: {e}"))?,
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
        let embedder = if embed { crate::cli::build_embedder(false) } else { None };
        ninox_core::BrainIndex::open(&brain)?.rebuild(embedder.as_deref())
    })
    .await
    .map_err(|e| format!("index task failed: {e}"))?
    .map_err(|e| format!("reindex failed: {e}"))?;
    Ok(format!("{}{remote}", job.summary(stats.indexed)))
}

/// Catalogue roots a UI should run `job` against: the active
/// single catalogue (always one path), or under "All sources" — a
/// single-entry job (edit-in-place reindex, delete) resolves to that
/// entry's own source catalogue (`catalogue_root_for_id`; a brand-new
/// entry with no source yet falls back to `fallback`), while a full
/// reindex (`R`) targets every *enabled* catalogue, since "All sources"
/// means the whole merged brain, not just the fallback one.
pub fn brain_job_paths(options: &[CatalogueRef], active: Option<usize>, job: &Job, fallback: &Path) -> Vec<PathBuf> {
    match active {
        Some(idx) => vec![options.get(idx).map(|c| c.path.clone()).unwrap_or_else(|| fallback.to_path_buf())],
        None => match job {
            Job::ReindexAll => {
                let enabled: Vec<PathBuf> = options.iter().filter(|c| c.enabled).map(|c| c.path.clone()).collect();
                if enabled.is_empty() { vec![fallback.to_path_buf()] } else { enabled }
            }
            _ => vec![ninox_core::brain::catalogue_root_for_id(job.id(), options, fallback)],
        },
    }
}

fn write_new(brain: &Path, id: &str, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    let path = brain.join(id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new().write(true).create_new(true).open(path)?.write_all(content.as_bytes())
}

/// `run_job`'s own `source_file(brain, id)` joins `id` onto the catalogue
/// root `brain_job_paths` already resolved — a still-qualified "All
/// sources" id (`"team:concepts/x.md"`) would join as a literal, nonexistent
/// `root/team:concepts/x.md`. Strips the source prefix now that the root
/// it named has already done its job of picking the path. With a single
/// catalogue active (`active` is `Some`) ids are never qualified, so a raw
/// id containing `:` is left alone.
pub fn unqualify_job_id(job: Job, active: Option<usize>) -> Job {
    if active.is_some() {
        return job;
    }
    fn raw(id: String) -> String {
        ninox_core::MergedBrainIndex::split_qualified(&id).map(|(_, raw)| raw.to_string()).unwrap_or(id)
    }
    match job {
        Job::Reindex { id } => Job::Reindex { id: raw(id) },
        Job::Write { id, content } => Job::Write { id: raw(id), content },
        Job::Save { id, content } => Job::Save { id: raw(id), content },
        Job::Delete { id } => Job::Delete { id: raw(id) },
        Job::ReindexAll => Job::ReindexAll,
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn ids_are_only_unqualified_under_all_sources() {
        let job = || Job::Delete { id: "notes/a:b.md".into() };
        assert_eq!(unqualify_job_id(job(), Some(0)), job(), "a raw id with a colon stays whole");
        assert_eq!(unqualify_job_id(Job::Delete { id: "team:concepts/x.md".into() }, None), Job::Delete { id: "concepts/x.md".into() });
    }

    #[tokio::test]
    async fn a_new_entry_never_replaces_a_file_that_appeared_meanwhile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("concepts")).unwrap();
        std::fs::write(dir.path().join("concepts/a.md"), "theirs").unwrap();
        let job = Job::Write { id: "concepts/a.md".into(), content: "mine".into() };
        assert!(run_job(dir.path().to_path_buf(), AppConfig::default(), job, false).await.is_err());
        assert_eq!(std::fs::read_to_string(dir.path().join("concepts/a.md")).unwrap(), "theirs");
    }

    use super::*;

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

    #[tokio::test]
    async fn edit_and_reindex_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let _iso = ninox_core::test_isolation::isolate();
        let brain = dir.path().to_path_buf();
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

    #[tokio::test]
    async fn save_overwrites_an_existing_entry_and_reports_saved() {
        let dir = tempfile::tempdir().unwrap();
        let _iso = ninox_core::test_isolation::isolate();
        let brain = dir.path().to_path_buf();
        let cfg = AppConfig::default();
        let first = "---\nname: Socket\ntags: [tmux]\n---\nbody\n".to_string();
        run_job(brain.clone(), cfg.clone(), Job::Write { id: "errors/socket.md".into(), content: first }, false).await.unwrap();

        let edited = "---\nname: Socket\ntags: [tmux, ptyd]\n---\nnew body\n".to_string();
        let job = Job::Save { id: "errors/socket.md".into(), content: edited.clone() };
        let msg = run_job(brain.clone(), cfg, job, false).await.unwrap();

        assert!(msg.starts_with("saved errors/socket.md · indexed 1 entries"), "{msg}");
        assert_eq!(std::fs::read_to_string(brain.join("errors/socket.md")).unwrap(), edited);
        let e = &ninox_core::BrainIndex::open(&brain).unwrap().query("", None, Default::default()).unwrap()[0];
        assert_eq!(e.tags, ["tmux", "ptyd"]);
    }

    #[test]
    fn unqualify_job_id_strips_the_source_prefix_from_a_save() {
        assert_eq!(
            unqualify_job_id(Job::Save { id: "team:concepts/a.md".into(), content: "x".into() }, None),
            Job::Save { id: "concepts/a.md".into(), content: "x".into() }
        );
    }

    #[test]
    fn reindex_all_summarizes_without_a_per_entry_id() {
        assert_eq!(Job::ReindexAll.id(), "");
        assert_eq!(Job::ReindexAll.verb(), "reindexed");
        assert_eq!(Job::ReindexAll.summary(3), "reindexed the brain · indexed 3 entries");
        assert_eq!(
            Job::Write { id: "a.md".into(), content: String::new() }.summary(1),
            "added a.md · indexed 1 entries",
            "ordinary jobs keep their per-entry summary"
        );
    }

    #[tokio::test]
    async fn reindex_all_rebuilds_from_whatever_changed_on_disk_directly() {
        let dir = tempfile::tempdir().unwrap();
        let _iso = ninox_core::test_isolation::isolate();
        let brain = dir.path().to_path_buf();
        let cfg = AppConfig::default();
        let query = |b: &Path| ninox_core::BrainIndex::open(b).unwrap().query("", None, Default::default()).unwrap();

        // Write directly to disk (no `ninox brain add` / Job::Write), the
        // way an external editor or a synced remote would — ReindexAll is
        // the only job that picks this up without already knowing an id.
        std::fs::create_dir_all(brain.join("concepts")).unwrap();
        std::fs::write(brain.join("concepts/note.md"), "---\nname: Note\n---\nbody\n").unwrap();
        let msg = run_job(brain.clone(), cfg, Job::ReindexAll, false).await.unwrap();
        assert!(msg.starts_with("reindexed the brain · indexed 1 entries"), "{msg}");
        assert_eq!(query(&brain)[0].id, "concepts/note.md");
    }

    #[test]
    fn brain_job_paths_targets_the_active_single_catalogue() {
        let options = vec![catalogue_ref("alpha", "/brains/alpha"), catalogue_ref("beta", "/brains/beta")];
        let fallback = Path::new("/fallback");
        let paths = brain_job_paths(&options, Some(1), &Job::ReindexAll, fallback);
        assert_eq!(paths, vec![PathBuf::from("/brains/beta")]);
    }

    #[test]
    fn brain_job_paths_resolves_a_single_entry_jobs_own_catalogue_under_all_sources() {
        let options = vec![catalogue_ref("alpha", "/brains/alpha"), catalogue_ref("beta", "/brains/beta")];
        let fallback = Path::new("/fallback");
        let job = Job::Reindex { id: "beta:concepts/gadget.md".to_string() };
        let paths = brain_job_paths(&options, None, &job, fallback);
        assert_eq!(paths, vec![PathBuf::from("/brains/beta")]);
    }

    #[test]
    fn brain_job_paths_reindexes_every_enabled_catalogue_under_all_sources() {
        let options = vec![catalogue_ref("alpha", "/brains/alpha"), catalogue_ref("beta", "/brains/beta")];
        let fallback = Path::new("/fallback");
        let paths = brain_job_paths(&options, None, &Job::ReindexAll, fallback);
        assert_eq!(paths, vec![PathBuf::from("/brains/alpha"), PathBuf::from("/brains/beta")]);
    }

    #[test]
    fn unqualify_job_id_strips_the_source_prefix_so_the_path_join_is_correct() {
        // A qualified id like "team:concepts/onboarding.md" is what entries/
        // selections carry under "All sources" — brain_job_paths already
        // resolves the *catalogue root* from it, but run_job's own
        // source_file(brain, id) joins `id` onto that root verbatim, so a
        // still-qualified id produced a bogus "root/team:concepts/..." path
        // that never existed on disk (confirmed live: deleting an "All
        // sources" entry failed with "has no markdown file").
        assert_eq!(
            unqualify_job_id(Job::Delete { id: "team:concepts/onboarding.md".into() }, None),
            Job::Delete { id: "concepts/onboarding.md".into() }
        );
    }

    #[test]
    fn unqualify_job_id_leaves_an_unqualified_id_alone() {
        assert_eq!(
            unqualify_job_id(Job::Delete { id: "concepts/ptyd.md".into() }, None),
            Job::Delete { id: "concepts/ptyd.md".into() }
        );
    }

    #[test]
    fn unqualify_job_id_leaves_reindex_all_alone() {
        assert_eq!(unqualify_job_id(Job::ReindexAll, None), Job::ReindexAll);
    }

    fn catalogue_ref(name: &str, path: &str) -> CatalogueRef {
        CatalogueRef {
            name: name.to_string(),
            path: PathBuf::from(path),
            remote: None,
            endpoint: None,
            region: None,
            cache_ttl_secs: None,
            enabled: true,
            cadence: "manual".to_string(),
            access: "read_write".to_string(),
        }
    }
}
