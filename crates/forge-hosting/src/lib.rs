//! forge-hosting: the group and project write commands of the API (M1
//! phase 8b MR7a), and the member and protected-ref commands (MR8). A
//! feature crate (ADR-0001 amendment): forge-web →
//! forge-hosting → forge-auth / forge-db; it runs no SQL of its own.
//!
//! **Every command is decided twice by the one decision point, and the
//! second decision binds the write.** The route registry proves the action
//! on what its loader read (the request reaches the handler only with that
//! proof, ADR-0015). The handler passes the proof here; the command opens
//! the write transaction (a forge-db `*_decided` command), which locks the
//! target, re-reads the caller's roles under `FOR SHARE` on the membership
//! rows and the caller's account row, and calls `decide` — here
//! [`forge_auth::Authorized::reconfirm`]: `authorize()` again, for the same
//! caller and action, on those facts. A role removed, a demotion or a
//! disable that committed after the registry's check is therefore seen
//! (denied, nothing written); one that comes later waits until the write
//! commits. No time-of-check / time-of-use gap (phase 8b plan).
//!
//! A proof for another action than the command's is a programming error
//! (`FG-SYS-0001 reason=proof_action_mismatch`), never a pass.

use forge_auth::{
    AccountFacts, Action, Authorized, Credential, GroupCtx, GroupMemberCtx, InstanceCtx,
    LoadedGroup, LoadedMembership, LoadedParent, LoadedProject, LoadedProtection,
    LoadedUserNamespace, MembershipChange, ParentCtx, Principal, ProjectCtx, ProjectMemberCtx,
    RefProtectionCtx, ResourceCtx, VisibilityChangeCtx,
};
use forge_core::{
    GroupId, GroupRecord, NamespaceId, ProjectId, ProjectRecord, ProtectedRefId, RefPattern,
    RepositoryKind, RequestId, Role, UserId, Visibility,
};
use forge_db::access::Account;
use forge_db::change::{Actor, ChangeContext};
use forge_db::groups::{self, GroupFields, GroupParent, GroupPatch};
use forge_db::members::{self, MemberScope};
use forge_db::projects::{self, NewProject, ProjectPatch};
use forge_db::protected_refs::{self, RuleState};
use forge_db::resolve::{self, ParentRecord};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

pub use forge_db::groups::{CreatedGroup, MemberChange, Updated};
pub use forge_db::members::{MemberRow, MAX_PAGE as MAX_MEMBER_PAGE};
pub use forge_db::projects::{CreatedProject, VisibilityOutcome};
/// A protection rule's state as a route loader reads it.
pub use forge_db::protected_refs::RuleState as ProtectionRuleState;
pub use forge_db::protected_refs::{RuleChange, StoredRule};

/// The parent of a new project as a viewer sees it (the `POST
/// /api/v1/projects` loader).
pub type Parent = ParentRecord;

/// The request a write belongs to (the audit row's request id and token).
#[derive(Debug, Clone)]
pub struct WriteRequest {
    pub request_id: RequestId,
    /// The personal access token the request authenticated with (`None`
    /// for a session).
    pub credential_id: Option<i64>,
}

/// A new group's fields (`POST /api/v1/groups`, `…/{id}/subgroups`).
#[derive(Debug, Clone)]
pub struct NewGroupFields {
    /// The path segment.
    pub path: String,
    pub display_name: String,
    pub description: String,
}

/// A new project's fields (`POST /api/v1/projects`); the parent is the
/// proof's namespace, the creator the proof's caller.
#[derive(Debug, Clone)]
pub struct NewProjectFields {
    pub path: String,
    pub display_name: String,
    pub description: String,
    pub visibility: Visibility,
    pub kind: RepositoryKind,
}

/// A display name / description change (`None` = unchanged).
#[derive(Debug, Clone, Default)]
pub struct Patch {
    pub display_name: Option<String>,
    pub description: Option<String>,
}

/// The hosting commands on the database.
#[derive(Debug, Clone)]
pub struct Hosting {
    db: Db,
}

/// `FG-SYS-0001 reason=proof_action_mismatch` unless the proof is for
/// `expected`.
fn require(action: Action, expected: Action) -> Result<(), ForgeError> {
    if action == expected {
        return Ok(());
    }
    Err(ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "proof_action_mismatch")
        .with_context("command_action", expected.name())
        .with_context("proof_action", action.name()))
}

