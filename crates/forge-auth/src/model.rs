//! The inputs of an authorization decision. Everything `authorize()` needs is
//! loaded into these values beforehand (memberships, effective roles, token
//! scopes, ref protection), so the decision itself is a pure function.
//!
//! Inputs fail closed: a `Principal` cannot be built without stating how it
//! authenticated and whether the account is active.

// Role, Visibility (D2) and Scope (D3) live in forge-core (forge-db stores and loads
// them); re-exported here so the authorization API is unchanged.
pub use forge_core::{MergeRequestState, Role, Scope, Visibility};

use forge_core::act_as::Intent;
/// The kind of the caller's account (ADR-0024 §8.4, M3 R3): `human`,
/// `service_account` or `bot` — forge-core's `UserKind`, the CHECK of
/// `forge.users.kind`. Set only by [`Principal::authenticated`] from the
/// account row, never defaulted and never changed afterwards. The actions
/// kept for people (`people_only` in `authorize()`) refuse the other two.
pub use forge_core::UserKind as AccountKind;
use forge_core::{IntegrationId, MergeRequestId, ProjectId, RefName, UserId};

/// How the caller authenticated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// Interactive sign-in: every scope.
    Session,
    /// Personal access token limited to these scopes.
    Token(Vec<Scope>),
    /// A service account's token acting for a person (`Forge-Act-As`, M3
    /// S2, ADR-0025): only forge-web's authentication builds it
    /// ([`Principal::act_as`]); `authorize()` decides it in its own arm
    /// (`act_as.rs`) and lends no administrator right.
    ActAs(Box<ActAsCredential>),
    /// A registered CI runner (M4 C0, ADR-0029 §1): only the runner API's
    /// actions (`CiRule::Runner`); never a repository or API read. Built by
    /// forge-web's authentication only ([`Principal::runner`]); decided in
    /// its own arm (`job.rs`).
    Runner(RunnerCredential),
    /// One CI job's short-lived token (ADR-0029 §1): one project, one
    /// pinned SHA, the job's lifetime. Built by forge-web's authentication
    /// only ([`Principal::job`]); decided in its own arm (`job.rs`).
    Job(Box<JobCredential>),
    /// The auto-merge principal (M4 C9, plan 04 M-5, ADR-0017 Amendment
    /// 2026-11-13): forge's own worker queueing a mergeable merge request
    /// of a project whose `merge.auto` acts. No person, no token, no
    /// session; built only by [`Principal::auto_merge`] and decided in its
    /// own arm (`auto_merge.rs`): `MergeQueueEnqueue` and
    /// `MergeRequestMerge` on `Resource::AutoMerge`, nothing else.
    AutoMerge,
}

/// What a runner request authenticated with (ADR-0029 §1). The token's
/// lookup (`ci_runners`, SHA-256 hash only) lands with the runner API
/// (M4 C3); authentication refuses an unknown or disabled runner's token
/// (`FG-AUTH-0001`) before a principal exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerCredential {
    /// `ci_runners.id` (the log `runner_id`).
    pub runner_id: i64,
}

/// What a job request authenticated with (ADR-0029 §1, §3): the job, its
/// project and the SHA its pipeline pinned, and the token's state as read
/// with it (the token is checked against the database on every request,
/// never a signature-only JWT). Issuance and lookup (`ci_job_tokens`) land
/// with the runner API (M4 C3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobCredential {
    /// `ci_jobs.id` (the log `job_id`).
    pub job_id: i64,
    /// The only project the job reads.
    pub project: ProjectId,
    /// The only commit the job reads (its pipeline's pinned SHA).
    pub sha: forge_core::push::ObjectId,
    /// Live, expired or revoked, as read with the token.
    pub state: JobTokenState,
}

/// A job token's state as read with it (ADR-0029 §1): `expires_at` past →
/// `Expired`; `revoked_at` set (the job ended: success, failure, cancel,
/// lost lease) → `Revoked`. Only `Live` is authorized (`FG-AUTH-0001`
/// otherwise; `authorize()` has no clock, so authentication states it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobTokenState {
    Live,
    Expired,
    Revoked,
}

impl JobTokenState {
    /// The log `reason` of a refusal.
    pub fn as_str(self) -> &'static str {
        match self {
            JobTokenState::Live => "live",
            JobTokenState::Expired => "expired",
            JobTokenState::Revoked => "revoked",
        }
    }
}

/// What an act-as request authenticated with (ADR-0025 §2, §5): the
/// service account's token, its integration, the verified assertion (if
/// one was sent) and the audit's keyed HMAC of the claimed person. Built
/// by forge-web's authentication from the token and forge-db's verified
/// assertion only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActAsCredential {
    /// The service account (`actor.via`).
    pub via: UserId,
    /// Its account kind as read with the token (`service_account`, or the
    /// act-as arm refuses `act_as_requires_service_account`).
    pub via_kind: AccountKind,
    /// The token's id (`credential_id` of the audit row) and scopes.
    pub via_token: i64,
    pub via_scopes: Vec<Scope>,
    /// The token was issued for an AI agent (refused: `agent_token`).
    pub via_agent: bool,
    /// The integration whose service account the token belongs to.
    pub integration: IntegrationId,
    pub integration_enabled: bool,
    /// The verified assertion (`None`: none was sent).
    pub assertion: Option<AssertionFacts>,
    /// `hex(HMAC(K, "forge.act_as.claimed.v1" ‖ header))` and its key id
    /// (the audit's `act_as_claimed` / `act_as_key_id`).
    pub claimed_mac: String,
    pub claimed_key_id: i16,
}

