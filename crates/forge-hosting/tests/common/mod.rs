//! Shared fixtures of the forge-hosting tests (synthetic users and groups
//! on a fresh migrated database, ADR-0011).
#![allow(dead_code, clippy::expect_used, clippy::panic)]

use forge_auth::{
    prove, Action, Authorized, Credential, GroupCtx, GroupMemberCtx, LoadedGroup, LoadedMembership,
    LoadedParent, LoadedProject, LoadedProtection, LoadedUserNamespace, MembershipChange,
    ParentCtx, Principal, ProjectCtx, ProjectMemberCtx, RefProtectionCtx, ResourceCtx,
    VisibilityChangeCtx,
};
use forge_core::{GroupId, ProjectId, ProtectedRefId, RequestId, Role, UserId, Visibility};
use forge_db::change::{Actor, ChangeContext};
use forge_db::groups::{self, CreatedGroup, NewGroup};
use forge_db::projects::{self, CreatedProject, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_hosting::{Hosting, NewGroupFields, NewProjectFields, WriteRequest};

pub fn system(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

pub fn request(id: &str) -> WriteRequest {
    WriteRequest {
        request_id: RequestId::parse(id).expect("request id"),
        credential_id: None,
    }
}

pub async fn user(t: &FreshDb, name: &str, is_admin: bool) -> users::Created {
    let mut new = NewUser::human(name, name, &format!("{name}@example.test"));
    new.is_admin = is_admin;
    users::create(&t.db, &system(&format!("req-user-{name}")), &new)
        .await
        .expect("user")
}

pub async fn group(t: &FreshDb, segment: &str, owner: UserId) -> CreatedGroup {
    groups::create(
        &t.db,
        &system(&format!("req-group-{segment}")),
        &NewGroup {
            segment: segment.to_owned(),
            parent: None,
            display_name: format!("Group {segment}"),
            description: String::new(),
            owner,
        },
    )
    .await
    .expect("group")
}

pub async fn project(
    t: &FreshDb,
    segment: &str,
    parent: CreatedGroup,
    creator: UserId,
) -> CreatedProject {
    projects::create(
        &t.db,
        &system(&format!("req-project-{segment}")),
        &NewProject::new(
            segment,
            parent.namespace_id,
            &format!("Project {segment}"),
            creator,
        ),
    )
    .await
    .expect("project")
}

pub fn session(user: UserId, is_admin: bool) -> Principal {
    Principal::authenticated(user, is_admin, true, Credential::Session)
}

/// The registry's proof, built the way its loaders build it: from what
/// `hosting` reads for the caller now.
pub async fn group_proof(
    hosting: &Hosting,
    who: &Principal,
    action: Action,
    id: GroupId,
) -> Authorized<GroupCtx> {
    let record = hosting
        .group(id, who.user())
        .await
        .expect("load")
        .expect("group exists");
    prove(
        who,
        action,
        GroupCtx::new(LoadedGroup {
            id: record.id,
            role: record.role,
        }),
    )
    .expect("allowed before the race")
}

pub async fn project_ctx(hosting: &Hosting, who: &Principal, id: ProjectId) -> LoadedProject {
    let record = hosting
        .project(id, who.user())
        .await
        .expect("load")
        .expect("project exists");
    LoadedProject {
        id: record.id,
        visibility: record.access.visibility,
        role: record.access.role,
        pending_deletion: record.access.pending_deletion,
    }
}

pub async fn project_proof(
    hosting: &Hosting,
    who: &Principal,
    id: ProjectId,
) -> Authorized<ProjectCtx> {
    let loaded = project_ctx(hosting, who, id).await;
    prove(who, Action::ProjectUpdate, ProjectCtx::new(loaded)).expect("allowed before the race")
}

pub async fn visibility_proof(
    hosting: &Hosting,
    who: &Principal,
    id: ProjectId,
    to: Visibility,
) -> Authorized<VisibilityChangeCtx> {
    let loaded = project_ctx(hosting, who, id).await;
    prove(
        who,
        Action::ProjectVisibilityChange,
        VisibilityChangeCtx::new(loaded, to),
    )
    .expect("allowed before the race")
}

pub async fn parent_proof(
    hosting: &Hosting,
    who: &Principal,
    namespace: forge_core::NamespaceId,
) -> Authorized<ParentCtx> {
    let parent = match hosting
        .parent(namespace, who.user())
        .await
        .expect("load")
        .expect("namespace exists")
    {
        forge_hosting::Parent::Group {
            namespace,
            group,
            role,
        } => LoadedParent::Group {
            namespace,
            group: LoadedGroup { id: group, role },
        },
        forge_hosting::Parent::User { namespace, owner } => {
            LoadedParent::User(LoadedUserNamespace { namespace, owner })
        }
        forge_hosting::Parent::Project => panic!("a project is not a parent"),
    };
    let ctx = ParentCtx::new(parent);
    assert_eq!(ctx.id(), namespace);
    prove(who, Action::ProjectCreate, ctx).expect("allowed before the race")
}

pub fn group_fields(path: &str) -> NewGroupFields {
    NewGroupFields {
        path: path.to_owned(),
        display_name: format!("Group {path}"),
        description: String::new(),
    }
}

pub fn project_fields(path: &str) -> NewProjectFields {
    NewProjectFields {
        path: path.to_owned(),
        display_name: format!("Project {path}"),
        description: "synthetic".to_owned(),
        visibility: Visibility::Private,
        kind: forge_core::RepositoryKind::System,
    }
}

/// The rows of `sql` (a count) as the superuser.
pub async fn count(t: &FreshDb, sql: &str) -> i64 {
    let rows = t.json_rows(sql).await;
    rows.first()
        .and_then(|r| r.get("n"))
        .and_then(serde_json::Value::as_i64)
        .expect("a count named n")
}

/// The member routes' proof (M1 phase 8b MR8), built as their loaders
/// build it: the member's direct role read now, `change` derived from it.
pub async fn project_member_proof(
    hosting: &Hosting,
    who: &Principal,
    project: ProjectId,
    member: UserId,
    change: impl FnOnce(Option<Role>) -> MembershipChange,
) -> Result<Authorized<ProjectMemberCtx>, forge_errors::ForgeError> {
    let loaded = project_ctx(hosting, who, project).await;
    let role = hosting
        .project_member_role(project, member)
        .await
        .expect("member role");
    prove(
        who,
        Action::MemberManage,
        ProjectMemberCtx::new(
            loaded,
            LoadedMembership {
                member,
                change: change(role),
                removes_last_owner: false,
            },
        ),
    )
}

pub async fn group_member_proof(
    hosting: &Hosting,
    who: &Principal,
    group: GroupId,
    member: UserId,
    change: impl FnOnce(Option<Role>) -> MembershipChange,
) -> Result<Authorized<GroupMemberCtx>, forge_errors::ForgeError> {
    let record = hosting
        .group(group, who.user())
        .await
        .expect("load")
        .expect("group exists");
    let role = hosting
        .group_member_role(group, member)
        .await
        .expect("member role");
    prove(
        who,
        Action::MemberManage,
        GroupMemberCtx::new(
            LoadedGroup {
                id: record.id,
                role: record.role,
            },
            LoadedMembership {
                member,
                change: change(role),
                removes_last_owner: false,
            },
        ),
    )
}

/// The protected-ref routes' proof: `rule` (None = a create) with the
/// level stored now and the requested one.
pub async fn protection_proof(
    hosting: &Hosting,
    who: &Principal,
    project: ProjectId,
    rule: Option<ProtectedRefId>,
    to: Option<Role>,
) -> Result<Authorized<RefProtectionCtx>, forge_errors::ForgeError> {
    let loaded = project_ctx(hosting, who, project).await;
    let from = match rule {
        None => None,
        Some(rule) => forge_hosting::protection_level(
            hosting
                .protected_ref_state(project, rule)
                .await
                .expect("rule state"),
        ),
    };
    prove(
        who,
        Action::ProtectedRefManage,
        RefProtectionCtx::new(loaded, LoadedProtection { rule, from, to }),
    )
}

/// `Change { from, to }` for a member, `Missing` for a non-member.
pub fn change_to(to: Role) -> impl FnOnce(Option<Role>) -> MembershipChange {
    move |from| match from {
        Some(from) => MembershipChange::Change { from, to },
        None => MembershipChange::Missing { to: Some(to) },
    }
}

/// `Remove { from }` for a member, `Missing` for a non-member.
pub fn removal(from: Option<Role>) -> MembershipChange {
    match from {
        Some(from) => MembershipChange::Remove { from },
        None => MembershipChange::Missing { to: None },
    }
}

/// The delete / restore routes' proof (D5 MR3), built as their loaders
/// build it: the project as `who` sees it now (a project pending deletion
/// is loaded too).
pub async fn lifecycle_proof(
    hosting: &Hosting,
    who: &Principal,
    action: Action,
    id: ProjectId,
) -> Result<Authorized<ProjectCtx>, forge_errors::ForgeError> {
    let loaded = project_ctx(hosting, who, id).await;
    prove(who, action, ProjectCtx::new(loaded))
}
