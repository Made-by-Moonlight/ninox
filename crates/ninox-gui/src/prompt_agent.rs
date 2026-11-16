//! G2 (round-1 try-out feedback): the ninox prompt must actually deliver
//! somewhere and show a reply, not just clear the draft. Two delivery
//! paths, both real now that MLOPS-5415 (PR #263) has merged:
//!
//! - A **directly-picked orchestrator** (the target chip's picker): the
//!   same [`ninox_core::messaging::deliver_message`] call
//!   `Message::FleetSendMessageSubmit` already uses, and its reply stream
//!   is [`ninox_core::runtime::read_screen`] polled on the existing 3s
//!   `PollSessions` tick — the same source `ninox read` uses.
//! - The **prompt agent** (the default target): `app.rs` calls
//!   [`ninox_ui_shared::prompt_agent::ask`]/[`clear`] directly — this
//!   module no longer wraps them. (A pre-#263 version of this file had a
//!   client-side stub here that fell back to messaging the most recently
//!   active local orchestrator when the agent didn't exist yet; removed
//!   now that it does.)

/// Who the prompt's text goes to. `PromptAgent` is the default; the
/// target chip's picker can override it to a specific orchestrator.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PromptTarget {
    #[default]
    PromptAgent,
    Orchestrator {
        id: String,
        name: String,
    },
}

impl PromptTarget {
    /// The `→ ninox ▾` chip's label.
    pub fn label(&self) -> &str {
        match self {
            PromptTarget::PromptAgent => "ninox",
            PromptTarget::Orchestrator { name, .. } => name,
        }
    }
}

/// What actually gets sent, resolved from the current target, the opened
/// island's scope (if any), and the draft text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptDelivery {
    ToPromptAgent { text: String },
    ToOrchestrator { id: String, text: String },
}

/// `scope`: the opened island's orchestrator name, when the Fleet tab has
/// one open. Only prefixed onto a prompt-agent delivery — a directly
/// picked orchestrator already knows unambiguously who it is, so scope
/// framing would be noise there (round-1 feedback item 1).
pub fn resolve_delivery(target: &PromptTarget, scope: Option<&str>, draft: &str) -> PromptDelivery {
    match target {
        PromptTarget::PromptAgent => {
            let text = match scope {
                Some(name) => format!("[scope: {name}] {draft}"),
                None => draft.to_string(),
            };
            PromptDelivery::ToPromptAgent { text }
        }
        PromptTarget::Orchestrator { id, .. } => PromptDelivery::ToOrchestrator {
            id: id.clone(),
            text: draft.to_string(),
        },
    }
}

/// The conversation state lives in `ninox-ui-shared` (shared with the TUI
/// reply panel); the GUI's per-turn render cache is iced Markdown, parsed
/// (with session/PR links) by `app.rs` after each change.
pub use ninox_ui_shared::prompt_agent::reply::{
    AnswerSource, AnswerState, DrawerStatus, Notice, PromptPoll, Relay,
};

pub type Turn = ninox_ui_shared::prompt_agent::reply::Turn<Vec<iced::widget::markdown::Item>>;
pub type ReplyDrawer = ninox_ui_shared::prompt_agent::reply::ReplyDrawer<Vec<iced::widget::markdown::Item>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_target_is_the_prompt_agent() {
        assert_eq!(PromptTarget::default(), PromptTarget::PromptAgent);
    }

    #[test]
    fn unscoped_prompt_agent_delivery_sends_the_draft_as_is() {
        let delivery = resolve_delivery(&PromptTarget::PromptAgent, None, "spawn an orchestrator");
        assert_eq!(
            delivery,
            PromptDelivery::ToPromptAgent {
                text: "spawn an orchestrator".to_string()
            }
        );
    }

    #[test]
    fn scoped_prompt_agent_delivery_prefixes_the_scope() {
        let delivery = resolve_delivery(
            &PromptTarget::PromptAgent,
            Some("Ninox"),
            "why is it paused?",
        );
        assert_eq!(
            delivery,
            PromptDelivery::ToPromptAgent {
                text: "[scope: Ninox] why is it paused?".to_string()
            },
        );
    }

    #[test]
    fn picked_orchestrator_delivery_never_gets_a_scope_prefix() {
        let target = PromptTarget::Orchestrator {
            id: "orch-1".into(),
            name: "Bellhop".into(),
        };
        let delivery = resolve_delivery(&target, Some("Ninox"), "status?");
        assert_eq!(
            delivery,
            PromptDelivery::ToOrchestrator {
                id: "orch-1".to_string(),
                text: "status?".to_string()
            },
            "a directly picked orchestrator already knows who it is — no [scope: …] noise",
        );
    }

    #[test]
    fn target_label_matches_the_chip_text() {
        assert_eq!(PromptTarget::PromptAgent.label(), "ninox");
        let target = PromptTarget::Orchestrator {
            id: "o1".into(),
            name: "Bellhop".into(),
        };
        assert_eq!(target.label(), "Bellhop");
    }
}
