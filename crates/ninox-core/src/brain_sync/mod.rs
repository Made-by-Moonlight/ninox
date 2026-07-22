//! Remote brain sync: team-shared brains over S3-compatible storage.
//! See docs/superpowers/specs/2026-07-22-remote-brain-design.md.

pub mod config;

pub use config::{SyncToml, SYNC_TOML};