/// The caller the proof was decided for (a non-public action has one).
fn caller<R: ResourceCtx>(proof: &Authorized<R>) -> Result<UserId, ForgeError> {
    proof.principal().user().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "proof_without_user")
            .with_context("action", proof.action().name())
    })
}

/// The change context of a write by `principal`: actor `user`, the
/// request id, the token and its scopes. (Service accounts and act-as
/// arrive with kanflow, M2: until then every API caller is `user`.)
fn change_context(request: &WriteRequest, principal: &Principal, user: UserId) -> ChangeContext {
    let mut ctx = ChangeContext::new(Actor::User(user), request.request_id.clone());
    ctx.credential_id = request.credential_id;
    ctx.effective_scope = match principal.credential() {
        Credential::Session => Vec::new(),
        Credential::Token(scopes) => scopes.iter().map(|s| s.as_str().to_owned()).collect(),
    };
    ctx
}

fn account(a: Account) -> AccountFacts {
    AccountFacts {
        active: a.active,
        is_admin: a.is_admin,
    }
}

impl Hosting {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    // ── reads for the route loaders (read pool, one statement each) ──

    /// One group for `viewer` (`None` = absent).
    pub async fn group(
        &self,
        id: GroupId,
        viewer: Option<UserId>,
    ) -> Result<Option<GroupRecord>, ForgeError> {
        groups::load(&self.db, id, viewer).await
    }

    /// One project for `viewer` (`None` = absent).
    pub async fn project(
        &self,
        id: ProjectId,
        viewer: Option<UserId>,
    ) -> Result<Option<ProjectRecord>, ForgeError> {
        resolve::load_project(&self.db, id, viewer).await
    }

    /// The namespace a project would be created under (`None` = absent).
    pub async fn parent(
        &self,
        namespace: NamespaceId,
        viewer: Option<UserId>,
    ) -> Result<Option<Parent>, ForgeError> {
        resolve::load_parent(&self.db, namespace, viewer).await
    }

    /// One page of a project's direct members (`after` = the last user id
    /// of the previous page).
    pub async fn project_members(
        &self,
        project: ProjectId,
        after: Option<UserId>,
        limit: usize,
    ) -> Result<Vec<MemberRow>, ForgeError> {
        members::page(&self.db, MemberScope::Project(project), after, limit).await
    }

    /// One page of a group's direct members.
    pub async fn group_members(
        &self,
        group: GroupId,
        after: Option<UserId>,
        limit: usize,
    ) -> Result<Vec<MemberRow>, ForgeError> {
        members::page(&self.db, MemberScope::Group(group), after, limit).await
    }

    /// One direct member of a project (`None` = not one).
    pub async fn project_member(
        &self,
        project: ProjectId,
        member: UserId,
    ) -> Result<Option<MemberRow>, ForgeError> {
        members::one(&self.db, MemberScope::Project(project), member).await
    }

    /// One direct member of a group (`None` = not one).
    pub async fn group_member(
        &self,
        group: GroupId,
        member: UserId,
    ) -> Result<Option<MemberRow>, ForgeError> {
        members::one(&self.db, MemberScope::Group(group), member).await
    }

    /// `member`'s direct role in `project` (`None` = not a direct member).
    pub async fn project_member_role(
        &self,
        project: ProjectId,
        member: UserId,
    ) -> Result<Option<Role>, ForgeError> {
        members::direct_role(&self.db, MemberScope::Project(project), member).await
    }

    /// `member`'s direct role in `group`.
    pub async fn group_member_role(
        &self,
        group: GroupId,
        member: UserId,
    ) -> Result<Option<Role>, ForgeError> {
        members::direct_role(&self.db, MemberScope::Group(group), member).await
    }

    /// Every protection rule of `project`.
    pub async fn protected_refs(&self, project: ProjectId) -> Result<Vec<StoredRule>, ForgeError> {
        protected_refs::list(&self.db, project).await
    }

    /// The state of one rule of `project` (for a loader).
    pub async fn protected_ref_state(
        &self,
        project: ProjectId,
        rule: ProtectedRefId,
    ) -> Result<RuleState, ForgeError> {
        protected_refs::rule_state(&self.db, project, rule).await
    }

    // ── writes, each decided again inside its transaction ──

