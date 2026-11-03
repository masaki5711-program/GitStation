//! Domain event types (ADR-0016 §5): the one list of the `forge.events`
//! `event_type` values forge writes. A variant joins in the commit that adds
//! its writer (step D3 MR1b: the token events, and `repository.created`
//! for D4; D4 MR2: `project.created`; D4 MR4: `member.*` of projects; D4 MR5: `protected_ref.*`; M1 phase 9 MR2: `repository.pushed`; M1 phase 8b MR7a: `project.visibility_changed`; D5 MR3: `project.deleted`, `project.restored`; D5 MR4: `project.purged`; D5 MR5: `project.moved`, `group.moved`, `user.renamed`; M3 R1a: `merge_request.opened`, `.updated`, `.draft_changed`, `.closed`, `.reopened`; M3 R1b: `merge_request.pushed`; M3 R2: `merge_request.conflict_detected`, `.conflict_resolved`; M3 R6: `merge_settings.codeowners_enforcement_changed`, `.codeowners_enforcement_restored`; M3 C1: `check.completed`); subscribers match it without `_`.

/// Who may read an event type (ADR-0016 §5 table); the feed and webhooks
/// decide with it through `authorize()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VisibilityClass {
    /// Integration feed and webhooks; project Reporter.
    Integration,
    /// Admin feed and webhooks naming the type; Maintainer.
    Maintainer,
    /// Admin feed and webhooks naming the type; Developer.
    Deployment,
    /// Admin feed only; administrator with the `admin` scope.
    Admin,
}

impl VisibilityClass {
    /// Every class, in declaration order.
    pub const ALL: [VisibilityClass; 4] = [
        VisibilityClass::Integration,
        VisibilityClass::Maintainer,
        VisibilityClass::Deployment,
        VisibilityClass::Admin,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            VisibilityClass::Integration => "integration",
            VisibilityClass::Maintainer => "maintainer",
            VisibilityClass::Deployment => "deployment",
            VisibilityClass::Admin => "admin",
        }
    }
}

