//! Hides Linear-integration comments from the GUI/TUI Marginalia list
//! (MLOPS-5339 slice 5 item 1b) — they're sync noise (a linkback to the
//! ticket), not review feedback from a human. Lives here, not in either
//! shell, so both filter identically.

use ninox_core::types::Comment;

const LINEAR_AUTHORS: &[&str] = &["linear", "linear-code", "linear[bot]"];
const LINEAR_LINKBACK_MARKER: &str = "<!-- linear-linkback -->";

pub fn is_linear_comment(comment: &Comment) -> bool {
    LINEAR_AUTHORS.iter().any(|a| comment.author.eq_ignore_ascii_case(a)) || comment.body.contains(LINEAR_LINKBACK_MARKER)
}

pub fn visible_comments(comments: &[Comment]) -> Vec<&Comment> {
    comments.iter().filter(|c| !is_linear_comment(c)).collect()
}

#[cfg(test)]
mod tests {
    use ninox_core::types::Comment;

    fn comment(author: &str, body: &str) -> Comment {
        Comment { id: 1, pr_id: 1, author: author.into(), body: body.into(), path: None, line: None, created_at: 0 }
    }

    #[test]
    fn flags_known_linear_author_handles_case_insensitively() {
        assert!(super::is_linear_comment(&comment("linear", "hi")));
        assert!(super::is_linear_comment(&comment("linear-code", "hi")));
        assert!(super::is_linear_comment(&comment("linear[bot]", "hi")));
        assert!(super::is_linear_comment(&comment("Linear", "hi")));
    }

    #[test]
    fn flags_a_body_containing_the_linkback_marker_regardless_of_author() {
        assert!(super::is_linear_comment(&comment("someone", "<!-- linear-linkback -->\nbody")));
    }

    #[test]
    fn leaves_an_ordinary_human_comment_alone() {
        assert!(!super::is_linear_comment(&comment("octocat", "looks good")));
    }

    #[test]
    fn visible_comments_drops_only_linear_comments() {
        let all = vec![comment("octocat", "lgtm"), comment("linear", "synced"), comment("linear[bot]", "x")];
        let visible = super::visible_comments(&all);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].author, "octocat");
    }
}