/// A claimed assertion that forge-db verified (its MAC, its state, its
/// integration and person): what it is bound to, for `authorize()` to
/// compare with the action and the resource (ADR-0025 §5.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssertionFacts {
    /// `act_as_assertions.id` (consumed in the action's transaction).
    pub id: i64,
    /// Safe to log (`assertion_public_id`, the audit's `via_assertion_id`).
    pub public_id: String,
    pub intent: Intent,
    pub project: ProjectId,
    pub subject: AssertionSubject,
}

/// The intent's own part of [`AssertionFacts`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssertionSubject {
    /// `approve` / `unapprove`: the merge request, its head and target.
    MergeRequest {
        merge_request: MergeRequestId,
        head_sha: String,
        target_ref: RefName,
        target_sha: String,
    },
    /// `binding_set` / `binding_remove` (M3 X3): the kanflow project and,
    /// for `binding_set`, the setting confirmed.
    Binding {
        external_project_id: String,
        resolves_work_items: Option<bool>,
    },
}

/// Who is asking. Built only through the constructors, which take every
/// security-relevant fact explicitly (no permissive defaults).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    user: Option<UserId>,
    is_admin: bool,
    active: bool,
    credential: Credential,
    /// The credential was issued for an AI agent (ADR-0017 §4.0; an
    /// authorization input since M1 phase 8b MR7a).
    agent: bool,
    /// The account's kind (`users.kind`, ADR-0024 §8.4). The anonymous
    /// principal has `Human` but no user, so it is refused first.
    kind: AccountKind,
}

impl Principal {
    /// No credentials (forge has no anonymous access; only public actions pass).
    pub fn anonymous() -> Self {
        Self {
            user: None,
            is_admin: false,
            active: false,
            credential: Credential::Session,
            agent: false,
            kind: AccountKind::Human,
        }
    }

    /// A signed-in user. `active` is `false` for disabled or locked
    /// accounts; `kind` is the account's `users.kind` as read with it (no
    /// default: every caller states it, ADR-0024 §8.4).
    pub fn authenticated(
        user: UserId,
        is_admin: bool,
        active: bool,
        credential: Credential,
        kind: AccountKind,
    ) -> Self {
        Self {
            user: Some(user),
            is_admin,
            active,
            credential,
            agent: false,
            kind,
        }
    }

    /// The account's kind (ADR-0024 §8.4).
    pub fn account_kind(&self) -> AccountKind {
        self.kind
    }

    /// The same principal with another account kind — tests only (the
    /// matrix checks that the kind is decisive exactly for the people-only
    /// actions); production code has no way to change the kind.
    #[cfg(test)]
    pub(crate) fn with_kind(mut self, kind: AccountKind) -> Self {
        self.kind = kind;
        self
    }

    /// The same caller, with a credential issued for an AI agent
    /// (`tokens.is_agent`, ADR-0017 §4.0). Agents are denied the actions
    /// ADR-0017 keeps for people (`ProjectVisibilityChange` since M1 phase
    /// 8b MR7a; `ProtectedRefManage` and `MemberManage` since MR8).
    #[must_use]
    pub fn as_agent(mut self, agent: bool) -> Self {
        self.agent = agent;
        self
    }

    /// The credential was issued for an AI agent.
    pub fn is_agent(&self) -> bool {
        self.agent
    }

    /// Whether the caller is an AI agent for the confidential boundary of
    /// plan 06 D-5 (M3 X5, ADR-0023 Amendment X5): a credential issued
    /// with `is_agent`, or an account that is no person (a service account
    /// or a bot, whatever its credential). The context and spec APIs never
    /// hand such a caller a work item or a specification marked
    /// confidential. Not an authorization decision: the caller was allowed
    /// to read the project; this only narrows what one answer carries.
    pub fn is_agent_audience(&self) -> bool {
        self.agent || self.kind != AccountKind::Human
    }

    pub fn user(&self) -> Option<UserId> {
        self.user
    }

    pub fn active(&self) -> bool {
        self.active
    }

    pub fn credential(&self) -> &Credential {
        &self.credential
    }

    /// A service account acting for `person` (M3 S2, ADR-0025): forge-web's
    /// authentication builds it after resolving `Forge-Act-As` to exactly
    /// one active person (`kind` as read with the person's row) and
    /// verifying the assertion, never anything else (test
    /// `only_forge_web_builds_act_as_principals`). The person's
    /// administrator flag is not carried: an act-as principal has no
    /// administrator rights.
    pub fn act_as(
        person: UserId,
        active: bool,
        kind: AccountKind,
        credential: ActAsCredential,
    ) -> Self {
        Self {
            user: Some(person),
            is_admin: false,
            active,
            credential: Credential::ActAs(Box::new(credential)),
            agent: false,
            kind,
        }
    }