    /// `POST /api/v1/groups` (`GroupCreateTopLevel`): a top-level group
    /// whose first Owner is the caller.
    pub async fn create_group(
        &self,
        request: &WriteRequest,
        proof: &Authorized<InstanceCtx>,
        fields: &NewGroupFields,
    ) -> Result<CreatedGroup, ForgeError> {
        require(proof.action(), Action::GroupCreateTopLevel)?;
        self.create_group_in(request, proof, GroupParent::TopLevel, fields, |facts| {
            proof
                .reconfirm(account(facts.account), InstanceCtx)
                .map(drop)
        })
        .await
    }

    /// `POST /api/v1/groups/{id}/subgroups` (`SubgroupCreate`): a group
    /// below the proof's group, first Owner the caller.
    pub async fn create_subgroup(
        &self,
        request: &WriteRequest,
        proof: &Authorized<GroupCtx>,
        fields: &NewGroupFields,
    ) -> Result<CreatedGroup, ForgeError> {
        require(proof.action(), Action::SubgroupCreate)?;
        let parent = *proof.id();
        self.create_group_in(
            request,
            proof,
            GroupParent::Group(parent),
            fields,
            |facts| {
                let Some(parent) = facts.parent else {
                    return Err(ForgeError::new(ErrorCode::Internal)
                        .with_context("reason", "parent_facts_missing"));
                };
                let fresh = GroupCtx::new(LoadedGroup {
                    id: parent.group,
                    role: parent.role,
                });
                proof.reconfirm(account(facts.account), fresh).map(drop)
            },
        )
        .await
    }

    async fn create_group_in<R: ResourceCtx>(
        &self,
        request: &WriteRequest,
        proof: &Authorized<R>,
        parent: GroupParent,
        fields: &NewGroupFields,
        decide: impl FnOnce(&groups::GroupCreateFacts) -> Result<(), ForgeError>,
    ) -> Result<CreatedGroup, ForgeError> {
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let fields = GroupFields {
            segment: fields.path.clone(), // clone: forge-db owns its request
            display_name: fields.display_name.clone(),
            description: fields.description.clone(),
        };
        let (created, ()) =
            groups::create_decided(&self.db, &ctx, parent, &fields, user, decide).await?;
        Ok(created)
    }

    /// `PATCH /api/v1/groups/{id}` (`GroupUpdate`).
    pub async fn update_group(
        &self,
        request: &WriteRequest,
        proof: &Authorized<GroupCtx>,
        patch: &Patch,
    ) -> Result<Updated, ForgeError> {
        require(proof.action(), Action::GroupUpdate)?;
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let patch = GroupPatch {
            display_name: patch.display_name.clone(), // clone: forge-db owns its request
            description: patch.description.clone(),
        };
        let (updated, ()) =
            groups::update_decided(&self.db, &ctx, *proof.id(), user, &patch, |facts| {
                let fresh = GroupCtx::new(LoadedGroup {
                    id: facts.group.group,
                    role: facts.group.role,
                });
                proof.reconfirm(account(facts.account), fresh).map(drop)
            })
            .await?;
        Ok(updated)
    }

    /// `POST /api/v1/projects` (`ProjectCreate`): a project under the
    /// proof's namespace, created by (and first owned by) the caller.
    pub async fn create_project(
        &self,
        request: &WriteRequest,
        proof: &Authorized<ParentCtx>,
        fields: &NewProjectFields,
    ) -> Result<CreatedProject, ForgeError> {
        require(proof.action(), Action::ProjectCreate)?;
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let new = NewProject {
            segment: fields.path.clone(), // clone: forge-db owns its request
            parent: *proof.id(),
            display_name: fields.display_name.clone(),
            description: fields.description.clone(),
            visibility: fields.visibility,
            kind: fields.kind,
            creator: user,
        };
        let (created, ()) = projects::create_decided(&self.db, &ctx, &new, |facts| {
            let parent = match facts.parent {
                projects::ParentAccess::Group {
                    namespace,
                    group,
                    role,
                } => LoadedParent::Group {
                    namespace,
                    group: LoadedGroup { id: group, role },
                },
                projects::ParentAccess::User { namespace, owner } => {
                    LoadedParent::User(LoadedUserNamespace { namespace, owner })
                }
            };
            proof
                .reconfirm(account(facts.account), ParentCtx::new(parent))
                .map(drop)
        })
        .await?;
        Ok(created)
    }

