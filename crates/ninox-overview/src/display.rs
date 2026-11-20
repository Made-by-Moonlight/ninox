//! Shared "no ticket/internal text" display helper (MLOPS-5339 slice 4,
//! carry-over 5a from the #254 review): card briefs and the GUI detail
//! panel's Brief field both showed raw spawn prompts verbatim, ticket IDs
//! and URLs included. `display_brief` strips ticket-id tokens
//! (`MLOPS-5309`), URLs, and file-path-looking tokens, then keeps the first
//! sentence that still has real content — so a prompt that's *entirely*
//! boilerplate up to its first period (e.g. "Linear ticket: MLOPS-5184
//! (https://…)." ) falls through to the next sentence instead of surfacing
//! an empty/near-empty line. Lives here (not in `ninox-gui`) so the TUI can
//! use it too.

/// A sentence needs at least this many surviving words after stripping to
/// count as real content rather than leftover label text ("Linear ticket:"
/// is 2 words) — chosen as the smallest threshold that rejects that exact
/// shape while accepting any ordinary short brief ("Fix the bug" is 3).
const MIN_CONTENT_WORDS: usize = 3;

fn is_ticket_id(token: &str) -> bool {
    let trimmed = token.trim_matches(|c: char| !c.is_alphanumeric());
    match trimmed.split_once('-') {
        Some((prefix, rest)) => {
            prefix.len() >= 2
                && !rest.is_empty()
                && prefix.chars().all(|c| c.is_ascii_uppercase())
                && rest.chars().all(|c| c.is_ascii_digit())
        }
        None => false,
    }
}

fn is_url(token: &str) -> bool {
    let trimmed = token.trim_matches(|c: char| !c.is_alphanumeric() && c != '/' && c != ':' && c != '.');
    trimmed.starts_with("http://") || trimmed.starts_with("https://")
}

/// A bare file-path-looking token: at least two `/`-separated segments
/// (requiring 2+ slashes, not 1, so an ordinary word like "and/or" isn't
/// mistaken for a path), isn't a URL, and is made up only of path-safe
/// characters.
fn is_path(token: &str) -> bool {
    let trimmed = token.trim_matches(|c: char| ".,;:()".contains(c));
    trimmed.matches('/').count() >= 2
        && !is_url(token)
        && trimmed.chars().all(|c| c.is_ascii_alphanumeric() || "/_.-".contains(c))
}

/// A bare `owner/repo` slug: exactly one `/`, both sides non-empty,
/// path-safe, and containing at least one letter (excludes a fraction like
/// `3/5` or a date like `10/28`, which have none), with the owner either
/// 3+ characters or hyphenated (excludes a short version tag like `v1/v2`)
/// and at least one side containing a `-` or a digit (excludes an ordinary
/// English word pair like `and/or`, `his/her`), which `is_path` already
/// avoids by requiring 2+ slashes but a single-slash slug has none to lean
/// on. Known residual false positive, accepted: a hyphenated word pair that
/// happens to look identical in shape to a slug (`read-only/read-write`) —
/// no purely structural signal distinguishes it from a real owner/repo.
fn is_repo_slug(token: &str) -> bool {
    let trimmed = token.trim_matches(|c: char| ".,;:()".contains(c));
    if is_url(token) {
        return false;
    }
    let Some((owner, repo)) = trimmed.split_once('/') else { return false };
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return false;
    }
    let path_safe = |s: &str| s.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c));
    if !path_safe(owner) || !path_safe(repo) {
        return false;
    }
    let has_letter = |s: &str| s.chars().any(|c| c.is_ascii_alphabetic());
    if !has_letter(owner) || !has_letter(repo) {
        return false;
    }
    if owner.chars().count() < 3 && !owner.contains('-') {
        return false;
    }
    let has_signal = |s: &str| s.contains('-') || s.chars().any(|c| c.is_ascii_digit());
    has_signal(owner) || has_signal(repo)
}