    /// The act-as credential, when the request acts for a person.
    pub fn act_as_credential(&self) -> Option<&ActAsCredential> {
        match &self.credential {
            Credential::ActAs(c) => Some(c),
            Credential::Session
            | Credential::Token(_)
            | Credential::Runner(_)
            | Credential::Job(_)
            | Credential::AutoMerge => None,
        }
    }

    /// A registered CI runner (M4 C0, ADR-0029 §1): forge-web's
    /// authentication builds it from a runner token it looked up, never
    /// anything else (test `only_forge_web_builds_ci_principals`). No
    /// person, no administrator rights; not a person, so an agent
    /// audience ([`Principal::is_agent_audience`] unchanged) — but
    /// `authorize()` decides it in `job.rs` before any other rule.
    pub fn runner(credential: RunnerCredential) -> Self {
        Self::ci(Credential::Runner(credential))
    }

    /// One CI job (M4 C0, ADR-0029 §1, §3): the same as
    /// [`Principal::runner`], for a job token.
    pub fn job(credential: JobCredential) -> Self {
        Self::ci(Credential::Job(Box::new(credential)))
    }

    /// The auto-merge principal (M4 C9): forge-review's auto-merge worker
    /// and the merge queue's landing of its entries build it, nothing else
    /// (no route authenticates as it). No person, no administrator rights,
    /// the kind of no person (`Bot`), never an agent token; `authorize()`
    /// decides it in `auto_merge.rs` before any other rule.
    pub fn auto_merge() -> Self {
        Self::ci(Credential::AutoMerge)
    }

    /// The auto-merge principal.
    pub fn is_auto_merge(&self) -> bool {
        matches!(self.credential, Credential::AutoMerge)
    }

    /// No user, active (the credential's own state decides), the kind of
    /// no person (`Bot`: never a member, never `Human`).
    fn ci(credential: Credential) -> Self {
        Self {
            user: None,
            is_admin: false,
            active: true,
            credential,
            agent: false,
            kind: AccountKind::Bot,
        }
    }

    /// Administrator rights in effect: an administrator in a session, or with
    /// a token that carries the `admin` scope; never when acting for a
    /// person (ADR-0025 §3).
    pub fn admin_rights(&self) -> bool {
        self.is_admin
            && match &self.credential {
                Credential::Session => true,
                Credential::Token(scopes) => scopes.contains(&Scope::Admin),
                Credential::ActAs(_)
                | Credential::Runner(_)
                | Credential::Job(_)
                | Credential::AutoMerge => false,
            }
    }

    /// The scopes the credential holds, as the audit's `effective_scope`
    /// (empty for a session; an act-as request's are its service account
    /// token's).
    pub fn scope_names(&self) -> Vec<String> {
        let names = |scopes: &[Scope]| scopes.iter().map(|s| s.as_str().to_owned()).collect();
        match &self.credential {
            Credential::Session => Vec::new(),
            Credential::Token(scopes) => names(scopes),
            Credential::ActAs(c) => names(&c.via_scopes),
            // CI principals hold no scope (ADR-0029: `CiRule` decides).
            Credential::Runner(_) | Credential::Job(_) | Credential::AutoMerge => Vec::new(),
        }
    }
}

/// A project as seen by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectView {
    pub visibility: Visibility,
    /// The caller's effective role: max over direct membership and ancestor
    /// groups. `None` = not a member.
    pub role: Option<Role>,
    /// Deleted and inside the grace period.
    pub pending_deletion: bool,
}

/// What the auto-merge principal is decided on (M4 C9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoMergeTarget {
    /// The project (the auto-merge principal is no member: `role` is
    /// `None`; only visibility and the pending deletion are read).
    pub project: ProjectView,
    pub source_branch: RefName,
    pub target_branch: RefName,
    /// The project's `merge.auto` now.
    pub mode: forge_core::auto_merge::AutoMergeMode,
    /// The emergency stop in effect now (the instance's first).
    pub stop: Option<forge_core::auto_merge::StopScope>,
}

/// What a push does to one ref.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefUpdate {
    Create,
    FastForward,
    /// Non-fast-forward (rewrites history).
    Force,
    Delete,
}

/// The namespace of a pushed ref.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    /// `refs/heads/*`
    Branch,
    /// `refs/tags/*`
    Tag,
    /// Anything else (`refs/merge-requests/*`, `refs/replace/*`, `refs/meta/*`,
    /// …): owned by forge, never pushed by clients.
    Reserved,
}

impl RefKind {
    /// The only way callers derive the kind: from the validated ref name, so
    /// no caller can default a reserved ref to `Branch`.
    pub fn of(name: &RefName) -> RefKind {
        if name.branch().is_some() {
            RefKind::Branch
        } else if name.tag().is_some() {
            RefKind::Tag
        } else {
            RefKind::Reserved
        }
    }
}

/// Protection that applies to the ref being pushed (`None` = unprotected).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefProtection {
    /// Minimum role allowed to push (create / fast-forward). Private: the
    /// constructors are `effective` and `of`, so "strictest rule wins"
    /// cannot be bypassed by building a protection from the first matching
    /// rule.
    push_min_role: Role,
    /// M3 R6: a matching rule's merge rule forbids direct pushes (the branch
    /// changes through merge requests only; ADR-0024 §4.3, §12).
    direct_push_forbidden: bool,
}