    /// `PATCH /api/v1/projects/{id}` (`ProjectUpdate`).
    pub async fn update_project(
        &self,
        request: &WriteRequest,
        proof: &Authorized<ProjectCtx>,
        patch: &Patch,
    ) -> Result<Updated, ForgeError> {
        require(proof.action(), Action::ProjectUpdate)?;
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let patch = ProjectPatch {
            display_name: patch.display_name.clone(), // clone: forge-db owns its request
            description: patch.description.clone(),
        };
        let (updated, ()) =
            projects::update_decided(&self.db, &ctx, *proof.id(), user, &patch, |facts| {
                let fresh = ProjectCtx::new(loaded(&facts.project));
                proof.reconfirm(account(facts.account), fresh).map(drop)
            })
            .await?;
        Ok(updated)
    }

    /// `PUT /api/v1/projects/{id}/visibility` (`ProjectVisibilityChange`,
    /// Owner): the visibility the proof was decided for.
    pub async fn change_visibility(
        &self,
        request: &WriteRequest,
        proof: &Authorized<VisibilityChangeCtx>,
    ) -> Result<VisibilityOutcome, ForgeError> {
        require(proof.action(), Action::ProjectVisibilityChange)?;
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let to = proof.ctx().to();
        let (outcome, ()) =
            projects::change_visibility_decided(&self.db, &ctx, *proof.id(), user, to, |facts| {
                let fresh = VisibilityChangeCtx::new(loaded(&facts.project), to);
                proof.reconfirm(account(facts.account), fresh).map(drop)
            })
            .await?;
        Ok(outcome)
    }
}

impl Hosting {
    // ── M1 phase 8b MR8: members ──

    /// `POST /api/v1/projects/{id}/members` (`MemberManage`, `Add`): the
    /// member and the role the proof was decided for. Decided again on the
    /// locked facts (the member is still `Add`; an existing member is then
    /// `FG-REPO-0010 already_member`).
    pub async fn add_project_member(
        &self,
        request: &WriteRequest,
        proof: &Authorized<ProjectMemberCtx>,
    ) -> Result<(), ForgeError> {
        require(proof.action(), Action::MemberManage)?;
        let role = granted(proof.ctx().membership().change, Kind::Add)?;
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let (project, member) = *proof.id();
        projects::add_member_decided(&self.db, &ctx, project, user, member, role, |facts| {
            let fresh = ProjectMemberCtx::new(
                loaded(&facts.project),
                membership(
                    member,
                    MembershipChange::Add(role),
                    facts.removes_last_owner,
                ),
            );
            proof.reconfirm(account(facts.account), fresh).map(drop)
        })
        .await
    }

    /// `PATCH /api/v1/projects/{id}/members/{user_id}` (`MemberManage`):
    /// the role the proof was decided for; `from` is the member's direct
    /// role re-read under the lock (a member gone meanwhile is decided as
    /// `Missing` and answered `FG-REPO-0010 not_member`).
    pub async fn change_project_member(
        &self,
        request: &WriteRequest,
        proof: &Authorized<ProjectMemberCtx>,
    ) -> Result<MemberChange, ForgeError> {
        require(proof.action(), Action::MemberManage)?;
        let to = granted(proof.ctx().membership().change, Kind::Change)?;
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let (project, member) = *proof.id();
        let (changed, ()) =
            projects::change_member_decided(&self.db, &ctx, project, user, member, to, |facts| {
                let change = match facts.member_role {
                    Some(from) => MembershipChange::Change { from, to },
                    None => MembershipChange::Missing { to: Some(to) },
                };
                let fresh = ProjectMemberCtx::new(
                    loaded(&facts.project),
                    membership(member, change, facts.removes_last_owner),
                );
                proof.reconfirm(account(facts.account), fresh).map(drop)
            })
            .await?;
        Ok(changed)
    }

    /// `DELETE /api/v1/projects/{id}/members/{user_id}` (`MemberManage`).
    pub async fn remove_project_member(
        &self,
        request: &WriteRequest,
        proof: &Authorized<ProjectMemberCtx>,
    ) -> Result<(), ForgeError> {
        require(proof.action(), Action::MemberManage)?;
        removal(proof.ctx().membership().change)?;
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let (project, member) = *proof.id();
        projects::remove_member_decided(&self.db, &ctx, project, user, member, |facts| {
            let fresh = ProjectMemberCtx::new(
                loaded(&facts.project),
                membership(member, removed(facts.member_role), facts.removes_last_owner),
            );
            proof.reconfirm(account(facts.account), fresh).map(drop)
        })
        .await
    }

