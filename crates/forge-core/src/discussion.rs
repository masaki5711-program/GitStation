//! Merge request discussions (M3 R3, ADR-0024 §6, §12; plan 11 §4.1): the
//! kind of a thread, the side of a diff line it sits on, and what a
//! `merge_request.commented` event says happened.
//!
//! A discussion is `general` (on the merge request) or `inline` (on one
//! line of one side of one version's diff, `merge_base..head`). Its notes
//! are plain text / Markdown stored as written; the server never renders
//! them. Resolving is a separate permission (`DiscussionResolve`, people
//! only) from commenting (`MergeRequestComment`).

/// `general` or `inline` (the CHECK of forge.merge_request_discussions.kind).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiscussionKind {
    General,
    Inline,
}

impl DiscussionKind {
    pub const ALL: &'static [DiscussionKind] = &[Self::General, Self::Inline];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Inline => "inline",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_str() == raw)
    }
}

/// The side of a diff an inline discussion sits on: `old` (the version's
/// merge base) or `new` (the version's head). The CHECK of
/// forge.merge_request_discussions.side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiscussionSide {
    Old,
    New,
}

impl DiscussionSide {
    pub const ALL: &'static [DiscussionSide] = &[Self::Old, Self::New];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Old => "old",
            Self::New => "new",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|s| s.as_str() == raw)
    }
}

/// What a `merge_request.commented` event reports (its `data.action`,
/// ADR-0024 §6: one event type for comments and thread resolution).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommentAction {
    /// A note was added: the first note of a new discussion
    /// (`discussion_started: true`) or a reply.
    NoteAdded,
    /// A discussion was resolved.
    ThreadResolved,
    /// A resolved discussion was opened again.
    ThreadUnresolved,
}

impl CommentAction {
    pub const ALL: &'static [CommentAction] = &[
        Self::NoteAdded,
        Self::ThreadResolved,
        Self::ThreadUnresolved,
    ];

    /// The `data.action` value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoteAdded => "note_added",
            Self::ThreadResolved => "thread_resolved",
            Self::ThreadUnresolved => "thread_unresolved",
        }
    }

    /// The fixed prefix of the event's dedup key: a note's own id
    /// (`merge_request_note_added:<note_id>`), or the discussion and its
    /// revision for a resolution (`…:<discussion_id>:<revision>`; the
    /// revision counts the thread's resolve / unresolve changes, so a
    /// resolve after an unresolve is a new event).
    pub fn dedup_prefix(self) -> &'static str {
        match self {
            Self::NoteAdded => "merge_request_note_added",
            Self::ThreadResolved => "merge_request_thread_resolved",
            Self::ThreadUnresolved => "merge_request_thread_unresolved",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discussion_values_are_the_check_values_and_round_trip() {
        let kinds: Vec<&str> = DiscussionKind::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(kinds, ["general", "inline"]);
        let sides: Vec<&str> = DiscussionSide::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(sides, ["old", "new"]);
        for k in DiscussionKind::ALL {
            assert_eq!(DiscussionKind::parse(k.as_str()), Some(*k));
        }
        for s in DiscussionSide::ALL {
            assert_eq!(DiscussionSide::parse(s.as_str()), Some(*s));
        }
        for other in ["", "New", "left", "right", "general "] {
            assert_eq!(DiscussionSide::parse(other), None, "{other:?}");
            assert_eq!(DiscussionKind::parse(other), None, "{other:?}");
        }
    }

    #[test]
    fn comment_actions_are_the_adr_values_with_distinct_dedup_prefixes() {
        let names: Vec<&str> = CommentAction::ALL.iter().map(|a| a.as_str()).collect();
        assert_eq!(
            names,
            ["note_added", "thread_resolved", "thread_unresolved"]
        );
        let mut prefixes: Vec<&str> = CommentAction::ALL
            .iter()
            .map(|a| a.dedup_prefix())
            .collect();
        prefixes.dedup();
        assert_eq!(prefixes.len(), CommentAction::ALL.len());
    }
}
