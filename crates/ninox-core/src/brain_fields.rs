//! The user-editable fields of a brain entry — `name`, `tags`, `repos` and
//! the markdown body — read out of an entry's file and written back into it
//! without disturbing anything else in its frontmatter.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EntryFields {
    pub name: String,
    pub tags: Vec<String>,
    pub repos: Vec<String>,
    pub body: String,
}

const EDITED_KEYS: [&str; 3] = ["name", "tags", "repos"];

/// Frontmatter text (between the fences, no leading newline) and the body,
/// split by the same rule `brain::parse_markdown` uses: an opening `---`
/// and the first `\n---` after it; leading newlines of the body dropped.
fn split(content: &str) -> (Option<&str>, &str) {
    let Some(rest) = content.strip_prefix("---") else { return (None, content) };
    let Some(pos) = rest.find("\n---") else { return (None, content) };
    let fm = rest[..pos].strip_prefix('\n').unwrap_or(&rest[..pos]);
    let body = rest[pos + 4..].trim_start_matches(['\n', '\r']);
    (Some(fm), body)
}

/// One top-level frontmatter entry: its key line plus any continuation
/// lines (indented, or `- ` list items). Comments and blank lines are
/// key-less groups of their own so they're carried through untouched.
struct Group<'a> {
    key: Option<&'a str>,
    lines: Vec<&'a str>,
}

fn groups(fm: &str) -> Vec<Group<'_>> {
    let mut out: Vec<Group> = Vec::new();
    for line in fm.lines() {
        let continuation = line.starts_with(char::is_whitespace) || line.starts_with('-');
        if continuation {
            if let Some(last) = out.last_mut() {
                last.lines.push(line);
                continue;
            }
        }
        let key = (!line.starts_with('#'))
            .then(|| line.split_once(':').map(|(k, _)| k.trim()))
            .flatten()
            .filter(|k| !k.is_empty());
        out.push(Group { key, lines: vec![line] });
    }
    out
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches('"').trim_matches('\'').to_string()
}

fn scalar(group: &Group) -> String {
    group.lines[0].split_once(':').map(|(_, v)| unquote(v)).unwrap_or_default()
}

/// Inline `[a, b]`, a block of `- item` lines, or a lone scalar (one item).
fn list(group: &Group) -> Vec<String> {
    let value = group.lines[0].split_once(':').map(|(_, v)| v.trim()).unwrap_or("");
    // `[a, b] # note`: the comment isn't part of the list.
    let value = match (value.starts_with('['), value.rfind(']')) {
        (true, Some(end)) => &value[..=end],
        _ => value,
    };
    if let Some(inner) = value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
        return inner.split(',').map(unquote).filter(|s| !s.is_empty()).collect();
    }
    if value.is_empty() {
        return group.lines[1..]
            .iter()
            .filter_map(|l| l.trim().strip_prefix("- "))
            .map(unquote)
            .filter(|s| !s.is_empty())
            .collect();
    }
    vec![unquote(value)]
}

pub fn read_fields(content: &str) -> EntryFields {
    let (fm, body) = split(content);
    let mut fields = EntryFields { body: body.to_string(), ..Default::default() };
    for group in groups(fm.unwrap_or("")) {
        match group.key {
            Some("name") => fields.name = scalar(&group),
            Some("tags") => fields.tags = list(&group),
            Some("repos") => fields.repos = list(&group),
            _ => {}
        }
    }
    fields
}

fn render_name(name: &str) -> String {
    let needs_quotes = name.contains(": ")
        || name.ends_with(':')
        || name.starts_with(['[', '"', '\'', '#', '-', '{']);
    if needs_quotes { format!("name: \"{name}\"") } else { format!("name: {name}") }
}

fn render(key: &str, fields: &EntryFields) -> String {
    match key {
        "name" => render_name(&fields.name),
        "tags" => format!("tags: [{}]", fields.tags.join(", ")),
        _ => format!("repos: [{}]", fields.repos.join(", ")),
    }
}

