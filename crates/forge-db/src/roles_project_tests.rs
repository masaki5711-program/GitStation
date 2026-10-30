//! Loading project roles (step D4, ADR-0010 Amendment D4): the direct
//! project role, the ancestor groups' roles and the owner of a personal
//! namespace. Fixtures are written through the user, group and project
//! commands.

use forge_core::ids::ProjectId;
use forge_core::RequestId;
use forge_errors::ErrorCode;

use super::*;
use crate::audited_tests::{audit, commit};
use crate::change::{Actor, ChangeContext};
use crate::groups::{self, CreatedGroup, NewGroup};
use crate::projects::{self, CreatedProject, NewProject};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

async fn user(t: &FreshDb, name: &str) -> users::Created {
    users::create(
        &t.db,
        &ctx(&format!("req-user-{name}")),
        &NewUser::human(name, name, &format!("{name}@example.test")),
    )
    .await
    .expect("user")
}

async fn group(
    t: &FreshDb,
    segment: &str,
    parent: Option<&CreatedGroup>,
    owner: UserId,
) -> CreatedGroup {
    let new = NewGroup {
        segment: segment.to_owned(),
        parent: parent.map(|p| p.namespace_id),
        display_name: format!("Group {segment}"),
        description: String::new(),
        owner,
    };
    groups::create(&t.db, &ctx(&format!("req-group-{segment}")), &new)
        .await
        .unwrap_or_else(|e| panic!("{segment}: {e}"))
}

async fn project(
    t: &FreshDb,
    segment: &str,
    parent: NamespaceId,
    creator: UserId,
) -> CreatedProject {
    let new = NewProject::new(segment, parent, &format!("Project {segment}"), creator);
    projects::create(&t.db, &ctx(&format!("req-project-{segment}")), &new)
        .await
        .unwrap_or_else(|e| panic!("{segment}: {e}"))
}

/// Add `user` as a direct project member (MR4 brings the command; the test
/// writes the row with its audit row, as forge_app).
async fn add_project_member(t: &FreshDb, p: &CreatedProject, user: UserId, role: Role) {
    commit(
        t,
        &[
            audit("project", p.project_id.get(), "succeeded"),
            format!(
                "INSERT INTO forge.project_members (project_id, user_id, role) \
                 VALUES ({}, {}, '{}')",
                p.project_id.get(),
                user.get(),
                role.as_str()
            ),
        ],
    )
    .await
    .expect("add project member");
}

/// The project loader and the namespace loader agree on a project; returns
/// their answer.
async fn roles(t: &FreshDb, p: &CreatedProject, user: UserId) -> MemberRoles {
    let by_project = load_project_roles(&t.db, p.project_id, user)
        .await
        .expect("load_project_roles");
    let by_namespace = load_namespace_roles(&t.db, p.namespace_id, user)
        .await
        .expect("load_namespace_roles");
    assert_eq!(by_project, by_namespace, "the two loaders disagree");
    by_project
}

fn member(native: Option<Role>, direct: Option<Role>) -> MemberRoles {
    MemberRoles { native, direct }
}

#[tokio::test]
async fn personal_namespace_owner_is_owner_of_its_projects() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    // Bob creates a project in Alice's namespace (authorize() decides who
    // may; the loader only computes): Bob is the direct Owner, Alice
    // inherits Owner from her namespace.
    let web = project(&t, "web", alice.namespace_id, bob.user_id).await;
    assert_eq!(
        roles(&t, &web, bob.user_id).await,
        member(Some(Role::Owner), Some(Role::Owner))
    );
    assert_eq!(
        roles(&t, &web, alice.user_id).await,
        member(Some(Role::Owner), None)
    );
    // A personal namespace itself still carries no role (unchanged, D2).
    assert_eq!(
        load_namespace_roles(&t.db, alice.namespace_id, alice.user_id)
            .await
            .expect("personal"),
        member(None, None)
    );
    // A stranger has none.
    let carol = user(&t, "carol").await;
    assert_eq!(roles(&t, &web, carol.user_id).await, member(None, None));
    t.drop_now().await;
}

