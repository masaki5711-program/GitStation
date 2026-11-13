//! Every operation that needs a decision. The enum, `Action::ALL` and the
//! names are generated together by one macro, so a variant can never be
//! missing from `ALL`. A new variant breaks the build of `authorize()`
//! (exhaustive match) and fails the matrix tests until its rows exist
//! (CLAUDE.md §3-4, ADR-0010).

use crate::model::Scope;

macro_rules! actions {
    ($( $(#[$doc:meta])* $name:ident ),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum Action {
            $( $(#[$doc])* $name, )+
        }

        impl Action {
            /// Every action, for the matrix tests and the coverage manifest.
            pub const ALL: &'static [Action] = &[ $( Action::$name ),+ ];

            /// Stable name used in logs, the route registry and the manifests.
            pub fn name(self) -> &'static str {
                match self {
                    $( Action::$name => stringify!($name), )+
                }
            }
        }
    };
}

actions! {
    // --- git operations ---
    /// git clone / fetch / ls-remote (upload-pack).
    RepoRead,
    /// One ref update in a git push (receive-pack).
    RepoPush,
    /// The repository browse API (`GET /api/v1/projects/{id}/repository/…`:
    /// refs, resolve, tree, blob; M2 MR7, ADR-0015 Amendment M2). Decided
    /// like `RepoRead` (the same arm), but with the `read_api` scope: a
    /// `read_repository` token (CI clones) does not read the JSON API, and a
    /// `read_api` token still cannot clone.
    RepoBrowse,
    /// `GET /api/v1/projects/{id}/repository/spec-refs` (M3 X1, plan 03
    /// §4.4, ADR-0023 Amendment X1): a revision's `.forge/repo.yml` and
    /// the spec references it pins. Decided like `RepoBrowse` (the same
    /// arm, `read_api`) on the referencing project by the route, and again
    /// on every referenced project with the reader's own principal
    /// (`prove_spec_ref`).
    SpecRefRead,
    /// The spec API (M3 X4, plan 03 §7.2, ADR-0023 Amendment X4): `GET
    /// /api/v1/projects/{id}/specs/{requirements,versions,diff,trace}` —
    /// the project's requirement index at a revision (its own
    /// specification and `@req` tags). Decided like `RepoBrowse` (the same
    /// arm, `read_api`); the trace decides every referenced project again
    /// with the reader's own principal (`prove_spec_ref`).
    SpecRead,
    /// The context API (M3 X5, plan 03 §6, ADR-0023 Amendment X5): `GET
    /// /api/v1/projects/{id}/context/{work-item,requirement}` — a work
    /// item's or a requirement's context with its provenance. Decided like
    /// `RepoBrowse` (the same arm, `read_api`). What is marked confidential
    /// is withheld from an AI agent after the decision
    /// (`Principal::is_agent_audience`, plan 06 D-5).
    ContextRead,
    // --- REST API: projects ---
    ProjectRead,
    /// `GET /api/v1/projects`: may the caller list at all (any active
    /// signed-in user; `read_api` on a token). Decided on the instance;
    /// every row is then decided again with `ProjectRead` (M1 phase 8b).
    ProjectList,
    ProjectCreate,
    ProjectUpdate,
    /// Private ⇄ internal: Owner only (exposes a project to every signed-in user).
    ProjectVisibilityChange,
    /// Move or rename a project (`Resource::Transfer`, ADR-0015).
    ProjectTransfer,
    ProjectDelete,
    ProjectRestore,
    ProtectedRefManage,
    MemberManage,
    /// `GET /api/v1/projects/{id}/members`, `GET /api/v1/groups/{id}/members`:
    /// the direct members (M1 phase 8b MR8). A project's as `ProjectRead`
    /// (Reporter; every signed-in user on an internal project), a group's
    /// for every signed-in user (groups are internal).
    MemberList,
    /// `GET /api/v1/projects/{id}/protected-refs`: the protection rules
    /// (M1 phase 8b MR8), as `ProjectRead`.
    ProtectedRefList,
    // --- REST API: merge requests (M3 R1a, ADR-0024 §12) ---
    /// `POST /api/v1/projects/{id}/merge-requests`: open a merge request
    /// (Developer and above, like `RepoPush`; opening one is not a push, so
    /// the source branch's protection is not consulted, plan 11 §11.7).
    MergeRequestCreate,
    /// `GET /api/v1/projects/{id}/merge-requests/{iid}`: as `RepoBrowse`.
    MergeRequestRead,
    /// `GET /api/v1/projects/{id}/merge-requests`: as `RepoBrowse`.
    MergeRequestList,
    /// `PATCH …/merge-requests/{iid}`: title, description, draft, target.
    /// The author or Developer and above; another's draft flag only the
    /// author or Maintainer and above.
    MergeRequestUpdate,
    /// `POST …/merge-requests/{iid}/close`: the author or Developer and above.
    MergeRequestClose,
    /// `POST …/merge-requests/{iid}/reopen`: as close; a merged one never.
    MergeRequestReopen,
    /// `POST …/merge-requests/{iid}/version-refs/purge` (M3 R1b, ADR-0024
    /// §2): remove the hidden version refs (secret removal). A human
    /// administrator's web session only: every token — an `admin`-scoped
    /// one included — is `FG-AUTH-0002 reason=session_required`; agents and
    /// service accounts hold only tokens (the sign-in admits people only,
    /// `not_human`), so they never reach it.
    MergeRequestVersionRefsPurge,
    /// `POST …/merge-requests/{iid}/discussions` and `…/discussions/{d}/notes`
    /// (M3 R3, ADR-0024 §12): start a discussion or reply to one. Reporter
    /// and above by membership; agents' tokens and service accounts too (a
    /// comment is no merge condition; resolving is `DiscussionResolve`).
    MergeRequestComment,
    /// `POST …/discussions/{d}/resolve` and `…/unresolve` (M3 R3,
    /// `Resource::Discussion`): people only (agents, service accounts and
    /// bots are refused); the thread's starter, or a Developer and above
    /// who is not the merge request's author (administrators included).
    DiscussionResolve,
    /// `POST …/merge-requests/{iid}/merge` (M3 R4, ADR-0024 §4.3, §12):
    /// merge a merge request. People only (agents, service accounts and
    /// bots are refused); the target branch's `push_min_role` and above
    /// (Developer when unprotected) — the direct-push ban of a protected
    /// branch (R6) does not stop it. A token needs `merge_execute` (`api`
    /// does not satisfy it, ADR-0017 §4.0). Whether the merge request is
    /// ready is `evaluate_merge()`'s, never a permission.
    MergeRequestMerge,
    /// `POST …/merge-requests/{iid}/approve` (M3 R5, ADR-0024 §8, §12):
    /// approve the head and target the caller saw. People only (agents,
    /// service accounts and bots are refused); Developer and above by
    /// membership; never the merge request's author, administrators
    /// included (`FG-REVIEW-0013`). A token needs `merge_approve` (`api`
    /// and `merge_execute` do not satisfy it). Whether the approval counts
    /// (rules, CODEOWNERS, committers) is `evaluate_merge()`'s, decided at
    /// evaluation time.
    MergeRequestApprove,
    /// `POST …/merge-requests/{iid}/unapprove` (M3 R5): withdraw one's own
    /// approval; decided like `MergeRequestApprove`.
    MergeRequestUnapprove,
    /// `PUT|DELETE …/merge-requests/{iid}/work-items/{ref}` (M3 I1b, plan
    /// 11 §4.3, plan 02 §2): link a work item (kanflow `PBI-<n>` of the
    /// project's binding) to a merge request by hand, or unlink one.
    /// Developer and above (agents' tokens too: no merge condition, and
    /// detection links the same from titles); `api`. Whether the project
    /// is bound is the command's (`FG-INT-0018`), not a permission. Acting
    /// for a person (kanflow) comes with S2.
    WorkItemLink,
    /// `PUT …/approval-rules/{name}`, `DELETE …/approval-rules/{name}` (M3
    /// R5, ADR-0024 §8.6, §12): manage a project's approval rules. People
    /// only (a rule is a merge condition); Maintainer and above; `api`.
    ApprovalRuleManage,
    /// `PUT /api/v1/projects/{id}/merge-settings` (M3 R6, ADR-0024 §4.1,
    /// §8.2, §8.3, §12): the methods a project allows and their default,
    /// the source deletion default, whether new commits clear approvals,
    /// whether committers' approvals are excluded. People only (they are
    /// merge conditions); Maintainer and above; `api`.
    MergeSettingsManage,
    /// `PUT /api/v1/projects/{id}/merge-settings/codeowners-enforcement`
    /// (M3 R6, ADR-0024 §9, `Resource::CodeownersEnforcement`): suspend
    /// CODEOWNERS enforcement for at most 24 hours (an expiry is required)
    /// or enforce it again. Its own action (not part of
    /// `MergeSettingsManage`): people only, Maintainer and above, and a
    /// suspension also needs a role at or above every protection rule of
    /// the project (`above_own_role`); `api`.
    CodeownersEnforcementToggle,
    // --- REST API: check runs (M3 C1, ADR-0024 §10, §12) ---
    /// `POST /api/v1/projects/{id}/commits/{sha}/checks`
    /// (`Resource::CheckRun`): report a check run (create or update by
    /// name). `ci/` and `kanflow/` only by a service account holding a grant
    /// of the namespace and a Developer (or above) membership; people —
    /// administrators included —, bots and agents' tokens are refused;
    /// `forge/` is never written over HTTP; `ext/` is refused in C1. `api`.
    CheckWrite,
    /// `GET /api/v1/projects/{id}/commits/{sha}/checks`: as `RepoBrowse`
    /// (`read_api`).
    CheckRead,
    /// `PUT|DELETE /api/v1/admin/service-accounts/{id}/check-grants/{project_id}/{ns}`
    /// (`Resource::ServiceAccount`): grant or revoke a check namespace
    /// (`ci`, `kanflow`) to a service account. Administrators, people only
    /// (who may report a merge condition is a person's decision); `api` +
    /// `admin` on a token.
    CheckNamespaceGrantManage,
    // --- REST API: groups ---
    GroupRead,
    /// `GET /api/v1/groups`: as `ProjectList`; every row is then decided
    /// with `GroupRead` (M1 phase 8b).
    GroupList,
    GroupCreateTopLevel,
    SubgroupCreate,
    GroupUpdate,
    /// Move or rename a group (`Resource::GroupTransfer`, ADR-0015, D5).
    GroupTransfer,
    /// Release a namespace tombstone (`Resource::Tombstone`, ADR-0015, D5).
    NamespaceTombstoneRelease,
    // --- REST API: tokens and users ---
    /// `POST /api/v1/user/tokens` (M2 MR6): create a token for oneself.
    /// Sessions only (ADR-0019 Amendment M2 4): a token never creates a
    /// token (`FG-AUTH-0002 reason=session_required`).
    TokenCreate,
    /// `GET /api/v1/user/tokens` (M2 MR6): one's own tokens.
    TokenList,
    /// `DELETE /api/v1/user/tokens/{id}` (M2 MR6).
    TokenRevoke,
    // --- REST API: notification settings (M3 N1, ADR-0016 Amendment N1) ---
    /// `GET /api/v1/user/notification-preferences`: one's own mail
    /// settings (mail on or off, language, muted kinds). Only the person:
    /// another user's are not found, administrators included.
    NotificationPreferencesRead,
    /// `PUT /api/v1/user/notification-preferences`: replace one's own.
    NotificationPreferencesWrite,
    /// `GET /api/v1/user/notifications` and `…/unread-count` (M3 U4): one's
    /// own in-app notifications. Only the person: another user's are not
    /// found, administrators included; each item is decided again on
    /// `MergeRequestRead` when it is listed.
    NotificationInboxRead,
    /// `POST /api/v1/user/notifications/{id}/read` and `…/read-all`: mark
    /// one's own read.
    NotificationInboxUpdate,
    /// `GET /api/v1/user/merge-requests` and `…/counts` (plan 15 MR 6): the
    /// merge requests of one's own work across projects (authored,
    /// approvable, needing attention). Only the person: another user's are
    /// not found, administrators included; each row is decided again on
    /// `MergeRequestRead` (and an approvable one on `MergeRequestApprove`)
    /// when it is listed or counted.
    UserMergeRequestList,
    /// `GET /api/v1/user/activity` (plan 15 MR 8): the recent events of the
    /// projects one is involved in. The route (`Resource::UserActivity`):
    /// only the person, people only (an agent's token, a service account
    /// and a bot are refused; act-as and CI credentials too); another
    /// user's is not found, administrators included. Each event is decided
    /// again per project and visibility class (`Resource::ProjectEvents`):
    /// the integration class only, Reporter and above by role
    /// (administrator rights pass the role, as everywhere: an administrator
    /// sees the events of every project they hold a membership in, a Guest
    /// one included). `read_api`.
    ActivityRead,
    UserCreate,
    /// `POST /api/v1/admin/users/{id}/disable` (M2 MR6): administrators,
    /// never an agent; the administration guard always holds over HTTP
    /// (`users::Guard::Enforce`, ADR-0019 §10).
    UserDisable,
    /// `GET /api/v1/users?search=` (M2 MR6): find users by name for the
    /// member picker (any active signed-in user; names only).
    UserSearch,
    /// `GET /api/v1/admin/users` (M2 MR6): every user with its state
    /// (administrators; a token needs the `admin` scope).
    UserAdminList,
    /// Change a user's username and personal namespace path
    /// (`Resource::UserRename`, ADR-0015, D5): administrators only.
    UserRename,
    // --- REST API: service accounts (M3 S0, ADR-0019 Addendum S0) ---
    /// `POST /api/v1/admin/service-accounts`: create a service account
    /// (an owner who is a person, a purpose). A human administrator's web
    /// session only: every token is `FG-AUTH-0002 reason=session_required`.
    ServiceAccountCreate,
    /// `GET /api/v1/admin/service-accounts`: administrators; a token needs
    /// the `admin` scope (and `read_api`).
    ServiceAccountList,
    /// `GET /api/v1/admin/service-accounts/{id}` (`Resource::ServiceAccount`):
    /// administrators; a user of another kind is not found
    /// (`reason=not_service_account`).
    ServiceAccountRead,
    /// `PATCH /api/v1/admin/service-accounts/{id}`: display name, purpose,
    /// owner. Administrators, people only (who answers for an account is a
    /// person's decision); `api` + `admin` on a token.
    ServiceAccountUpdate,
    /// `POST /api/v1/admin/service-accounts/{id}/tokens`
    /// (`Resource::ServiceAccountTokenRequest`): issue a token for an active
    /// service account. A human administrator's web session only (a token
    /// never issues a token); never with `admin`; a disabled account is
    /// refused (`target_disabled`).
    ServiceAccountTokenIssue,
    /// `GET /api/v1/admin/service-accounts/{id}/tokens`: administrators
    /// (`admin` + `read_api` on a token).
    ServiceAccountTokenList,
    /// `DELETE /api/v1/admin/service-accounts/{id}/tokens/{token_id}`:
    /// administrators, an `admin`-scoped token included (a leak is answered
    /// from wherever the administrator is, ADR-0019 Addendum S0).
    ServiceAccountTokenRevoke,
    // --- REST API: integrations (M3 I1a, plan 11 §5 I1, ADR-0016 §7) ---
    /// `POST /api/v1/admin/integrations`, `PATCH …/{id}`: create an
    /// integration (kanflow) acting as a service account, enable or
    /// disable it, change its event type allowlist. A human
    /// administrator's web session only: every token, an `admin`-scoped
    /// one included, is `FG-AUTH-0002 reason=session_required` (an
    /// integration's reach is a person's decision; the emergency stop with
    /// a token is disabling its service account, `UserDisable`).
    IntegrationManage,
    /// `GET /api/v1/admin/integrations`, `…/{id}`, `…/{id}/bindings`:
    /// administrators, people only (agents, service accounts and bots are
    /// refused); a token needs `admin` and `read_api`.
    IntegrationRead,
    /// `PUT|DELETE /api/v1/projects/{id}/integration-bindings/{integration_id}`:
    /// bind a project to an external (kanflow) project, or unbind it; and
    /// (M3 X3) `POST /api/v1/integrations/{id}/bindings` and `PATCH
    /// …/bindings/{binding_id}`: bind a repository, change a binding's
    /// setting. People only (agents, service accounts and bots are
    /// refused); acting for a person only with a `binding_set` assertion
    /// (ADR-0025 Amendment X3), never the integration's service account on
    /// its own; Maintainer and above; `api`. Binding makes the
    /// integration's service account a Reporter of the project, so it is a
    /// membership change.
    IntegrationBindingManage,
    /// `DELETE /api/v1/integrations/{id}/bindings/{binding_id}` (M3 X3):
    /// remove one binding. Decided like `IntegrationBindingManage`, acting
    /// for a person only with a `binding_remove` assertion (one intent per
    /// action, ADR-0025 §5.1).
    IntegrationBindingRemove,
    /// `GET /api/v1/integrations/{id}/bindings?kanflow_project_id=` (M3 X3,
    /// plan 03 §7.1): the repositories bound to the integration (to one of
    /// its kanflow projects). The integration's own service account only,
    /// as its feed (`Resource::IntegrationFeed`); never acting for a
    /// person; `read_api`.
    IntegrationBindingList,
    /// `GET /api/v1/repositories?q=` (M3 X3, plan 03 §7.1): the
    /// repositories the caller — or the person an act-as request names —
    /// may bind, by path or name. The route on `Resource::Instance` (any
    /// active signed-in caller), each candidate on its `Resource::Project`
    /// (Maintainer and above): only what the person could bind is listed,
    /// nothing about the rest (no existence oracle). `read_api`.
    RepositorySearch,
    /// `GET /api/v1/integrations/{id}/events` (M3 I1c, ADR-0016 §7, plan
    /// 02 §5.7 / §11.2): the integration's event feed. Only that
    /// integration's own service account (`Resource::IntegrationFeed`:
    /// another integration, a person — administrators included —, a bot,
    /// an agent's token, an act-as request and a disabled integration are
    /// refused); `read_api`. What it then sees is decided per bound project
    /// and visibility class (`Resource::ProjectEvents`: the integration
    /// class by a Reporter membership; never the maintainer, deployment or
    /// admin class) by `prove_visible`.
    IntegrationEventRead,
    /// `GET /api/v1/integrations/{id}/events/cursor` (M3 I1c): the feed's
    /// current head as a cursor (the first step of a resync). Decided like
    /// `IntegrationEventRead`.
    IntegrationEventCursorRead,
    /// `GET /api/v1/integrations/{id}/work-items/{ref}/dev-status` and
    /// `GET /api/v1/integrations/{id}/dev-status` (M3 I1d, plan 02 §5.2,
    /// §8.2, §11.2): the development status of a work item, or of every
    /// work item page by page (the resync after a 410). Decided like
    /// `IntegrationEventRead` (the integration's own service account only,
    /// `read_api`; the same denials) and on the same visible set
    /// (`prove_visible`): the linked merge requests of the bound projects
    /// it may see.
    IntegrationDevStatusRead,
    // --- acting for a person (M3 S2, ADR-0025) ---
    /// `GET /api/v1/projects/{id}/merge-requests/{iid}/act-as-confirmation`
    /// (`Resource::ActAsConfirm`, ADR-0025 §5, §5.4): what the
    /// confirmation screen shows before the person presses it. A person's
    /// web session only; the integration enabled and bound to the project;
    /// the intent's action allowed for the person's session on the merge
    /// request (an invisible one is not found).
    ActAsConfirmView,
    /// `POST /-/confirm/projects/{id}/merge-requests/{iid}/assertions`
    /// (`Resource::ActAsConfirm`): the person presses the confirmation and
    /// forge writes a `pending` assertion (no secret yet). Decided like
    /// `ActAsConfirmView`.
    ActAsAssertionIssue,
    /// `POST /api/v1/integrations/{id}/act-as/claim`
    /// (`Resource::IntegrationFeed`): the integration's own service account
    /// claims a pending assertion by its state and receives the secret
    /// once. A token with `act_as` only (no person, no agent, no act-as
    /// request, no disabled integration).
    ActAsAssertionClaim,
    // --- webhooks (M3 I2, ADR-0016 §8, plan 11 §4.3) ---
    /// `GET /api/v1/projects/{id}/webhooks` and `…/webhooks/{webhook_id}`:
    /// a project's webhooks (never a secret). Maintainer and above;
    /// `read_api`.
    WebhookRead,
    /// `POST /api/v1/projects/{id}/webhooks`, `PATCH|DELETE
    /// …/webhooks/{webhook_id}`, `POST …/{webhook_id}/secret` (rotate):
    /// create, change, delete a webhook, replace its secret. People only (a
    /// webhook sends the project's events out; its owner answers for it);
    /// Maintainer and above; `api`. The URL must pass the operator's
    /// target policy (`FG-INT-0010`), checked after the decision.
    WebhookManage,
    /// `GET …/webhooks/{webhook_id}/deliveries`: the delivery history.
    /// Maintainer and above; `read_api`.
    WebhookDeliveryRead,
    /// `POST …/webhooks/{webhook_id}/deliveries/{delivery_id}/redeliver`:
    /// ask for one delivery again (rate-limited). People only; Maintainer
    /// and above; `api`. The pair is looked up together after the
    /// decision (a delivery of another webhook is not found).
    WebhookRedeliver,
    /// Not a route: whether a webhook's owner may still have one class of
    /// its project's events delivered (`Resource::ProjectEvents`), decided
    /// by `prove_webhook_delivery` when an event is dispatched and before
    /// every attempt. People only; Maintainer of the project; the admin
    /// class never.
    WebhookDeliver,
    // --- the push mirror to GitLab (M3 O2, ADR-0024 Amendment O2) ---
    /// `GET /api/v1/projects/{id}/push-mirror`: a project's mirror (its
    /// full URL, credential name and status). Administrators, people only
    /// (agents, service accounts and bots are refused); a token needs
    /// `admin` and `read_api`.
    PushMirrorRead,
    /// `PUT|DELETE /api/v1/projects/{id}/push-mirror` and `POST
    /// …/push-mirror/sync`: set, remove, or push now. Administrators,
    /// people only (a mirror sends the whole repository out of the
    /// instance); a token needs `admin` and `api`. Not session only: `gstn
    /// mirror` works with an administrator's `admin` token. The URL passes
    /// the operator's mirror policy (`FG-INT-0021`) after the decision.
    PushMirrorManage,
    // --- documents (plan 13 Q13, ADR-0026) ---
    /// `PUT|DELETE /api/v1/projects/{id}/doc-lint/migration`: lower the
    /// broken document references of `forge/doc-lint` to warnings until a
    /// date (required, at most 180 days ahead), or end it now. A
    /// repository governance setting like the merge settings: people only,
    /// Maintainer and above; `api`; never acting for a person. Reading it
    /// is `ProjectRead` (`GET …/doc-lint/migration`).
    DocLintMigrationManage,
    // --- CI pipelines (M4 C2, plan 14 §4.2) ---
    /// `GET /api/v1/projects/{id}/pipelines` and `…/pipelines/{pipeline_id}`:
    /// a project's pipelines and their jobs. As `RepoBrowse` (Reporter, or
    /// any signed-in person on an internal project); an agent audience is
    /// allowed: the routes answer metadata only (the logs and the
    /// diagnosis are `CiLogRead` / `PipelineDiagnosisRead`, decided on
    /// their marks, ADR-0029 decision 5); `read_api`.
    PipelineRead,
    /// `GET …/pipelines/{pipeline_id}/jobs/{job_id}/log` (M4 C5, ADR-0029
    /// decision 5): a job's log, masked. On the project as `RepoBrowse`;
    /// then `authorize_ci_output` on `Resource::CiOutput`: an agent
    /// audience only when both the pipeline's mark (measured at its
    /// creation) and the default branch's tip are `Open`, else
    /// `FG-AUTH-0022` (`route=ci_log`). People are never refused by the
    /// marks; `read_api`.
    CiLogRead,
    /// `GET …/pipelines/{pipeline_id}/diagnosis` (M4 C5, plan 04 P-6 / P-9,
    /// the deterministic part): the failed jobs' fingerprints, excerpts,
    /// error codes and known fixes. Decided as `CiLogRead`
    /// (`route=ci_diagnosis`): the excerpts are log output.
    PipelineDiagnosisRead,
    /// `GET …/pipelines/{pipeline_id}/jobs/{job_id}/artifacts[/{artifact_id}]`
    /// (M4 C6, ADR-0029 decision 5): a job's artifacts. Decided as
    /// `CiLogRead` (`route=ci_artifact`): an AI agent audience only when
    /// both marks are `Open`; `read_api`.
    CiArtifactRead,
    /// `POST /api/v1/projects/{id}/pipelines`: run a pipeline by hand on a
    /// branch's tip. Developer and above (agents and service accounts by
    /// their membership, plan 14 §4.2); `api`. A manual run is never
    /// protected: a job with secrets makes it `invalid` (C12 decides more).
    PipelineCreate,
    /// `POST …/pipelines/{pipeline_id}/cancel`: Developer and above; `api`.
    PipelineCancel,
    /// `POST …/pipelines/{pipeline_id}/retry`: Developer and above; `api`.
    PipelineRetry,
    // --- merge queue (M4 C8, plan 14 §4.2, ADR-0030) ---
    /// `GET /api/v1/projects/{id}/merge-queue?target=`: a queue's entries
    /// (one merge request's place is `MergeRequestRead` on
    /// `GET …/merge-requests/{iid}/merge-queue`). As `RepoBrowse` (an agent audience too: the
    /// queue is metadata); `read_api`.
    MergeQueueRead,
    /// `POST …/merge-requests/{iid}/merge-queue`: "merge when ready" — add
    /// the merge request to its target's queue. Decided as
    /// `MergeRequestMerge` (people only; the target's push role; a token
    /// needs `merge_execute`): the queue merges for the person who asked.
    MergeQueueEnqueue,
    /// `DELETE …/merge-requests/{iid}/merge-queue`: take it out of the
    /// queue. As `MergeQueueEnqueue` (who may merge it may stop it).
    MergeQueueDequeue,
    /// `POST /api/v1/projects/{id}/merge-queue/pause`: the emergency stop
    /// of a target's queue (no new batch, no landing). People only,
    /// Developer and above (plan 14 §4.2); `api`.
    MergeQueuePause,
    /// `POST /api/v1/projects/{id}/merge-queue/resume`: start a paused
    /// queue again. People only, Maintainer and above; `api`.
    MergeQueueManage,
    /// `GET /api/v1/projects/{id}/ci/variables`, `PUT|DELETE
    /// …/ci/variables/{name}` (M4 C6, plan 14 §4.2): list a project's CI
    /// variables (names and flags, never a value), set or delete one. A
    /// value is never answered after it was written. Maintainer and above,
    /// people only (a secret's owner answers for it; an agent never sets or
    /// sees one, ADR-0029 decision 8); `api`.
    CiVariableManage,
    /// `PUT /api/v1/runner/jobs/{job_id}/artifacts?path=` (M4 C6): a job
    /// uploads one file of its artifacts — its own token, its own job,
    /// while it runs (`CiRule::Job`, `Resource::JobSelf`).
    JobArtifactUpload,
    /// `GET /api/v1/runner/jobs/{job_id}/needs/artifacts[/{artifact_id}]`
    /// (M4 C6): a job reads the artifacts of the jobs it `needs` in its own
    /// pipeline — its own token, its own job (`CiRule::Job`); which
    /// artifacts are its needs' is the read's own scope.
    JobArtifactDownload,
    // --- CI runners (M4 C3, plan 14 §4.2, ADR-0029 §1) ---
    /// `POST /api/v1/admin/runners`: register a runner and show its token
    /// once. An administrator's web session, people only (a token is
    /// `FG-AUTH-0002 reason=session_required`).
    RunnerRegister,
    /// `GET /api/v1/admin/runners`, `PATCH /api/v1/admin/runners/{id}`:
    /// list the runners, enable / disable one, change its tags. As
    /// `RunnerRegister`.
    RunnerManage,
    /// `POST /api/v1/runner/jobs/request`: a runner's long-poll for a job
    /// (`CiRule::Runner`, on the instance). A runner only.
    RunnerJobRequest,
    /// `POST /api/v1/runner/jobs/{job_id}/heartbeat`: extend the lease and
    /// learn of a cancel. A runner, on a job leased to it only.
    RunnerJobHeartbeat,
    /// `POST /api/v1/runner/jobs/{job_id}/log`: append a log chunk. As
    /// `RunnerJobHeartbeat`.
    RunnerLogAppend,
    /// `POST /api/v1/runner/jobs/{job_id}/complete`: report the result (or
    /// acknowledge a cancel). As `RunnerJobHeartbeat`.
    RunnerJobComplete,
    AuditRead,
    /// `GET /api/v1/user`: the caller's own account (any active signed-in
    /// user; `read_api` on a token). Decided on the instance: there is no
    /// other user to see.
    UserSelfRead,
    /// `GET /api/v1/session`: the caller's own web session — who it stands
    /// for and its CSRF token (M2 MR3, ADR-0021 §6, §17). Sessions only: a
    /// token is `FG-AUTH-0002 reason=session_required` (a token has no
    /// session to read, and must never learn a CSRF token).
    SessionRead,
    /// `GET /api/v1/user/sessions`: one's own web sessions (M2 MR6,
    /// ADR-0021 §17, `Resource::Session`). Sessions only, so a token is
    /// refused before anything is looked up.
    SessionList,
    /// `DELETE /api/v1/user/sessions/{public_id}`: end one of one's own
    /// sessions; another user's is not found, administrators included.
    /// Sessions only.
    SessionRevoke,
    /// `GET /api/v1/admin/users/{id}/sessions`: a user's sessions
    /// (administrator rights; a token needs the `admin` scope).
    AdminSessionList,
    /// `DELETE /api/v1/admin/users/{id}/sessions`: end every session of a
    /// user (administrator rights, never an agent).
    AdminSessionRevoke,
    /// `GET /-/auth/login`, `GET /-/auth/callback`: start and finish the
    /// OIDC sign-in (M2 MR4, ADR-0021 §3). Public: the caller has no
    /// session yet; what the callback admits is decided by `SessionCreate`.
    WebLogin,
    /// Inside `GET /-/auth/callback`: may the person the IdP signed in get
    /// a session (`Resource::Admission`, ADR-0021 §3, §17)? Decided for the
    /// anonymous principal on the admission facts only — the IdP's access
    /// role and the account the identity belongs to (or `New`).
    SessionCreate,
    /// On a cookie-authenticated request whose 5-minute refresh check ran
    /// (M2 MR5, ADR-0021 §7, §17): may the session go on after the
    /// identity provider answered? Decided like `SessionCreate` on
    /// `Resource::Admission` alone, but only an active account with the
    /// access role continues (`New` is refused: a session's account exists).
    SessionContinue,
    /// `POST /-/auth/logout`: end the caller's own web session and send the
    /// browser to the identity provider's logout (M2 MR5, ADR-0021 §8).
    /// Sessions only (a token is `FG-AUTH-0002 reason=session_required`).
    SessionLogout,
    /// `POST /-/auth/backchannel-logout`: the identity provider ends
    /// sessions (OIDC Back-Channel Logout, M2 MR5, ADR-0021 §8). Public:
    /// the caller is the IdP's server, proven by the signed logout token,
    /// not by a credential (cookies and `Authorization` are never read).
    OidcBackchannelLogout,
    // --- web (ADR-0015) ---
    /// `GET /api/v1/namespaces/resolve?path=`: which namespace a path names,
    /// decided by whether the caller can see it (`Resource::Unresolved` /
    /// `Resource::Resolved`). Git paths are decided by `RepoRead` / `RepoPush`.
    PathResolve,
    /// The SPA shell (`index.html`) for namespace pages and permalinks: the
    /// same body for every path, without looking at the database (public).
    WebShell,
    // --- operations ---
    Health,
    Metrics,
}

impl Action {
    /// Reachable without credentials.
    pub fn is_public(self) -> bool {
        matches!(
            self,
            Action::Health | Action::WebShell | Action::WebLogin | Action::OidcBackchannelLogout
        )
    }

    /// Decided on `Resource::Admission` alone, whoever the principal is
    /// (the sign-in has no principal yet, ADR-0021 §3). With any other
    /// resource it is decided like every other action (an anonymous
    /// caller is `FG-AUTH-0001`).
    pub fn is_admission(self) -> bool {
        matches!(self, Action::SessionCreate | Action::SessionContinue)
    }

    /// Only a web session may perform it (ADR-0021 §17): a personal access
    /// token is refused whatever its scopes (`FG-AUTH-0002
    /// reason=session_required`, before the resource is looked at).
    pub fn is_session_only(self) -> bool {
        matches!(
            self,
            Action::SessionRead
                | Action::SessionLogout
                | Action::TokenCreate
                | Action::SessionList
                | Action::SessionRevoke
                | Action::MergeRequestVersionRefsPurge
                | Action::ServiceAccountCreate
                | Action::ServiceAccountTokenIssue
                | Action::IntegrationManage
                | Action::ActAsConfirmView
                | Action::ActAsAssertionIssue
                | Action::RunnerRegister
                | Action::RunnerManage
        )
    }

    /// Kept for people (ADR-0017 §4.0 amendments, ADR-0024 §8.4): an
    /// agent's token, a service account and a bot are refused whatever
    /// their role (`authorize()`'s `people_only`, `reason=agent_token` /
    /// `service_account` / `bot`). The matrix holds a service account and
    /// a bot deny row for each, allowed for the same person
    /// (`every_people_only_action_denies_service_accounts_and_bots`).
    pub fn is_people_only(self) -> bool {
        matches!(
            self,
            Action::ProjectVisibilityChange
                | Action::ProtectedRefManage
                | Action::ProjectTransfer
                | Action::GroupTransfer
                | Action::UserRename
                | Action::NamespaceTombstoneRelease
                | Action::ProjectDelete
                | Action::ProjectRestore
                | Action::MemberManage
                | Action::UserDisable
                | Action::AdminSessionRevoke
                | Action::MergeRequestVersionRefsPurge
                | Action::DiscussionResolve
                | Action::MergeRequestMerge
                | Action::MergeRequestApprove
                | Action::MergeRequestUnapprove
                | Action::ApprovalRuleManage
                | Action::MergeSettingsManage
                | Action::CodeownersEnforcementToggle
                | Action::ServiceAccountCreate
                | Action::ServiceAccountUpdate
                | Action::ServiceAccountTokenIssue
                | Action::CheckNamespaceGrantManage
                | Action::IntegrationManage
                | Action::IntegrationRead
                // Plan 15 MR 8: the activity list is a person's.
                | Action::ActivityRead
                | Action::IntegrationBindingManage
                | Action::IntegrationBindingRemove
                | Action::ActAsConfirmView
                | Action::ActAsAssertionIssue
                | Action::WebhookManage
                | Action::WebhookRedeliver
                | Action::WebhookDeliver
                | Action::PushMirrorRead
                | Action::PushMirrorManage
                | Action::DocLintMigrationManage
                | Action::RunnerRegister
                | Action::RunnerManage
                // M4 C8: the queue merges for a person (ADR-0030 §2, §9).
                | Action::MergeQueueEnqueue
                | Action::MergeQueueDequeue
                | Action::MergeQueuePause
                | Action::MergeQueueManage
                // M4 C6: CI secrets are a person's.
                | Action::CiVariableManage
        )
    }

    /// Also performed through the git protocol (Smart HTTP). `RepoRead` is
    /// an HTTP route too (`/-/raw`, `/-/archive`, ADR-0015): `true` means
    /// "git does it as well", not "only git" (M1 phase 8 MR4).
    pub fn is_git_operation(self) -> bool {
        matches!(self, Action::RepoRead | Action::RepoPush)
    }

    /// The token scope the action needs (`None` for public actions).
    pub fn required_scope(self) -> Option<Scope> {
        match self {
            // Public, or session only (`is_session_only`: no token reaches
            // the scope check).
            // The sign-in: public, or decided on the admission facts.
            Action::Health
            | Action::WebShell
            | Action::SessionRead
            | Action::TokenCreate
            | Action::SessionList
            | Action::SessionRevoke
            | Action::WebLogin
            | Action::SessionCreate
            | Action::SessionContinue
            | Action::SessionLogout
            | Action::MergeRequestVersionRefsPurge
            | Action::ServiceAccountCreate
            | Action::ServiceAccountTokenIssue
            | Action::IntegrationManage
            | Action::ActAsConfirmView
            | Action::ActAsAssertionIssue
            | Action::RunnerRegister
            | Action::RunnerManage
            // M4 C3: the runner API is a runner's (`ci_rule`); no person's
            // token reaches it (`authorize()`: `runner_only`).
            | Action::RunnerJobRequest
            | Action::RunnerJobHeartbeat
            | Action::RunnerLogAppend
            | Action::RunnerJobComplete
            // M4 C6: a job's own artifacts (`ci_rule`: the job's token only).
            | Action::JobArtifactUpload
            | Action::JobArtifactDownload
            | Action::OidcBackchannelLogout => None,
            // M3 S2: the claim needs the act-as scope itself.
            Action::ActAsAssertionClaim => Some(Scope::ActAs),
            Action::RepoRead => Some(Scope::ReadRepository),
            Action::RepoPush => Some(Scope::WriteRepository),
            Action::ProjectRead
            | Action::RepoBrowse
            | Action::SpecRefRead
            | Action::SpecRead
            | Action::ContextRead
            | Action::ProjectList
            | Action::MemberList
            | Action::ProtectedRefList
            | Action::GroupRead
            | Action::GroupList
            | Action::TokenList
            | Action::NotificationPreferencesRead
            | Action::NotificationInboxRead
            | Action::UserMergeRequestList
            | Action::ActivityRead
            | Action::AdminSessionList
            | Action::UserSearch
            | Action::UserAdminList
            | Action::AuditRead
            | Action::UserSelfRead
            | Action::PathResolve
            | Action::MergeRequestRead
            | Action::MergeRequestList
            | Action::ServiceAccountList
            | Action::ServiceAccountRead
            | Action::ServiceAccountTokenList
            | Action::CheckRead
            | Action::IntegrationRead
            | Action::IntegrationEventRead
            | Action::IntegrationEventCursorRead
            | Action::IntegrationDevStatusRead
            | Action::IntegrationBindingList
            | Action::RepositorySearch
            | Action::WebhookRead
            | Action::WebhookDeliveryRead
            | Action::WebhookDeliver
            | Action::PushMirrorRead
            | Action::PipelineRead
            | Action::CiLogRead
            | Action::PipelineDiagnosisRead
            | Action::MergeQueueRead
            | Action::CiArtifactRead
            | Action::Metrics => Some(Scope::ReadApi),
            Action::ProjectCreate
            | Action::ProjectUpdate
            | Action::ProjectVisibilityChange
            | Action::ProjectTransfer
            | Action::ProjectDelete
            | Action::ProjectRestore
            | Action::ProtectedRefManage
            | Action::MemberManage
            | Action::GroupCreateTopLevel
            | Action::SubgroupCreate
            | Action::GroupUpdate
            | Action::GroupTransfer
            | Action::NamespaceTombstoneRelease
            | Action::UserRename
            | Action::TokenRevoke
            | Action::NotificationPreferencesWrite
            | Action::NotificationInboxUpdate
            | Action::AdminSessionRevoke
            | Action::UserCreate
            | Action::UserDisable
            | Action::MergeRequestCreate
            | Action::MergeRequestUpdate
            | Action::MergeRequestClose
            | Action::MergeRequestReopen
            | Action::MergeRequestComment
            | Action::DiscussionResolve
            | Action::ApprovalRuleManage
            | Action::MergeSettingsManage
            | Action::CodeownersEnforcementToggle
            | Action::ServiceAccountUpdate
            | Action::ServiceAccountTokenRevoke
            | Action::CheckWrite
            | Action::CheckNamespaceGrantManage
            | Action::IntegrationBindingManage
            | Action::IntegrationBindingRemove
            | Action::WebhookManage
            | Action::WebhookRedeliver
            | Action::PushMirrorManage
            | Action::DocLintMigrationManage
            | Action::PipelineCreate
            | Action::PipelineCancel
            | Action::PipelineRetry
            | Action::MergeQueuePause
            | Action::MergeQueueManage
            | Action::CiVariableManage
            | Action::WorkItemLink => Some(Scope::Api),
            // M3 R4 (ADR-0024 §8.5): satisfied by `merge_execute` alone.
            Action::MergeRequestMerge
            // M4 C8: adding to the queue is the merge, delayed.
            | Action::MergeQueueEnqueue
            | Action::MergeQueueDequeue => Some(Scope::MergeExecute),
            // M3 R5: satisfied by `merge_approve` alone.
            Action::MergeRequestApprove | Action::MergeRequestUnapprove => {
                Some(Scope::MergeApprove)
            }
        }
    }

    /// How a service account acting for a person may perform the action
    /// (M3 S2, ADR-0025 §4). Closed by default: every action is listed, so
    /// a new one does not compile until it is decided here.
    pub fn act_as_rule(self) -> ActAsRule {
        use forge_core::act_as::Intent;
        match self {
            // Read without an assertion, capped at the smaller of the
            // service account's and the person's role (§3).
            Action::ProjectRead | Action::MergeRequestRead | Action::MergeRequestList => {
                ActAsRule::ReadWithoutAssertion
            }
            // F-36: a person approves from kanflow with an assertion they
            // confirmed in a forge session (§4, §5).
            Action::MergeRequestApprove => ActAsRule::RequiresAssertion(Intent::Approve),
            Action::MergeRequestUnapprove => ActAsRule::RequiresAssertion(Intent::Unapprove),
            // M3 X3 (ADR-0025 Amendment X3): a kanflow PO / SM binds,
            // changes and removes a repository's binding with an assertion
            // confirmed in their forge session; decided on their own role
            // (Maintainer), the binding's kanflow project and setting sealed.
            Action::IntegrationBindingManage => ActAsRule::RequiresAssertion(Intent::BindingSet),
            Action::IntegrationBindingRemove => ActAsRule::RequiresAssertion(Intent::BindingRemove),
            // M3 X3: the repositories the person may bind (names only, by
            // the person's own role: a binding's target is not bound yet,
            // so the min rule would show nothing).
            Action::RepositorySearch => ActAsRule::ListAsPerson,
            // Merging and the merge conditions (M4 at the earliest, by an
            // amendment), creating and changing merge requests, threads,
            // work item links, checks, the feed, every session-only and
            // administration action, git and browsing: never (§4).
            Action::RepoRead
            | Action::RepoPush
            | Action::RepoBrowse
            | Action::ProjectList
            | Action::ProjectCreate
            | Action::ProjectUpdate
            | Action::ProjectVisibilityChange
            | Action::ProjectTransfer
            | Action::ProjectDelete
            | Action::ProjectRestore
            | Action::ProtectedRefManage
            | Action::MemberManage
            | Action::MemberList
            | Action::ProtectedRefList
            | Action::MergeRequestCreate
            | Action::MergeRequestUpdate
            | Action::MergeRequestClose
            | Action::MergeRequestReopen
            | Action::MergeRequestVersionRefsPurge
            | Action::MergeRequestComment
            | Action::DiscussionResolve
            | Action::MergeRequestMerge
            | Action::WorkItemLink
            | Action::ApprovalRuleManage
            | Action::MergeSettingsManage
            | Action::CodeownersEnforcementToggle
            | Action::CheckWrite
            | Action::CheckRead
            | Action::CheckNamespaceGrantManage
            | Action::GroupRead
            | Action::GroupList
            | Action::GroupCreateTopLevel
            | Action::SubgroupCreate
            | Action::GroupUpdate
            | Action::GroupTransfer
            | Action::NamespaceTombstoneRelease
            | Action::TokenCreate
            | Action::TokenList
            | Action::TokenRevoke
            | Action::UserCreate
            | Action::UserDisable
            | Action::UserSearch
            | Action::UserAdminList
            | Action::UserRename
            | Action::ServiceAccountCreate
            | Action::ServiceAccountList
            | Action::ServiceAccountRead
            | Action::ServiceAccountUpdate
            | Action::ServiceAccountTokenIssue
            | Action::ServiceAccountTokenList
            | Action::ServiceAccountTokenRevoke
            | Action::IntegrationManage
            | Action::IntegrationRead
            | Action::IntegrationBindingList
            | Action::IntegrationEventRead
            | Action::IntegrationEventCursorRead
            | Action::IntegrationDevStatusRead
            | Action::ActAsConfirmView
            | Action::ActAsAssertionIssue
            | Action::ActAsAssertionClaim
            | Action::SpecRefRead
            | Action::SpecRead
            // M3 X5: the context API is no act-as read (ADR-0025 §4).
            | Action::ContextRead
            | Action::NotificationPreferencesRead
            | Action::NotificationInboxRead
            | Action::UserMergeRequestList
            | Action::ActivityRead
            | Action::NotificationPreferencesWrite
            | Action::NotificationInboxUpdate
            | Action::WebhookRead
            | Action::WebhookManage
            | Action::WebhookDeliveryRead
            | Action::WebhookRedeliver
            | Action::WebhookDeliver
            // M3 O2: the push mirror is an administrator's.
            | Action::PushMirrorRead
            | Action::PushMirrorManage
            // Plan 13 Q13: a governance setting, a person's own decision.
            | Action::DocLintMigrationManage
            // M4 C2: pipelines are no act-as action (ADR-0025 §4).
            | Action::PipelineRead
            // M4 C5: nor are their outputs.
            | Action::CiLogRead
            | Action::PipelineDiagnosisRead
            | Action::CiArtifactRead
            | Action::JobArtifactUpload
            | Action::JobArtifactDownload
            | Action::PipelineCreate
            | Action::PipelineCancel
            | Action::PipelineRetry
            // M4 C8: the merge queue is no act-as action.
            | Action::MergeQueueRead
            | Action::MergeQueueEnqueue
            | Action::MergeQueueDequeue
            | Action::MergeQueuePause
            | Action::MergeQueueManage
            // M4 C6: nor are CI secrets.
            | Action::CiVariableManage
            // M4 C3: runners are no act-as action.
            | Action::RunnerRegister
            | Action::RunnerManage
            | Action::RunnerJobRequest
            | Action::RunnerJobHeartbeat
            | Action::RunnerLogAppend
            | Action::RunnerJobComplete
            | Action::AuditRead
            | Action::UserSelfRead
            | Action::SessionRead
            | Action::SessionList
            | Action::SessionRevoke
            | Action::AdminSessionList
            | Action::AdminSessionRevoke
            | Action::WebLogin
            | Action::SessionCreate
            | Action::SessionContinue
            | Action::SessionLogout
            | Action::OidcBackchannelLogout
            | Action::PathResolve
            | Action::WebShell
            | Action::Health
            | Action::Metrics => ActAsRule::Denied,
        }
    }

    /// How a CI principal may perform the action (M4 C0, ADR-0029 §2).
    /// Closed by default: every action is listed, so a new one does not
    /// compile until it is decided here.
    pub fn ci_rule(self) -> CiRule {
        match self {
            // A job's copy of its own project at its pinned SHA (§3), on
            // `Resource::PinnedCopy` only.
            Action::RepoRead => CiRule::Job,
            // M4 C6: a job's own artifacts and its needs'.
            Action::JobArtifactUpload | Action::JobArtifactDownload => CiRule::Job,
            // M4 C3: the runner API, on the instance (a claim) or on a job
            // leased to the runner (`Resource::RunnerJob`).
            Action::RunnerJobRequest
            | Action::RunnerJobHeartbeat
            | Action::RunnerLogAppend
            | Action::RunnerJobComplete => CiRule::Runner,
            // Registering and managing runners: a person's (§1).
            Action::RunnerRegister | Action::RunnerManage => CiRule::Denied,
            // Push, checks, merge requests, secrets, other pipelines, the
            // context API, every read of the API and every administration
            // and session action: never (§8).
            Action::RepoPush
            | Action::RepoBrowse
            | Action::ProjectList
            | Action::ProjectCreate
            | Action::ProjectUpdate
            | Action::ProjectVisibilityChange
            | Action::ProjectTransfer
            | Action::ProjectDelete
            | Action::ProjectRestore
            | Action::ProtectedRefManage
            | Action::MemberManage
            | Action::MemberList
            | Action::ProtectedRefList
            | Action::MergeRequestCreate
            | Action::MergeRequestUpdate
            | Action::MergeRequestClose
            | Action::MergeRequestReopen
            | Action::MergeRequestVersionRefsPurge
            | Action::MergeRequestComment
            | Action::DiscussionResolve
            | Action::MergeRequestMerge
            | Action::WorkItemLink
            | Action::ApprovalRuleManage
            | Action::MergeSettingsManage
            | Action::CodeownersEnforcementToggle
            | Action::CheckWrite
            | Action::CheckRead
            | Action::CheckNamespaceGrantManage
            | Action::GroupRead
            | Action::GroupList
            | Action::GroupCreateTopLevel
            | Action::SubgroupCreate
            | Action::GroupUpdate
            | Action::GroupTransfer
            | Action::NamespaceTombstoneRelease
            | Action::TokenCreate
            | Action::TokenList
            | Action::TokenRevoke
            | Action::UserCreate
            | Action::UserDisable
            | Action::UserSearch
            | Action::UserAdminList
            | Action::UserRename
            | Action::ServiceAccountCreate
            | Action::ServiceAccountList
            | Action::ServiceAccountRead
            | Action::ServiceAccountUpdate
            | Action::ServiceAccountTokenIssue
            | Action::ServiceAccountTokenList
            | Action::ServiceAccountTokenRevoke
            | Action::IntegrationManage
            | Action::IntegrationRead
            | Action::IntegrationBindingList
            | Action::IntegrationEventRead
            | Action::IntegrationEventCursorRead
            | Action::IntegrationDevStatusRead
            | Action::ActAsConfirmView
            | Action::ActAsAssertionIssue
            | Action::ActAsAssertionClaim
            | Action::SpecRefRead
            | Action::SpecRead
            | Action::ContextRead
            | Action::NotificationPreferencesRead
            | Action::NotificationInboxRead
            | Action::UserMergeRequestList
            | Action::ActivityRead
            | Action::NotificationPreferencesWrite
            | Action::NotificationInboxUpdate
            | Action::WebhookRead
            | Action::WebhookManage
            | Action::WebhookDeliveryRead
            | Action::WebhookRedeliver
            | Action::WebhookDeliver
            | Action::PushMirrorRead
            | Action::PushMirrorManage
            | Action::DocLintMigrationManage
            // M4 C2: a job never reads or changes pipelines (plan 14 §4.2);
            // M4 C5: nor their outputs (its own log goes through the runner).
            | Action::PipelineRead
            | Action::CiLogRead
            | Action::PipelineDiagnosisRead
            | Action::CiArtifactRead
            | Action::PipelineCreate
            | Action::PipelineCancel
            | Action::PipelineRetry
            // M4 C8: a job never reads or changes a merge queue.
            | Action::MergeQueueRead
            | Action::MergeQueueEnqueue
            | Action::MergeQueueDequeue
            | Action::MergeQueuePause
            | Action::MergeQueueManage
            // M4 C6: a job never reads or sets a secret through the API
            // (ADR-0029 decision 8: they come with the claim).
            | Action::CiVariableManage
            | Action::AuditRead
            | Action::UserSelfRead
            | Action::SessionRead
            | Action::SessionList
            | Action::SessionRevoke
            | Action::AdminSessionList
            | Action::AdminSessionRevoke
            | Action::WebLogin
            | Action::SessionCreate
            | Action::SessionContinue
            | Action::SessionLogout
            | Action::OidcBackchannelLogout
            | Action::PathResolve
            | Action::WebShell
            | Action::Health
            | Action::Metrics
            | Action::ProjectRead
            | Action::MergeRequestRead
            | Action::MergeRequestList
            | Action::MergeRequestApprove
            | Action::MergeRequestUnapprove
            | Action::IntegrationBindingManage
            | Action::IntegrationBindingRemove
            | Action::RepositorySearch => CiRule::Denied,
        }
    }
}

/// How an action may be performed by a service account acting for a
/// person (ADR-0025 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActAsRule {
    /// A read without an assertion: the effective role is the smaller of
    /// the service account's membership role and the person's, and both
    /// must see the project.
    ReadWithoutAssertion,
    /// A change that needs an assertion of this intent: decided on the
    /// person's role, the assertion bound to the resource.
    RequiresAssertion(forge_core::act_as::Intent),
    /// A listing of what the person may act on (M3 X3,
    /// `RepositorySearch`): no assertion, decided as the person (their own
    /// role, no administrator rights) on the route's `Resource::Instance`
    /// and on each listed project's plain `Resource::Project`. Only names
    /// are listed, and only of what the person could bind with an
    /// assertion.
    ListAsPerson,
    /// Never performed acting for a person (`FG-AUTH-0003
    /// reason=act_as_not_allowed`).
    Denied,
}

impl ActAsRule {
    /// The log `act_as_rule`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadWithoutAssertion => "read_without_assertion",
            Self::RequiresAssertion(_) => "requires_assertion",
            Self::ListAsPerson => "list_as_person",
            Self::Denied => "denied",
        }
    }
}

/// How an action may be performed by a CI principal (M4 C0, ADR-0029 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiRule {
    /// Never performed by a runner or a job (`FG-AUTH-0003
    /// reason=ci_not_allowed`).
    Denied,
    /// The runner API (claiming a job, heartbeat and lease, log append,
    /// completion; M4 C3): a runner only.
    Runner,
    /// A job's own work: a copy of its project at its pinned SHA.
    Job,
}

impl CiRule {
    /// The log `ci_rule`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Denied => "denied",
            Self::Runner => "runner",
            Self::Job => "job",
        }
    }
}