    /// `POST /api/v1/groups/{id}/members` (`MemberManage`, `Add`).
    pub async fn add_group_member(
        &self,
        request: &WriteRequest,
        proof: &Authorized<GroupMemberCtx>,
    ) -> Result<(), ForgeError> {
        require(proof.action(), Action::MemberManage)?;
        let role = granted(proof.ctx().membership().change, Kind::Add)?;
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let (group, member) = *proof.id();
        groups::add_member_decided(&self.db, &ctx, group, user, member, role, |facts| {
            let fresh = GroupMemberCtx::new(
                LoadedGroup {
                    id: facts.group,
                    role: facts.role,
                },
                membership(
                    member,
                    MembershipChange::Add(role),
                    facts.removes_last_owner,
                ),
            );
            proof.reconfirm(account(facts.account), fresh).map(drop)
        })
        .await
    }

    /// `PATCH /api/v1/groups/{id}/members/{user_id}` (`MemberManage`).
    pub async fn change_group_member(
        &self,
        request: &WriteRequest,
        proof: &Authorized<GroupMemberCtx>,
    ) -> Result<MemberChange, ForgeError> {
        require(proof.action(), Action::MemberManage)?;
        let to = granted(proof.ctx().membership().change, Kind::Change)?;
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let (group, member) = *proof.id();
        let (changed, ()) =
            groups::change_member_decided(&self.db, &ctx, group, user, member, to, |facts| {
                let change = match facts.member_role {
                    Some(from) => MembershipChange::Change { from, to },
                    None => MembershipChange::Missing { to: Some(to) },
                };
                let fresh = GroupMemberCtx::new(
                    LoadedGroup {
                        id: facts.group,
                        role: facts.role,
                    },
                    membership(member, change, facts.removes_last_owner),
                );
                proof.reconfirm(account(facts.account), fresh).map(drop)
            })
            .await?;
        Ok(changed)
    }

    /// `DELETE /api/v1/groups/{id}/members/{user_id}` (`MemberManage`).
    pub async fn remove_group_member(
        &self,
        request: &WriteRequest,
        proof: &Authorized<GroupMemberCtx>,
    ) -> Result<(), ForgeError> {
        require(proof.action(), Action::MemberManage)?;
        removal(proof.ctx().membership().change)?;
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let (group, member) = *proof.id();
        groups::remove_member_decided(&self.db, &ctx, group, user, member, |facts| {
            let fresh = GroupMemberCtx::new(
                LoadedGroup {
                    id: facts.group,
                    role: facts.role,
                },
                membership(member, removed(facts.member_role), facts.removes_last_owner),
            );
            proof.reconfirm(account(facts.account), fresh).map(drop)
        })
        .await
    }

    // ── M1 phase 8b MR8: protected refs ──

    /// `POST /api/v1/projects/{id}/protected-refs` (`ProtectedRefManage`,
    /// `from: None`): `pattern` with the minimum push role the proof was
    /// decided for.
    pub async fn create_protected_ref(
        &self,
        request: &WriteRequest,
        proof: &Authorized<RefProtectionCtx>,
        pattern: &RefPattern,
    ) -> Result<ProtectedRefId, ForgeError> {
        require(proof.action(), Action::ProtectedRefManage)?;
        let wanted = proof.ctx().protection();
        let (None, Some(to)) = (wanted.rule, wanted.to) else {
            return Err(shape_mismatch("protected_ref_create"));
        };
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let project = proof.id().0;
        let (rule, ()) =
            protected_refs::create_decided(&self.db, &ctx, project, user, pattern, to, |facts| {
                let fresh = RefProtectionCtx::new(
                    loaded(&facts.project),
                    LoadedProtection {
                        rule: None,
                        from: protection_level(facts.rule),
                        to: Some(to),
                    },
                );
                proof.reconfirm(account(facts.account), fresh).map(drop)
            })
            .await?;
        Ok(rule)
    }

