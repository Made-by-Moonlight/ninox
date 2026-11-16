//! The Brain tab's in-place entry editor: a draft of an entry's editable
//! fields and the job that writes it back (`brain_edit_view` draws it).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use iced::widget::text_editor;
use ninox_core::brain_fields::{apply_fields, read_fields, EntryFields};
use ninox_ui_shared::brain_edit::{entry_from_edit, source_file, template, Job};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditTarget {
    /// `id` is the entry's id as the list shows it (qualified under "All
    /// sources"), `raw_id` its path under `BrainEditor::root`; `original` is
    /// the file text the draft was read from.
    Existing { id: String, raw_id: String, original: String },
    New { kind: String },
}

static NEXT_SERIAL: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub struct BrainEditor {
    /// The catalogue this draft was opened in, fixed for its lifetime: a
    /// later catalogue switch must not redirect the save.
    pub root: PathBuf,
    /// Tells this draft's save job apart from another draft's.
    pub serial: u64,
    pub target: EditTarget,
    pub name: String,
    pub tags: Vec<String>,
    pub repos: Vec<String>,
    pub tag_draft: String,
    pub repo_draft: String,
    pub body: text_editor::Content,
    pub error: Option<String>,
}

impl BrainEditor {
    pub fn open_existing(root: &Path, raw_id: &str, id: String) -> Result<Self, String> {
        let path = source_file(root, raw_id)?;
        let original = std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let fields = read_fields(&original);
        Ok(Self {
            root: root.to_path_buf(),
            serial: NEXT_SERIAL.fetch_add(1, Ordering::Relaxed),
            target: EditTarget::Existing { id, raw_id: raw_id.to_string(), original },
            name: fields.name,
            tags: fields.tags,
            repos: fields.repos,
            tag_draft: String::new(),
            repo_draft: String::new(),
            body: text_editor::Content::with_text(&fields.body),
            error: None,
        })
    }

    pub fn new_entry(kind: String, root: PathBuf) -> Self {
        Self {
            root,
            serial: NEXT_SERIAL.fetch_add(1, Ordering::Relaxed),
            target: EditTarget::New { kind },
            name: String::new(),
            tags: Vec::new(),
            repos: Vec::new(),
            tag_draft: String::new(),
            repo_draft: String::new(),
            body: text_editor::Content::new(),
            error: None,
        }
    }

    /// Adds each comma-separated, trimmed value not already present.
    pub fn add_values(list: &mut Vec<String>, draft: &str) {
        for value in draft.split(',').map(str::trim).filter(|v| !v.is_empty()) {
            if !list.iter().any(|v| v == value) {
                list.push(value.to_string());
            }
        }
    }

    pub fn fields(&self) -> EntryFields {
        EntryFields {
            name: self.name.trim().to_string(),
            tags: self.tags.clone(),
            repos: self.repos.clone(),
            body: self.body.text(),
        }
    }