/// The type of a domain event: `<aggregate>.<past-tense verb>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventType {
    /// `project.created` (D4 MR2; integration class, step D plan: the one
    /// `project.*` event of that class).
    ProjectCreated,
    /// `repository.created` (D4; integration class, D-plan answer Q7).
    RepositoryCreated,
    /// `token.created` (D3, ADR-0019).
    TokenCreated,
    /// `token.revoked` (D3, ADR-0019).
    TokenRevoked,
    /// `member.added` of a project (D4 MR4; maintainer class, ADR-0016 §5).
    /// Group member changes stay audit-only.
    MemberAdded,
    /// `member.role_changed` of a project (D4 MR4; maintainer class).
    MemberRoleChanged,
    /// `member.removed` of a project (D4 MR4; maintainer class).
    MemberRemoved,
    /// `protected_ref.created` (D4 MR5; maintainer class, ADR-0016 §5).
    ProtectedRefCreated,
    /// `protected_ref.updated` (D4 MR5; maintainer class).
    ProtectedRefUpdated,
    /// `protected_ref.deleted` (D4 MR5; maintainer class).
    ProtectedRefDeleted,
    /// `repository.pushed` (M1 phase 9 MR2; integration class, ADR-0016 §5):
    /// one per applied push, written by `repositories::record_push`.
    RepositoryPushed,
    /// `project.visibility_changed` (M1 phase 8b MR7a; maintainer class,
    /// ADR-0016 §5): written by `projects::change_visibility`.
    ProjectVisibilityChanged,
    /// `project.deleted` (D5 MR3; integration class, ADR-0016 §10
    /// Amendment D5): the project entered its grace period. Deduplicated by
    /// `project_deleted:<project_id>:<deleted_at epoch>`.
    ProjectDeleted,
    /// `project.restored` (D5 MR3; integration class): a project pending
    /// deletion came back. Deduplicated by
    /// `project_restored:<project_id>:<deleted_at epoch>` of the deletion
    /// it undoes.
    ProjectRestored,
    /// `project.purged` (D5 MR4; admin class, step D5 plan answer and
    /// ADR-0016 §10 Amendment D5): the purge job removed the repository
    /// and the rows of a project whose grace period was over. Admin class:
    /// nobody holds a role on a project that no longer exists, so no
    /// Reporter could be authorized for it. Deduplicated by
    /// `project_purged:<project_id>` (ids are never reused).
    ProjectPurged,
    /// `project.moved` (D5 MR5; integration class, step D5 plan answer
    /// "other project.* integration"): the project itself was moved or
    /// renamed. A project inside a moved group or a renamed user's
    /// namespace gets no event of its own (one change carries at most 100
    /// events, a move up to 1000 projects); `group.moved` / `user.renamed`
    /// name that move. Deduplicated by `project_moved:<project_id>:<µs epoch>`.
    ProjectMoved,
    /// `group.moved` (D5 MR5; admin class: an instance-wide event — it names
    /// no project, so no project role can authorize a reader, ADR-0016 §5).
    /// Deduplicated by `group_moved:<group_id>:<µs epoch>`.
    GroupMoved,
    /// `user.renamed` (D5 MR5; admin class, instance-wide). Deduplicated by
    /// `user_renamed:<user_id>:<µs epoch>`.
    UserRenamed,
    // ── M3 R1a: merge requests (ADR-0024 §6; integration class, ADR-0016 §5) ──
    /// `merge_request.opened`: (none) → open. Deduplicated by
    /// `merge_request_opened:<merge_request_id>:<revision>`, like every
    /// `merge_request.*` event (`revision` counts the row's changes).
    MergeRequestOpened,
    /// `merge_request.updated`: the title, description or target changed.
    MergeRequestUpdated,
    /// `merge_request.draft_changed`: draft ⇄ ready while open.
    MergeRequestDraftChanged,
    /// `merge_request.closed`: open → closed.
    MergeRequestClosed,
    /// `merge_request.reopened`: closed → open.
    MergeRequestReopened,
    // ── end M3 R1a ──
    /// `merge_request.pushed` (M3 R1b): a version was added (the source
    /// branch moved to a commit the merge request has no version of).
    /// Deduplicated by `merge_request_pushed:<merge_request_id>:<head_sha>`
    /// (ADR-0024 §6): one event per head commit of a merge request.
    MergeRequestPushed,
    /// `merge_request.conflict_detected` (M3 R2, ADR-0024 §3, §6): the
    /// mergeability check flipped from "can be merged" to "cannot be
    /// merged" (never from or to an undecided status). Written by the
    /// mergeability job (actor `system`); deduplicated by
    /// `merge_request_conflict_detected:<merge_request_id>:<check_seq>`.
    MergeRequestConflictDetected,
    /// `merge_request.conflict_resolved` (M3 R2): "cannot" → "can be
    /// merged". Deduplicated by
    /// `merge_request_conflict_resolved:<merge_request_id>:<check_seq>`.
    MergeRequestConflictResolved,
    /// `merge_request.commented` (M3 R3, ADR-0024 §6): a note was added to
    /// a discussion, or a discussion was resolved or unresolved
    /// (`data.action` = `note_added` / `thread_resolved` /
    /// `thread_unresolved`). Deduplicated by
    /// `merge_request_note_added:<note_id>` or
    /// `merge_request_thread_resolved|unresolved:<discussion_id>:<revision>`.
    MergeRequestCommented,
    /// `merge_request.merged` (M3 R4, ADR-0024 §4.2): open → merged, by the
    /// merge job. Deduplicated by `merge_request_merged:<merge_id>`.
    MergeRequestMerged,
    /// `merge_request.source_branch_deleted` (M3 R4): the merge deleted the
    /// source branch. Deduplicated by `merge_source_deleted:<merge_id>`.
    MergeRequestSourceBranchDeleted,
    /// `merge_request.approved` (M3 R5, ADR-0024 §6, §8): an approval row
    /// was written for the head and target the approver saw. Carries
    /// `counted` / `uncounted_reason` as evaluated then — never "counted"
    /// by itself (whether a merge may happen is `evaluate_merge()`'s).
    /// Deduplicated by `merge_request_approved:<approval_id>`.
    MergeRequestApproved,
    /// `merge_request.unapproved` (M3 R5): an approval was withdrawn
    /// (`reason=unapproved`) or revoked by a new head (`new_commits`) or a
    /// new target (`target_changed`). Deduplicated by
    /// `merge_request_unapproved:<approval_id>` (each row ends once).
    MergeRequestUnapproved,
    /// `merge_settings.codeowners_enforcement_changed` (M3 R6, ADR-0024 §9;
    /// maintainer class): a person suspended CODEOWNERS enforcement (with
    /// its expiry, at most 24 hours) or enforced it again. No dedup key
    /// (like `protected_ref.*`).
    CodeownersEnforcementChanged,
    /// `merge_settings.codeowners_enforcement_restored` (M3 R6; maintainer
    /// class, actor `system`): a suspension expired and the worker recorded
    /// it once. Deduplicated by
    /// `codeowners_enforcement_restored:<project_id>:<expiry epoch>`.
    CodeownersEnforcementRestored,
    /// `check.completed` (M3 C1, ADR-0024 §10; integration class): a report
    /// changed a check run to a new conclusion (a report that leaves the
    /// conclusion as it was publishes nothing). Deduplicated by
    /// `check_completed:<check_run_id>:<revision>`.
    CheckCompleted,
}

