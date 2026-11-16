//! Single source of truth for "how do we launch editor X", shared by the
//! desktop app's "Open in editor" button (`app.rs`) and the TUI's `e`
//! action (`tui::state`/`tui::mod`).

use ninox_core::config::EditorChoice;

/// The CLI binary that opens a path in the configured editor. VS Code
/// (`code`) and Cursor (`cursor`) ship a `PATH` launcher that opens the
/// given path in the running GUI app; `nvim` opens the path itself as a
/// terminal editor (see `is_terminal`).
pub fn program(choice: EditorChoice) -> &'static str {
    match choice {
        EditorChoice::VsCode => "code",
        EditorChoice::Cursor => "cursor",
        EditorChoice::Neovim => "nvim",
    }
}

/// Whether `choice` needs an attached terminal to run in, rather than being
/// launchable fire-and-forget like a GUI editor. Neovim has no window of
/// its own: spawning it detached with no terminal does nothing useful.
pub fn is_terminal(choice: EditorChoice) -> bool {
    matches!(choice, EditorChoice::Neovim)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_maps_each_choice() {
        assert_eq!(program(EditorChoice::VsCode), "code");
        assert_eq!(program(EditorChoice::Cursor), "cursor");
        assert_eq!(program(EditorChoice::Neovim), "nvim");
    }

    #[test]
    fn only_neovim_needs_a_terminal() {
        assert!(!is_terminal(EditorChoice::VsCode));
        assert!(!is_terminal(EditorChoice::Cursor));
        assert!(is_terminal(EditorChoice::Neovim));
    }
}
