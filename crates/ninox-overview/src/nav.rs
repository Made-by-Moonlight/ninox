//! Fleet-tab navigation state: the open island, the selected island (which
//! an opened island's breadcrumb highlights even after it's closed — see
//! the brief's "selection preserved when an island closes"), the current
//! node selection, and the session tree's collapsed flag.

use crate::islands::{Island, IslandKey};
use crate::node::NodeId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetNav {
    pub selected: Option<NodeId>,
    pub selected_island: Option<IslandKey>,
    pub open: Option<IslandKey>,
    pub tree_collapsed: bool,
}

impl Default for FleetNav {
    fn default() -> Self { Self { selected: None, selected_island: None, open: None, tree_collapsed: true } }
}

impl FleetNav {
    /// Opens `key` and marks it selected on the Fleet tab. If the current
    /// node selection doesn't belong to `key`, it's reset to `key`'s root
    /// (or cleared, for Unparented) rather than left pointing at a node in
    /// a different island's detail panel.
    pub fn open_island(&mut self, key: IslandKey, islands: &[Island]) {
        let selection_belongs = self.selected.as_deref()
            .and_then(|id| owning_island(islands, id))
            .is_some_and(|owner| owner == key);
        if !selection_belongs {
            self.selected = islands.iter().find(|i| i.key == key).and_then(|i| i.root.clone());
        }
        self.selected_island = Some(key.clone());
        self.open = Some(key);
    }

    /// Clears only `open` — `selected` and `selected_island` are preserved
    /// so Esc returns to the Fleet tab with the island still selected.
    pub fn close_island(&mut self) { self.open = None; }

    /// Clears just the node selection (closes the detail panel/action row)
    /// — the first of the opened island's two Esc stages (slice 4 brief:
    /// "Esc first clears the selection... a second Esc goes back to
    /// Fleet"). Leaves `open`/`selected_island` alone, unlike
    /// `close_island`.
    pub fn clear_selection(&mut self) { self.selected = None; }

    pub fn select(&mut self, id: impl Into<NodeId>) { self.selected = Some(id.into()); }
    pub fn toggle_tree(&mut self) { self.tree_collapsed = !self.tree_collapsed; }

    /// Composite action for "click a worker in the tree": opens the worker's
    /// owning island (if found) and selects the worker regardless.
    pub fn open_node(&mut self, id: impl Into<NodeId>, islands: &[Island]) {
        let id = id.into();
        if let Some(key) = owning_island(islands, &id) {
            self.selected_island = Some(key.clone());
            self.open = Some(key);
        }
        self.selected = Some(id);
    }
}

pub fn owning_island(islands: &[Island], node_id: &str) -> Option<IslandKey> {
    islands.iter().find(|i| i.members.iter().any(|m| m.id == node_id)).map(|i| i.key.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::islands::{Island, IslandKey, IslandMember, IslandSummary};
    use crate::status::MemberStatus;

    fn island(key: IslandKey, root: Option<&str>, members: Vec<&str>) -> Island {
        Island {
            key, name: "x".into(), root: root.map(String::from),
            members: members.into_iter().map(|id| IslandMember { id: id.into(), status: MemberStatus::Working }).collect(),
            summary: IslandSummary::default(), machine: None, pending_request: false,
        }
    }

    #[test]
    fn tree_starts_collapsed_by_default() {
        assert!(FleetNav::default().tree_collapsed);
    }

    #[test]
    fn toggle_tree_flips_the_flag() {
        let mut nav = FleetNav::default();
        nav.toggle_tree();
        assert!(!nav.tree_collapsed);
    }

    #[test]
    fn closing_an_island_preserves_the_current_selection_and_the_selected_island() {
        let islands = vec![island(IslandKey::Root("root".into()), Some("root"), vec!["root", "leaf"])];
        let mut nav = FleetNav::default();
        nav.select("leaf");
        nav.open_island(IslandKey::Root("root".into()), &islands);
        nav.close_island();
        assert_eq!(nav.open, None);
        assert_eq!(nav.selected.as_deref(), Some("leaf"));
        assert_eq!(nav.selected_island, Some(IslandKey::Root("root".into())));
    }

    #[test]
    fn opening_an_island_the_current_selection_already_belongs_to_keeps_it() {
        let islands = vec![island(IslandKey::Root("root".into()), Some("root"), vec!["root", "leaf"])];
        let mut nav = FleetNav::default();
        nav.select("leaf");
        nav.open_island(IslandKey::Root("root".into()), &islands);
        assert_eq!(nav.selected.as_deref(), Some("leaf"));
    }

    #[test]
    fn opening_a_different_island_clears_a_selection_that_belonged_to_the_previous_one() {
        let islands = vec![
            island(IslandKey::Root("a".into()), Some("a"), vec!["a", "a-leaf"]),
            island(IslandKey::Root("b".into()), Some("b"), vec!["b"]),
        ];
        let mut nav = FleetNav::default();
        nav.select("a-leaf");
        nav.open_island(IslandKey::Root("a".into()), &islands);
        nav.open_island(IslandKey::Root("b".into()), &islands);
        assert_eq!(nav.selected.as_deref(), Some("b"), "falls back to b's root, not a's stale leaf");
    }

    #[test]
    fn clear_selection_drops_the_node_selection_but_keeps_the_island_open() {
        let islands = vec![island(IslandKey::Root("root".into()), Some("root"), vec!["root", "leaf"])];
        let mut nav = FleetNav::default();
        nav.open_island(IslandKey::Root("root".into()), &islands);
        nav.select("leaf");
        nav.clear_selection();
        assert_eq!(nav.selected, None);
        assert_eq!(nav.open, Some(IslandKey::Root("root".into())));
        assert_eq!(nav.selected_island, Some(IslandKey::Root("root".into())));
    }

    #[test]
    fn open_node_opens_the_owning_island_and_selects_the_node() {
        let islands = vec![island(IslandKey::Root("root".into()), Some("root"), vec!["root", "leaf"])];
        let mut nav = FleetNav::default();
        nav.open_node("leaf", &islands);
        assert_eq!(nav.open, Some(IslandKey::Root("root".into())));
        assert_eq!(nav.selected_island, Some(IslandKey::Root("root".into())));
        assert_eq!(nav.selected.as_deref(), Some("leaf"));
    }

    #[test]
    fn open_node_resolves_the_unparented_group_too() {
        let islands = vec![island(IslandKey::Unparented, None, vec!["solo"])];
        let mut nav = FleetNav::default();
        nav.open_node("solo", &islands);
        assert_eq!(nav.open, Some(IslandKey::Unparented));
    }

    #[test]
    fn owning_island_is_none_for_an_id_in_no_island() {
        let islands = vec![island(IslandKey::Root("root".into()), Some("root"), vec!["root"])];
        assert_eq!(owning_island(&islands, "nowhere"), None);
    }
}