impl RefProtection {
    /// The protection in effect when several rules match one ref: the
    /// **strictest** (highest minimum role). Adding a rule can therefore never
    /// loosen an existing protection (ADR-0010 amendment). The lookup layer
    /// must build the push resource only through this function (or [`of`],
    /// which adds the merge rules), matching rule patterns case-insensitively
    /// (`MAIN` and `main` collide on case-insensitive file systems).
    ///
    /// [`of`]: RefProtection::of
    pub fn effective(levels: impl IntoIterator<Item = Role>) -> Option<RefProtection> {
        levels.into_iter().max().map(|push_min_role| RefProtection {
            push_min_role,
            direct_push_forbidden: false,
        })
    }

    /// The protection `rules` put on `name`: the strictest minimum role of
    /// every matching rule and, since M3 R6, the direct-push ban when any
    /// matching rule's merge rule has it (`ProtectionRules::merge_rule`).
    /// What a push and a merge decide with.
    pub fn of(rules: &forge_core::ProtectionRules, name: &forge_core::RefName) -> Option<Self> {
        Self::effective(rules.levels(name)).map(|p| RefProtection {
            direct_push_forbidden: rules.merge_rule(name).direct_push_forbidden,
            ..p
        })
    }

    /// The same protection with direct pushes forbidden (it only ever
    /// tightens; the matrix fixtures and tests).
    #[must_use]
    pub fn forbidding_direct_push(self) -> Self {
        RefProtection {
            direct_push_forbidden: true,
            ..self
        }
    }

    pub fn push_min_role(&self) -> Role {
        self.push_min_role
    }

    /// A push may not create or move the ref (M3 R6).
    pub fn direct_push_forbidden(&self) -> bool {
        self.direct_push_forbidden
    }
}

/// A change of a project's CODEOWNERS enforcement
/// (`CodeownersEnforcementToggle`, M3 R6, ADR-0024 §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeownersToggle {
    /// Enforce CODEOWNERS again.
    Enforce,
    /// Suspend it for `minutes` (`None`: no expiry asked — refused; more
    /// than 24 hours or 0 — refused).
    Suspend { minutes: Option<u32> },
}

/// Where a membership is changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MembershipScope {
    Project(ProjectView),
    /// The caller's effective role on the group.
    Group {
        role: Option<Role>,
    },
}

/// What is done to one membership.
///
/// `from` is the member's current **direct** role in the project or group
/// (the membership the change acts on), loaded from the database inside the
/// write transaction — never taken from the request (M1 phase 8b MR8). `Add` is only for a user who is
/// not a member yet (an existing member is a `Change`, never an upsert).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MembershipChange {
    /// Add a new member with this role.
    Add(Role),
    /// Change a member whose current role is `from` to `to`.
    Change { from: Role, to: Role },
    /// Remove a member whose current role is `from`.
    Remove { from: Role },
    /// Change (`to`) or remove (`to: None`) a user who is **not** a direct
    /// member (M1 phase 8b MR8): decided like the least change that names
    /// `to` (the caller needs Maintainer, and `to` must be at or below the
    /// caller's role), so only a caller who may manage members learns that
    /// the user is not one — the command then answers `FG-REPO-0010
    /// not_member`.
    Missing { to: Option<Role> },
}

/// What the measurement of a repository's default branch found, for a
/// whole copy of it — a clone or fetch over Smart HTTP, an archive
/// (`RepoRead` on [`Resource::RepoCopy`], plan 06 D-5, ADR-0026 decision
/// of 2026-11-10). Measured by the route for an agent audience
/// ([`Principal::is_agent_audience`]) only; `authorize()` refuses that
/// audience everything but [`CopyMark::Open`] (fail closed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMark {
    /// Not measured (the caller is a person: a measurement would only
    /// cost). An agent audience with this is refused (`tip_not_measured`).
    NotMeasured,
    /// The default branch's tip has no confidential file, or the
    /// repository has no ref at all (nothing to copy).
    Open,
    /// The default branch's tip has a confidential file
    /// (`forge_checks::confidential`: either mark, the type's default as a
    /// floor, an unreadable front matter or type definition).
    Confidential,
    /// The default branch is not born while other refs exist: nothing to
    /// measure against (the read routes withhold every document then).
    Unborn,
    /// The measurement did not complete (its timeout, a bound of the
    /// document read, a git error).
    Undecided,
}

impl CopyMark {
    /// The log `reason` of a refusal.
    pub fn as_str(self) -> &'static str {
        match self {
            CopyMark::NotMeasured => "tip_not_measured",
            CopyMark::Open => "tip_open",
            CopyMark::Confidential => "tip_confidential",
            CopyMark::Unborn => "default_branch_unborn",
            CopyMark::Undecided => "tip_undecided",
        }
    }

    /// The log `reason` of a CI output refused on the pipeline's own mark
    /// (M4 C5, `mark_of=pipeline`).
    pub fn pipeline_reason(self) -> &'static str {
        match self {
            CopyMark::NotMeasured => "pipeline_not_measured",
            CopyMark::Open => "pipeline_open",
            CopyMark::Confidential => "pipeline_confidential",
            CopyMark::Unborn => "pipeline_unborn",
            CopyMark::Undecided => "pipeline_undecided",
        }
    }
}