/// `"Linear ticket:"`/`"Ticket:"` label prefixes (case-insensitive) some
/// spawn prompts lead every brief with — naming the automation that filed
/// the ticket, not what the work actually is.
const TICKET_LABEL_PREFIXES: &[&str] = &["linear ticket:", "ticket:"];

/// Strips a leading ticket-label prefix (`TICKET_LABEL_PREFIXES`), if `raw`
/// starts with one once case-folded — the label itself carries no content,
/// so it's dropped outright rather than kept as a "sentence."
fn strip_ticket_label(raw: &str) -> &str {
    let trimmed = raw.trim_start();
    let lower = trimmed.to_lowercase();
    for prefix in TICKET_LABEL_PREFIXES {
        if lower.starts_with(prefix) {
            return trimmed[prefix.len()..].trim_start();
        }
    }
    raw
}

/// Status words Linear's own ticket metadata uses (lower-cased) — a
/// parenthetical containing one of these, or an "assigned to" clause, is
/// ticket-status bookkeeping ("already In Progress, assigned to Alice
/// Smith"), not real content, regardless of what labeled it.
const TICKET_STATUS_WORDS: &[&str] =
    &["in progress", "in review", "todo", "done", "backlog", "blocked", "cancelled", "duplicate", "triage"];

fn is_status_assignee_parenthetical(inner: &str) -> bool {
    let lower = inner.trim().to_lowercase();
    lower.contains("assigned to") || TICKET_STATUS_WORDS.iter().any(|w| lower.contains(w))
}

