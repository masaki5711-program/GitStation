//! Typed `audit_log.details` (ADR-0016 §9). Variants carry no personal data
//! (no email, employee number, external id, display name, IP).

use crate::approval::ApprovalRuleChange;
use crate::checks::CheckRunChange;
use crate::doc_lint_migration::DocLintMigrationChange;
use crate::event_type::EventType;
use crate::history_import::HistoryImportChange;
use crate::ids::{GroupId, NamespaceId, ProjectId, ProtectedRefId, RepoId, TombstoneId, UserId};
use crate::integration::IntegrationChange;
use crate::merge_conditions::{MergeRuleChange, MergeSettingsChange};
use crate::merge_queue::MergeQueueChange;
use crate::merge_request::{MergeRequestChange, MergeRequestVersionChange};
use crate::mirror::PushMirrorChange;
use crate::move_details::{MoveSummary, TombstoneKind};
use crate::pipeline::{CiVariableChange, PipelineChange, RunnerChange};
use crate::push::{push_json, PushReplay, PushedRef, AUDIT_BUDGET, EVENT_BUDGET};
use crate::repository::RepositoryState;
use crate::role::{Role, Visibility};
use crate::scope::Scope;
use crate::user_kind::UserKind;
use crate::webhook::WebhookChange;
use crate::work_item::WorkItemChange;

/// A field an update changed (`project.updated`, `group.updated`): the
/// name only, never the value (display names and descriptions are free
/// text).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChangedField {
    DisplayName,
    Description,
}

impl ChangedField {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DisplayName => "display_name",
            Self::Description => "description",
        }
    }
}

/// A field of a service account an update changed
/// (`service_account.updated`, M3 S0): the name only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ServiceAccountField {
    DisplayName,
    Purpose,
    Owner,
}

impl ServiceAccountField {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DisplayName => "display_name",
            Self::Purpose => "purpose",
            Self::Owner => "owner",
        }
    }
}

