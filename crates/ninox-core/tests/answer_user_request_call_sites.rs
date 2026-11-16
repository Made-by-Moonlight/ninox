//! Adversarial test #3 for `user-request`: `Store::answer_user_request` is
//! the one function allowed to mark a human-escalation request answered.
//! Its forgery resistance only holds if nothing new ever gets a shortcut to
//! call it — a convenience wrapper, a third CLI subcommand, an HTTP route —
//! bypassing `ninox-gui`/`ninox-tui`'s own call sites. This test enumerates
//! every textual occurrence of `answer_user_request` across the workspace
//! and fails the build the moment an un-sanctioned one appears.

use std::fs;
use std::path::{Path, PathBuf};

const NEEDLE: &str = "answer_user_request";
const THIS_FILE: &str = "answer_user_request_call_sites.rs";
const DEFINITION_FILE: &str = "crates/ninox-core/src/store.rs";
const DEFINITION_SIGNATURE: &str = "pub fn answer_user_request(";
/// Sorted.
const SANCTIONED_CALL_SITE_FILES: [&str; 2] = ["crates/ninox-gui/src/app.rs", "crates/ninox-tui/src/tui/mod.rs"];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent() // crates/
        .and_then(Path::parent) // workspace root
        .expect("CARGO_MANIFEST_DIR should be two levels under the workspace root")
        .to_path_buf()
}

fn collect_rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // `target/` holds build output (including codegen'd .rs files
            // under build scripts), not source we need to audit.
            if name == "target" {
                continue;
            }
            collect_rust_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Line ranges (0-indexed, inclusive) covered by a `#[cfg(test)] mod ... {
/// ... }` block, found by brace-depth counting from each `#[cfg(test)]`
/// attribute to the matching close of the item it guards. Good enough for
/// this codebase's test modules; it does not need to be a real parser,
/// since any false negative here is exactly what `plant_a_fake_call_site`
/// below exists to catch.
fn cfg_test_line_ranges(body: &str) -> Vec<(usize, usize)> {
    let lines: Vec<&str> = body.lines().collect();
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() == "#[cfg(test)]" {
            let start = i;
            let mut j = i + 1;
            while j < lines.len() && !lines[j].contains('{') {
                j += 1;
            }
            if j >= lines.len() {
                break;
            }
            let mut depth = 0i32;
            let mut k = j;
            loop {
                depth += lines[k].matches('{').count() as i32;
                depth -= lines[k].matches('}').count() as i32;
                if depth <= 0 || k + 1 >= lines.len() {
                    break;
                }
                k += 1;
            }
            ranges.push((start, k));
            i = k + 1;
        } else {
            i += 1;
        }
    }
    ranges
}

fn line_in_ranges(line_idx: usize, ranges: &[(usize, usize)]) -> bool {
    ranges.iter().any(|(s, e)| line_idx >= *s && line_idx <= *e)
}

/// The lines of one file's `body` that are real call sites: anything
/// mentioning `answer_user_request` that isn't a comment or inside a
/// `#[cfg(test)]` block. In the definition file, the function's own
/// signature line is exempt too — but nothing else there is, so a new
/// caller added elsewhere in `store.rs` is still caught.
fn real_call_sites_in(body: &str, is_definition_file: bool) -> Vec<(usize, String)> {
    let test_ranges = cfg_test_line_ranges(body);
    body.lines()
        .enumerate()
        .filter(|(idx, line)| {
            line.contains(NEEDLE)
                && !line.trim_start().starts_with("//")
                && !line_in_ranges(*idx, &test_ranges)
                && !(is_definition_file && line.contains(DEFINITION_SIGNATURE))
        })
        .map(|(idx, line)| (idx + 1, line.to_string()))
        .collect()
}

/// Every real call site in the workspace, as (workspace-relative path with
/// `/` separators, 1-based line, line text).
fn find_real_call_sites() -> Vec<(String, usize, String)> {
    let root = workspace_root();
    let mut files = Vec::new();
    collect_rust_files(&root.join("crates"), &mut files);

    let mut real_call_sites = Vec::new();
    for path in files {
        if path.file_name().and_then(|n| n.to_str()) == Some(THIS_FILE) {
            continue;
        }
        let rel = path.strip_prefix(&root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
        let Ok(body) = fs::read_to_string(&path) else { continue };
        for (line, text) in real_call_sites_in(&body, rel == DEFINITION_FILE) {
            real_call_sites.push((rel.clone(), line, text));
        }
    }
    real_call_sites
}

#[test]
fn answer_user_request_has_exactly_its_two_intended_call_sites() {
    let real_call_sites = find_real_call_sites();

    // ninox-gui's `Message::AnswerUserRequest` handler and ninox-tui's
    // `perform` — one each, and nowhere else. Checked by file, not just by
    // count, so a call site that moves (or a swap of one for another) fails.
    let mut files: Vec<&str> = real_call_sites.iter().map(|(f, _, _)| f.as_str()).collect();
    files.sort();
    assert_eq!(
        files,
        SANCTIONED_CALL_SITE_FILES,
        "answer_user_request must be called exactly once from each sanctioned UI file; found: {real_call_sites:#?}"
    );
}

/// Proves the test above isn't vacuous: planting a fake call site outside
/// store.rs and outside any `#[cfg(test)]` block must be detected.
#[test]
fn detection_logic_catches_a_planted_call_site() {
    let body = "fn sneaky() {\n    store.answer_user_request(\"id\", \"answer\").unwrap();\n}\n";
    assert_eq!(real_call_sites_in(body, false).len(), 1, "planted call site must be detected exactly once");
}

/// The definition file's exemption covers only the signature, its doc
/// comments, and its `#[cfg(test)]` module — a third caller added
/// elsewhere in `store.rs` is still a real call site.
#[test]
fn definition_file_exemption_does_not_hide_a_new_caller_in_store_rs() {
    let body = "\
/// Doc comment naming answer_user_request.
pub fn answer_user_request(&self, id: &str, answer: &str) -> Result<()> {
    Ok(())
}

pub fn convenience_wrapper(&self) -> Result<()> {
    self.answer_user_request(\"id\", \"yes\")
}

#[cfg(test)]
mod tests {
    fn t() { store.answer_user_request(\"id\", \"yes\").unwrap(); }
}
";
    let found = real_call_sites_in(body, true);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].1.contains("self.answer_user_request"), "{found:?}");
}

#[test]
fn the_real_definition_file_has_no_real_call_sites() {
    let path = workspace_root().join(DEFINITION_FILE);
    let body = fs::read_to_string(&path).unwrap();
    assert!(body.contains(DEFINITION_SIGNATURE), "the signature exemption must match the real definition");
    assert_eq!(real_call_sites_in(&body, true), Vec::<(usize, String)>::new());
}