    /// The job (with a raw id under [`Self::root`]) that writes this draft.
    pub fn to_job(&self) -> Result<Job, String> {
        let root = self.root.as_path();
        let fields = self.fields();
        if fields.name.is_empty() {
            return Err("an entry needs a name".into());
        }
        match &self.target {
            EditTarget::Existing { raw_id, original, .. } => {
                let current = std::fs::read_to_string(root.join(raw_id)).unwrap_or_default();
                if current != *original {
                    return Err("this entry changed on disk since you opened it — cancel and edit it again".into());
                }
                Ok(Job::Save { id: raw_id.clone(), content: apply_fields(original, &fields) })
            }
            EditTarget::New { kind } => {
                let template = template(kind, &[]);
                let edited = apply_fields(&template, &fields);
                match entry_from_edit(root, &template, &edited)? {
                    Some((id, content)) => Ok(Job::Write { id, content }),
                    None => Err("the new entry is empty".into()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn opening_an_entry_reads_its_fields() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "concepts/a.md", "---\nname: A\ntype: concepts\ntags: [x, y]\nrepos: [ninox]\n---\n\nbody\n");
        let ed = BrainEditor::open_existing(dir.path(), "concepts/a.md", "team:concepts/a.md".into()).unwrap();
        assert_eq!((ed.name.as_str(), ed.tags.clone(), ed.repos.clone()), ("A", vec!["x".into(), "y".into()], vec!["ninox".into()]));
        assert_eq!(ed.body.text(), "body\n");
        assert!(matches!(ed.target, EditTarget::Existing { ref id, ref raw_id, .. } if id == "team:concepts/a.md" && raw_id == "concepts/a.md"));
    }

    #[test]
    fn opening_a_missing_entry_says_why() {
        let dir = tempfile::tempdir().unwrap();
        assert!(BrainEditor::open_existing(dir.path(), "gone.md", "gone.md".into()).unwrap_err().contains("no markdown file"));
    }

    #[test]
    fn saving_an_edit_rewrites_only_the_edited_fields() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "concepts/a.md", "---\nname: A\ntype: concepts\ncustom: keep\ntags: [x]\n---\n\nbody\n");
        let mut ed = BrainEditor::open_existing(dir.path(), "concepts/a.md", "concepts/a.md".into()).unwrap();
        ed.name = "B".into();
        BrainEditor::add_values(&mut ed.tags, "y");
        ed.body = text_editor::Content::with_text("new body");

        assert_eq!(
            ed.to_job().unwrap(),
            Job::Save {
                id: "concepts/a.md".into(),
                content: "---\nname: B\ntype: concepts\ncustom: keep\ntags: [x, y]\n---\n\nnew body\n".into(),
            }
        );
    }

    #[test]
    fn a_new_entry_is_written_under_its_kind_and_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut ed = BrainEditor::new_entry("concepts".into(), dir.path().to_path_buf());
        ed.name = "Socket Paths".into();
        BrainEditor::add_values(&mut ed.tags, "tmux");
        ed.body = text_editor::Content::with_text("body");

        let Job::Write { id, content } = ed.to_job().unwrap() else { panic!("expected a Write") };
        assert_eq!(id, "concepts/socket-paths.md");
        let fields = read_fields(&content);
        assert_eq!((fields.name.as_str(), fields.tags, fields.body.as_str()), ("Socket Paths", vec!["tmux".to_string()], "body\n"));
        assert_eq!(ninox_core::brain::frontmatter_str(&content, "type").as_deref(), Some("concepts"));
    }

    #[test]
    fn a_new_entry_never_overwrites_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "concepts/a.md", "x");
        let mut ed = BrainEditor::new_entry("concepts".into(), dir.path().to_path_buf());
        ed.name = "A".into();
        assert!(matches!(ed.to_job().unwrap(), Job::Write { id, .. } if id == "concepts/a-2.md"));
    }

    #[test]
    fn an_entry_needs_a_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut ed = BrainEditor::new_entry("concepts".into(), dir.path().to_path_buf());
        ed.name = "   ".into();
        ed.body = text_editor::Content::with_text("body");
        assert!(ed.to_job().unwrap_err().contains("name"));

        write(dir.path(), "concepts/a.md", "---\nname: A\n---\n\nbody\n");
        let mut ed = BrainEditor::open_existing(dir.path(), "concepts/a.md", "concepts/a.md".into()).unwrap();
        ed.name.clear();
        assert!(ed.to_job().unwrap_err().contains("name"));
    }

    #[test]
    fn a_save_refuses_to_overwrite_a_change_made_on_disk_meanwhile() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "concepts/a.md", "---\nname: A\n---\n\nbody\n");
        let mut ed = BrainEditor::open_existing(dir.path(), "concepts/a.md", "concepts/a.md".into()).unwrap();
        write(dir.path(), "concepts/a.md", "---\nname: A\n---\n\npulled from the remote\n");
        ed.name = "B".into();
        assert!(ed.to_job().unwrap_err().contains("changed on disk"));
    }

    #[test]
    fn added_values_are_split_trimmed_and_deduplicated() {
        let mut list = vec!["a".to_string()];
        BrainEditor::add_values(&mut list, " b, a ,, c ");
        assert_eq!(list, ["a", "b", "c"]);
    }
}