#[tokio::test]
async fn group_owner_inherits_project_owner() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let carol = user(&t, "carol").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let api = group(&t, "api", Some(&acme), alice.user_id).await;
    groups::add_member(
        &t.db,
        &ctx("req-carol"),
        api.group_id,
        carol.user_id,
        Role::Reporter,
    )
    .await
    .expect("carol");
    let web = project(&t, "web", api.namespace_id, bob.user_id).await;
    // Alice owns the top group: Owner of every project below it.
    assert_eq!(
        roles(&t, &web, alice.user_id).await,
        member(Some(Role::Owner), None)
    );
    // Carol: Reporter inherited, Developer direct: the higher wins, and a
    // direct role below an inherited one never caps it.
    add_project_member(&t, &web, carol.user_id, Role::Developer).await;
    assert_eq!(
        roles(&t, &web, carol.user_id).await,
        member(Some(Role::Developer), Some(Role::Developer))
    );
    add_project_member(&t, &web, alice.user_id, Role::Guest).await;
    assert_eq!(
        roles(&t, &web, alice.user_id).await,
        member(Some(Role::Owner), Some(Role::Guest))
    );
    // The creator, not a group member, holds the direct Owner role.
    assert_eq!(
        roles(&t, &web, bob.user_id).await,
        member(Some(Role::Owner), Some(Role::Owner))
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_disabled_member_has_no_project_role() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let carol = user(&t, "carol").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    groups::add_member(
        &t.db,
        &ctx("req-bob"),
        acme.group_id,
        bob.user_id,
        Role::Owner,
    )
    .await
    .expect("bob");
    let in_group = project(&t, "web", acme.namespace_id, carol.user_id).await;
    let personal = project(&t, "notes", carol.namespace_id, carol.user_id).await;
    assert_eq!(
        users::disable(&t.db, &ctx("req-disable-bob"), bob.user_id)
            .await
            .expect("disable bob"),
        users::DisableOutcome::Disabled
    );
    // carol is the only Owner of carol/notes: since D4 MR4 the disable
    // guard refuses that unless forced (ADR-0019 Amendment D4 MR4).
    assert_eq!(
        users::disable_with(
            &t.db,
            &ctx("req-disable-carol"),
            carol.user_id,
            users::Guard::Force
        )
        .await
        .expect("disable carol"),
        users::DisableOutcome::DisabledPastGuard {
            ownerless_groups: 0,
            ownerless_projects: 1,
            left_without_admin: false,
        }
    );
    // An inherited group role, a direct role and a personal namespace's
    // ownership all end with the account.
    assert_eq!(roles(&t, &in_group, bob.user_id).await, member(None, None));
    assert_eq!(
        roles(&t, &in_group, carol.user_id).await,
        member(None, None)
    );
    assert_eq!(
        roles(&t, &personal, carol.user_id).await,
        member(None, None)
    );
    // Alice is unaffected.
    assert_eq!(
        roles(&t, &in_group, alice.user_id).await,
        member(Some(Role::Owner), None)
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_project_20_deep_resolves_its_top_group() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let top = group(&t, "l1", None, alice.user_id).await;
    let mut parent = top;
    for level in 2..=19 {
        parent = group(&t, &format!("l{level}"), Some(&parent), alice.user_id).await;
    }
    let deep = project(&t, "p", parent.namespace_id, bob.user_id).await;
    assert_eq!(
        roles(&t, &deep, alice.user_id).await,
        member(Some(Role::Owner), None)
    );
    t.drop_now().await;
}

#[tokio::test]
async fn an_unknown_project_is_not_found() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let err = load_project_roles(&t.db, ProjectId::new(9_999), alice.user_id)
        .await
        .expect_err("absent");
    assert_reason(&err, ErrorCode::NotFound, "absent");
    assert!(err.to_string().contains("resource=project"), "{err}");
    t.drop_now().await;
}

