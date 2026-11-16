pub mod acp_client;
pub mod brain;
pub mod brain_archive;
pub mod brain_fields;
pub mod brain_set;
pub mod brain_sync;
pub mod capabilities;
pub mod claude_settings;
pub mod client;
pub mod config;
pub mod config_cli;
pub mod daemon;
pub mod daemon_log;
pub mod embeddings;
pub mod events;
pub mod fleet;
pub mod ghost_plan;
pub mod github;
pub mod github_budget;
pub mod github_graphql;
pub mod harness;
pub mod harness_auth;
pub mod hooks;
pub mod human_console_auth;
pub mod identity;
pub mod idle_workers;
pub mod inbox;
pub mod lifecycle;
pub mod messaging;
pub mod model_resolution;
pub mod orchestrator_auth;
pub mod orchestrator_root;
pub mod plugin;
pub mod pr_events;
pub mod prompt_reply;
pub mod pty;
pub mod remote;
pub mod runtime;
pub mod server_control;
pub mod session_limits;
pub mod session_socket;
pub mod store;
pub mod test_isolation;
pub mod tmux;
pub mod trust;
pub mod types;
pub mod util;
pub mod worker_status;
pub mod worker_type;
pub mod worker_type_cli;
pub mod workspace;

pub use brain::{BrainEntry, BrainIndex, BrainScope, BrainScopeId, MergedBrainIndex, MergedSource, QueryFilters};
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
