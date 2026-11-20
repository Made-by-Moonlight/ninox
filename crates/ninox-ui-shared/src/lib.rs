//! Framework-agnostic logic shared by the `ninox-gui` (iced) and
//! `ninox-tui` (ratatui) binaries: session lifecycle CLI commands, the
//! shared daemon/engine bootstrap, and toolkit-neutral theme/input types.

pub mod brain_edit;
pub mod brain_sources;
pub mod cli;
pub mod delete_view;
pub mod connect;
pub mod context_meter;
pub mod editor;
pub mod fleet;
pub mod fleet_import;
pub mod ghost_accept;
pub mod ghost_view;
pub mod idle_cli;
pub mod input;
pub mod machine;
pub mod orchestrator_auto_reset;
pub mod orchestrator_plans;
pub mod models;
pub mod pr_status;
pub mod prompt_agent;
pub mod reset_from_plan;
pub mod restart;
pub mod runtime_cli;
pub mod server_restart;
pub mod service;
pub mod session_queue;
pub mod settings_form;
pub mod spawn_form;
pub mod spawn_util;
pub mod theme;
pub mod transcript;
pub mod update_cli;