impl EventType {
    /// Every event type, in declaration order.
    pub const ALL: [EventType; 34] = [
        EventType::ProjectCreated,
        EventType::RepositoryCreated,
        EventType::TokenCreated,
        EventType::TokenRevoked,
        EventType::MemberAdded,
        EventType::MemberRoleChanged,
        EventType::MemberRemoved,
        EventType::ProtectedRefCreated,
        EventType::ProtectedRefUpdated,
        EventType::ProtectedRefDeleted,
        EventType::RepositoryPushed,
        EventType::ProjectVisibilityChanged,
        EventType::ProjectDeleted,
        EventType::ProjectRestored,
        EventType::ProjectPurged,
        EventType::ProjectMoved,
        EventType::GroupMoved,
        EventType::UserRenamed,
        EventType::MergeRequestOpened,
        EventType::MergeRequestUpdated,
        EventType::MergeRequestDraftChanged,
        EventType::MergeRequestClosed,
        EventType::MergeRequestReopened,
        EventType::MergeRequestPushed,
        EventType::MergeRequestConflictDetected,
        EventType::MergeRequestConflictResolved,
        EventType::MergeRequestCommented,
        EventType::MergeRequestMerged,
        EventType::MergeRequestSourceBranchDeleted,
        EventType::MergeRequestApproved,
        EventType::MergeRequestUnapproved,
        EventType::CodeownersEnforcementChanged,
        EventType::CodeownersEnforcementRestored,
        EventType::CheckCompleted,
    ];

    /// The stored name (passes the `forge.events.event_type` CHECK; tested).
    pub fn as_str(self) -> &'static str {
        match self {
            EventType::ProjectCreated => "project.created",
            EventType::RepositoryCreated => "repository.created",
            EventType::TokenCreated => "token.created",
            EventType::TokenRevoked => "token.revoked",
            EventType::MemberAdded => "member.added",
            EventType::MemberRoleChanged => "member.role_changed",
            EventType::MemberRemoved => "member.removed",
            EventType::ProtectedRefCreated => "protected_ref.created",
            EventType::ProtectedRefUpdated => "protected_ref.updated",
            EventType::ProtectedRefDeleted => "protected_ref.deleted",
            EventType::RepositoryPushed => "repository.pushed",
            EventType::ProjectVisibilityChanged => "project.visibility_changed",
            EventType::ProjectDeleted => "project.deleted",
            EventType::ProjectRestored => "project.restored",
            EventType::ProjectPurged => "project.purged",
            EventType::ProjectMoved => "project.moved",
            EventType::GroupMoved => "group.moved",
            EventType::UserRenamed => "user.renamed",
            EventType::MergeRequestOpened => "merge_request.opened",
            EventType::MergeRequestUpdated => "merge_request.updated",
            EventType::MergeRequestDraftChanged => "merge_request.draft_changed",
            EventType::MergeRequestClosed => "merge_request.closed",
            EventType::MergeRequestReopened => "merge_request.reopened",
            EventType::MergeRequestPushed => "merge_request.pushed",
            EventType::MergeRequestConflictDetected => "merge_request.conflict_detected",
            EventType::MergeRequestConflictResolved => "merge_request.conflict_resolved",
            EventType::MergeRequestCommented => "merge_request.commented",
            EventType::MergeRequestMerged => "merge_request.merged",
            EventType::MergeRequestSourceBranchDeleted => "merge_request.source_branch_deleted",
            EventType::MergeRequestApproved => "merge_request.approved",
            EventType::MergeRequestUnapproved => "merge_request.unapproved",
            EventType::CodeownersEnforcementChanged => {
                "merge_settings.codeowners_enforcement_changed"
            }
            EventType::CodeownersEnforcementRestored => {
                "merge_settings.codeowners_enforcement_restored"
            }
            EventType::CheckCompleted => "check.completed",
        }
    }

    /// The type named `name` (its `as_str`); `None` for any other string.
    pub fn parse(name: &str) -> Option<EventType> {
        Self::ALL.into_iter().find(|t| t.as_str() == name)
    }

    /// The least privilege that may read events of this type.
    pub fn visibility_class(self) -> VisibilityClass {
        match self {
            EventType::ProjectCreated
            | EventType::RepositoryCreated
            | EventType::RepositoryPushed
            | EventType::ProjectDeleted
            | EventType::ProjectRestored
            | EventType::ProjectMoved
            | EventType::MergeRequestOpened
            | EventType::MergeRequestUpdated
            | EventType::MergeRequestDraftChanged
            | EventType::MergeRequestClosed
            | EventType::MergeRequestReopened
            | EventType::MergeRequestPushed
            | EventType::MergeRequestConflictDetected
            | EventType::MergeRequestConflictResolved
            | EventType::MergeRequestCommented
            | EventType::MergeRequestMerged
            | EventType::MergeRequestSourceBranchDeleted
            | EventType::MergeRequestApproved
            | EventType::MergeRequestUnapproved
            | EventType::CheckCompleted => VisibilityClass::Integration,
            EventType::TokenCreated
            | EventType::TokenRevoked
            | EventType::ProjectPurged
            | EventType::GroupMoved
            | EventType::UserRenamed => VisibilityClass::Admin,
            EventType::MemberAdded
            | EventType::MemberRoleChanged
            | EventType::MemberRemoved
            | EventType::ProtectedRefCreated
            | EventType::ProtectedRefUpdated
            | EventType::ProtectedRefDeleted
            | EventType::ProjectVisibilityChanged
            | EventType::CodeownersEnforcementChanged
            | EventType::CodeownersEnforcementRestored => VisibilityClass::Maintainer,
        }
    }
}
