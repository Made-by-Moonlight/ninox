//! Debug builds only (`super::seed_reply_fixture`):
//! `NINOX_TUI_REPLY_FIXTURE=open|collapsed|cleared|thinking` seeds the Ninox prompt
//! reply panel at startup, so headless captures can show it without a live
//! prompt agent. Turns are left `Unresolved`, so nothing ever polls them.

use ninox_ui_shared::prompt_agent::reply::AnswerState;

use super::fleet::ReplyPanel;

const SHORT_ANSWER: &str = "No. **#1099** is green and waiting on review from the bellhop orchestrator.";

const LONG_ANSWER: &str = "Bellhop's dashboard work is spread over `mlops-5372-bellhop-dashboard-resources` (PR #1099, open) and the GC backlog worker. I'd split the migration into one orchestrator with three workers, so each slice can land on its own.

## Proposed split

1. **Map** every panel's query against the old API.
2. **Port** the panels to the resources API, once the map lands.
3. **Cut over** behind a flag, then delete the old client.

The port is mostly mechanical:

```rust
let client = ResourcesClient::new(&cfg);
for panel in dashboard.panels() {
    client.migrate(panel.query())?;
}
```

Nothing is spawned until you accept.";

pub fn seed(panel: &mut ReplyPanel, mode: &str, now_ms: i64) {
    for (question, answer) in [
        ("Is #1099 blocked?", SHORT_ANSWER),
        ("I want to migrate the bellhop dashboard to the new resources API — propose how to split it.", LONG_ANSWER),
    ] {
        panel.ask(question.into(), "ninox".into(), now_ms);
        if let Some(turn) = panel.drawer.latest_mut() {
            turn.answer = answer.into();
            turn.state = AnswerState::Done;
            turn.ended_at_ms = Some(now_ms);
        }
    }
    panel.drawer.context_pct = Some(18.0);
    match mode {
        "collapsed" => panel.collapse(),
        "cleared" => panel.cleared(),
        "thinking" => {
            panel.ask("What's left on the bellhop migration?".into(), "ninox".into(), now_ms);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_each_capture_state() {
        let mut open = ReplyPanel::default();
        seed(&mut open, "open", 1);
        assert!(open.is_open());
        assert_eq!(open.drawer.turns.len(), 2);

        let mut collapsed = ReplyPanel::default();
        seed(&mut collapsed, "collapsed", 1);
        assert!(!collapsed.is_open() && collapsed.drawer.has_content());

        let mut cleared = ReplyPanel::default();
        seed(&mut cleared, "cleared", 1);
        assert!(cleared.is_open() && cleared.drawer.turns.is_empty());

        let mut thinking = ReplyPanel::default();
        seed(&mut thinking, "thinking", 1);
        assert!(thinking.drawer.shows_thinking());
    }

    #[test]
    fn seeded_turns_never_poll() {
        let mut panel = ReplyPanel::default();
        seed(&mut panel, "thinking", 1);
        assert!(panel.drawer.turns.iter().all(|t| !t.wants_poll(2)));
    }
}
