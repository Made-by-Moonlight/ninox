//! The Remotes tab: a read-only list of configured remote machines
//! (`AppConfig.remote_machines.machines`) joined with their last-observed
//! connection health (`Store::machine_health`) — both already collected
//! elsewhere in the app but never rendered in `ninox-tui` before v1.
//!
//! `RemoteRow`/`collect` live in `ninox_ui_shared::machine` so the GUI's
//! Remotes tab can join the same data without duplicating this logic.
pub use ninox_ui_shared::machine::{collect_rows as collect, RemoteRow};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RemotesView {
    pub selected: usize,
}

impl RemotesView {
    pub fn move_sel(&mut self, delta: i32, len: usize) {
        if len == 0 {
            self.selected = 0;
            return;
        }
        let max = len - 1;
        let next = self.selected as i32 + delta;
        self.selected = next.clamp(0, max as i32) as usize;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use ninox_core::config::{AppConfig, MachineProfile};
    use ninox_core::types::MachineHealth;

    fn profile(id: &str, label: &str) -> MachineProfile {
        MachineProfile {
            id: id.to_string(),
            label: label.to_string(),
            ssh_target: format!("user@{id}"),
            remote_session: "default".to_string(),
            enabled: true,
        }
    }

    #[test]
    fn collect_joins_configured_machines_with_health() {
        let mut cfg = AppConfig::default();
        cfg.remote_machines.machines.push(profile("m1", "Laptop"));
        cfg.remote_machines.machines.push(profile("m2", "Desktop"));
        let mut health = HashMap::new();
        health.insert("m1".to_string(), MachineHealth::Connected);

        let rows = super::collect(&cfg, &health);

        assert_eq!(rows.len(), 2);
        let m1 = rows.iter().find(|r| r.id == "m1").expect("m1 row present");
        assert_eq!(m1.health, Some(MachineHealth::Connected));
        let m2 = rows.iter().find(|r| r.id == "m2").expect("m2 row present");
        assert_eq!(m2.health, None, "no health entry recorded for m2 yet");
    }

    #[test]
    fn move_sel_clamps_at_zero_and_at_the_max() {
        let mut v = super::RemotesView::default();
        v.move_sel(-1, 3);
        assert_eq!(v.selected, 0);
        v.move_sel(2, 3);
        assert_eq!(v.selected, 2);
        v.move_sel(1, 3);
        assert_eq!(v.selected, 2, "must clamp at len - 1");
    }
}
