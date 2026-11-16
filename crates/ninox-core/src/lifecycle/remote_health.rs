//! Reconnection-with-backoff policy for the background remote-machine health
//! probe — design spec §6
//! (`docs/superpowers/specs/2026-10-05-remote-sessions-design.md`). The
//! actual per-tick bookkeeping (due-time gating, notify-once dedup) lives on
//! `Poller`, mirroring how `update_check.rs` holds the pluggable source while
//! `Poller::last_notified_update` holds the dedup state.

use anyhow::{Context, Result};
use async_trait::async_trait;

/// Attempts to (re)establish a machine profile's SSH control connection —
/// real production code uses [`SshMachineHealthProbe`]; tests inject a fake
/// so they never shell out to a real `ssh`.
#[async_trait]
pub trait MachineHealthProbe: Send + Sync {
    /// `Ok(())` when `target`'s control connection is alive (already was, or
    /// was just non-interactively re-established). `profile_id` derives the
    /// control socket path ([`crate::remote::control_socket_path`]) — the
    /// same one every other remote operation for this profile uses.
    async fn reconnect(&self, target: &str, profile_id: &str) -> Result<()>;
}

/// Shells out to a real, non-interactive `ssh` — see
/// [`crate::remote::reconnect_control_master`]'s own doc comment for why
/// this is safe to call unattended (never installs anything, never prompts).
pub struct SshMachineHealthProbe;

#[async_trait]
impl MachineHealthProbe for SshMachineHealthProbe {
    async fn reconnect(&self, target: &str, profile_id: &str) -> Result<()> {
        let target = target.to_string();
        let control_path = crate::remote::control_socket_path(profile_id);
        tokio::task::spawn_blocking(move || crate::remote::reconnect_control_master(&target, &control_path))
            .await
            .context("remote health probe task panicked")?
    }
}

/// Backoff cap "around two minutes" per the design spec's §6 (mirroring
/// Herdr). Doubles-up to this from the 5s floor; also the threshold
/// `Poller` uses to tell a brief blip (`MachineHealth::Reconnecting`) apart
/// from a sustained outage (`MachineHealth::Unreachable`) — reusing this
/// already-meaningful number rather than inventing a second one.
pub const MAX_BACKOFF_SECS: u64 = 120;

/// Next backoff duration (seconds) after a failed reconnect attempt: doubles
/// from a 5s floor, capped at [`MAX_BACKOFF_SECS`]. `current` is the
/// previously applied backoff (0 = none established yet, i.e. this is the
/// first consecutive failure).
pub fn next_backoff_secs(current: u64) -> u64 {
    if current == 0 {
        5
    } else {
        (current * 2).min(MAX_BACKOFF_SECS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_backoff_secs_doubles_from_a_five_second_floor() {
        assert_eq!(next_backoff_secs(0), 5);
        assert_eq!(next_backoff_secs(5), 10);
        assert_eq!(next_backoff_secs(10), 20);
        assert_eq!(next_backoff_secs(20), 40);
        assert_eq!(next_backoff_secs(40), 80);
    }

    #[test]
    fn next_backoff_secs_caps_around_two_minutes() {
        assert_eq!(next_backoff_secs(80), 120);
        assert_eq!(next_backoff_secs(120), 120);
        assert_eq!(next_backoff_secs(1_000_000), 120);
    }
}
