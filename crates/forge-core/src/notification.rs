//! Notifications (M3 N1, plan 11 §5 N1, ADR-0016 Amendment N1): the kinds a
//! person is told about and the delivery state of one notification.
//!
//! The kinds are the events the merge request flow already publishes and
//! whose recipient is known: the author of a merge request hears when it is
//! approved, merged or commented. A "review requested" kind and a mention
//! kind join with their events (none exists yet), a push-mirror failure kind
//! with O2; the table CHECKs and this list change together, by migration.

use crate::event_type::EventType;

/// What a notification is about. `as_str` is the stored value
/// (`forge.notifications.kind`, `notification_preferences.muted_kinds`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NotificationKind {
    /// `merge_request.approved`: someone approved your merge request.
    MergeRequestApproved,
    /// `merge_request.merged`: your merge request was merged.
    MergeRequestMerged,
    /// `merge_request.commented`: a note was added to your merge request.
    MergeRequestCommented,
}

impl NotificationKind {
    pub const ALL: &'static [NotificationKind] = &[
        Self::MergeRequestApproved,
        Self::MergeRequestMerged,
        Self::MergeRequestCommented,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::MergeRequestApproved => "mr_approved",
            Self::MergeRequestMerged => "mr_merged",
            Self::MergeRequestCommented => "mr_commented",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_str() == raw)
    }

    /// The kind an event of `event` is told as, `None` for an event nobody
    /// is notified about. Exhaustive: a new event type is decided here.
    pub fn from_event(event: EventType) -> Option<Self> {
        match event {
            EventType::MergeRequestApproved => Some(Self::MergeRequestApproved),
            EventType::MergeRequestMerged => Some(Self::MergeRequestMerged),
            EventType::MergeRequestCommented => Some(Self::MergeRequestCommented),
            EventType::ProjectCreated
            | EventType::RepositoryCreated
            | EventType::TokenCreated
            | EventType::TokenRevoked
            | EventType::MemberAdded
            | EventType::MemberRoleChanged
            | EventType::MemberRemoved
            | EventType::ProtectedRefCreated
            | EventType::ProtectedRefUpdated
            | EventType::ProtectedRefDeleted
            | EventType::RepositoryPushed
            | EventType::ProjectVisibilityChanged
            | EventType::ProjectDeleted
            | EventType::ProjectRestored
            | EventType::ProjectPurged
            | EventType::ProjectMoved
            | EventType::GroupMoved
            | EventType::UserRenamed
            | EventType::MergeRequestOpened
            | EventType::MergeRequestUpdated
            | EventType::MergeRequestDraftChanged
            | EventType::MergeRequestClosed
            | EventType::MergeRequestReopened
            | EventType::MergeRequestPushed
            | EventType::MergeRequestConflictDetected
            | EventType::MergeRequestConflictResolved
            | EventType::MergeRequestSourceBranchDeleted
            | EventType::MergeRequestUnapproved
            | EventType::CodeownersEnforcementChanged
            | EventType::CodeownersEnforcementRestored
            | EventType::CheckCompleted
            | EventType::WorkItemLinked
            | EventType::WorkItemUnlinked => None,
            // M3 I2: the automatic disable of a webhook (maintainer class).
            // Not mailed yet: every notification kind here is about one
            // merge request and its author (`forge.notifications` keys on
            // the merge request), while this one would go to the project's
            // Maintainers. Its own kind and recipients come with U4 (the
            // webhook and notification screens); until then FG-INT-0011 and
            // doctor `webhook.deliveries` report it.
            EventType::WebhookDisabled => None,
        }
    }
}

/// Where one notification is in its delivery (`forge.notifications.state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotificationState {
    /// Queued, not tried yet.
    Pending,
    /// A send is in progress (or died in progress: then it becomes
    /// `Unknown`, never sent again).
    Sending,
    /// A try failed before the server took the message; another follows.
    Retrying,
    /// The server accepted the message.
    Sent,
    /// Not delivered: refused for good, or the tries are used up.
    Failed,
    /// The outcome is not known (the connection broke after the message was
    /// handed over, or the worker died while sending): not sent again.
    Unknown,
    /// Not sent by the recipient's choice (e-mail off, kind muted).
    Skipped,
    /// Not sent because the recipient cannot be written to or can no longer
    /// see the project.
    Suppressed,
}

impl NotificationState {
    pub const ALL: &'static [NotificationState] = &[
        Self::Pending,
        Self::Sending,
        Self::Retrying,
        Self::Sent,
        Self::Failed,
        Self::Unknown,
        Self::Skipped,
        Self::Suppressed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Sending => "sending",
            Self::Retrying => "retrying",
            Self::Sent => "sent",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
            Self::Skipped => "skipped",
            Self::Suppressed => "suppressed",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|s| s.as_str() == raw)
    }

    /// No further delivery work will happen.
    pub fn is_terminal(self) -> bool {
        match self {
            Self::Pending | Self::Sending | Self::Retrying => false,
            Self::Sent | Self::Failed | Self::Unknown | Self::Skipped | Self::Suppressed => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_names_are_unique_stored_words_and_round_trip() {
        let mut seen = std::collections::HashSet::new();
        for kind in NotificationKind::ALL {
            let name = kind.as_str();
            assert!(
                name.starts_with(|c: char| c.is_ascii_lowercase())
                    && name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{name}"
            );
            assert!(seen.insert(name), "duplicate {name}");
            assert_eq!(NotificationKind::parse(name), Some(*kind));
        }
        assert_eq!(NotificationKind::parse("MR_MERGED"), None);
        assert_eq!(NotificationKind::parse(""), None);
        assert_eq!(NotificationKind::parse("mr_merged "), None);
    }

    #[test]
    fn exactly_the_planned_events_notify() {
        let mut notifying = Vec::new();
        for event in EventType::ALL {
            if let Some(kind) = NotificationKind::from_event(event) {
                notifying.push((event.as_str(), kind.as_str()));
            }
        }
        notifying.sort_unstable();
        assert_eq!(
            notifying,
            [
                ("merge_request.approved", "mr_approved"),
                ("merge_request.commented", "mr_commented"),
                ("merge_request.merged", "mr_merged"),
            ]
        );
        // Every kind is reached by some event.
        for kind in NotificationKind::ALL {
            assert!(
                EventType::ALL
                    .iter()
                    .any(|e| NotificationKind::from_event(*e) == Some(*kind)),
                "{kind:?} has no event"
            );
        }
    }

    #[test]
    fn state_names_round_trip_and_the_terminal_ones_are_final() {
        let mut terminal = Vec::new();
        for state in NotificationState::ALL {
            assert_eq!(NotificationState::parse(state.as_str()), Some(*state));
            if state.is_terminal() {
                terminal.push(state.as_str());
            }
        }
        terminal.sort_unstable();
        assert_eq!(
            terminal,
            ["failed", "sent", "skipped", "suppressed", "unknown"]
        );
        assert_eq!(NotificationState::parse("done"), None);
    }
}
