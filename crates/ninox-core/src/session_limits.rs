//! Per-location concurrent session limit: display helpers over the spawn
//! queue in `store_queue.rs`, and the launcher hook the poller drains it
//! through.

use crate::config::AppConfig;
use crate::store::{QueueEntry, QueueKind, QueueState, Store};
use crate::types::{Session, SessionStatus};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

/// Starts one claimed queue entry. Installed by the UI layer (which owns
/// the spawn code) via `Poller::with_queue_launcher`; must release the
/// entry (`Store::release_queue_entry`) or requeue it when done.
pub type QueueLauncher = Arc<
    dyn Fn(QueueEntry) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync,
>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocationStatus {
    /// `None` = local.
    pub machine_id: Option<String>,
    pub label:      String,
    /// Workers only: orchestrators and the prompt agent don't count.
    pub running:    usize,
    pub orchestrators: usize,
    /// `None` = unlimited.
    pub limit:      Option<u32>,
    pub queued:     usize,
}

impl LocationStatus {
    /// "6/6 workers running locally · 3 orchestrators · 2 queued",
    /// "3 workers running on gpu-box" (unlimited).
    pub fn headline(&self) -> String {
        let place = if self.machine_id.is_none() { "locally".to_string() } else { format!("on {}", self.label) };
        let mut s = match self.limit {
            Some(l) => format!("{}/{l} workers running {place}", self.running),
            None => format!("{} workers running {place}", self.running),
        };
        if self.orchestrators > 0 {
            let n = self.orchestrators;
            s.push_str(&format!(" · {n} orchestrator{}", if n == 1 { "" } else { "s" }));
        }
        if self.queued > 0 {
            s.push_str(&format!(" · {} queued", self.queued));
        }
        s
    }
}

pub fn location_label(config: &AppConfig, machine_id: Option<&str>) -> String {
    match machine_id {
        None => "local".to_string(),
        Some(id) => config
            .remote_machines
            .machines
            .iter()
            .find(|m| m.id == id)
            .map(|m| m.label.clone())
            .unwrap_or_else(|| id.to_string()),
    }
}

/// Resolve a CLI location argument: `local`, a machine id, or a machine
/// label. `Ok(None)` = local.
pub fn parse_location(config: &AppConfig, raw: &str) -> anyhow::Result<Option<String>> {
    if raw.eq_ignore_ascii_case("local") {
        return Ok(None);
    }
    let machines = &config.remote_machines.machines;
    if let Some(m) = machines.iter().find(|m| m.id == raw).or_else(|| machines.iter().find(|m| m.label == raw)) {
        return Ok(Some(m.id.clone()));
    }
    let known: Vec<&str> = std::iter::once("local").chain(machines.iter().map(|m| m.label.as_str())).collect();
    anyhow::bail!("unknown location '{raw}' — known: {}", known.join(", "))
}

/// Local first, then every configured machine, then any other location that
/// has queued entries.
pub fn summarize(store: &Store, config: &AppConfig) -> anyhow::Result<Vec<LocationStatus>> {
    let queue = store.list_queue()?;
    let mut locations: Vec<Option<String>> = vec![None];
    locations.extend(config.remote_machines.machines.iter().map(|m| Some(m.id.clone())));
    let extra: BTreeSet<String> = queue.iter().filter_map(|e| e.location.clone()).collect();
    for id in extra {
        if !locations.contains(&Some(id.clone())) {
            locations.push(Some(id));
        }
    }
    locations
        .into_iter()
        .map(|loc| {
            let queued = queue
                .iter()
                .filter(|e| e.state == QueueState::Queued && e.location == loc)
                .count();
            Ok(LocationStatus {
                label: location_label(config, loc.as_deref()),
                running: store.running_session_count(loc.as_deref())?,
                orchestrators: store.running_orchestrator_count(loc.as_deref())?,
                limit: config.limits.limit_for(loc.as_deref()),
                queued,
                machine_id: loc,
            })
        })
        .collect()
}

/// 1-based position of each waiting entry within its own location's line.
pub fn queue_positions(queue: &[QueueEntry]) -> HashMap<String, usize> {
    let mut per_location: HashMap<Option<String>, usize> = HashMap::new();
    let mut out = HashMap::new();
    for e in queue.iter().filter(|e| e.state == QueueState::Queued) {
        let n = per_location.entry(e.location.clone()).or_default();
        *n += 1;
        out.insert(e.id.clone(), *n);
    }
    out
}

/// Display view of `sessions` with the waiting queue folded in: a waiting
/// relaunch shows its existing row as `Queued`, a waiting spawn appears as a
/// synthetic `Queued` row. Launching entries add nothing: the real row
/// appears once the spawn writes it, exactly as without a limit. Never
/// write the result back to the store.
pub fn overlay_queue(sessions: &mut Vec<Session>, queue: &[QueueEntry]) {
    for e in queue.iter().filter(|e| e.state == QueueState::Queued) {
        match sessions.iter_mut().find(|s| s.id == e.id) {
            Some(s) if e.kind == QueueKind::Relaunch => s.status = SessionStatus::Queued,
            None if e.kind == QueueKind::Spawn => {
                let mut s = e.session.clone();
                s.status = SessionStatus::Queued;
                sessions.push(s);
            }
            _ => {}
        }
    }
}

/// What a spawn prints when it was queued rather than started.
pub fn queued_message(id: &str, ahead: usize, location_label: &str) -> String {
    let place = if location_label == "local" { "this machine".to_string() } else { location_label.to_string() };
    format!("queued {id}: {ahead} ahead of you on {place} (session limit reached; it starts automatically when a slot frees)")
}

/// Starts as many queued sessions as each location has free slots, via
/// `launcher`. Re-reads `[limits]` every call, so a raised limit takes
/// effect on the next poll.
pub async fn drain(store: &Arc<Store>, launcher: &QueueLauncher, now: i64) {
    let config = AppConfig::load_or_default();
    let queue = match store.list_queue() {
        Ok(q) => q,
        Err(e) => {
            tracing::warn!("session queue: list: {e}");
            return;
        }
    };
    let locations: BTreeSet<Option<String>> = queue
        .iter()
        .filter(|e| e.state == QueueState::Queued)
        .map(|e| e.location.clone())
        .collect();
    for loc in locations {
        let claimed = match store.claim_queued(loc.as_deref(), config.limits.limit_for(loc.as_deref()), now) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("session queue: claim on {}: {e}", location_label(&config, loc.as_deref()));
                continue;
            }
        };
        for entry in claimed {
            tracing::info!("session queue: starting {} on {}", entry.id, location_label(&config, loc.as_deref()));
            tokio::spawn(launcher(entry));
        }
    }
}

#[cfg(test)]
mod tests;
