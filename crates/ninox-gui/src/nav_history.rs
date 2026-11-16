//! Browser-style back/forward history over [`Location`]s.

use crate::app::View;
use ninox_overview::islands::IslandKey;
use ninox_overview::node::NodeId;

pub const MAX_DEPTH: usize = 100;

#[derive(Debug, Clone, PartialEq)]
pub struct Location {
    pub view: View,
    pub open: Option<IslandKey>,
    pub selected: Option<NodeId>,
    pub selected_island: Option<IslandKey>,
}

impl Location {
    /// Selection is saved for restore but doesn't make a place distinct: only
    /// the view (including the session panel) and the open island do.
    pub fn same_place(&self, other: &Location) -> bool {
        self.view == other.view && self.open == other.open
    }
}

#[derive(Debug, Default)]
pub struct NavHistory {
    back: Vec<Location>,
    forward: Vec<Location>,
}

impl NavHistory {
    #[cfg(test)]
    pub fn can_back(&self) -> bool { !self.back.is_empty() }
    #[cfg(test)]
    pub fn can_forward(&self) -> bool { !self.forward.is_empty() }

    /// Records `from` as the place the user just left.
    pub fn push(&mut self, from: Location) {
        self.forward.clear();
        if self.back.last().is_some_and(|l| l.same_place(&from)) {
            return;
        }
        self.back.push(from);
        if self.back.len() > MAX_DEPTH {
            self.back.remove(0);
        }
    }

    /// Steps back from `current`, skipping entries `is_stale` rejects.
    /// Returns the target and how many stale entries were dropped.
    pub fn back(&mut self, current: Location, is_stale: impl Fn(&Location) -> bool) -> Option<(Location, usize)> {
        Self::step(&mut self.back, &mut self.forward, current, is_stale)
    }

    pub fn forward(&mut self, current: Location, is_stale: impl Fn(&Location) -> bool) -> Option<(Location, usize)> {
        Self::step(&mut self.forward, &mut self.back, current, is_stale)
    }

    fn step(
        from: &mut Vec<Location>,
        to: &mut Vec<Location>,
        current: Location,
        is_stale: impl Fn(&Location) -> bool,
    ) -> Option<(Location, usize)> {
        let mut skipped = 0;
        while let Some(target) = from.pop() {
            if is_stale(&target) || target.same_place(&current) {
                skipped += usize::from(is_stale(&target));
                continue;
            }
            if !to.last().is_some_and(|l| l.same_place(&current)) {
                to.push(current);
                if to.len() > MAX_DEPTH {
                    to.remove(0);
                }
            }
            return Some((target, skipped));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::session_detail::DetailPanel;

    fn loc(view: View) -> Location {
        Location { view, open: None, selected: None, selected_island: None }
    }
    fn session(id: &str) -> Location {
        loc(View::SessionDetail { session_id: id.into(), panel: DetailPanel::Terminal })
    }
    fn never(_: &Location) -> bool { false }

    #[test]
    fn push_back_forward_round_trip() {
        let mut h = NavHistory::default();
        h.push(loc(View::Fleet));
        h.push(session("o1"));
        let (t, _) = h.back(session("w1"), never).unwrap();
        assert_eq!(t, session("o1"));
        let (t, _) = h.back(session("o1"), never).unwrap();
        assert_eq!(t, loc(View::Fleet));
        assert!(h.back(loc(View::Fleet), never).is_none());
        let (t, _) = h.forward(loc(View::Fleet), never).unwrap();
        assert_eq!(t, session("o1"));
        let (t, _) = h.forward(session("o1"), never).unwrap();
        assert_eq!(t, session("w1"));
        assert!(!h.can_forward());
    }

    #[test]
    fn consecutive_duplicates_collapse() {
        let mut h = NavHistory::default();
        h.push(loc(View::Fleet));
        h.push(loc(View::Fleet));
        assert_eq!(h.back.len(), 1);
    }

    #[test]
    fn selection_only_difference_is_a_duplicate() {
        let mut h = NavHistory::default();
        h.push(loc(View::Fleet));
        let mut b = loc(View::Fleet);
        b.selected = Some("w1".into());
        h.push(b);
        assert_eq!(h.back.len(), 1);
    }

    #[test]
    fn depth_is_capped_dropping_the_oldest() {
        let mut h = NavHistory::default();
        for i in 0..MAX_DEPTH + 5 {
            h.push(session(&i.to_string()));
        }
        assert_eq!(h.back.len(), MAX_DEPTH);
        assert_eq!(h.back[0], session("5"));
    }

    #[test]
    fn new_navigation_clears_forward() {
        let mut h = NavHistory::default();
        h.push(loc(View::Fleet));
        h.back(session("a"), never).unwrap();
        assert!(h.can_forward());
        h.push(loc(View::Brain));
        assert!(!h.can_forward());
    }

    #[test]
    fn stale_entries_are_skipped_and_counted() {
        let mut h = NavHistory::default();
        h.push(loc(View::Fleet));
        h.push(session("dead"));
        h.push(session("dead2"));
        let (t, skipped) = h.back(loc(View::Brain), |l| matches!(&l.view, View::SessionDetail { session_id, .. } if session_id.starts_with("dead"))).unwrap();
        assert_eq!(t, loc(View::Fleet));
        assert_eq!(skipped, 2);
    }

    #[test]
    fn all_stale_leaves_nothing_to_go_back_to() {
        let mut h = NavHistory::default();
        h.push(session("dead"));
        assert!(h.back(loc(View::Brain), |_| true).is_none());
    }
}