fn field_names(fields: &[ChangedField]) -> Vec<&'static str> {
    fields.iter().map(|f| f.as_str()).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditDetails {
    /// `user.created`.
    UserCreated {
        kind: UserKind,
        namespace_id: NamespaceId,
        is_admin: bool,
    },
    /// `user.disabled`.
    UserDisabled {
        kind: UserKind,
        /// Active tokens revoked in the same change (D3, ADR-0019).
        revoked_tokens: i64,
        /// Active web sessions revoked in the same change
        /// (`revoked_reason = 'user_disabled'`, M2 MR3, ADR-0019 Amendment M2).
        revoked_sessions: i64,
        /// The operator passed the administration guard (`--force`,
        /// ADR-0019 §10), whether or not it would have refused.
        forced: bool,
        /// Top-level groups this disable left without a direct active Owner
        /// (non-zero only when `forced`).
        ownerless_groups: i64,
        /// Projects this disable left without any active Owner, direct or
        /// inherited (D4 MR4; non-zero only when `forced`).
        ownerless_projects: i64,
        /// This disable left no active human administrator (only when `forced`).
        left_without_admin: bool,
    },
    /// `user.admin_bootstrapped` (D3 MR2, ADR-0019 Amendment): the recovery /
    /// first-install path made an active human an administrator. `created`:
    /// the user was created by the same command (then `namespace_id` is its
    /// personal namespace; no separate `user.created` row).
    AdminBootstrapped {
        created: bool,
        namespace_id: Option<NamespaceId>,
    },
    /// `group.created` (D2): its namespace, the parent group's namespace
    /// (`None` at the top level) and the first Owner.
    GroupCreated {
        namespace_id: NamespaceId,
        parent_namespace_id: Option<NamespaceId>,
        owner: UserId,
    },
    /// `project.created` (D4 MR2): the project, its namespace, the parent
    /// (a user or group namespace), its repository, the visibility and the creator (the first direct Owner). Also
    /// the payload of the `project.created` event (integration class), so
    /// ids and visibility only — never the path or the names.
    ProjectCreated {
        project_id: ProjectId,
        namespace_id: NamespaceId,
        parent_namespace_id: NamespaceId,
        repository_id: RepoId,
        visibility: Visibility,
        owner: UserId,
    },
    /// `member.added` (D2, group membership).
    MemberAdded { user: UserId, role: Role },
    /// `member.role_changed` (D2).
    MemberRoleChanged { user: UserId, from: Role, to: Role },
    /// `member.removed` (D2): the role the member had.
    MemberRemoved { user: UserId, role: Role },
    /// `token.created` (D3): ids, the public id, scope names,
    /// the expiry (RFC 3339, UTC) and `is_agent`. Never the secret, its hash or the name.
    TokenCreated {
        token_id: i64,
        user: UserId,
        public_id: String,
        scopes: Vec<Scope>,
        expires_at: String,
        is_agent: bool,
        /// The administrator who issued it for someone else (a service
        /// account's token, M3 S0); `None` for one's own token and the
        /// operator CLI's. In the JSON only when set.
        issued_by: Option<UserId>,
    },
    /// `service_account.created` (M3 S0, audited on the account): the
    /// account's personal namespace and the person who answers for it.
    /// Never the purpose text (free text) — `purpose_chars` only.
    ServiceAccountCreated {
        namespace_id: NamespaceId,
        owner: UserId,
        purpose_chars: i64,
    },
    /// `service_account.updated` (M3 S0): the owner (from → to, when it
    /// changed) and the names of the other fields changed.
    ServiceAccountUpdated {
        owner_from: UserId,
        owner_to: UserId,
        fields: Vec<ServiceAccountField>,
    },
    /// `token.revoked` (D3).
    TokenRevoked {
        token_id: i64,
        user: UserId,
        public_id: String,
    },
    /// `ssh_key.added` (M4 C13, ADR-0032 §2, audit-only, on the owner): the
    /// key's id, algorithm and SHA-256 fingerprint. Never the title (free
    /// text) or the key itself.
    SshKeyAdded {
        key_id: i64,
        user: UserId,
        algorithm: String,
        fingerprint: String,
    },
    /// `ssh_key.deleted` (M4 C13, audit-only, on the owner).
    SshKeyDeleted {
        key_id: i64,
        user: UserId,
        fingerprint: String,
    },
    // ── D4 MR3b: repository provisioning ──
    /// `repository.created` (D4 MR3b): the provisioning job created (or
    /// adopted) the bare repository and marked it ready. Also the payload of
    /// the `repository.created` event (integration class): ids only.
    RepositoryCreated {
        project_id: ProjectId,
        repository_id: RepoId,
    },
    /// `repository.reprovision_requested` (D4 MR3b, audit-only): an
    /// operator enqueued the provisioning job again (`forge admin repo
    /// reprovision`) for a repository still `provisioning`.
    /// `after_succeeded_job`: the succeeded job that held the key (the new
    /// job is keyed after it); `None` when no job held it (dead or missing).
    RepositoryReprovisionRequested {
        repository_id: RepoId,
        job_id: i64,
        after_succeeded_job: Option<i64>,
    },
    // ── end D4 MR3b ──
    /// `member.added` of a project (D4 MR4): the same action and JSON as
    /// the group variant, but on a `project` audit row, and it publishes
    /// the `member.added` event (maintainer class, ADR-0016 §5).
    ProjectMemberAdded { user: UserId, role: Role },
    /// `member.role_changed` of a project (D4 MR4).
    ProjectMemberRoleChanged { user: UserId, from: Role, to: Role },
    /// `member.removed` of a project (D4 MR4): the role the member had.
    ProjectMemberRemoved { user: UserId, role: Role },
    /// `protected_ref.created` (D4 MR5, ADR-0008): the rule, its pattern as
    /// written (a ref pattern, not personal data) and its minimum push role.
    /// Also the payload of the event (maintainer class).
    ProtectedRefCreated {
        rule: ProtectedRefId,
        pattern: String,
        push_min_role: Role,
    },
    /// `protected_ref.updated` (D4 MR5): the minimum push role changed.
    ProtectedRefUpdated {
        rule: ProtectedRefId,
        pattern: String,
        from: Role,
        to: Role,
    },
    /// `protected_ref.deleted` (D4 MR5): the rule as it was stored. A row
    /// this build cannot read (FG-DB-0020) can still be deleted: then
    /// `stored_invalid` is set, `pattern` is the stored text cut to 255
    /// bytes and `push_min_role` the stored text cut to 32 characters
    /// (D4 review).
    ProtectedRefDeleted {
        rule: ProtectedRefId,
        pattern: String,
        push_min_role: String,
        stored_invalid: bool,
    },
    /// `repository.pushed` (M1 phase 9 MR2, ADR-0016 §5): one applied push.
    /// `updates` sorted by ref name ([`crate::PushRecord::sorted_updates`]).
    /// The audit details and the event payload are the same JSON
    /// ([`push_json`]) within different byte budgets: the details at most
    /// [`AUDIT_BUDGET`] (the row holds 8 KiB), the payload at most
    /// [`EVENT_BUDGET`] (64 KiB) — so the event usually lists more refs
    /// and commits, and both say what they left out.
    RepositoryPushed {
        project_id: ProjectId,
        repository_id: RepoId,
        updates: Vec<PushedRef>,
        /// The original pusher when the runbook replay recorded it (the
        /// row's actor is then the operator).
        replayed_from: Option<PushReplay>,
    },
    // ── M1 phase 8b MR7a ──
    /// `project.updated` (audit-only in M1): which fields changed.
    ProjectUpdated {
        project_id: ProjectId,
        fields: Vec<ChangedField>,
    },
    /// `project.visibility_changed`: also the payload of the event
    /// (maintainer class, ADR-0016 §5).
    ProjectVisibilityChanged {
        project_id: ProjectId,
        from: Visibility,
        to: Visibility,
    },
    /// `group.updated` (audit-only in M1): which fields changed.
    GroupUpdated {
        group_id: GroupId,
        fields: Vec<ChangedField>,
    },
    // ── end M1 phase 8b MR7a ──
    // ── D5 MR2: namespace moves and tombstone releases ──
    /// `project.moved`: the project itself moved or was renamed.
    ProjectMoved {
        project_id: ProjectId,
        summary: MoveSummary,
    },
    /// `project.moved` of a project inside a moved group or a renamed
    /// user's namespace (one row per project, ADR-0016 Amendment D5):
    /// `moved_namespace_id` is the namespace that moved.
    ProjectMovedWithAncestor {
        project_id: ProjectId,
        namespace_id: NamespaceId,
        moved_namespace_id: NamespaceId,
    },
    /// `group.moved`: a group moved (transfer) or was renamed.
    GroupMoved {
        group_id: GroupId,
        summary: MoveSummary,
    },
    /// `user.renamed`: the username and the personal namespace's path.
    UserRenamed {
        user_id: UserId,
        summary: MoveSummary,
    },
    /// `namespace_tombstone.released`: the tombstone, its kind and the
    /// namespace it named (`None` once gone); `reclaim` = released by a
    /// move of that same namespace back to the path.
    TombstoneReleased {
        tombstone_id: TombstoneId,
        kind: TombstoneKind,
        namespace_id: Option<NamespaceId>,
        reclaim: bool,
    },
    // ── end D5 MR2 ──
    // ── D5 MR3: deletion and restore ──
    /// `project.deleted`: also the payload of the event (integration class).
    /// `deleted_at` / `purge_after` are UTC RFC 3339; `deleted_by` is the
    /// deleting user (`None`: the operator CLI deletes as `system`).
    ProjectDeleted {
        project_id: ProjectId,
        deleted_at: String,
        purge_after: String,
        deleted_by: Option<UserId>,
    },
    /// `project.restored`: also the payload of the event (integration
    /// class). `deleted_at` / `purge_after` are the cleared deletion's;
    /// `deleted_by` the user who had deleted it.
    ProjectRestored {
        project_id: ProjectId,
        deleted_at: String,
        purge_after: String,
        deleted_by: Option<UserId>,
    },
    // ── end D5 MR3 ──
    // ── D5 MR4: purge ──
    /// `repository.purge_started` (audit-only): phase 1 of the purge moved
    /// the repository of a project whose grace period is over to
    /// `purging` (`from`: its state before, `provisioning` or `ready`).
    RepositoryPurgeStarted {
        project_id: ProjectId,
        repository_id: RepoId,
        from: RepositoryState,
    },
    /// `project.purged`: also the payload of the event (admin class). The
    /// bare directory is gone (`storage_removed`: it was there; false when
    /// it was already absent), the rows are deleted, the path is kept by a
    /// `deleted` tombstone (`tombstone_id`), and `redirects_ended` `moved`
    /// tombstones lost their target (they keep blocking their paths). The
    /// deletion's times are UTC RFC 3339.
    ProjectPurged {
        project_id: ProjectId,
        repository_id: RepoId,
        namespace_id: NamespaceId,
        deleted_at: String,
        purge_after: String,
        deleted_by: Option<UserId>,
        tombstone_id: TombstoneId,
        redirects_ended: i64,
        members_removed: i64,
        protected_refs_removed: i64,
        /// Merge request rows deleted with the project (M3 R1a, plan 11
        /// §11.8).
        merge_requests_removed: i64,
        storage_removed: bool,
    },
    // ── end D5 MR4 ──
    /// `merge_request.*` (M3 R1a, ADR-0024 §6): one transition of one merge
    /// request, audited on its project (resource `project`, like
    /// `protected_ref.*`) and published as the transition's event
    /// (integration class). The shapes: [`MergeRequestChange::to_value`].
    MergeRequest(MergeRequestChange),
    /// `merge_request.head_restored` / `.version_refs_purged` (M3 R1b):
    /// audit-only, on the project. Shapes: [`MergeRequestVersionChange::to_value`].
    MergeRequestVersion(MergeRequestVersionChange),
    /// `approval_rule.put` / `approval_rule.removed` (M3 R5, ADR-0024 §8.6):
    /// audit-only, on the project. Shapes: [`ApprovalRuleChange::to_value`].
    ApprovalRule(ApprovalRuleChange),
    /// `merge_settings.*` (M3 R6, ADR-0024 §4.1, §8, §9) of `project`:
    /// `updated` is audit-only; the CODEOWNERS enforcement changes are
    /// events of the maintainer class. Shapes: [`MergeSettingsChange::to_value`].
    MergeSettings {
        project: ProjectId,
        change: MergeSettingsChange,
    },
    /// `protected_ref.merge_rule_put` / `.merge_rule_removed` (M3 R6,
    /// ADR-0024 §12): audit-only, on the project. Shapes:
    /// [`MergeRuleChange::to_value`].
    ProtectedRefMergeRule(MergeRuleChange),
    /// `check_run.reported` / `check.completed` (M3 C1, ADR-0024 §10), on
    /// the project: one report of a check run; `check.completed` (an event,
    /// integration class) only when the report changed the conclusion.
    /// Shapes: [`CheckRunChange::to_value`].
    CheckRun(CheckRunChange),
    /// `check_run.retention` (feat/db-check-run-retention), audit only, on
    /// the project: one retention batch deleted `runs` check runs (last
    /// updated more than `older_than_days` days ago, not the current head of
    /// an open merge request) and their `annotations`. Counts only:
    /// `{"project_id","runs","annotations","older_than_days"}`.
    CheckRunsExpired {
        project: ProjectId,
        runs: i64,
        annotations: i64,
        older_than_days: u32,
    },
    /// `check_namespace.granted` / `.revoked` (M3 C1), audit only, on the
    /// project the grant is for: `{"user_id","project_id","namespace"}`.
    CheckNamespaceGrant {
        user: UserId,
        project: ProjectId,
        namespace: crate::merge_conditions::CheckNamespace,
        granted: bool,
    },
    /// `integration.*` (M3 I1a, plan 11 §5 I1): audit-only; created /
    /// updated on (`integration`, id), bound / unbound on the project.
    /// Shapes: [`IntegrationChange::to_value`].
    Integration(IntegrationChange),
    /// `work_item.linked` / `.unlinked` by a person (M3 I1b): audited on
    /// the project and published as the event (integration class; the
    /// subscriber's detected links are derived events without audit rows).
    /// Shapes: [`WorkItemChange::to_value`].
    WorkItem(WorkItemChange),
    /// `webhook.*` (M3 I2, ADR-0016 Amendment I2): on the project; only
    /// `webhook.disabled` (the worker's automatic disable) publishes an
    /// event. Shapes: [`WebhookChange::to_value`].
    Webhook(WebhookChange),
    /// M3 O2: a push mirror set or removed (resource the project, no
    /// event). Shapes: [`PushMirrorChange::to_value`].
    PushMirror(PushMirrorChange),
    /// `repository.history_import_allowed` / `.history_imported` /
    /// `.history_secret_imported` (2026-11-12, audit only, no event).
    /// Shapes: [`HistoryImportChange::to_value`].
    HistoryImport(HistoryImportChange),
    /// Plan 13 Q13: the document lint's migration date set or cleared
    /// (resource the project, no event). Shapes:
    /// [`DocLintMigrationChange::to_value`].
    DocLintMigration(DocLintMigrationChange),
    /// M4 C2: a person cancelled, retried or ran a CI pipeline (resource
    /// the project, no event). Shapes: [`PipelineChange::to_value`].
    Pipeline(PipelineChange),
    /// M4 C3: an administrator registered or changed a CI runner (resource
    /// the runner, no event). Shapes: [`RunnerChange::to_value`].
    Runner(RunnerChange),
    /// M4 C8 (ADR-0030): a person added a merge request to a merge queue,
    /// removed it, or paused / resumed a queue (resource the project, no
    /// event). Shapes: [`MergeQueueChange::to_value`].
    MergeQueue(MergeQueueChange),
    /// M4 C6: a Maintainer set or deleted a CI variable (resource the
    /// project, no event; never the value). Shapes:
    /// [`CiVariableChange::to_value`].
    CiVariable(CiVariableChange),
    /// M4 C11: a deployment environment or target set, a deployment
    /// requested or approved (resource the project, no event; never a key
    /// or script output). Shapes: [`crate::deploy::DeployChange::to_value`].
    Deploy(crate::deploy::DeployChange),
    // ── M2 MR3: web sessions (ADR-0021 §5), audit-only, on the user ──
    /// `session.created`: a web session was issued (the OIDC login, MR4).
    /// Ids and the public id only; never the secret, its hash or the IdP's
    /// subject.
    SessionCreated {
        session_id: i64,
        user: UserId,
        public_id: String,
    },
    /// `session.revoked`: one session ended before its expiry.
    SessionRevoked {
        session_id: i64,
        user: UserId,
        public_id: String,
        reason: SessionRevokeReason,
    },
    /// `session.revoked_all`: every active session of the user ended at
    /// once (an administrator, MR6); `revoked_sessions` = how many.
    SessionsRevoked {
        user: UserId,
        reason: SessionRevokeReason,
        revoked_sessions: i64,
    },
    /// `session.purged` (M2 MR6): the worker deleted `purged_sessions` of
    /// the user's sessions, each 30 days past its revocation or absolute
    /// expiry. One row per user in the purge's change (the DELETE needs an
    /// audit row for each owner); counts only.
    SessionsPurged { user: UserId, purged_sessions: i64 },
    // ── end M2 MR3 ──
    // ── M2 MR4: the OIDC sign-in (ADR-0021 §4), audit-only, on the user ──
    /// `user.created` by a first sign-in (a human, never an
    /// administrator): `{"kind":"human","namespace_id","is_admin":false,"via":"oidc"}`.
    UserCreatedOnLogin { namespace_id: NamespaceId },
    /// `user.identity_linked`: an IdP identity was linked to the user. The
    /// issuer and how; never the IdP's subject.
    UserIdentityLinked {
        issuer: String,
        via: IdentityLinkVia,
    },
    // ── end M2 MR4 ──
}

