//! Write rules: a natural-language prompt per brain saying what belongs in
//! it, so an agent can decide which brains an entry goes to.
//!
//! Brain rules live inside the brain as the reserved root file `_rules.md`.
//! It is an ordinary `.md` file as far as sync is concerned (manifest,
//! conflict copies, S3 and github-wiki alike) but is invisible to the index,
//! so it never shows in search results or entry lists. A local override
//! (`write_rules_override` in config) is appended at read time and never
//! leaves the machine.

use std::{fs, path::Path};

use anyhow::{bail, Context, Result};
use serde::Serialize;

pub const RULES_FILE: &str = "_rules.md";

/// Set only on the harvest subprocess: `ninox brain add` stamps entries it
/// writes with `source: harvest` and this session id.
pub const HARVEST_SESSION_ENV: &str = "NINOX_HARVEST_SESSION";

/// Adds `source: harvest` and `harvest_session: <id>` to the entry's YAML
/// frontmatter (creating one if absent), leaving any value already present.
pub fn stamp_harvest(content: &str, session_id: &str) -> String {
    let add = |fm: &str| {
        let mut out = fm.trim_end_matches('\n').to_string();
        if !out.lines().any(|l| l.starts_with("source:")) {
            out.push_str("\nsource: harvest");
        }
        if !out.lines().any(|l| l.starts_with("harvest_session:")) {
            out.push_str(&format!("\nharvest_session: {session_id}"));
        }
        out
    };
    if let Some(rest) = content.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---") {
            let (fm, tail) = rest.split_at(end);
            return format!("---\n{}{}", add(fm), tail);
        }
    }
    format!("---\n{}\n---\n\n{content}", add("").trim_start_matches('\n'))
}

/// True for the reserved rules path (`_rules.md` at the brain root, in any
/// spelling that resolves to it).
pub fn is_reserved_rel(rel: &str) -> bool {
    let norm = rel.replace('\\', "/");
    let mut parts: Vec<&str> = Vec::new();
    for c in norm.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    // Case-insensitive: on a case-insensitive filesystem `_Rules.md` is the
    // same file.
    parts.len() == 1 && parts[0].eq_ignore_ascii_case(RULES_FILE)
}

pub fn rules_path(brain_path: &Path) -> std::path::PathBuf {
    brain_path.join(RULES_FILE)
}

/// The brain's own rules, trimmed; `None` when absent or blank.
pub fn read_brain_rules(brain_path: &Path) -> Option<String> {
    let text = fs::read_to_string(rules_path(brain_path)).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Replace the brain's rules. Blank text removes the file.
pub fn write_brain_rules(brain_path: &Path, text: &str) -> Result<()> {
    let path = rules_path(brain_path);
    let text = text.trim();
    if text.is_empty() {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("remove {path:?}")),
        }
        return Ok(());
    }
    fs::create_dir_all(brain_path).with_context(|| format!("create {brain_path:?}"))?;
    fs::write(&path, format!("{text}\n")).with_context(|| format!("write {path:?}"))
}

/// Refuse writes to the reserved path through the ordinary entry flow.
pub fn ensure_not_reserved(rel: &str) -> Result<()> {
    if is_reserved_rel(rel) {
        bail!("'{RULES_FILE}' is reserved for brain write rules; use `ninox brain rules set` (only when asked to change them)");
    }
    Ok(())
}

/// Save `text` as the local override of catalogue `name` ("default" is the
/// `[brain]` table). Blank clears it. False when no such catalogue exists.
pub fn set_override(cfg: &mut crate::config::AppConfig, name: &str, text: &str) -> bool {
    let value = {
        let t = text.trim();
        (!t.is_empty()).then(|| t.to_string())
    };
    if name == "default" {
        cfg.brain.write_rules_override = value;
        return true;
    }
    match cfg.brain.catalogues.iter_mut().find(|c| c.name == name) {
        Some(c) => {
            c.write_rules_override = value;
            true
        }
        None => false,
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CombinedRules {
    pub brain: Option<String>,
    #[serde(rename = "override")]
    pub local_override: Option<String>,
}

impl CombinedRules {
    pub fn load(brain_path: &Path, local_override: Option<&str>) -> Self {
        Self {
            brain: read_brain_rules(brain_path),
            local_override: local_override.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.brain.is_none() && self.local_override.is_none()
    }

    /// What an agent reads: brain rules, then the local override, which wins
    /// where the two conflict.
    pub fn render(&self) -> String {
        let mut out = String::new();
        match &self.brain {
            Some(t) => out.push_str(&format!("Brain rules (shared with everyone who uses this brain):\n{t}\n")),
            None => out.push_str("Brain rules: none set.\n"),
        }
        if let Some(o) = &self.local_override {
            out.push_str(&format!("\nLocal override (this user only; wins where it conflicts with the brain rules):\n{o}\n"));
        }
        if self.is_empty() {
            out.push_str("\nNo rules set for this brain.\n");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_path_matches_only_root_rules_file() {
        assert!(is_reserved_rel("_rules.md"));
        assert!(is_reserved_rel("./_rules.md"));
        assert!(is_reserved_rel("repos/../_rules.md"));
        assert!(is_reserved_rel("a/./b/../../_rules.md"));
        assert!(is_reserved_rel("//_rules.md"));
        assert!(is_reserved_rel("_Rules.MD"));
        assert!(!is_reserved_rel("repos/_rules.md"));
        assert!(!is_reserved_rel("rules.md"));
    }

    #[test]
    fn write_read_and_clear_brain_rules() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_brain_rules(dir.path()), None);
        write_brain_rules(dir.path(), "  MLOps only.  ").unwrap();
        assert_eq!(read_brain_rules(dir.path()).as_deref(), Some("MLOps only."));
        write_brain_rules(dir.path(), "  ").unwrap();
        assert!(!rules_path(dir.path()).exists());
        write_brain_rules(dir.path(), "").unwrap();
    }

    #[test]
    fn render_appends_override_after_brain_rules() {
        let c = CombinedRules { brain: Some("alpha-rule".into()), local_override: Some("beta-rule".into()) };
        let r = c.render();
        assert!(r.find("alpha-rule").unwrap() < r.find("beta-rule").unwrap());
        assert!(r.contains("wins where it conflicts"));
        assert!(CombinedRules { brain: None, local_override: None }.render().contains("No rules set"));
    }

    #[test]
    fn stamp_harvest_adds_markers_to_existing_or_missing_frontmatter() {
        let with = stamp_harvest("---\nname: x\n---\n\nbody", "s1");
        assert!(with.starts_with("---\nname: x\nsource: harvest\nharvest_session: s1\n---"), "{with}");
        assert!(with.ends_with("\n\nbody"));
        let without = stamp_harvest("body only", "s1");
        assert!(without.starts_with("---\nsource: harvest\nharvest_session: s1\n---\n\nbody only"), "{without}");
        let twice = stamp_harvest(&with, "s2");
        assert_eq!(twice.matches("source:").count(), 1);
        assert!(twice.contains("harvest_session: s1"), "an existing marker is kept");
    }

    #[test]
    fn blank_override_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        assert!(CombinedRules::load(dir.path(), Some("  ")).is_empty());
    }

    #[test]
    fn ensure_not_reserved_refuses_rules_file() {
        assert!(ensure_not_reserved("_rules.md").is_err());
        assert!(ensure_not_reserved("repos/a.md").is_ok());
    }
}