/// Write a namespace row (and, for a project, its project row) directly as
/// the superuser, with the audit row the trigger needs: shapes the commands
/// refuse (a project in a bot's namespace) or that only D5 moves could
/// produce (a group under a user namespace).
async fn hand_insert(t: &FreshDb, sql: &str) {
    let mut su = t.superuser().await;
    sqlx::raw_sql(&format!("BEGIN; {sql}; COMMIT"))
        .execute(&mut su)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// A project `segment` with id `id` under namespace `parent` (full path
/// `path`, depth `depth`), written by hand.
async fn hand_project(t: &FreshDb, id: i64, parent: NamespaceId, path: &str, depth: i16) {
    let segment = path.rsplit('/').next().unwrap_or(path);
    hand_insert(
        t,
        &format!(
            "{}; INSERT INTO forge.projects (id, display_name) OVERRIDING SYSTEM VALUE \
             VALUES ({id}, 'P'); \
             INSERT INTO forge.namespaces (parent_id, kind, owner_id, segment, full_path, depth) \
             VALUES ({}, 'project', {id}, '{segment}', '{path}', {depth})",
            audit("project", id, "succeeded"),
            parent.get()
        ),
    )
    .await;
}

#[tokio::test]
async fn a_bots_personal_namespace_gives_the_bot_no_role() {
    let t = FreshDb::migrated().await;
    let bot = users::create(
        &t.db,
        &ctx("req-bot"),
        &NewUser {
            kind: forge_core::UserKind::Bot,
            email: None,
            ..NewUser::human("ci-bot", "CI", "unused@example.test")
        },
    )
    .await
    .expect("bot");
    // projects::create refuses this parent; a hand-written row must still
    // give the bot nothing.
    hand_project(&t, 900, bot.namespace_id, "ci-bot/tools", 2).await;
    assert_eq!(
        load_project_roles(&t.db, ProjectId::new(900), bot.user_id)
            .await
            .expect("roles"),
        member(None, None)
    );
    t.drop_now().await;
}

/// The personal-namespace Owner rule covers only a project directly in that
/// namespace: a group that ended up under a user namespace (only a D5 move
/// could do that) gives the user no role on the group or its projects.
#[tokio::test]
async fn a_user_namespace_above_a_group_gives_no_role() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    hand_insert(
        &t,
        &format!(
            "{}; INSERT INTO forge.groups (id, display_name) OVERRIDING SYSTEM VALUE \
             VALUES (900, 'Team'); \
             INSERT INTO forge.namespaces (parent_id, kind, owner_id, segment, full_path, depth) \
             VALUES ({}, 'group', 900, 'team', 'alice/team', 2)",
            audit("group", 900, "succeeded"),
            alice.namespace_id.get()
        ),
    )
    .await;
    let team_ns: i64 = sqlx::query_scalar(
        "SELECT id FROM forge.namespaces WHERE kind = 'group' AND owner_id = 900",
    )
    .fetch_one(&mut t.superuser().await)
    .await
    .expect("team namespace");
    let team_ns = NamespaceId::new(team_ns);
    assert_eq!(
        load_group_roles(&t.db, GroupId::new(900), alice.user_id)
            .await
            .expect("group roles"),
        member(None, None)
    );
    let web = project(&t, "web", team_ns, bob.user_id).await;
    assert_eq!(roles(&t, &web, alice.user_id).await, member(None, None));
    t.drop_now().await;
}

#[tokio::test]
async fn a_personal_namespace_owner_has_no_role_on_a_project_in_a_group() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let carol = user(&t, "carol").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let web = project(&t, "web", acme.namespace_id, bob.user_id).await;
    // Carol owns a personal namespace, but not the group the project is in.
    assert_eq!(roles(&t, &web, carol.user_id).await, member(None, None));
    t.drop_now().await;
}
