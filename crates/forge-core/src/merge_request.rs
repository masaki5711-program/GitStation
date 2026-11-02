//! Merge requests (M3 R1a, ADR-0024 §1, §6): the state, the transitions and
//! the audit details of a change to one merge request.
//!
//! A merge request is `open` (with a `draft` flag), `merged` (final) or
//! `closed`. Every transition (and every other change the ADR names) emits
//! exactly one domain event of its own type: [`MergeRequestTransition`]
//! lists them with that type, and forge-db's tests drive each one against
//! the database and count the events (`every_merge_request_transition_emits_exactly_one_event`).
//! R1a writes `opened`, `updated`, `draft_changed`, `closed` and
//! `reopened`; `merged`, `pushed` and the rest join with their writers.

use crate::event_type::EventType;
use crate::ids::{MergeRequestId, UserId};
use crate::ref_name::RefName;

/// `open`, `merged` or `closed` (the CHECK of forge.merge_requests.state).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MergeRequestState {
    Open,
    /// Final: nothing leaves it (the state guard refuses it, ADR-0024 §6).
    Merged,
    Closed,
}

impl MergeRequestState {
    /// Every state, in the order open, merged, closed.
    pub const ALL: &'static [MergeRequestState] = &[Self::Open, Self::Merged, Self::Closed];

    /// The database / API string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Merged => "merged",
            Self::Closed => "closed",
        }
    }

    /// The state named by `raw` (exact match), `None` for anything else.
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|s| s.as_str() == raw)
    }
}

/// A field `merge_request.updated` names (never its value: titles and
/// descriptions are free text).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MergeRequestField {
    Title,
    Description,
    TargetBranch,
}

impl MergeRequestField {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Description => "description",
            Self::TargetBranch => "target_branch",
        }
    }
}

/// Every transition or change of a merge request that publishes an event
/// (ADR-0024 §6), as far as R1a writes them. A variant joins with its
/// writer; the table test fails when one emits no event or two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MergeRequestTransition {
    /// (none) → `open`.
    Open,
    /// `open` → `closed`.
    Close,
    /// `closed` → `open`.
    Reopen,
    /// `draft` ⇄ ready inside `open`.
    DraftToggle,
    /// The title, the description or the target branch changed.
    Edit,
}

impl MergeRequestTransition {
    /// Every transition R1a writes.
    pub const ALL: &'static [MergeRequestTransition] = &[
        Self::Open,
        Self::Close,
        Self::Reopen,
        Self::DraftToggle,
        Self::Edit,
    ];

    /// The one event the transition publishes.
    pub fn event_type(self) -> EventType {
        match self {
            Self::Open => EventType::MergeRequestOpened,
            Self::Close => EventType::MergeRequestClosed,
            Self::Reopen => EventType::MergeRequestReopened,
            Self::DraftToggle => EventType::MergeRequestDraftChanged,
            Self::Edit => EventType::MergeRequestUpdated,
        }
    }
}

/// The audit details (and event payload) of one transition of one merge
/// request. Ids, ref names, flags and field names only: never the title or
/// the description (free text), never a user name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeRequestChange {
    /// `merge_request.opened`.
    Opened {
        merge_request: MergeRequestId,
        iid: i64,
        author: UserId,
        source_branch: RefName,
        target_branch: RefName,
        draft: bool,
    },
    /// `merge_request.updated`: the changed fields, and the target branch
    /// before and after when it changed.
    Updated {
        merge_request: MergeRequestId,
        iid: i64,
        fields: Vec<MergeRequestField>,
        target_branch: Option<(RefName, RefName)>,
    },
    /// `merge_request.draft_changed`: the new value.
    DraftChanged {
        merge_request: MergeRequestId,
        iid: i64,
        draft: bool,
    },
    /// `merge_request.closed`.
    Closed {
        merge_request: MergeRequestId,
        iid: i64,
    },
    /// `merge_request.reopened`.
    Reopened {
        merge_request: MergeRequestId,
        iid: i64,
    },
}

impl MergeRequestChange {
    /// The transition these details record.
    pub fn transition(&self) -> MergeRequestTransition {
        match self {
            Self::Opened { .. } => MergeRequestTransition::Open,
            Self::Updated { .. } => MergeRequestTransition::Edit,
            Self::DraftChanged { .. } => MergeRequestTransition::DraftToggle,
            Self::Closed { .. } => MergeRequestTransition::Close,
            Self::Reopened { .. } => MergeRequestTransition::Reopen,
        }
    }

    /// The merge request changed.
    pub fn merge_request(&self) -> MergeRequestId {
        match self {
            Self::Opened { merge_request, .. }
            | Self::Updated { merge_request, .. }
            | Self::DraftChanged { merge_request, .. }
            | Self::Closed { merge_request, .. }
            | Self::Reopened { merge_request, .. } => *merge_request,
        }
    }

    /// The `audit_log.action` (and event type name).
    pub fn action(&self) -> &'static str {
        self.transition().event_type().as_str()
    }

    /// The fixed prefix of the event's dedup key
    /// (`<prefix>:<merge_request_id>:<revision>`, server ids only).
    pub fn dedup_prefix(&self) -> &'static str {
        match self {
            Self::Opened { .. } => "merge_request_opened",
            Self::Updated { .. } => "merge_request_updated",
            Self::DraftChanged { .. } => "merge_request_draft_changed",
            Self::Closed { .. } => "merge_request_closed",
            Self::Reopened { .. } => "merge_request_reopened",
        }
    }

    /// Opened `{"merge_request_id","iid","author_id","source_branch","target_branch","draft"}`,
    /// Updated `{"merge_request_id","iid","fields","target_branch"}` (`target_branch`:
    /// `{"from","to"}` or `null`), DraftChanged `{"merge_request_id","iid","draft"}`,
    /// Closed / Reopened `{"merge_request_id","iid"}`.
    pub fn to_value(&self) -> serde_json::Value {
        match self {
            Self::Opened {
                merge_request,
                iid,
                author,
                source_branch,
                target_branch,
                draft,
            } => serde_json::json!({
                "merge_request_id": merge_request.get(),
                "iid": iid,
                "author_id": author.get(),
                "source_branch": source_branch.as_str(),
                "target_branch": target_branch.as_str(),
                "draft": draft,
            }),
            Self::Updated {
                merge_request,
                iid,
                fields,
                target_branch,
            } => serde_json::json!({
                "merge_request_id": merge_request.get(),
                "iid": iid,
                "fields": fields.iter().map(|f| f.as_str()).collect::<Vec<_>>(),
                "target_branch": target_branch.as_ref().map(|(from, to)| serde_json::json!({
                    "from": from.as_str(),
                    "to": to.as_str(),
                })),
            }),
            Self::DraftChanged {
                merge_request,
                iid,
                draft,
            } => serde_json::json!({
                "merge_request_id": merge_request.get(),
                "iid": iid,
                "draft": draft,
            }),
            Self::Closed { merge_request, iid } | Self::Reopened { merge_request, iid } => {
                serde_json::json!({
                    "merge_request_id": merge_request.get(),
                    "iid": iid,
                })
            }
        }
    }
}