/// What the action acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resource {
    /// Instance-wide (user administration, top-level groups, metrics).
    Instance,
    /// A group; `role` is the caller's effective role on it.
    Group {
        role: Option<Role>,
    },
    /// A user's personal namespace. `owner_kind` is the owner's
    /// `users.kind`, loaded with the namespace (S0 review: only a person's
    /// namespace holds projects, whoever the caller is).
    Namespace {
        owner: UserId,
        owner_kind: AccountKind,
    },
    Project(ProjectView),
    /// A whole copy of the project's repository — a clone or fetch
    /// (upload-pack, its advertisement) or an archive — with what the
    /// default branch's tip was measured to hold ([`CopyMark`]). Decided
    /// like `RepoRead` on the project, then an agent audience is refused
    /// every mark but `Open` (`FG-AUTH-0022`). Built only by
    /// [`crate::authorize_repo_copy`] from the dispatcher's `RepoRead`
    /// proof.
    RepoCopy {
        project: ProjectView,
        mark: CopyMark,
    },
    /// A CI job's copy of its own project at its pipeline's pinned SHA (M4
    /// C0, ADR-0029 §3). Only the job arm (`job.rs`) reads it: the project
    /// and the SHA must be the job token's, whatever the confidential
    /// marks. Unconstructible outside forge-auth ([`PinnedCopy`] has
    /// private fields): built only by [`crate::authorize_pinned_copy`];
    /// every other principal is refused.
    PinnedCopy(PinnedCopy),
    /// A CI job as the runner API sees it (M4 C3, ADR-0029 §1): which
    /// runner holds its lease and whether it has ended. Only the job arm
    /// (`job.rs`) reads it: the runner must hold the lease. Unconstructible
    /// outside forge-auth ([`RunnerJob`] has private fields): built by
    /// [`crate::RunnerJobCtx`] from one loaded row; every other principal
    /// is refused.
    RunnerJob(RunnerJob),
    /// A CI job's own artifacts (M4 C6): the job a job token's route names
    /// and that job's project. Only the job arm reads it: the token's job
    /// and project must be these. Built by [`crate::JobSelfCtx`] only.
    JobSelf(JobSelf),
    /// One ref update in a push. The kind is derived from `name` inside
    /// `authorize()` (callers cannot mislabel a reserved ref); `update` must
    /// come from the ancestry check of old and new object ids; `protection`
    /// from `RefProtection::effective`.
    Ref {
        project: ProjectView,
        name: RefName,
        update: RefUpdate,
        protection: Option<RefProtection>,
    },
    /// Moving or renaming a project (`ProjectTransfer`, ADR-0015): the
    /// project as seen by the caller and where it goes. A rename is a move
    /// to the current parent.
    Transfer {
        source: ProjectView,
        target: TransferTarget,
    },
    /// Moving or renaming a group (`GroupTransfer`, ADR-0015): the caller's
    /// effective role on the group and where it goes.
    GroupTransfer {
        source_role: Option<Role>,
        target: TransferTarget,
    },
    /// Changing a user's username and personal namespace path
    /// (`UserRename`, ADR-0015).
    UserRename {
        target_user: UserId,
    },
    /// Releasing a namespace tombstone (`NamespaceTombstoneRelease`,
    /// ADR-0015).
    Tombstone(TombstoneRelease),
    /// Changing one membership. `removes_last_owner` is computed by the
    /// lookup layer: the change would leave the project or group without an
    /// Owner.
    Membership {
        scope: MembershipScope,
        change: MembershipChange,
        removes_last_owner: bool,
    },
    /// Changing the protection of a ref pattern: the minimum push role
    /// before and after (`None` = unprotected), loaded / validated by the
    /// caller.
    RefProtectionChange {
        project: ProjectView,
        from: Option<Role>,
        to: Option<Role>,
    },
    /// Changing a project's visibility.
    VisibilityChange {
        project: ProjectView,
        to: Visibility,
    },
    /// A personal access token owned by `owner` (list / revoke).
    Token {
        owner: UserId,
    },
    /// The notification settings and in-app notifications of `owner`
    /// (`NotificationInboxRead` / `…Update`, M3 U4) (`NotificationPreferencesRead`
    /// / `…Write`, M3 N1): only the person's own.
    NotificationPreferences {
        owner: UserId,
    },
    /// The merge requests of `owner`'s own work across projects
    /// (`UserMergeRequestList`, plan 15 MR 6): only the person's own.
    UserMergeRequests {
        owner: UserId,
    },
    /// The recent events of the projects `owner` is involved in
    /// (`ActivityRead`, plan 15 MR 8): only the person's own.
    UserActivity {
        owner: UserId,
    },
    /// The web sessions of `owner` (`SessionList` / `SessionRevoke` on the
    /// user's own route, `AdminSessionList` / `AdminSessionRevoke` on the
    /// administration's; M2 MR6, ADR-0021 §17).
    Session {
        owner: UserId,
    },
    /// A token about to be created for `owner` with these scopes.
    TokenRequest {
        owner: UserId,
        scopes: Vec<Scope>,
    },
    /// The admission of a person the IdP signed in (`SessionCreate`,
    /// ADR-0021 §3): whether the ID token carries the access role, and the
    /// account the identity is (or will be) linked to.
    Admission {
        access_role: bool,
        account: AdmissionAccount,
    },
    /// A path that resolved to nothing (`PathResolve`): `FG-REPO-0005` for
    /// every signed-in, active caller.
    Unresolved(UnresolvedReason),
    /// One merge request (M3 R1a, ADR-0024 §12): `MergeRequestRead`,
    /// `MergeRequestUpdate`, `MergeRequestClose`, `MergeRequestReopen` on a
    /// stored one, `MergeRequestCreate` on the one about to be opened
    /// (`author` = the caller). Both branch names are `refs/heads/*` and
    /// differ, or every arm denies (`not_a_branch`,
    /// `source_equals_target`). `state` and `draft` are read only by the
    /// Update / Close / Reopen arms; `edit` only by Update;
    /// `target_protection` (the target branch's protection from the push
    /// path's `ProtectionLoader` snapshot, M3 R4) only by Merge.
    MergeRequest {
        project: ProjectView,
        author: UserId,
        state: MergeRequestState,
        draft: bool,
        source_branch: RefName,
        target_branch: RefName,
        edit: MergeRequestEdit,
        target_protection: Option<RefProtection>,
    },
    /// A merge request as the auto-merge principal reaches it (M4 C9):
    /// `MergeQueueEnqueue` (the auto-merge worker) and `MergeRequestMerge`
    /// (the merge queue's landing of its entry). Read with the project's
    /// `merge.auto` and the emergency stops in the same transaction as the
    /// merge request. Every other principal is refused it
    /// (`reason=auto_merge_resource`).
    AutoMerge(AutoMergeTarget),
    /// One discussion of a merge request (M3 R3, ADR-0024 §12):
    /// `DiscussionResolve`. `mr_author` is the merge request's author,
    /// `started_by` the author of the thread's first note. The merge
    /// request's state is no input (resolving is allowed in every state;
    /// what blocks a merge is `evaluate_merge()`'s).
    Discussion {
        project: ProjectView,
        mr_author: UserId,
        started_by: UserId,
    },
    /// A change of a project's CODEOWNERS enforcement
    /// (`CodeownersEnforcementToggle`, M3 R6, ADR-0024 §9):
    /// `highest_protection` is the highest minimum push role of the
    /// project's protection rules (the `above_own_role` check), read with
    /// the project.
    CodeownersEnforcement {
        project: ProjectView,
        highest_protection: Option<Role>,
        change: CodeownersToggle,
    },
    /// A change of a project's merge settings (`MergeSettingsManage`, M3
    /// R6 review): `loosens` when it turns off a rule of the approvals
    /// (`committer_approvals_excluded` or `reset_approvals_on_push` from
    /// on to off, compared with the settings as stored);
    /// `highest_protection` as for the CODEOWNERS switch.
    MergeSettingsChange {
        project: ProjectView,
        highest_protection: Option<Role>,
        loosens: bool,
    },
    /// A path that resolved to a namespace (`PathResolve`), decided by
    /// whether the caller can see it — not by a read role (ADR-0015).
    Resolved {
        via: ResolvedVia,
        view: NamespaceView,
    },
    /// The user an administration route of service accounts acts on
    /// (`ServiceAccountRead`, `ServiceAccountUpdate`,
    /// `ServiceAccountTokenList`, `ServiceAccountTokenRevoke`; M3 S0), as
    /// loaded from its row: a user of another kind is not found.
    ServiceAccount(ServiceAccountView),
    /// A token about to be issued for a service account
    /// (`ServiceAccountTokenIssue`, M3 S0) with these scopes.
    ServiceAccountTokenRequest {
        account: ServiceAccountView,
        scopes: Vec<Scope>,
    },
    /// A check run report (`CheckWrite`, M3 C1, ADR-0024 §10): the project
    /// as seen by the caller, the report name's namespace, and whether the
    /// caller holds a grant of that namespace (read by the loader, and
    /// again under the lock in the write transaction).
    ///
    /// `reserved`: the name is kept from HTTP although its namespace is
    /// grantable (`ci/pipeline`, M4 C5, ADR-0029 decision 7): refused to
    /// everyone (`reason=ci_namespace_internal`).
    CheckRun {
        project: ProjectView,
        namespace: forge_core::CheckNamespace,
        granted: bool,
        reserved: bool,
    },
    /// A CI output (M4 C5, ADR-0029 decision 5): a job's log or a
    /// pipeline's diagnosis (`CiLogRead`, `PipelineDiagnosisRead`), built by
    /// `authorize_ci_output` only. The project as seen by the caller, the
    /// pipeline's mark (`ci_pipelines.copy_mark`, measured at its
    /// creation) and the default branch's tip now (`NotMeasured` for a
    /// person). `declared_agent`: the request said it is for an AI agent
    /// (`Forge-Context-Audience: agent`, which `gstn mcp` always sends), so
    /// a person's token is decided as an agent's.
    CiOutput {
        project: ProjectView,
        pipeline_mark: CopyMark,
        tip_mark: CopyMark,
        declared_agent: bool,
        /// M4 C6 review: an artifact of a job that was given secrets —
        /// readable by a project Maintainer who is a person only, never by
        /// an AI agent audience (whatever the marks).
        secret_job: bool,
    },
    /// An integration's event feed as its route loaded it
    /// (`IntegrationEventRead`, `IntegrationEventCursorRead`; M3 I1c).
    IntegrationFeed(IntegrationFeedView),
    /// The events of one visibility class of one bound project, for the
    /// feed's principal (`prove_visible`, M3 I1c, ADR-0016 §7): the
    /// project as seen by the integration's service account.
    ProjectEvents {
        project: ProjectView,
        class: forge_core::VisibilityClass,
    },
    /// A resource as an act-as request reaches it (M3 S2, ADR-0025 §3):
    /// the resource as seen by the person, the integration's service
    /// account on its project, and what an assertion is compared with.
    /// Only the act-as arm reads it; every other principal is refused.
    ActAs(Box<ActAsView>),
    /// The confirmation of an act-as action (`ActAsConfirmView`,
    /// `ActAsAssertionIssue`, ADR-0025 §5): the merge request as seen by
    /// the person, the intent, and the receiving integration's state.
    ActAsConfirm(Box<ActAsConfirmView>),
}

