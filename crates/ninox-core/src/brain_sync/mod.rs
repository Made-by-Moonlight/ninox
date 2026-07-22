//! Remote brain sync: team-shared brains over S3-compatible storage.
//! See docs/superpowers/specs/2026-07-22-remote-brain-design.md.

pub mod config;
pub mod manifest;
pub mod store;

pub use config::{SyncToml, SYNC_TOML};
pub use manifest::{Manifest, ManifestEntry, SyncState, MANIFEST_KEY, SYNC_STATE};
pub use store::{GetResponse, InMemoryRemoteStore, PutOutcome, RemoteStore};