    /// `PATCH /api/v1/projects/{id}/protected-refs/{rule_id}`: the minimum
    /// push role the proof was decided for; `from` re-read under the lock.
    pub async fn change_protected_ref(
        &self,
        request: &WriteRequest,
        proof: &Authorized<RefProtectionCtx>,
    ) -> Result<RuleChange, ForgeError> {
        require(proof.action(), Action::ProtectedRefManage)?;
        let wanted = proof.ctx().protection();
        let (Some(rule), Some(to)) = (wanted.rule, wanted.to) else {
            return Err(shape_mismatch("protected_ref_change"));
        };
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let project = proof.id().0;
        let (changed, ()) =
            protected_refs::change_decided(&self.db, &ctx, project, rule, user, to, |facts| {
                let fresh = RefProtectionCtx::new(
                    loaded(&facts.project),
                    LoadedProtection {
                        rule: Some(rule),
                        from: protection_level(facts.rule),
                        to: Some(to),
                    },
                );
                proof.reconfirm(account(facts.account), fresh).map(drop)
            })
            .await?;
        Ok(changed)
    }

    /// `DELETE /api/v1/projects/{id}/protected-refs/{rule_id}`.
    pub async fn delete_protected_ref(
        &self,
        request: &WriteRequest,
        proof: &Authorized<RefProtectionCtx>,
    ) -> Result<(), ForgeError> {
        require(proof.action(), Action::ProtectedRefManage)?;
        let wanted = proof.ctx().protection();
        let (Some(rule), None) = (wanted.rule, wanted.to) else {
            return Err(shape_mismatch("protected_ref_delete"));
        };
        let user = caller(proof)?;
        let ctx = change_context(request, proof.principal(), user);
        let project = proof.id().0;
        protected_refs::delete_decided(&self.db, &ctx, project, rule, user, |facts| {
            let fresh = RefProtectionCtx::new(
                loaded(&facts.project),
                LoadedProtection {
                    rule: Some(rule),
                    from: protection_level(facts.rule),
                    to: None,
                },
            );
            proof.reconfirm(account(facts.account), fresh).map(drop)
        })
        .await
    }
}

/// Which member command a proof is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Add,
    Change,
}

/// `FG-SYS-0001 reason=proof_shape_mismatch`: a proof decided for another
/// kind of change than the command's (a programming error, never a pass).
fn shape_mismatch(command: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "proof_shape_mismatch")
        .with_context("command", command)
}

/// The role an add (`Add(to)`) or a change (`Change { to }` /
/// `Missing { to: Some(to) }`) proof was decided for.
fn granted(change: MembershipChange, kind: Kind) -> Result<Role, ForgeError> {
    match (kind, change) {
        (Kind::Add, MembershipChange::Add(to))
        | (
            Kind::Change,
            MembershipChange::Change { to, .. } | MembershipChange::Missing { to: Some(to) },
        ) => Ok(to),
        (Kind::Add, _) => Err(shape_mismatch("member_add")),
        (Kind::Change, _) => Err(shape_mismatch("member_change")),
    }
}

/// A removal proof: `Remove { .. }` or `Missing { to: None }`.
fn removal(change: MembershipChange) -> Result<(), ForgeError> {
    match change {
        MembershipChange::Remove { .. } | MembershipChange::Missing { to: None } => Ok(()),
        MembershipChange::Add(_)
        | MembershipChange::Change { .. }
        | MembershipChange::Missing { to: Some(_) } => Err(shape_mismatch("member_remove")),
    }
}

/// The removal of a member whose direct role is `role` (`None` = not one).
fn removed(role: Option<Role>) -> MembershipChange {
    match role {
        Some(from) => MembershipChange::Remove { from },
        None => MembershipChange::Missing { to: None },
    }
}

fn membership(
    member: UserId,
    change: MembershipChange,
    removes_last_owner: bool,
) -> LoadedMembership {
    LoadedMembership {
        member,
        change,
        removes_last_owner,
    }
}

/// The `from` of a protection change: the stored level; an unreadable row
/// counts as Owner-level (only an Owner or an administrator removes it),
/// an absent one as none (the command then answers 404).
pub fn protection_level(state: RuleState) -> Option<Role> {
    match state {
        RuleState::New | RuleState::Absent => None,
        RuleState::Level(role) => Some(role),
        RuleState::Unreadable => Some(Role::Owner),
    }
}

/// forge-auth's loaded project from the transaction's facts.
fn loaded(facts: &projects::ProjectAccessFacts) -> LoadedProject {
    LoadedProject {
        id: facts.project,
        visibility: facts.access.visibility,
        role: facts.access.role,
        pending_deletion: facts.access.pending_deletion,
    }
}