/// See [`Resource::PinnedCopy`]: what a job asks to copy. Private fields,
/// a crate-only constructor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedCopy {
    project: ProjectId,
    sha: forge_core::push::ObjectId,
}

impl PinnedCopy {
    pub(crate) fn new(project: ProjectId, sha: forge_core::push::ObjectId) -> Self {
        Self { project, sha }
    }

    pub fn project(&self) -> ProjectId {
        self.project
    }

    pub fn sha(&self) -> &forge_core::push::ObjectId {
        &self.sha
    }
}

/// See [`Resource::RunnerJob`]: a job's lease as loaded. Private fields, a
/// crate-only constructor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunnerJob {
    holder: Option<i64>,
    finished: bool,
}

impl RunnerJob {
    pub(crate) fn new(holder: Option<i64>, finished: bool) -> Self {
        Self { holder, finished }
    }

    /// The runner holding the lease (`ci_jobs.runner_id`).
    pub fn holder(&self) -> Option<i64> {
        self.holder
    }

    /// The job has ended (succeeded, failed, cancelled).
    pub fn finished(&self) -> bool {
        self.finished
    }
}

/// See [`Resource::JobSelf`]. Private fields, a crate-only constructor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobSelf {
    project: ProjectId,
    job: i64,
}

impl JobSelf {
    pub(crate) fn new(project: ProjectId, job: i64) -> Self {
        Self { project, job }
    }

