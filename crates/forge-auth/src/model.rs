//! The inputs of an authorization decision. Everything `authorize()` needs is
//! loaded into these values beforehand (memberships, effective roles, token
//! scopes, ref protection), so the decision itself is a pure function.
//!
//! Inputs fail closed: a `Principal` cannot be built without stating how it
//! authenticated and whether the account is active.

// Role, Visibility (D2) and Scope (D3) live in forge-core (forge-db stores and loads
// them); re-exported here so the authorization API is unchanged.
pub use forge_core::{MergeRequestState, Role, Scope, Visibility};

/// The kind of the caller's account (ADR-0024 §8.4, M3 R3): `human`,
/// `service_account` or `bot` — forge-core's `UserKind`, the CHECK of
/// `forge.users.kind`. Set only by [`Principal::authenticated`] from the
/// account row, never defaulted and never changed afterwards. The actions
/// kept for people (`people_only` in `authorize()`) refuse the other two.
pub use forge_core::UserKind as AccountKind;
use forge_core::{RefName, UserId};

/// How the caller authenticated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// Interactive sign-in: every scope.
    Session,
    /// Personal access token limited to these scopes.
    Token(Vec<Scope>),
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

    pub fn user(&self) -> Option<UserId> {
        self.user
    }

    pub fn active(&self) -> bool {
        self.active
    }

    pub fn credential(&self) -> &Credential {
        &self.credential
    }

    /// Administrator rights in effect: an administrator in a session, or with
    /// a token that carries the `admin` scope.
    pub fn admin_rights(&self) -> bool {
        self.is_admin
            && match &self.credential {
                Credential::Session => true,
                Credential::Token(scopes) => scopes.contains(&Scope::Admin),
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
    /// The notification settings of `owner` (`NotificationPreferencesRead`
    /// / `…Write`, M3 N1): only the person's own.
    NotificationPreferences {
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
    CheckRun {
        project: ProjectView,
        namespace: forge_core::CheckNamespace,
        granted: bool,
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