/// `content` with its `name`, `tags`, `repos` and body replaced by
/// `fields`. Every other frontmatter line — unknown keys, comments, blank
/// lines — and the order of all keys are kept as they were; an edited key
/// is rewritten where it stood. A key that wasn't there is appended only
/// when it has a value, and a list key that was there stays (as `[]`) even
/// when emptied. A file written with CRLF line endings keeps them.
pub fn apply_fields(content: &str, fields: &EntryFields) -> String {
    if !content.contains("\r\n") {
        return apply_lf(content, fields);
    }
    let fields = EntryFields { body: fields.body.replace("\r\n", "\n"), ..fields.clone() };
    apply_lf(&content.replace("\r\n", "\n"), &fields).replace('\n', "\r\n")
}

fn apply_lf(content: &str, fields: &EntryFields) -> String {
    let (fm, _) = split(content);
    let mut lines: Vec<String> = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for group in groups(fm.unwrap_or("")) {
        match group.key.filter(|k| EDITED_KEYS.contains(k)) {
            Some(key) if seen.contains(&key) => {}
            Some(key) => {
                seen.push(key);
                lines.push(render(key, fields));
            }
            None => lines.extend(group.lines.iter().map(|l| l.to_string())),
        }
    }
    let has_value = |key: &str| match key {
        "name" => !fields.name.is_empty(),
        "tags" => !fields.tags.is_empty(),
        _ => !fields.repos.is_empty(),
    };
    for key in EDITED_KEYS {
        if !seen.contains(&key) && has_value(key) {
            lines.push(render(key, fields));
        }
    }

    let mut out = format!("---\n{}\n---\n", lines.join("\n"));
    if !fields.body.is_empty() {
        out.push('\n');
        out.push_str(&fields.body);
        if !fields.body.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_crlf_file_keeps_its_line_endings() {
        let original = "---\r\nname: A\r\ntype: concepts\r\nowner: me\r\ntags: [x]\r\n---\r\n\r\nbody line\r\nsecond\r\n";
        let mut f = read_fields(original);
        assert_eq!((f.name.as_str(), f.tags.clone()), ("A", vec!["x".to_string()]));
        f.name = "B".into();
        f.body = "edited\nbody\n".into();
        let out = apply_fields(original, &f);
        assert_eq!(out, "---\r\nname: B\r\ntype: concepts\r\nowner: me\r\ntags: [x]\r\n---\r\n\r\nedited\r\nbody\r\n");
        assert!(!out.replace("\r\n", "").contains('\n') && !out.replace("\r\n", "").contains('\r'), "no mixed endings: {out:?}");
        assert_eq!(read_fields(&out).name, "B");
    }

    #[test]
    fn an_lf_file_stays_lf() {
        let out = apply_fields("---\nname: A\n---\n\nbody\n", &read_fields("---\nname: A\n---\n\nbody\n"));
        assert!(!out.contains('\r'));
    }


    #[test]
    fn a_trailing_comment_on_a_list_line_is_not_a_tag() {
        let f = read_fields("---\nname: A\ntags: [a, b] # keep these\n---\n\nbody\n");
        assert_eq!(f.tags, ["a", "b"]);
        let out = apply_fields("---\nname: A\ntags: [a, b] # keep these\n---\n\nbody\n", &f);
        assert!(out.contains("tags: [a, b]") && !out.contains("[["), "{out}");
    }

    use super::*;

    fn fields(name: &str, tags: &[&str], repos: &[&str], body: &str) -> EntryFields {
        EntryFields {
            name: name.into(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            repos: repos.iter().map(|s| s.to_string()).collect(),
            body: body.into(),
        }
    }

    #[test]
    fn read_fields_handles_inline_and_block_lists() {
        let md = "---\nname: Tmux sockets\ntype: concepts\ntags: [a, b]\nrepos:\n  - ninox\n  - bellhop\n---\n\n# Body\ntext\n";
        assert_eq!(read_fields(md), fields("Tmux sockets", &["a", "b"], &["ninox", "bellhop"], "# Body\ntext\n"));
    }

    #[test]
    fn read_fields_without_frontmatter_is_all_body() {
        assert_eq!(read_fields("just text\n"), fields("", &[], &[], "just text\n"));
    }

    #[test]
    fn apply_keeps_unknown_keys_comments_and_key_order() {
        let md = "---\n# provenance: written by hand\ntype: decisions\nname: Old name\nupdated: 2026-10-01\ntags: [x]\ncustom_key: keep me\nrepos:\n  - ninox\nstatus: active\n---\n\nold body\n";
        let out = apply_fields(md, &fields("New name", &["y", "z"], &["bellhop"], "new body\n"));
        assert_eq!(
            out,
            "---\n# provenance: written by hand\ntype: decisions\nname: New name\nupdated: 2026-10-01\ntags: [y, z]\ncustom_key: keep me\nrepos: [bellhop]\nstatus: active\n---\n\nnew body\n"
        );
    }

    #[test]
    fn apply_then_read_round_trips_the_fields() {
        let md = "---\ntype: patterns\nweird: [1, 2]\nname: A\n---\n\nbody\n";
        let want = fields("B: with a colon", &["t1"], &[], "line 1\n\nline 3\n");
        assert_eq!(read_fields(&apply_fields(md, &want)), want);
    }

    #[test]
    fn apply_with_unchanged_fields_is_a_no_op_on_a_canonical_file() {
        let md = "---\nname: Same\ntype: concepts\ntags: [a, b]\nrepos: [ninox]\nextra: 1\n---\n\nbody\n";
        assert_eq!(apply_fields(md, &read_fields(md)), md);
    }

    #[test]
    fn apply_appends_missing_keys_and_omits_empty_lists_that_were_absent() {
        let md = "---\ntype: concepts\n---\n\nbody\n";
        assert_eq!(
            apply_fields(md, &fields("N", &["t"], &[], "body\n")),
            "---\ntype: concepts\nname: N\ntags: [t]\n---\n\nbody\n"
        );
    }

    #[test]
    fn apply_keeps_an_emptied_list_key_that_was_present() {
        let md = "---\nname: N\ntags: [a]\n---\n\nb\n";
        assert_eq!(apply_fields(md, &fields("N", &[], &[], "b\n")), "---\nname: N\ntags: []\n---\n\nb\n");
    }

    #[test]
    fn apply_adds_frontmatter_to_a_file_without_any() {
        assert_eq!(
            apply_fields("plain\n", &fields("N", &[], &["r"], "plain\n")),
            "---\nname: N\nrepos: [r]\n---\n\nplain\n"
        );
    }

    /// `read_fields`/`apply_fields` scan frontmatter themselves (they need
    /// line spans the indexer's map-based parser doesn't keep), so pin them
    /// to what the indexer actually stores for the same file.
    #[test]
    fn the_indexer_reads_an_applied_file_back_as_the_same_fields() {
        let _iso = crate::test_isolation::isolate();
        let dir = tempfile::tempdir().unwrap();
        let original = "---\n# note\ntype: concepts\nname: Old\ntags:\n  - x\nkeep: me\n---\n\nold\n";
        let want = fields("New: name", &["a", "b"], &["ninox"], "# Heading\n\nnew body\n");
        let written = apply_fields(original, &want);
        std::fs::create_dir_all(dir.path().join("concepts")).unwrap();
        std::fs::write(dir.path().join("concepts/e.md"), &written).unwrap();

        let index = crate::BrainIndex::open(dir.path()).unwrap();
        index.rebuild(None).unwrap();
        let entry = index.get("concepts/e.md").unwrap().expect("indexed");
        assert_eq!(entry.name, want.name);
        assert_eq!(entry.tags, want.tags);
        assert_eq!(entry.repos, want.repos);
        assert_eq!(entry.entry_type, "concepts");
        assert_eq!(read_fields(&written), want);
    }

    #[test]
    fn apply_ensures_the_body_ends_with_a_newline() {
        let out = apply_fields("---\nname: N\n---\n", &fields("N", &[], &[], "no newline"));
        assert_eq!(out, "---\nname: N\n---\n\nno newline\n");
    }
}