/// Removes every balanced `(...)` parenthetical whose content reads as
/// Linear ticket status/assignee bookkeeping
/// (`is_status_assignee_parenthetical`) — never one with any other
/// content, so a brief's own legitimate aside survives untouched. Mirrors
/// `strip_pr_parentheticals`'s balanced-depth/no-stray-whitespace approach.
fn strip_status_assignee_parentheticals(raw: &str) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < raw.len() {
        if raw[i..].starts_with('(') {
            let mut depth = 0i32;
            let mut end = None;
            for (off, c) in raw[i..].char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = Some(off + 1);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if let Some(end) = end {
                let inner = &raw[i + 1..i + end - 1];
                if is_status_assignee_parenthetical(inner) {
                    let trimmed_len = out.trim_end().len();
                    out.truncate(trimmed_len);
                    i += end;
                    continue;
                }
            }
        }
        let ch = raw[i..].chars().next().expect("i < raw.len()");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Removes each `PR #<digits> (…)` occurrence as one unit, tracking paren
/// depth so a parenthetical containing its own `(...)` doesn't close early.
/// A `"PR #"` occurrence with no digits, or no `(...)` immediately after
/// (past whitespace), is left untouched.
fn strip_pr_parentheticals(raw: &str) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < raw.len() {
        if let Some(rest) = raw[i..].strip_prefix("PR #") {
            let digits_len = rest.chars().take_while(|c| c.is_ascii_digit()).count();
            if digits_len > 0 {
                let after_digits = &rest[digits_len..];
                let trimmed = after_digits.trim_start();
                let ws_len = after_digits.len() - trimmed.len();
                if trimmed.starts_with('(') {
                    let mut depth = 0i32;
                    let mut end = None;
                    for (off, c) in trimmed.char_indices() {
                        match c {
                            '(' => depth += 1,
                            ')' => {
                                depth -= 1;
                                if depth == 0 {
                                    end = Some(off + 1);
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    if let Some(end) = end {
                        // Drop the whitespace already pushed before "PR #…"
                        // too, so removing the whole unit doesn't leave a
                        // stray space (or, after rejoining downstream, an
                        // orphaned " ." / " ,") where it used to sit.
                        let trimmed_len = out.trim_end().len();
                        out.truncate(trimmed_len);
                        i += "PR #".len() + digits_len + ws_len + end;
                        continue;
                    }
                }
            }
        }
        let ch = raw[i..].chars().next().expect("i < raw.len()");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Drops ticket-id/URL/path tokens from `raw`, keeping a dropped token's own
/// trailing sentence-terminator (`.`/`!`/`?`) so sentence boundaries survive
/// even when the token that carried one is removed (e.g. a URL that ends a
/// sentence).
fn strip_tokens(raw: &str) -> String {
    let mut out = String::new();
    for token in raw.split_whitespace() {
        if is_ticket_id(token) || is_url(token) || is_path(token) || is_repo_slug(token) {
            // Attach the terminator directly to whatever's already in `out`
            // (no separating space) so it doesn't become its own
            // whitespace-delimited "word" and inflate the sentence's word
            // count — it must merge into the preceding word, not count as
            // one.
            if let Some(last) = token.chars().last() {
                if matches!(last, '.' | '!' | '?') {
                    out.push(last);
                }
            }
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(token);
    }
    out
}

fn split_sentences(text: &str) -> Vec<&str> {
    let mut sentences = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if matches!(c, '.' | '!' | '?') {
            let next_is_boundary = i + 1 >= bytes.len() || bytes[i + 1] == b' ';
            if next_is_boundary {
                sentences.push(text[start..=i].trim());
                start = i + 1;
            }
        }
        i += 1;
    }
    if start < text.len() {
        let rest = text[start..].trim();
        if !rest.is_empty() {
            sentences.push(rest);
        }
    }
    sentences
}

fn clean_sentence(sentence: &str) -> String {
    sentence
        .trim_matches(|c: char| c.is_whitespace() || "-:—()[]".contains(c))
        .trim()
        .to_string()
}

/// Whether a *cleaned* sentence is spawn-prompt repo/worktree boilerplate
/// ("Repo: worktree at /Users/…/some-worktree.") rather than real content —
/// checked post-stripping, since the path token itself is already gone by
/// then and the surviving label words ("Repo:", "worktree", "at") can
/// otherwise clear [`MIN_CONTENT_WORDS`] on their own and masquerade as the
/// brief.
fn is_repo_worktree_sentence(cleaned: &str) -> bool {
    let lower = cleaned.to_lowercase();
    lower.starts_with("repo:") || lower.starts_with("worktree:") || lower.contains("worktree at")
}

/// Strips ticket IDs, URLs and file paths out of `raw`, skips any
/// ticket-label/status-parenthetical or repo/worktree boilerplate sentence,
/// then returns the first sentence left with at least [`MIN_CONTENT_WORDS`]
/// of real content. Returns an empty string when nothing meets that bar —
/// e.g. a prompt that's entirely ticket-reference boilerplate ("Linear
/// ticket: MLOPS-5184 (https://…).") strips down to just a label fragment,
/// and callers already treat an empty brief as "nothing to show" rather
/// than needing a guaranteed non-empty fallback that would surface that
/// fragment as if it were real content.
pub fn display_brief(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }

    let raw = strip_ticket_label(raw);
    let without_status = strip_status_assignee_parentheticals(raw);
    let without_pr = strip_pr_parentheticals(&without_status);
    let stripped = strip_tokens(&without_pr);
    let sentences = split_sentences(&stripped);

    for sentence in &sentences {
        let cleaned = clean_sentence(sentence);
        if is_repo_worktree_sentence(&cleaned) {
            continue;
        }
        if cleaned.split_whitespace().count() >= MIN_CONTENT_WORDS {
            return cleaned;
        }
    }

    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ticket_id_and_file_path_keeping_the_one_real_sentence() {
        let raw = "MLOPS-5309: implement docs/superpowers/specs/2026-10-20-fleet-overview-ui-vision.md — add the detail panel and action row described in the spec.";
        let result = display_brief(raw);
        assert!(!result.contains("MLOPS-5309"), "ticket id must be stripped: {result}");
        assert!(!result.contains("docs/superpowers"), "file path must be stripped: {result}");
        assert!(result.contains("add the detail panel and action row"), "real content must survive: {result}");
    }

    #[test]
    fn skips_a_boilerplate_linear_ticket_sentence_for_the_real_one_after_it() {
        let raw = "Linear ticket: MLOPS-5184 (https://linear.app/example/issue/MLOPS-5184/some-slug). Fix the dangling pointer in the poller. Also clean up the tests.";
        let result = display_brief(raw);
        assert!(!result.contains("MLOPS-5184"), "ticket id must be stripped: {result}");
        assert!(!result.contains("linear.app"), "url must be stripped: {result}");
        assert!(!result.to_lowercase().contains("linear ticket"), "boilerplate label sentence must be skipped: {result}");
        assert_eq!(result, "Fix the dangling pointer in the poller.");
    }

    #[test]
    fn a_bare_ticket_label_with_a_status_assignee_parenthetical_and_nothing_else_is_empty() {
        // Exact report: no ticket-id token at all, so nothing in the
        // existing id/url/path stripping touched it — the label and its
        // status/assignee parenthetical must be stripped outright, leaving
        // nothing for the empty-brief fallback to surface.
        let raw = "Linear ticket: (already In Progress, assigned to Alice Smith).";
        assert_eq!(display_brief(raw), "");
    }

    #[test]
    fn a_ticket_label_and_status_parenthetical_fall_through_to_the_real_sentence_after_them() {
        let raw = "Ticket: (Done, assigned to Alice Smith). Fix the dangling pointer in the poller.";
        assert_eq!(display_brief(raw), "Fix the dangling pointer in the poller.");
    }

    #[test]
    fn a_parenthetical_with_ordinary_content_is_left_alone() {
        assert_eq!(
            display_brief("Rebase the branch (after the CI run finishes) and merge it in."),
            "Rebase the branch (after the CI run finishes) and merge it in."
        );
    }

    #[test]
    fn skips_a_repo_worktree_boilerplate_sentence_for_the_real_one_after_it() {
        // The surviving label words ("Repo:", "worktree", "at") clear
        // MIN_CONTENT_WORDS on their own once the path token itself is
        // stripped — must be recognized and skipped as a unit regardless.
        let raw = "Repo: worktree at /Users/dev/proj/ninox/.claude/worktrees/v1-mlops-5350-tui-islands. Fix the dangling pointer in the poller.";
        let result = display_brief(raw);
        assert!(!result.to_lowercase().contains("repo:"), "boilerplate label sentence must be skipped: {result}");
        assert!(!result.to_lowercase().contains("worktree"), "{result}");
        assert_eq!(result, "Fix the dangling pointer in the poller.");
    }

    #[test]
    fn returns_empty_when_the_entire_input_is_just_repo_worktree_boilerplate() {
        let raw = "Repo: worktree at /Users/dev/proj/ninox/.claude/worktrees/v1-mlops-5350-tui-islands.";
        assert_eq!(display_brief(raw), "");
    }

    #[test]
    fn a_bare_worktree_label_with_no_repo_prefix_is_also_skipped() {
        let raw = "Worktree: /Users/dev/proj/ninox/.claude/worktrees/v1-mlops-5350-tui-islands. Also clean up the tests.";
        assert_eq!(display_brief(raw), "Also clean up the tests.");
    }

    #[test]
    fn returns_empty_when_the_entire_input_is_boilerplate_with_no_real_content() {
        // A prompt that's nothing but the ticket-reference boilerplate, with
        // no further sentence to fall back on — must suppress it entirely
        // (callers already treat an empty brief as "nothing to show")
        // rather than surface the stripped remnant ("Linear ticket:.") as
        // if it were real content.
        let raw = "Linear ticket: MLOPS-5184 (https://linear.app/example/issue/MLOPS-5184/some-slug).";
        assert_eq!(display_brief(raw), "");
    }

    #[test]
    fn leaves_an_ordinary_brief_alone() {
        assert_eq!(display_brief("Fix the flaky poller test."), "Fix the flaky poller test.");
    }

    #[test]
    fn empty_input_stays_empty() {
        assert_eq!(display_brief(""), "");
        assert_eq!(display_brief("   "), "");
    }

    #[test]
    fn is_ticket_id_matches_project_dash_number_only() {
        assert!(is_ticket_id("MLOPS-5309"));
        assert!(is_ticket_id("MLOPS-5309:"));
        assert!(!is_ticket_id("re-implement"));
        assert!(!is_ticket_id("already-done-here"));
    }

    #[test]
    fn is_url_requires_an_http_scheme() {
        assert!(is_url("https://linear.app/x"));
        assert!(is_url("(https://linear.app/x)."));
        assert!(!is_url("not-a-url"));
    }

    #[test]
    fn is_path_requires_a_slash_and_path_safe_characters() {
        assert!(is_path("docs/superpowers/specs/2026-10-20-vision.md"));
        assert!(!is_path("https://linear.app/x"));
        assert!(!is_path("and/or"));
    }

    #[test]
    fn is_repo_slug_matches_an_owner_slash_repo_token_with_a_hyphen_or_digit() {
        assert!(is_repo_slug("Example-Org/training-dashboard"));
        assert!(is_repo_slug("octocat/repo2"));
        assert!(!is_repo_slug("and/or"), "a common word pair must not match");
        assert!(!is_repo_slug("his/her"));
        assert!(!is_repo_slug("docs/superpowers/specs/x.md"), "multi-slash paths are is_path's job, not this");
    }

    #[test]
    fn is_repo_slug_does_not_match_fractions_dates_or_version_pairs() {
        // Code-review finding: "contains a hyphen or digit" alone also
        // matches ordinary non-slug numeric pairs.
        assert!(!is_repo_slug("3/5"), "a fraction has no letters at all");
        assert!(!is_repo_slug("10/28"), "a date has no letters at all");
        assert!(!is_repo_slug("0.13/0.14"), "a version pair has no letters at all");
        assert!(!is_repo_slug("v1/v2"), "a 2-char version tag is shorter than any real owner segment");
    }

    #[test]
    fn strips_fraction_and_date_like_tokens_are_left_alone_in_a_brief() {
        assert_eq!(display_brief("Fixed 3/5 failing tests in the poller."), "Fixed 3/5 failing tests in the poller.");
        assert_eq!(display_brief("Merge it on 10/28 after review passes."), "Merge it on 10/28 after review passes.");
    }

    #[test]
    fn strips_a_repo_slug_from_the_middle_of_a_sentence() {
        let raw = "Rebase Example-Org/training-dashboard and merge the history-server fix.";
        let result = display_brief(raw);
        assert!(!result.contains("Example-Org"));
        assert_eq!(result, "Rebase and merge the history-server fix.");
    }

    #[test]
    fn strips_a_pr_number_and_its_parenthetical_as_one_unit() {
        let raw = "Rebase Example-Org/training-dashboard PR #121 (fix: redundant pr title) and merge the history-server fix.";
        let result = display_brief(raw);
        assert!(!result.contains("PR #121"));
        assert!(!result.contains("redundant pr title"));
        assert_eq!(result, "Rebase and merge the history-server fix.");
    }

    #[test]
    fn a_pr_parenthetical_with_nested_parens_strips_as_one_balanced_unit() {
        let raw = "Clean up PR #42 (fix: handle the (edge) case) before merging.";
        let result = display_brief(raw);
        assert!(!result.contains("PR #42"));
        assert!(!result.contains("edge"));
        assert_eq!(result, "Clean up before merging.");
    }

    #[test]
    fn a_bare_pr_number_with_no_parenthetical_is_left_alone() {
        assert_eq!(display_brief("See PR #7 for context and ship it."), "See PR #7 for context and ship it.");
    }

    #[test]
    fn a_pr_parenthetical_ending_a_sentence_leaves_no_stray_space_before_the_period() {
        let raw = "Rebase and merge PR #121 (fix: redundant title).";
        assert_eq!(display_brief(raw), "Rebase and merge.");
    }

    #[test]
    fn two_pr_parentheticals_in_one_sentence_leave_no_stray_punctuation_spacing() {
        let raw = "Close PR #5 (wip), then merge PR #6 (fix: y) and ship.";
        assert_eq!(display_brief(raw), "Close, then merge and ship.");
    }
}