/// How an identity came to be linked (`user.identity_linked`'s `via`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityLinkVia {
    /// The employee number and the verified e-mail matched (ADR-0021 §4 2).
    EmployeeNumber,
    /// The user was created by this sign-in (§4 3).
    FirstLogin,
    /// `forge admin user link-identity` (§4 2a).
    Operator,
}

impl IdentityLinkVia {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EmployeeNumber => "employee_number",
            Self::FirstLogin => "first_login",
            Self::Operator => "operator",
        }
    }
}

/// Why a web session ended early (`web_sessions.revoked_reason`, its CHECK,
/// ADR-0021 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRevokeReason {
    /// The user signed out (or signed in again: the old session ends).
    Logout,
    /// Keycloak's back-channel logout (ADR-0021 §8).
    Backchannel,
    /// The user was disabled in GitStation (ADR-0019 Amendment M2).
    UserDisabled,
    /// An administrator ended it (MR6).
    Admin,
    /// The refresh check found the IdP session gone or the role removed
    /// (ADR-0021 §7).
    IdpRevoked,
    /// `forge restore finish` ended every session issued before the restore
    /// (M3 O1, ADR-0019 Amendment plan 11 P1).
    Restore,
}

impl SessionRevokeReason {
    pub const ALL: &'static [SessionRevokeReason] = &[
        Self::Logout,
        Self::Backchannel,
        Self::UserDisabled,
        Self::Admin,
        Self::IdpRevoked,
        Self::Restore,
    ];

    /// The stored value (and the log / audit `reason`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Logout => "logout",
            Self::Backchannel => "backchannel",
            Self::UserDisabled => "user_disabled",
            Self::Admin => "admin",
            Self::IdpRevoked => "idp_revoked",
            Self::Restore => "restore",
        }
    }

    /// A stored value back; `None` for anything else.
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|r| r.as_str() == text)
    }
}