    pub fn project(&self) -> ProjectId {
        self.project
    }

    pub fn job(&self) -> i64 {
        self.job
    }
}

/// See [`Resource::ActAs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActAsView {
    /// The resource as seen by the person (their native role).
    pub inner: Resource,
    /// The integration's service account on the resource's project.
    pub via: ViaView,
    /// What an assertion is compared with (merge request routes); `None`
    /// where there is nothing to bind (reads).
    pub subject: Option<ActAsSubject>,
    /// What a binding assertion is compared with (the integration binding
    /// routes, M3 X3); `None` elsewhere.
    pub binding: Option<BindingSubject>,
}

/// The binding an act-as change acts on, as the request names it (M3 X3,
/// ADR-0025 Amendment X3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingSubject {
    /// The integration of the route's path (must be the request's own).
    pub integration: IntegrationId,
    pub project: ProjectId,
    pub external_project_id: String,
    /// The setting the request writes (`binding_set`); `None` for a
    /// removal.
    pub resolves_work_items: Option<bool>,
}

/// The integration's service account on a project (ADR-0025 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViaView {
    /// Its membership role (a binding makes it a Reporter).
    pub role: Option<Role>,
    /// The project is bound to the request's integration (`via_bound`).
    pub bound: bool,
}

/// The merge request an act-as change acts on, as loaded (ADR-0025 §5.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActAsSubject {
    pub project: ProjectId,
    pub merge_request: MergeRequestId,
    /// The merge request's head now (`None` before its first version).
    pub head_sha: Option<String>,
    /// Its target ref now (`refs/heads/…`).
    pub target_ref: RefName,
    /// The target's tip now (`None`: the branch is missing).
    pub target_sha: Option<String>,
    /// The `sha` the request's body names (an approve's).
    pub requested_sha: Option<String>,
}

/// See [`Resource::ActAsConfirm`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActAsConfirmView {
    /// What the intent acts on as seen by the person: the merge request
    /// (`Resource::MergeRequest`) for `approve` / `unapprove`, the project
    /// (`Resource::Project`) for the binding intents (M3 X3).
    pub target: Resource,
    pub intent: Intent,
    pub integration_enabled: bool,
    /// The integration is bound to the target's project (for
    /// `binding_remove`: to the named kanflow project).
    pub bound: bool,
}

/// An integration's feed as the caller reaches it (M3 I1c): whether the
/// caller is the integration's own service account (`own`), whether the
/// integration is enabled, and whether the request asked to act for a
/// person (`Forge-Act-As`, never allowed on the feed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntegrationFeedView {
    pub own: bool,
    pub enabled: bool,
    pub act_as: bool,
}

