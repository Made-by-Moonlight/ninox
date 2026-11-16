//! An ACP-native session's live structured transcript (there is no raw
//! terminal to show), as both UIs buffer and label it.

use ninox_core::runtime::{ToolCallStatus, TranscriptEntry};

/// Per-session cap; the oldest entries drop first. Arbitrary, bump if a
/// real long-running ACP session needs more history on screen.
pub const MAX_ENTRIES: usize = 500;

pub const BANNER: &str = "ACP-native — no raw terminal (structured transcript)";

/// Appends `entry`, trimming the oldest once past [`MAX_ENTRIES`].
pub fn push(entries: &mut Vec<TranscriptEntry>, entry: TranscriptEntry) {
    entries.push(entry);
    if entries.len() > MAX_ENTRIES {
        let overflow = entries.len() - MAX_ENTRIES;
        entries.drain(0..overflow);
    }
}

pub fn status_icon(status: ToolCallStatus) -> &'static str {
    match status {
        ToolCallStatus::Pending => "⧗",
        ToolCallStatus::InProgress => "⚒",
        ToolCallStatus::Completed => "✓",
        ToolCallStatus::Failed => "✗",
    }
}

pub fn status_word(status: ToolCallStatus) -> &'static str {
    match status {
        ToolCallStatus::Pending => "pending",
        ToolCallStatus::InProgress => "in progress",
        ToolCallStatus::Completed => "completed",
        ToolCallStatus::Failed => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_trims_the_oldest_past_the_cap() {
        let mut v = Vec::new();
        for i in 0..MAX_ENTRIES + 3 {
            push(&mut v, TranscriptEntry::AgentText(i.to_string()));
        }
        assert_eq!(v.len(), MAX_ENTRIES);
        assert_eq!(v[0], TranscriptEntry::AgentText("3".into()));
    }
}
