//! forge-hosting: the group and project write commands of the API (M1
//! phase 8b MR7a). A feature crate (ADR-0001 amendment): forge-web →
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
    AccountFacts, Action, Authorized, Credential, GroupCtx, InstanceCtx, LoadedGroup, LoadedParent,
    LoadedProject, LoadedUserNamespace, ParentCtx, Principal, ProjectCtx, ResourceCtx,
    VisibilityChangeCtx,
};
use forge_core::{
    GroupId, GroupRecord, NamespaceId, ProjectId, ProjectRecord, RepositoryKind, RequestId, UserId,
    Visibility,
};
use forge_db::access::Account;
use forge_db::change::{Actor, ChangeContext};
use forge_db::groups::{self, GroupFields, GroupParent, GroupPatch};
use forge_db::projects::{self, NewProject, ProjectPatch};
use forge_db::resolve::{self, ParentRecord};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

pub use forge_db::groups::{CreatedGroup, Updated};
pub use forge_db::projects::{CreatedProject, VisibilityOutcome};

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

/// forge-auth's loaded project from the transaction's facts.
fn loaded(facts: &projects::ProjectAccessFacts) -> LoadedProject {
    LoadedProject {
        id: facts.project,
        visibility: facts.access.visibility,
        role: facts.access.role,
        pending_deletion: facts.access.pending_deletion,
    }
}
