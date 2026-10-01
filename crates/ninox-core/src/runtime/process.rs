//! Process-tree lookups for panes that aren't tmux panes — a ptyd pane is
//! only identifiable by its root pid.

use std::collections::HashMap;

/// pid → parent pid for every process, from one `ps` call.
pub(crate) fn parent_map() -> HashMap<u32, u32> {
    let Ok(output) = std::process::Command::new("/bin/ps").args(["-A", "-o", "pid=,ppid="]).output() else {
        return HashMap::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            Some((cols.next()?.parse().ok()?, cols.next()?.parse().ok()?))
        })
        .collect()
}

/// Whether `pid` is `root` or one of its descendants.
pub(crate) fn descends_in(parents: &HashMap<u32, u32>, pid: u32, root: u32) -> bool {
    let mut current = pid;
    // Bounded so a pid-reuse cycle in a stale snapshot can't spin forever.
    for _ in 0..256 {
        if current == root {
            return true;
        }
        match parents.get(&current) {
            Some(&parent) if parent != current && current > 1 => current = parent,
            _ => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descends_walks_the_parent_chain() {
        let parents: HashMap<u32, u32> = [(40, 30), (30, 20), (20, 1), (99, 1)].into();
        assert!(descends_in(&parents, 40, 20));
        assert!(descends_in(&parents, 20, 20));
        assert!(!descends_in(&parents, 99, 20));
        assert!(!descends_in(&parents, 20, 40));
        assert!(!descends_in(&parents, 5, 20), "unknown pids descend from nothing");
    }

    #[test]
    fn descends_terminates_on_a_cycle() {
        let parents: HashMap<u32, u32> = [(10, 11), (11, 10)].into();
        assert!(!descends_in(&parents, 10, 3));
    }

    #[test]
    fn parent_map_sees_this_process() {
        let parents = parent_map();
        assert_eq!(parents.get(&std::process::id()).copied(), Some(std::os::unix::process::parent_id()));
    }
}
