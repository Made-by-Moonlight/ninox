//! A unified `git diff` split into per-file sections with `+/-` counts —
//! the shape a file list over a diff needs.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub added: usize,
    pub removed: usize,
    pub binary: bool,
    /// This file's section of the diff, from its `diff --git` header on.
    pub text: String,
}

/// The `b/` side of a `diff --git` header's paths. Assumes git's standard
/// `a/`/`b/` prefixes, which `compute_diff` forces.
fn header_path(rest: &str) -> String {
    match rest.rsplit_once(" \"b/") {
        Some((_, quoted)) => unquote(quoted.strip_suffix('"').unwrap_or(quoted)),
        None => rest.rsplit_once(" b/").map_or(rest, |(_, b)| b).to_string(),
    }
}

/// Undoes git's C-style path quoting: `\ooo` octal bytes (how it writes
/// non-ASCII), and `\"`, `\\`, `\t`, `\n`.
fn unquote(s: &str) -> String {
    let mut bytes = Vec::with_capacity(s.len());
    let mut it = s.bytes().peekable();
    while let Some(b) = it.next() {
        if b != b'\\' {
            bytes.push(b);
            continue;
        }
        match it.next() {
            Some(d @ b'0'..=b'7') => {
                let mut v = u32::from(d - b'0');
                for _ in 0..2 {
                    if let Some(&n @ b'0'..=b'7') = it.peek() {
                        v = v * 8 + u32::from(n - b'0');
                        it.next();
                    }
                }
                bytes.push(v as u8);
            }
            Some(b't') => bytes.push(b'\t'),
            Some(b'n') => bytes.push(b'\n'),
            Some(other) => bytes.push(other),
            None => bytes.push(b'\\'),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Splits `diff` at each `diff --git` header. Counts only hunk lines, so a
/// `+++`/`---` file header is never counted as an addition or removal.
pub fn files(diff: &str) -> Vec<FileDiff> {
    let mut out: Vec<FileDiff> = Vec::new();
    let mut in_hunk = false;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            in_hunk = false;
            let path = header_path(rest);
            out.push(FileDiff { path, added: 0, removed: 0, binary: false, text: String::new() });
        }
        let Some(file) = out.last_mut() else { continue };
        file.text.push_str(line);
        file.text.push('\n');
        if line.starts_with("@@") {
            in_hunk = true;
        } else if in_hunk {
            if line.starts_with('+') {
                file.added += 1;
            } else if line.starts_with('-') {
                file.removed += 1;
            }
        } else if let Some(to) = line.strip_prefix("rename to ") {
            file.path = to.to_string();
        } else if line.starts_with("Binary files ") {
            file.binary = true;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,4 @@
 fn a() {}
-fn b() {}
+fn b() -> u8 { 1 }
+fn c() {}
diff --git a/new.txt b/new.txt
new file mode 100644
--- /dev/null
+++ b/new.txt
@@ -0,0 +1 @@
+hello
diff --git a/old.txt b/old.txt
deleted file mode 100644
--- a/old.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-gone
---- a line that starts with three dashes
diff --git a/before.rs b/after.rs
similarity index 90%
rename from before.rs
rename to after.rs
diff --git a/logo.png b/logo.png
Binary files a/logo.png and b/logo.png differ
";

    #[test]
    fn splits_per_file_with_hunk_only_counts() {
        let f = files(DIFF);
        let summary: Vec<(&str, usize, usize, bool)> = f.iter().map(|d| (d.path.as_str(), d.added, d.removed, d.binary)).collect();
        assert_eq!(
            summary,
            vec![
                ("src/lib.rs", 2, 1, false),
                ("new.txt", 1, 0, false),
                ("old.txt", 0, 2, false),
                ("after.rs", 0, 0, false),
                ("logo.png", 0, 0, true),
            ]
        );
    }

    #[test]
    fn each_section_starts_at_its_own_header_and_holds_its_hunks() {
        let f = files(DIFF);
        assert!(f[0].text.starts_with("diff --git a/src/lib.rs"));
        assert!(f[0].text.contains("+fn c() {}\n") && !f[0].text.contains("new.txt"));
        assert_eq!(f.iter().map(|d| d.text.lines().count()).sum::<usize>(), DIFF.lines().count());
    }

    #[test]
    fn quoted_and_spaced_paths_come_out_bare() {
        let f = files("diff --git \"a/caf\\303\\251 menu.rs\" \"b/caf\\303\\251 menu.rs\"\ndiff --git a/my file.rs b/my file.rs\n");
        let paths: Vec<&str> = f.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, vec!["café menu.rs", "my file.rs"]);
    }

    #[test]
    fn an_empty_diff_has_no_files() {
        assert!(files("").is_empty());
    }
}