/// A user as a service account route sees it (M3 S0): its kind
/// (`users.kind`) and whether it is active, read from its row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceAccountView {
    pub kind: AccountKind,
    pub active: bool,
}

/// What a `MergeRequestUpdate` changes besides the title and the
/// description (which are not authorization inputs): a new target branch,
/// a new draft flag. `Default` = neither.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeRequestEdit {
    pub target_branch: Option<RefName>,
    pub draft: Option<bool>,
}

/// The account a signed-in identity belongs to, as the login's linking step
/// found it (ADR-0021 §3, §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionAccount {
    /// No user yet: the sign-in creates one.
    New,
    /// An active human.
    Active,
    /// A disabled user.
    Disabled,
    /// A bot or service account (never signs in to the web).
    NotHuman,
}

impl AdmissionAccount {
    pub const ALL: &'static [AdmissionAccount] = &[
        AdmissionAccount::New,
        AdmissionAccount::Active,
        AdmissionAccount::Disabled,
        AdmissionAccount::NotHuman,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Active => "active",
            Self::Disabled => "disabled",
            Self::NotHuman => "not_human",
        }
    }
}

/// Why a path resolved to nothing a route can act on. All are answered
/// with the same `FG-REPO-0005`; the difference reaches only the log's
/// `reason` (ADR-0015 "判定の順序" step 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnresolvedReason {
    /// No namespace and no tombstone at the path.
    Absent,
    /// A `deleted` tombstone, or a `moved` one whose target is gone.
    TombstoneDeleted,
    /// The path names a namespace of another kind than the route's scope
    /// (`raw` on a group, M1 phase 8 MR4): treated as absent (step 5).
    KindMismatch,
}

impl UnresolvedReason {
    /// Every reason.
    pub const ALL: &'static [UnresolvedReason] = &[
        UnresolvedReason::Absent,
        UnresolvedReason::TombstoneDeleted,
        UnresolvedReason::KindMismatch,
    ];

    /// The log `reason`.
    pub fn as_str(self) -> &'static str {
        match self {
            UnresolvedReason::Absent => "absent",
            UnresolvedReason::TombstoneDeleted => "tombstone_deleted",
            UnresolvedReason::KindMismatch => "kind_mismatch",
        }
    }
}

/// How a path reached the namespace it resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedVia {
    /// The path is the namespace's current path.
    Live,
    /// The path is a `moved` tombstone pointing at the namespace (an
    /// invisible target is logged as `tombstone_not_visible`).
    Moved,
}

/// The namespace a path resolved to, as seen by the caller. The kind is the
/// variant, so a kind and a view can never disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamespaceView {
    /// A personal namespace: visible to every signed-in user.
    User,
    /// A group: visible to every signed-in user (as for `GroupRead`); `role`
    /// is the caller's effective role on it.
    Group { role: Option<Role> },
    /// A project: visible to its members (Guest included), to every
    /// signed-in user when internal, and to administrators.
    Project(ProjectView),
}

impl NamespaceView {
    /// `user` / `group` / `project` (the `namespaces.kind` values).
    pub fn kind(&self) -> &'static str {
        match self {
            NamespaceView::User => "user",
            NamespaceView::Group { .. } => "group",
            NamespaceView::Project(_) => "project",
        }
    }
}

/// Where a project or group moves to (ADR-0015 "移管は両側で判定する").
/// Built by the lookup layer for the caller; a rename (the parent stays
/// the same) is [`TransferTarget::CurrentParent`], decided here (D5 MR5
/// review).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferTarget {
    /// A group, with the caller's effective role on it. `visible` is false
    /// when the caller cannot see the target — and for a target that does
    /// not exist, so both get the same `FG-REPO-0005`.
    Group { role: Option<Role>, visible: bool },
    /// A user's personal namespace, with its owner's `users.kind`.
    PersonalNamespace {
        owner: UserId,
        owner_kind: AccountKind,
    },
    /// No parent: the top level. Never for a project; a group only by an
    /// administrator.
    TopLevel,
    /// The parent the resource already has: a rename (ADR-0015 "名前だけの
    /// 変更は、移動先 = 今の親として扱う"). Decided on the caller's role on
    /// the resource alone — nobody gains a role, so the parent's role is
    /// not needed — except at the top level (`top_level`: a top-level
    /// group's rename), which stays an administrator's, like its creation.
    CurrentParent { top_level: bool },
}

/// A tombstone about to be released (ADR-0015 "解放"), with the facts the
/// decision needs as loaded for the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TombstoneRelease {
    /// A `moved` tombstone whose target still exists: its **current**
    /// Owner (or an administrator) releases it. `target_visible` is false
    /// when the caller cannot see the target (the same 404 as no
    /// tombstone).
    Moved {
        target_owner_role: Option<Role>,
        target_visible: bool,
    },
    /// A `deleted` tombstone (or a `moved` one whose target is gone): the
    /// recorded owner (`owner_at_write`) releases it only while active and
    /// still Owner of the parent namespace (`caller_parent_role`);
    /// otherwise, and always without a parent, administrators only.
    Deleted {
        recorded_owner: Option<UserId>,
        caller_parent_role: Option<Role>,
        has_parent: bool,
    },
}
