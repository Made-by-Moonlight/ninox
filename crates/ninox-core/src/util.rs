//! Small helpers shared across modules that would otherwise each reimplement
//! them independently.

/// Truncate `s` to at most `max_chars` *characters* (not bytes — UTF-8 safe),
/// returning it unchanged when already within budget. Shared by every
/// call site that needs to cap a user- or agent-facing string before it rides
/// a hook payload or note field: [`crate::identity::render_reminder`],
/// [`crate::worker_status::format_permission_note`], and
/// [`crate::inbox::drain_batch`].
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() > max_chars {
        s.chars().take(max_chars).collect()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaves_a_string_within_budget_unchanged() {
        assert_eq!(truncate_chars("hello", 10), "hello");
        assert_eq!(truncate_chars("hello", 5), "hello");
    }

    #[test]
    fn truncates_to_exactly_max_chars() {
        assert_eq!(truncate_chars("hello world", 5), "hello");
    }

    #[test]
    fn counts_characters_not_bytes() {
        let s = "é".repeat(10); // 2 bytes each in UTF-8, 1 char each
        assert_eq!(truncate_chars(&s, 3).chars().count(), 3);
    }
}
