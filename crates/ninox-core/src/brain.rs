use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};
use walkdir::WalkDir;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrainEntry {
    pub id: String,         // relative path from brain root (e.g. "people/alice.md")
    pub entry_type: String, // derived from parent directory name
    pub name: String,       // from frontmatter or filename stem
    pub tags: Vec<String>,
    pub repos: Vec<String>,
    pub updated: Option<String>,
    pub body: String,
}

#[derive(Debug, Default)]
pub struct QueryFilters {
    pub entry_type: Option<String>,
    pub tag: Option<String>,
}

// ---------------------------------------------------------------------------
// BrainIndex
// ---------------------------------------------------------------------------

pub struct BrainIndex {
    conn: Mutex<Connection>,
    brain_path: PathBuf,
}

impl BrainIndex {
    pub fn open(brain_path: impl AsRef<Path>) -> Result<Self> {
        let brain_path = brain_path.as_ref().to_path_buf();
        fs::create_dir_all(&brain_path)
            .with_context(|| format!("create brain dir {brain_path:?}"))?;
        let db_path = brain_path.join(".index.db");
        let conn = Connection::open(&db_path)
            .with_context(|| format!("open brain db {db_path:?}"))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS entries (
                 id      TEXT PRIMARY KEY,
                 type    TEXT NOT NULL,
                 name    TEXT NOT NULL,
                 tags    TEXT NOT NULL DEFAULT '[]',
                 repos   TEXT NOT NULL DEFAULT '[]',
                 updated TEXT,
                 body    TEXT NOT NULL DEFAULT ''
             );
             CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts
                 USING fts5(name, tags, body, content=entries, content_rowid=rowid);",
        )?;
        ensure_gitignore(&brain_path)?;
        Ok(Self { conn: Mutex::new(conn), brain_path })
    }

    /// Walk the brain directory, parse markdown files, and repopulate the index.
    /// Returns the number of files indexed.
    pub fn rebuild(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch("DELETE FROM entries; DELETE FROM entries_fts;")?;

        let mut count = 0usize;
        for entry in WalkDir::new(&self.brain_path)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            // Skip non-files, non-.md files, and the index db itself
            if !path.is_file() {
                continue;
            }
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext != "md" {
                continue;
            }

            let rel = path
                .strip_prefix(&self.brain_path)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();

            let content = match fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let parsed = parse_markdown(&content);

            // Derive entry_type from parent dir name (or frontmatter "type" field)
            let parent_type = path
                .parent()
                .and_then(|p| {
                    // If the parent IS brain_path itself, there's no meaningful type dir
                    if p == self.brain_path { None } else { p.file_name() }
                })
                .and_then(|n| n.to_str())
                .map(str::to_string);

            let entry_type = parsed
                .frontmatter
                .get("type")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or(parent_type)
                .unwrap_or_else(|| "note".to_string());

            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown");
            let name = parsed
                .frontmatter
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| stem.to_string());

            let tags: Vec<String> = parsed
                .frontmatter
                .get("tags")
                .and_then(|v| v.as_sequence())
                .cloned()
                .unwrap_or_default();

            let repos: Vec<String> = parsed
                .frontmatter
                .get("repos")
                .and_then(|v| v.as_sequence())
                .cloned()
                .unwrap_or_default();

            let updated = parsed
                .frontmatter
                .get("updated")
                .and_then(|v| v.as_str())
                .map(str::to_string);

            let tags_json = serde_json::to_string(&tags)?;
            let repos_json = serde_json::to_string(&repos)?;

            conn.execute(
                "INSERT INTO entries (id, type, name, tags, repos, updated, body)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![rel, entry_type, name, tags_json, repos_json, updated, parsed.body],
            )?;

            count += 1;
        }

        // Rebuild the FTS index from the content table
        conn.execute_batch("INSERT INTO entries_fts(entries_fts) VALUES('rebuild');")?;

        Ok(count)
    }

    /// Full-text search with optional filters.
    pub fn query(&self, text: &str, filters: QueryFilters) -> Result<Vec<BrainEntry>> {
        let conn = self.conn.lock().unwrap();

        if text.is_empty() && filters.entry_type.is_none() && filters.tag.is_none() {
            // Return all entries when no constraints given
            let mut stmt = conn.prepare(
                "SELECT id, type, name, tags, repos, updated, body FROM entries ORDER BY name",
            )?;
            let rows = stmt.query_map([], row_to_entry)?;
            let entries: Vec<BrainEntry> =
                rows.collect::<rusqlite::Result<Vec<_>>>()?;
            return Ok(entries);
        }

        if text.is_empty() {
            // Filter-only query
            let mut stmt = conn.prepare(
                "SELECT id, type, name, tags, repos, updated, body FROM entries
                 WHERE (?1 IS NULL OR type = ?1)
                 ORDER BY name",
            )?;
            let rows = stmt.query_map(params![filters.entry_type.as_deref()], row_to_entry)?;
            let mut results: Vec<BrainEntry> =
                rows.collect::<rusqlite::Result<Vec<_>>>()?;
            if let Some(ref tag) = filters.tag {
                results.retain(|e| e.tags.iter().any(|t| t == tag));
            }
            return Ok(results);
        }

        let mut stmt = conn.prepare(
            "SELECT e.id, e.type, e.name, e.tags, e.repos, e.updated, e.body
             FROM entries_fts
             JOIN entries e ON entries_fts.rowid = e.rowid
             WHERE entries_fts MATCH ?1
             ORDER BY rank",
        )?;
        let rows = stmt.query_map(params![text], row_to_entry)?;
        let mut results: Vec<BrainEntry> =
            rows.collect::<rusqlite::Result<Vec<_>>>()?;

        // Post-filter by type and tag
        if let Some(ref et) = filters.entry_type {
            results.retain(|e| &e.entry_type == et);
        }
        if let Some(ref tag) = filters.tag {
            results.retain(|e| e.tags.iter().any(|t| t == tag));
        }

        Ok(results)
    }

    /// Fetch a single entry by its relative path id.
    pub fn get(&self, id: &str) -> Result<Option<BrainEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, type, name, tags, repos, updated, body FROM entries WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map([id], row_to_entry)?;
        match rows.next() {
            None => Ok(None),
            Some(r) => Ok(Some(r?)),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn row_to_entry(r: &rusqlite::Row) -> rusqlite::Result<BrainEntry> {
    let tags_json: String = r.get(3)?;
    let repos_json: String = r.get(4)?;
    let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
    let repos: Vec<String> = serde_json::from_str(&repos_json).unwrap_or_default();
    Ok(BrainEntry {
        id: r.get(0)?,
        entry_type: r.get(1)?,
        name: r.get(2)?,
        tags,
        repos,
        updated: r.get(5)?,
        body: r.get(6)?,
    })
}

/// Ensure `.index.db` is in the brain directory's `.gitignore`.
fn ensure_gitignore(brain_path: &Path) -> Result<()> {
    let gi = brain_path.join(".gitignore");
    let entry = ".index.db\n";
    if gi.exists() {
        let content = fs::read_to_string(&gi)?;
        if !content.contains(".index.db") {
            fs::write(&gi, format!("{content}{entry}"))?;
        }
    } else {
        fs::write(&gi, entry)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Frontmatter parsing (manual YAML split, no extra dep)
// ---------------------------------------------------------------------------

struct FmValue {
    str_val: Option<String>,
    seq_val: Option<Vec<String>>,
}

impl FmValue {
    fn str(s: &str) -> Self {
        Self { str_val: Some(s.to_string()), seq_val: None }
    }

    fn seq(v: Vec<String>) -> Self {
        Self { str_val: None, seq_val: Some(v) }
    }

    fn as_str(&self) -> Option<&str> {
        self.str_val.as_deref()
    }

    fn as_sequence(&self) -> Option<&Vec<String>> {
        self.seq_val.as_ref()
    }
}

struct Frontmatter(HashMap<String, FmValue>);

impl Frontmatter {
    fn get(&self, key: &str) -> Option<&FmValue> {
        self.0.get(key)
    }
}

struct ParsedMd {
    frontmatter: Frontmatter,
    body: String,
}

fn parse_markdown(content: &str) -> ParsedMd {
    if !content.starts_with("---") {
        return ParsedMd {
            frontmatter: Frontmatter(HashMap::new()),
            body: content.to_string(),
        };
    }
    let rest = &content[3..];
    let end = rest.find("\n---").or_else(|| rest.find("\r\n---"));
    let (fm_text, body) = match end {
        None => ("", content),
        Some(pos) => {
            let after = &rest[pos + 4..]; // skip "\n---"
            // skip optional trailing newline
            let body = after.trim_start_matches('\n').trim_start_matches('\r');
            (&rest[..pos], body)
        }
    };
    let fm = parse_frontmatter(fm_text);
    ParsedMd { frontmatter: fm, body: body.to_string() }
}

fn parse_frontmatter(text: &str) -> Frontmatter {
    let mut map: HashMap<String, FmValue> = HashMap::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((key, val)) = line.split_once(':') {
            let key = key.trim().to_string();
            let val = val.trim();
            if val.is_empty() {
                // Possibly a sequence starting on the next lines
                let mut seq = Vec::new();
                while let Some(next) = lines.peek() {
                    let t = next.trim();
                    if let Some(stripped) = t.strip_prefix("- ") {
                        seq.push(stripped.trim().to_string());
                        lines.next();
                    } else {
                        break;
                    }
                }
                if !seq.is_empty() {
                    map.insert(key, FmValue::seq(seq));
                }
            } else if val.starts_with('[') && val.ends_with(']') {
                // Inline sequence: [a, b, c]
                let inner = &val[1..val.len() - 1];
                let seq: Vec<String> = inner
                    .split(',')
                    .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                map.insert(key, FmValue::seq(seq));
            } else {
                map.insert(
                    key,
                    FmValue::str(val.trim_matches('"').trim_matches('\'')),
                );
            }
        }
    }
    Frontmatter(map)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_brain() -> (BrainIndex, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let brain = BrainIndex::open(dir.path()).unwrap();
        (brain, dir)
    }

    #[test]
    fn open_creates_schema() {
        let (_brain, dir) = make_brain();
        let db_path = dir.path().join(".index.db");
        assert!(db_path.exists());
        // Verify the gitignore was created
        let gi = dir.path().join(".gitignore");
        assert!(gi.exists());
        let content = fs::read_to_string(&gi).unwrap();
        assert!(content.contains(".index.db"));
    }

    #[test]
    fn rebuild_indexes_files() {
        let (brain, dir) = make_brain();
        let people_dir = dir.path().join("people");
        fs::create_dir_all(&people_dir).unwrap();
        fs::write(
            people_dir.join("alice.md"),
            "---\nname: Alice Smith\ntags:\n- engineering\n- leadership\n---\nAlice leads the infra team.",
        )
        .unwrap();
        fs::write(
            people_dir.join("bob.md"),
            "# Bob\n\nBob works on frontend.",
        )
        .unwrap();

        let count = brain.rebuild().unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn query_returns_matches() {
        let (brain, dir) = make_brain();
        let dir_path = dir.path().join("notes");
        fs::create_dir_all(&dir_path).unwrap();
        fs::write(
            dir_path.join("rust-tips.md"),
            "---\nname: Rust Tips\ntags:\n- rust\n---\nUse anyhow for error handling.",
        )
        .unwrap();
        fs::write(
            dir_path.join("python-tips.md"),
            "---\nname: Python Tips\ntags:\n- python\n---\nUse dataclasses for data.",
        )
        .unwrap();

        brain.rebuild().unwrap();

        let results = brain.query("anyhow", QueryFilters::default()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "Rust Tips");
    }

    #[test]
    fn query_filters_by_type() {
        let (brain, dir) = make_brain();
        let people = dir.path().join("people");
        let projects = dir.path().join("projects");
        fs::create_dir_all(&people).unwrap();
        fs::create_dir_all(&projects).unwrap();
        fs::write(people.join("alice.md"), "Alice is a person.").unwrap();
        fs::write(projects.join("athene.md"), "Athene is a project.").unwrap();

        brain.rebuild().unwrap();

        let results = brain
            .query("", QueryFilters { entry_type: Some("people".into()), tag: None })
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].entry_type, "people");
    }
}
