pub mod brain;
pub mod config;
pub mod events;
pub mod github;
pub mod hooks;
pub mod lifecycle;
pub mod plugin;
pub mod pty;
pub mod store;
pub mod tmux;
pub mod types;

pub use brain::{BrainEntry, BrainIndex, QueryFilters};
pub use config::{AppConfig, ThemeVariant};
pub use events::{Engine, Event};
pub use store::Store;
pub use types::*;

/// Convert a name into a tmux/URL-safe session ID.
///
/// Lowercases, maps non-alphanumeric chars to hyphens, and collapses consecutive
/// hyphens. Returns an empty string if the input contains no alphanumeric chars.
///
/// Examples: `"ATH-123 auth fix"` → `"ath-123-auth-fix"`, `"my feature"` → `"my-feature"`
pub fn slugify(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_ticket_id() {
        assert_eq!(slugify("ATH-123"), "ath-123");
    }

    #[test]
    fn slugify_spaced_name() {
        assert_eq!(slugify("auth fix"), "auth-fix");
    }

    #[test]
    fn slugify_ticket_with_description() {
        assert_eq!(slugify("ATH-123 auth fix"), "ath-123-auth-fix");
    }

    #[test]
    fn slugify_collapses_consecutive_separators() {
        assert_eq!(slugify("my  feature!"), "my-feature");
    }

    #[test]
    fn slugify_already_clean() {
        assert_eq!(slugify("my-feature"), "my-feature");
    }

    #[test]
    fn slugify_empty_returns_empty() {
        assert_eq!(slugify(""), "");
        assert_eq!(slugify("---"), "");
    }
}
