//! How a notification's kind reads in a list — its stamp label and which
//! status tone colours it — shared by the GUI panel and the TUI list.

use ninox_core::types::NotificationKind;

/// Which status colour a notification takes; each app maps it to its own
/// palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Failed,
    Attention,
    Open,
    Done,
}

pub fn kind_label(kind: &NotificationKind) -> &'static str {
    match kind {
        NotificationKind::CiFailure => "CI",
        NotificationKind::AgentStuck => "Stuck",
        NotificationKind::PrNeedsAttention => "PR",
        NotificationKind::MergeConflict => "Conflict",
        NotificationKind::WorkerDone => "Done",
        NotificationKind::WorkerRetired => "Retired",
        NotificationKind::WorkRequested => "Work",
        NotificationKind::ExtraPr => "Extra PR",
        NotificationKind::GithubLookupFailed => "GitHub",
        NotificationKind::UpdateAvailable | NotificationKind::UpdateInstalled | NotificationKind::UpdateFailed => "Update",
        NotificationKind::RestartAllCompleted | NotificationKind::RestartAllFailed => "Restart",
        NotificationKind::PrReadyToMerge => "Ready",
        NotificationKind::MachineUnreachable | NotificationKind::MachineReachable => "Machine",
        NotificationKind::FleetRestorePending => "Restore",
        NotificationKind::InitialPromptFailed | NotificationKind::LaunchFailed => "Spawn",
        NotificationKind::DiskPressure => "Disk",
    }
}

pub fn kind_tone(kind: &NotificationKind) -> Tone {
    match kind {
        NotificationKind::CiFailure
        | NotificationKind::MergeConflict
        | NotificationKind::GithubLookupFailed
        | NotificationKind::UpdateFailed
        | NotificationKind::RestartAllFailed
        | NotificationKind::MachineUnreachable
        | NotificationKind::InitialPromptFailed
        | NotificationKind::LaunchFailed => Tone::Failed,
        NotificationKind::AgentStuck
        | NotificationKind::WorkerRetired
        | NotificationKind::ExtraPr
        | NotificationKind::DiskPressure
        | NotificationKind::PrNeedsAttention
        | NotificationKind::FleetRestorePending => Tone::Attention,
        NotificationKind::WorkRequested
        | NotificationKind::UpdateAvailable
        | NotificationKind::PrReadyToMerge => Tone::Open,
        NotificationKind::WorkerDone
        | NotificationKind::UpdateInstalled
        | NotificationKind::RestartAllCompleted
        | NotificationKind::MachineReachable => Tone::Done,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_read_as_failed_and_their_good_counterparts_as_done() {
        for (bad, good) in [
            (NotificationKind::UpdateFailed, NotificationKind::UpdateInstalled),
            (NotificationKind::RestartAllFailed, NotificationKind::RestartAllCompleted),
            (NotificationKind::MachineUnreachable, NotificationKind::MachineReachable),
        ] {
            assert_eq!(kind_label(&bad), kind_label(&good), "one stamp per family");
            assert_eq!((kind_tone(&bad), kind_tone(&good)), (Tone::Failed, Tone::Done));
        }
        assert_eq!(kind_tone(&NotificationKind::CiFailure), Tone::Failed);
        assert_eq!(kind_tone(&NotificationKind::AgentStuck), Tone::Attention);
        assert_eq!(kind_label(&NotificationKind::PrReadyToMerge), "Ready");
    }

    #[test]
    fn tones_match_the_notifications_board() {
        for (kind, tone) in [
            (NotificationKind::DiskPressure, Tone::Attention),
            (NotificationKind::PrNeedsAttention, Tone::Attention),
            (NotificationKind::UpdateAvailable, Tone::Open),
            (NotificationKind::PrReadyToMerge, Tone::Open),
            (NotificationKind::WorkerDone, Tone::Done),
        ] {
            assert_eq!(kind_tone(&kind), tone, "{kind:?}");
        }
    }

    #[test]
    fn an_interrupted_fleet_reads_as_a_restore_needing_attention() {
        assert_eq!(kind_label(&NotificationKind::FleetRestorePending), "Restore");
        assert_eq!(kind_tone(&NotificationKind::FleetRestorePending), Tone::Attention);
    }
}