impl AuditDetails {
    /// The `audit_log.action` this variant belongs to.
    pub fn action(&self) -> &'static str {
        match self {
            Self::UserCreated { .. } => "user.created",
            Self::UserDisabled { .. } => "user.disabled",
            Self::AdminBootstrapped { .. } => "user.admin_bootstrapped",
            Self::GroupCreated { .. } => "group.created",
            Self::ProjectCreated { .. } => "project.created",
            Self::MemberAdded { .. } => "member.added",
            Self::MemberRoleChanged { .. } => "member.role_changed",
            Self::MemberRemoved { .. } => "member.removed",
            Self::TokenCreated { .. } => "token.created",
            Self::ServiceAccountCreated { .. } => "service_account.created",
            Self::ServiceAccountUpdated { .. } => "service_account.updated",
            Self::TokenRevoked { .. } => "token.revoked",
            Self::SshKeyAdded { .. } => "ssh_key.added",
            Self::SshKeyDeleted { .. } => "ssh_key.deleted",
            // ── D4 MR3b ──
            Self::RepositoryCreated { .. } => "repository.created",
            Self::RepositoryReprovisionRequested { .. } => "repository.reprovision_requested",
            // ── end D4 MR3b ──
            Self::ProjectMemberAdded { .. } => "member.added",
            Self::ProjectMemberRoleChanged { .. } => "member.role_changed",
            Self::ProjectMemberRemoved { .. } => "member.removed",
            Self::ProtectedRefCreated { .. } => "protected_ref.created",
            Self::ProtectedRefUpdated { .. } => "protected_ref.updated",
            Self::ProtectedRefDeleted { .. } => "protected_ref.deleted",
            Self::RepositoryPushed { .. } => "repository.pushed",
            Self::ProjectUpdated { .. } => "project.updated",
            Self::ProjectVisibilityChanged { .. } => "project.visibility_changed",
            Self::GroupUpdated { .. } => "group.updated",
            Self::ProjectMoved { .. } | Self::ProjectMovedWithAncestor { .. } => "project.moved",
            Self::GroupMoved { .. } => "group.moved",
            Self::UserRenamed { .. } => "user.renamed",
            Self::TombstoneReleased { .. } => "namespace_tombstone.released",
            Self::ProjectDeleted { .. } => "project.deleted",
            Self::ProjectRestored { .. } => "project.restored",
            Self::RepositoryPurgeStarted { .. } => "repository.purge_started",
            Self::ProjectPurged { .. } => "project.purged",
            Self::SessionCreated { .. } => "session.created",
            Self::SessionRevoked { .. } => "session.revoked",
            Self::SessionsRevoked { .. } => "session.revoked_all",
            Self::SessionsPurged { .. } => "session.purged",
            Self::UserCreatedOnLogin { .. } => "user.created",
            Self::UserIdentityLinked { .. } => "user.identity_linked",
            Self::MergeRequest(change) => change.action(),
            Self::MergeRequestVersion(change) => change.action(),
            Self::ApprovalRule(change) => change.action(),
            Self::MergeSettings { change, .. } => change.action(),
            Self::ProtectedRefMergeRule(change) => change.action(),
            Self::CheckRun(change) => change.action(),
            Self::CheckRunsExpired { .. } => "check_run.retention",
            Self::CheckNamespaceGrant { granted: true, .. } => "check_namespace.granted",
            Self::CheckNamespaceGrant { granted: false, .. } => "check_namespace.revoked",
            Self::Integration(change) => change.action(),
            Self::WorkItem(change) => change.action(),
            Self::Webhook(change) => change.action(),
            Self::PushMirror(change) => change.action(),
            Self::HistoryImport(change) => change.action(),
            Self::DocLintMigration(change) => change.action(),
            Self::Pipeline(change) => change.action(),
            Self::Runner(change) => change.action(),
            Self::MergeQueue(change) => change.action(),
            Self::CiVariable(change) => change.action(),
            Self::Deploy(change) => change.action(),
        }
    }

    /// The event a change with these details publishes, `None` for the
    /// audit-only actions (`user.*`, `group.*`, `member.*` of groups; D-plan
    /// Q4, Q8). `member.*` of a project is an event (D4 plan answer 3).
    /// When `Some`, its name is [`Self::action`].
    pub fn event_type(&self) -> Option<EventType> {
        match self {
            Self::UserCreated { .. }
            | Self::UserDisabled { .. }
            | Self::AdminBootstrapped { .. }
            | Self::GroupCreated { .. }
            | Self::MemberAdded { .. }
            | Self::MemberRoleChanged { .. }
            | Self::MemberRemoved { .. }
            | Self::ServiceAccountCreated { .. }
            | Self::ServiceAccountUpdated { .. }
            | Self::SshKeyAdded { .. }
            | Self::SshKeyDeleted { .. } => None,
            Self::ProjectCreated { .. } => Some(EventType::ProjectCreated),
            Self::TokenCreated { .. } => Some(EventType::TokenCreated),
            Self::TokenRevoked { .. } => Some(EventType::TokenRevoked),
            // ── D4 MR3b ──
            Self::RepositoryCreated { .. } => Some(EventType::RepositoryCreated),
            Self::RepositoryReprovisionRequested { .. } => None,
            // ── end D4 MR3b ──
            Self::ProjectMemberAdded { .. } => Some(EventType::MemberAdded),
            Self::ProjectMemberRoleChanged { .. } => Some(EventType::MemberRoleChanged),
            Self::ProjectMemberRemoved { .. } => Some(EventType::MemberRemoved),
            Self::ProtectedRefCreated { .. } => Some(EventType::ProtectedRefCreated),
            Self::ProtectedRefUpdated { .. } => Some(EventType::ProtectedRefUpdated),
            Self::ProtectedRefDeleted { .. } => Some(EventType::ProtectedRefDeleted),
            Self::RepositoryPushed { .. } => Some(EventType::RepositoryPushed),
            Self::ProjectUpdated { .. } | Self::GroupUpdated { .. } => None,
            Self::ProjectVisibilityChanged { .. } => Some(EventType::ProjectVisibilityChanged),
            // D5 MR5: one event per move, for the moved resource. A group
            // move can touch 1000 projects, more than the 100 events one
            // change may carry, so the per-project rows of a moved ancestor
            // stay audit-only; tombstone releases are audit-only too.
            Self::ProjectMoved { .. } => Some(EventType::ProjectMoved),
            Self::GroupMoved { .. } => Some(EventType::GroupMoved),
            Self::UserRenamed { .. } => Some(EventType::UserRenamed),
            Self::ProjectMovedWithAncestor { .. } | Self::TombstoneReleased { .. } => None,
            Self::ProjectDeleted { .. } => Some(EventType::ProjectDeleted),
            Self::ProjectRestored { .. } => Some(EventType::ProjectRestored),
            // D5 MR4: the start is audit-only; the end is the one event.
            Self::RepositoryPurgeStarted { .. } => None,
            Self::ProjectPurged { .. } => Some(EventType::ProjectPurged),
            Self::MergeRequest(change) => Some(change.transition().event_type()),
            Self::WorkItem(change) => Some(change.event_type()),
            Self::Webhook(change) => change.event_type(),
            Self::MergeRequestVersion(_)
            | Self::ApprovalRule(_)
            | Self::ProtectedRefMergeRule(_)
            | Self::CheckNamespaceGrant { .. }
            | Self::CheckRunsExpired { .. }
            | Self::Integration(_)
            | Self::PushMirror(_)
            | Self::HistoryImport(_)
            | Self::DocLintMigration(_)
            | Self::Pipeline(_)
            | Self::Runner(_)
            | Self::MergeQueue(_)
            | Self::CiVariable(_)
            | Self::Deploy(_) => None,
            Self::CheckRun(change) => change.completed().then_some(EventType::CheckCompleted),
            Self::MergeSettings { change, .. } => match change {
                MergeSettingsChange::Updated { .. } => None,
                MergeSettingsChange::CodeownersEnforcementChanged { .. } => {
                    Some(EventType::CodeownersEnforcementChanged)
                }
                MergeSettingsChange::CodeownersEnforcementRestored { .. } => {
                    Some(EventType::CodeownersEnforcementRestored)
                }
            },
            // ADR-0021 §5: sessions are audit rows, never domain events.
            Self::SessionCreated { .. }
            | Self::SessionRevoked { .. }
            | Self::SessionsRevoked { .. }
            | Self::SessionsPurged { .. }
            | Self::UserCreatedOnLogin { .. }
            | Self::UserIdentityLinked { .. } => None,
        }
    }

    /// The JSON stored in `audit_log.details`:
    /// UserCreated `{"kind","namespace_id","is_admin"}`,
    /// UserDisabled `{"kind","revoked_tokens","revoked_sessions","forced","ownerless_groups",
    /// "ownerless_projects","left_without_admin"}`,
    /// SessionCreated `{"session_id","user_id","public_id"}`,
    /// SessionRevoked `{"session_id","user_id","public_id","reason"}`,
    /// SessionsRevoked `{"user_id","reason","revoked_sessions"}`,
    /// SessionsPurged `{"user_id","purged_sessions"}`,
    /// AdminBootstrapped `{"created","namespace_id"}`,
    /// GroupCreated `{"namespace_id","parent_namespace_id","owner_id"}`,
    /// ProjectCreated `{"project_id","namespace_id","parent_namespace_id",
    /// "repository_id","visibility","owner_id"}`,
    /// MemberAdded / MemberRemoved `{"user_id","role"}`,
    /// MemberRoleChanged `{"user_id","from","to"}`,
    /// TokenCreated `{"token_id","user_id","public_id","scopes","expires_at","is_agent"}`,
    /// (+ `"issued_by_user_id"` when an administrator issued it, M3 S0),
    /// TokenRevoked `{"token_id","user_id","public_id"}`,
    /// SshKeyAdded `{"ssh_key_id","user_id","algorithm","fingerprint"}`,
    /// SshKeyDeleted `{"ssh_key_id","user_id","fingerprint"}`,
    /// ServiceAccountCreated `{"namespace_id","owner_id","purpose_chars"}`,
    /// ServiceAccountUpdated `{"owner_from","owner_to","fields"}`,
    /// RepositoryCreated `{"project_id","repository_id"}`,
    /// RepositoryReprovisionRequested `{"repository_id","job_id","holder_job_id",
    /// "holder_state"}` (`holder_state` = `succeeded` with its id, or `none`).
    /// ProjectMemberAdded / ProjectMemberRemoved `{"user_id","role"}`,
    /// ProjectMemberRoleChanged `{"user_id","from","to"}` (the group shapes),
    /// ProtectedRefCreated `{"protected_ref_id","pattern","push_min_role"}`,
    /// ProtectedRefDeleted
    /// `{"protected_ref_id","pattern","push_min_role","stored_invalid"}`,
    /// ProtectedRefUpdated `{"protected_ref_id","pattern","from","to"}`,
    /// RepositoryPushed: [`push_json`] within [`AUDIT_BUDGET`],
    /// ProjectUpdated `{"project_id","fields"}`, GroupUpdated
    /// `{"group_id","fields"}` (field names only),
    /// ProjectVisibilityChanged `{"project_id","from","to"}`,
    /// ProjectDeleted / ProjectRestored
    /// `{"project_id","deleted_at","purge_after","deleted_by"}`,
    /// RepositoryPurgeStarted `{"project_id","repository_id","from"}`,
    /// ProjectPurged `{"project_id","repository_id","namespace_id","deleted_at",
    /// "purge_after","deleted_by","tombstone_id","redirects_ended","members_removed",
    /// "protected_refs_removed","merge_requests_removed","storage_removed"}`,
    /// MergeRequest: [`MergeRequestChange::to_value`],
    /// ProjectMoved `{"project_id"}` + [`MoveSummary`]'s fields,
    /// ProjectMovedWithAncestor `{"project_id","namespace_id","moved_namespace_id"}`,
    /// GroupMoved `{"group_id"}` + the summary, UserRenamed `{"user_id"}` +
    /// the summary, TombstoneReleased
    /// `{"tombstone_id","kind","namespace_id","reclaim"}`.
    pub fn to_value(&self) -> serde_json::Value {
        match self {
            Self::UserCreated {
                kind,
                namespace_id,
                is_admin,
            } => serde_json::json!({
                "kind": kind.as_str(),
                "namespace_id": namespace_id.get(),
                "is_admin": is_admin,
            }),
            Self::UserDisabled {
                kind,
                revoked_tokens,
                revoked_sessions,
                forced,
                ownerless_groups,
                ownerless_projects,
                left_without_admin,
            } => serde_json::json!({
                "kind": kind.as_str(),
                "revoked_tokens": revoked_tokens,
                "revoked_sessions": revoked_sessions,
                "forced": forced,
                "ownerless_groups": ownerless_groups,
                "ownerless_projects": ownerless_projects,
                "left_without_admin": left_without_admin,
            }),
            Self::AdminBootstrapped {
                created,
                namespace_id,
            } => serde_json::json!({
                "created": created,
                "namespace_id": namespace_id.map(NamespaceId::get),
            }),
            Self::GroupCreated {
                namespace_id,
                parent_namespace_id,
                owner,
            } => serde_json::json!({
                "namespace_id": namespace_id.get(),
                "parent_namespace_id": parent_namespace_id.map(NamespaceId::get),
                "owner_id": owner.get(),
            }),
            Self::ProjectCreated {
                project_id,
                namespace_id,
                parent_namespace_id,
                repository_id,
                visibility,
                owner,
            } => serde_json::json!({
                "project_id": project_id.get(),
                "namespace_id": namespace_id.get(),
                "parent_namespace_id": parent_namespace_id.get(),
                "repository_id": repository_id.get(),
                "visibility": visibility.as_str(),
                "owner_id": owner.get(),
            }),
            Self::MemberAdded { user, role }
            | Self::MemberRemoved { user, role }
            | Self::ProjectMemberAdded { user, role }
            | Self::ProjectMemberRemoved { user, role } => {
                serde_json::json!({ "user_id": user.get(), "role": role.as_str() })
            }
            Self::MemberRoleChanged { user, from, to }
            | Self::ProjectMemberRoleChanged { user, from, to } => serde_json::json!({
                "user_id": user.get(),
                "from": from.as_str(),
                "to": to.as_str(),
            }),
            Self::ProtectedRefCreated {
                rule,
                pattern,
                push_min_role,
            } => serde_json::json!({
                "protected_ref_id": rule.get(),
                "pattern": pattern,
                "push_min_role": push_min_role.as_str(),
            }),
            Self::ProtectedRefDeleted {
                rule,
                pattern,
                push_min_role,
                stored_invalid,
            } => serde_json::json!({
                "protected_ref_id": rule.get(),
                "pattern": pattern,
                "push_min_role": push_min_role,
                "stored_invalid": stored_invalid,
            }),
            Self::ProtectedRefUpdated {
                rule,
                pattern,
                from,
                to,
            } => serde_json::json!({
                "protected_ref_id": rule.get(),
                "pattern": pattern,
                "from": from.as_str(),
                "to": to.as_str(),
            }),
            Self::TokenCreated {
                token_id,
                user,
                public_id,
                scopes,
                expires_at,
                is_agent,
                issued_by,
            } => {
                let mut value = serde_json::json!({
                    "token_id": token_id,
                    "user_id": user.get(),
                    "public_id": public_id,
                    "scopes": scopes.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                    "expires_at": expires_at,
                    "is_agent": is_agent,
                });
                if let (Some(by), serde_json::Value::Object(map)) = (issued_by, &mut value) {
                    map.insert("issued_by_user_id".to_owned(), by.get().into());
                }
                value
            }
            Self::ServiceAccountCreated {
                namespace_id,
                owner,
                purpose_chars,
            } => serde_json::json!({
                "namespace_id": namespace_id.get(),
                "owner_id": owner.get(),
                "purpose_chars": purpose_chars,
            }),
            Self::ServiceAccountUpdated {
                owner_from,
                owner_to,
                fields,
            } => serde_json::json!({
                "owner_from": owner_from.get(),
                "owner_to": owner_to.get(),
                "fields": fields.iter().map(|f| f.as_str()).collect::<Vec<_>>(),
            }),
            Self::TokenRevoked {
                token_id,
                user,
                public_id,
            } => serde_json::json!({
                "token_id": token_id,
                "user_id": user.get(),
                "public_id": public_id,
            }),
            Self::SshKeyAdded {
                key_id,
                user,
                algorithm,
                fingerprint,
            } => serde_json::json!({
                "ssh_key_id": key_id,
                "user_id": user.get(),
                "algorithm": algorithm,
                "fingerprint": fingerprint,
            }),
            Self::SshKeyDeleted {
                key_id,
                user,
                fingerprint,
            } => serde_json::json!({
                "ssh_key_id": key_id,
                "user_id": user.get(),
                "fingerprint": fingerprint,
            }),
            // ── D4 MR3b ──
            Self::RepositoryCreated {
                project_id,
                repository_id,
            } => serde_json::json!({
                "project_id": project_id.get(),
                "repository_id": repository_id.get(),
            }),
            Self::RepositoryReprovisionRequested {
                repository_id,
                job_id,
                after_succeeded_job,
            } => serde_json::json!({
                "repository_id": repository_id.get(),
                "job_id": job_id,
                "holder_job_id": after_succeeded_job,
                "holder_state": if after_succeeded_job.is_some() { "succeeded" } else { "none" },
            }),
            // ── end D4 MR3b ──
            Self::RepositoryPushed {
                project_id,
                repository_id,
                updates,
                replayed_from,
            } => push_json(
                *project_id,
                *repository_id,
                updates,
                *replayed_from,
                AUDIT_BUDGET,
            ),
            Self::ProjectUpdated { project_id, fields } => serde_json::json!({
                "project_id": project_id.get(),
                "fields": field_names(fields),
            }),
            Self::GroupUpdated { group_id, fields } => serde_json::json!({
                "group_id": group_id.get(),
                "fields": field_names(fields),
            }),
            Self::ProjectVisibilityChanged {
                project_id,
                from,
                to,
            } => serde_json::json!({
                "project_id": project_id.get(),
                "from": from.as_str(),
                "to": to.as_str(),
            }),
            Self::ProjectMoved {
                project_id,
                summary,
            } => with_id("project_id", project_id.get(), summary),
            Self::ProjectMovedWithAncestor {
                project_id,
                namespace_id,
                moved_namespace_id,
            } => serde_json::json!({
                "project_id": project_id.get(),
                "namespace_id": namespace_id.get(),
                "moved_namespace_id": moved_namespace_id.get(),
            }),
            Self::ProjectDeleted {
                project_id,
                deleted_at,
                purge_after,
                deleted_by,
            }
            | Self::ProjectRestored {
                project_id,
                deleted_at,
                purge_after,
                deleted_by,
            } => serde_json::json!({
                "project_id": project_id.get(),
                "deleted_at": deleted_at,
                "purge_after": purge_after,
                "deleted_by": deleted_by.map(UserId::get),
            }),
            Self::RepositoryPurgeStarted {
                project_id,
                repository_id,
                from,
            } => serde_json::json!({
                "project_id": project_id.get(),
                "repository_id": repository_id.get(),
                "from": from.as_str(),
            }),
            Self::ProjectPurged {
                project_id,
                repository_id,
                namespace_id,
                deleted_at,
                purge_after,
                deleted_by,
                tombstone_id,
                redirects_ended,
                members_removed,
                protected_refs_removed,
                merge_requests_removed,
                storage_removed,
            } => serde_json::json!({
                "project_id": project_id.get(),
                "repository_id": repository_id.get(),
                "namespace_id": namespace_id.get(),
                "deleted_at": deleted_at,
                "purge_after": purge_after,
                "deleted_by": deleted_by.map(UserId::get),
                "tombstone_id": tombstone_id.get(),
                "redirects_ended": redirects_ended,
                "members_removed": members_removed,
                "protected_refs_removed": protected_refs_removed,
                "merge_requests_removed": merge_requests_removed,
                "storage_removed": storage_removed,
            }),
            Self::GroupMoved { group_id, summary } => with_id("group_id", group_id.get(), summary),
            Self::UserRenamed { user_id, summary } => with_id("user_id", user_id.get(), summary),
            Self::TombstoneReleased {
                tombstone_id,
                kind,
                namespace_id,
                reclaim,
            } => serde_json::json!({
                "tombstone_id": tombstone_id.get(),
                "kind": kind.as_str(),
                "namespace_id": namespace_id.map(NamespaceId::get),
                "reclaim": reclaim,
            }),
            Self::SessionCreated {
                session_id,
                user,
                public_id,
            } => serde_json::json!({
                "session_id": session_id,
                "user_id": user.get(),
                "public_id": public_id,
            }),
            Self::SessionRevoked {
                session_id,
                user,
                public_id,
                reason,
            } => serde_json::json!({
                "session_id": session_id,
                "user_id": user.get(),
                "public_id": public_id,
                "reason": reason.as_str(),
            }),
            Self::SessionsRevoked {
                user,
                reason,
                revoked_sessions,
            } => serde_json::json!({
                "user_id": user.get(),
                "reason": reason.as_str(),
                "revoked_sessions": revoked_sessions,
            }),
            Self::SessionsPurged {
                user,
                purged_sessions,
            } => serde_json::json!({
                "user_id": user.get(),
                "purged_sessions": purged_sessions,
            }),
            Self::UserCreatedOnLogin { namespace_id } => serde_json::json!({
                "kind": UserKind::Human.as_str(),
                "namespace_id": namespace_id.get(),
                "is_admin": false,
                "via": "oidc",
            }),
            Self::UserIdentityLinked { issuer, via } => serde_json::json!({
                "issuer": issuer,
                "via": via.as_str(),
            }),
            Self::MergeRequest(change) => change.to_value(),
            Self::MergeRequestVersion(change) => change.to_value(),
            Self::ApprovalRule(change) => change.to_value(),
            Self::MergeSettings { project, change } => change.to_value(*project),
            Self::ProtectedRefMergeRule(change) => change.to_value(),
            Self::CheckRun(change) => change.to_value(),
            Self::CheckRunsExpired {
                project,
                runs,
                annotations,
                older_than_days,
            } => serde_json::json!({
                "project_id": project.get(),
                "runs": runs,
                "annotations": annotations,
                "older_than_days": older_than_days,
            }),
            Self::CheckNamespaceGrant {
                user,
                project,
                namespace,
                ..
            } => serde_json::json!({
                "user_id": user.get(),
                "project_id": project.get(),
                "namespace": namespace.as_str(),
            }),
            Self::Integration(change) => change.to_value(),
            Self::WorkItem(change) => change.to_value(),
            Self::Webhook(change) => change.to_value(),
            Self::PushMirror(change) => change.to_value(),
            Self::HistoryImport(change) => change.to_value(),
            Self::DocLintMigration(change) => change.to_value(),
            Self::Pipeline(change) => change.to_value(),
            Self::Runner(change) => change.to_value(),
            Self::MergeQueue(change) => change.to_value(),
            Self::CiVariable(change) => change.to_value(),
            Self::Deploy(change) => change.to_value(),
        }
    }

    /// The payload of the event these details publish: the same JSON as
    /// [`Self::to_value`], except `repository.pushed`, whose payload is
    /// built within the larger [`EVENT_BUDGET`], and the move events
    /// (`project.moved`, `group.moved`, `user.renamed`, D5 MR5), whose
    /// payload is the summary without the role changes
    /// ([`MoveSummary::event_value`]: who gained or lost an inherited role
    /// stays in the audit log).
    pub fn event_payload(&self) -> serde_json::Value {
        match self {
            Self::RepositoryPushed {
                project_id,
                repository_id,
                updates,
                replayed_from,
            } => push_json(
                *project_id,
                *repository_id,
                updates,
                *replayed_from,
                EVENT_BUDGET,
            ),
            Self::ProjectMoved {
                project_id,
                summary,
            } => with_id_value("project_id", project_id.get(), summary.event_value()),
            Self::GroupMoved { group_id, summary } => {
                with_id_value("group_id", group_id.get(), summary.event_value())
            }
            Self::UserRenamed { user_id, summary } => {
                with_id_value("user_id", user_id.get(), summary.event_value())
            }
            Self::UserCreated { .. }
            | Self::UserDisabled { .. }
            | Self::AdminBootstrapped { .. }
            | Self::GroupCreated { .. }
            | Self::ProjectCreated { .. }
            | Self::MemberAdded { .. }
            | Self::MemberRoleChanged { .. }
            | Self::MemberRemoved { .. }
            | Self::TokenCreated { .. }
            | Self::TokenRevoked { .. }
            | Self::SshKeyAdded { .. }
            | Self::SshKeyDeleted { .. }
            | Self::ServiceAccountCreated { .. }
            | Self::ServiceAccountUpdated { .. }
            | Self::RepositoryCreated { .. }
            | Self::RepositoryReprovisionRequested { .. }
            | Self::ProjectMemberAdded { .. }
            | Self::ProjectMemberRoleChanged { .. }
            | Self::ProjectMemberRemoved { .. }
            | Self::ProtectedRefCreated { .. }
            | Self::ProtectedRefUpdated { .. }
            | Self::ProtectedRefDeleted { .. }
            | Self::ProjectUpdated { .. }
            | Self::ProjectVisibilityChanged { .. }
            | Self::GroupUpdated { .. }
            | Self::ProjectMovedWithAncestor { .. }
            | Self::TombstoneReleased { .. }
            | Self::ProjectDeleted { .. }
            | Self::ProjectRestored { .. }
            | Self::RepositoryPurgeStarted { .. }
            | Self::ProjectPurged { .. }
            | Self::SessionCreated { .. }
            | Self::SessionRevoked { .. }
            | Self::SessionsRevoked { .. }
            | Self::SessionsPurged { .. }
            | Self::UserCreatedOnLogin { .. }
            | Self::UserIdentityLinked { .. }
            | Self::MergeRequest(_)
            | Self::MergeRequestVersion(_)
            | Self::ApprovalRule(_)
            | Self::MergeSettings { .. }
            | Self::ProtectedRefMergeRule(_)
            | Self::CheckRun(_)
            | Self::CheckRunsExpired { .. }
            | Self::CheckNamespaceGrant { .. }
            | Self::Integration(_)
            | Self::WorkItem(_)
            | Self::Webhook(_)
            | Self::PushMirror(_)
            | Self::HistoryImport(_)
            | Self::DocLintMigration(_)
            | Self::Pipeline(_)
            | Self::Runner(_)
            | Self::MergeQueue(_)
            | Self::CiVariable(_)
            | Self::Deploy(_) => self.to_value(),
        }
    }
}

/// `{<key>: id}` merged with the move summary's fields.
fn with_id(key: &str, id: i64, summary: &MoveSummary) -> serde_json::Value {
    with_id_value(key, id, summary.to_value())
}

/// `{<key>: id}` merged with `value`'s fields.
fn with_id_value(key: &str, id: i64, mut value: serde_json::Value) -> serde_json::Value {
    if let serde_json::Value::Object(map) = &mut value {
        map.insert(key.to_owned(), id.into());
    }
    value
}
