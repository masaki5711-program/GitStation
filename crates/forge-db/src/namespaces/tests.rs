//! D5 MR2: the subtree move (`projects::move_decided`,
//! `groups::move_decided`, `users::rename_decided`), tombstone releases
//! (`tombstones::release_decided`) and the per-resource audit rows, against
//! a migrated database. Inspection uses a superuser connection.

use forge_core::ids::{GroupId, NamespaceId, ProjectId, TombstoneId, UserId};
use forge_core::{RequestId, Role, TombstoneKind, UserKind};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use super::{Destination, MoveOptions, MoveOutcome, RateLimit, TargetAccess, MAX_MOVES_PER_DAY};
use crate::change::{Actor, ChangeContext};
use crate::groups::{self, CreatedGroup, GroupMoveFacts, NewGroup};
use crate::projects::{self, CreatedProject, NewProject, ProjectMoveFacts};
use crate::test_support::*;
use crate::tombstones::{self, TombstoneAccess, TombstoneReleaseFacts};
use crate::users::{self, NewUser, UserRenameFacts};

mod race;
mod shape;

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

fn as_user(user: UserId, request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::User(user),
        RequestId::parse(request).expect("request id"),
    )
}

fn deny() -> ForgeError {
    ForgeError::new(ErrorCode::Forbidden).with_context("reason", "test_denied")
}

async fn count(su: &mut PgConnection, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(su)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
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
    parent: Option<NamespaceId>,
    owner: UserId,
) -> CreatedGroup {
    let new = NewGroup {
        segment: segment.to_owned(),
        parent,
        display_name: format!("Group {segment}"),
        description: String::new(),
        owner,
    };
    groups::create(&t.db, &ctx("req-group"), &new)
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
    projects::create(&t.db, &ctx("req-project"), &new)
        .await
        .unwrap_or_else(|e| panic!("{segment}: {e}"))
}

fn to(parent: Option<NamespaceId>, segment: &str) -> Destination {
    Destination {
        parent,
        segment: segment.to_owned(),
    }
}

async fn move_project(
    t: &FreshDb,
    p: ProjectId,
    viewer: UserId,
    dest: Destination,
    options: MoveOptions,
) -> Result<(MoveOutcome, ProjectMoveFacts), ForgeError> {
    projects::move_decided(&t.db, &ctx("req-move"), p, viewer, &dest, options, |f| {
        Ok(*f)
    })
    .await
}

async fn move_group(
    t: &FreshDb,
    g: GroupId,
    viewer: UserId,
    dest: Destination,
) -> Result<(MoveOutcome, GroupMoveFacts), ForgeError> {
    groups::move_decided(
        &t.db,
        &ctx("req-move"),
        g,
        viewer,
        &dest,
        MoveOptions::default(),
        |f| Ok(*f),
    )
    .await
}

async fn path_of(su: &mut PgConnection, ns: NamespaceId) -> String {
    sqlx::query_scalar("SELECT full_path FROM forge.namespaces WHERE id = $1")
        .bind(ns.get())
        .fetch_one(su)
        .await
        .expect("path")
}

/// Every namespace's path and depth follow from its parent (the invariant
/// every move must keep), and no live path is also a tombstone.
pub(super) async fn assert_tree_consistent(su: &mut PgConnection) {
    let broken = count(
        su,
        "SELECT count(*) FROM forge.namespaces c LEFT JOIN forge.namespaces p ON p.id = c.parent_id \
         WHERE (c.parent_id IS NULL AND (c.full_path <> c.segment OR c.depth <> 1)) \
            OR (c.parent_id IS NOT NULL AND (c.full_path <> p.full_path || '/' || c.segment \
                OR c.depth <> p.depth + 1))",
    )
    .await;
    assert_eq!(
        broken, 0,
        "a namespace's path or depth disagrees with its parent"
    );
    let shadowed = count(
        su,
        "SELECT count(*) FROM forge.namespaces n \
         JOIN forge.namespace_tombstones t ON t.old_path = n.full_path",
    )
    .await;
    assert_eq!(shadowed, 0, "a live path is also a tombstone");
}

#[tokio::test]
async fn a_project_move_writes_one_audit_row_and_a_tombstone_at_the_old_path() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let beta = group(&t, "beta", None, alice.user_id).await;
    groups::add_member(
        &t.db,
        &ctx("req-bob"),
        beta.group_id,
        bob.user_id,
        Role::Developer,
    )
    .await
    .expect("bob develops in beta");
    let web = project(&t, "web", acme.namespace_id, alice.user_id).await;
    let (moved, facts) = move_project(
        &t,
        web.project_id,
        alice.user_id,
        to(Some(beta.namespace_id), "site"),
        MoveOptions::default(),
    )
    .await
    .expect("move");
    assert_eq!(moved.old_path, "acme/web");
    assert_eq!(moved.new_path, "beta/site");
    assert_eq!(moved.projects, vec![web.project_id]);
    assert_eq!(facts.source_parent, Some(acme.namespace_id));
    assert_eq!(facts.project.access.role, Some(Role::Owner));
    assert_eq!(
        facts.target,
        TargetAccess::Group {
            namespace: beta.namespace_id,
            group: beta.group_id,
            role: Some(Role::Owner),
        }
    );
    // bob inherits Developer from beta now; alice owned both sides.
    assert_eq!(moved.summary.role_changes.len(), 1);
    assert_eq!(moved.summary.role_changes[0].user, bob.user_id);
    assert_eq!(moved.summary.role_changes[0].from, None);
    assert_eq!(moved.summary.role_changes[0].to, Some(Role::Developer));
    let mut su = t.superuser().await;
    assert_eq!(path_of(&mut su, web.namespace_id).await, "beta/site");
    let rows = t
        .json_rows(
            "SELECT resource_type, resource_id, project_id, details FROM forge.audit_log \
             WHERE action = 'project.moved'",
        )
        .await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["project_id"], web.project_id.get());
    assert_eq!(rows[0]["details"]["renamed"], true);
    assert_eq!(
        rows[0]["details"]["role_changes"]["none>developer"][0],
        bob.user_id.get()
    );
    let stones = t
        .json_rows("SELECT old_path, kind, namespace_id, cause_type, cause_id FROM forge.namespace_tombstones")
        .await;
    assert_eq!(stones.len(), 1);
    assert_eq!(stones[0]["old_path"], "acme/web");
    assert_eq!(stones[0]["kind"], "moved");
    assert_eq!(stones[0]["namespace_id"], web.namespace_id.get());
    assert_eq!(stones[0]["cause_type"], "project");
    // D5 MR5: one project.moved event (integration), the project's id, no
    // role changes in the payload, a dedup key with the µs epoch.
    let events = t
        .json_rows(
            "SELECT event_type, project_id, aggregate_type, aggregate_id, payload, dedup_key \
             FROM forge.events WHERE event_type <> 'project.created'",
        )
        .await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["event_type"], "project.moved");
    assert_eq!(events[0]["project_id"], web.project_id.get());
    assert_eq!(events[0]["aggregate_type"], "project");
    assert_eq!(events[0]["payload"]["project_id"], web.project_id.get());
    assert_eq!(
        events[0]["payload"]["to_parent_id"],
        beta.namespace_id.get()
    );
    assert!(events[0]["payload"].get("role_changes").is_none());
    let key = events[0]["dedup_key"].as_str().expect("dedup key");
    assert!(
        key.starts_with(&format!("project_moved:{}:", web.project_id.get())),
        "{key}"
    );
    // A second move of the same project a moment later: another key.
    move_project(
        &t,
        web.project_id,
        alice.user_id,
        to(Some(beta.namespace_id), "site2"),
        MoveOptions::default(),
    )
    .await
    .expect("move again");
    assert_eq!(
        count(
            &mut su,
            "SELECT count(DISTINCT dedup_key) FROM forge.events WHERE event_type = 'project.moved'"
        )
        .await,
        2
    );
    assert_tree_consistent(&mut su).await;
    t.drop_now().await;
}

#[tokio::test]
async fn a_group_move_rewrites_its_subtree_and_audits_every_project() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let beta = group(&t, "beta", None, alice.user_id).await;
    let platform = group(&t, "platform", Some(acme.namespace_id), alice.user_id).await;
    let infra = group(&t, "infra", Some(platform.namespace_id), alice.user_id).await;
    let p1 = project(&t, "p1", infra.namespace_id, alice.user_id).await;
    let p2 = project(&t, "p2", platform.namespace_id, alice.user_id).await;
    let (moved, facts) = move_group(
        &t,
        platform.group_id,
        alice.user_id,
        to(Some(beta.namespace_id), "platform"),
    )
    .await
    .expect("move");
    assert_eq!(facts.source_parent, Some(acme.namespace_id));
    assert_eq!(facts.group.role, Some(Role::Owner));
    assert_eq!(moved.summary.nodes, 4);
    assert_eq!(moved.summary.projects, 2);
    assert!(!moved.summary.renamed);
    let mut su = t.superuser().await;
    assert_eq!(
        path_of(&mut su, p1.namespace_id).await,
        "beta/platform/infra/p1"
    );
    assert_eq!(path_of(&mut su, p2.namespace_id).await, "beta/platform/p2");
    assert_tree_consistent(&mut su).await;
    assert_eq!(
        count(&mut su, "SELECT count(*) FROM forge.namespace_tombstones WHERE kind = 'moved' AND cause_type = 'group'").await,
        4,
        "one tombstone per node"
    );
    assert_eq!(
        count(
            &mut su,
            "SELECT count(*) FROM forge.audit_log WHERE action = 'group.moved'"
        )
        .await,
        1
    );
    let projects = t
        .json_rows(
            "SELECT project_id, details FROM forge.audit_log WHERE action = 'project.moved' \
             ORDER BY project_id",
        )
        .await;
    assert_eq!(projects.len(), 2, "one row per affected project");
    assert_eq!(projects[0]["project_id"], p1.project_id.get());
    assert_eq!(projects[1]["project_id"], p2.project_id.get());
    assert_eq!(
        projects[0]["details"]["moved_namespace_id"],
        platform.namespace_id.get()
    );
    // D5 MR5: one instance-wide group.moved event; the projects of the
    // subtree get none (more than 100 events per change otherwise).
    let events = t
        .json_rows(
            "SELECT event_type, project_id, aggregate_id, payload, dedup_key FROM forge.events \
             WHERE event_type <> 'project.created'",
        )
        .await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["event_type"], "group.moved");
    assert_eq!(events[0]["project_id"], serde_json::Value::Null);
    assert_eq!(events[0]["aggregate_id"], platform.group_id.get());
    assert_eq!(events[0]["payload"]["projects"], 2);
    assert!(events[0]["dedup_key"]
        .as_str()
        .is_some_and(|k| k.starts_with(&format!("group_moved:{}:", platform.group_id.get()))));
    t.drop_now().await;
}

#[tokio::test]
async fn a_user_rename_changes_the_username_and_reclaim_brings_the_old_name_back() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let web = project(&t, "web", alice.namespace_id, alice.user_id).await;
    let rename = |name: &'static str, reclaim: bool| {
        let db = t.db.clone();
        async move {
            users::rename_decided(
                &db,
                &ctx("req-rename"),
                alice.user_id,
                alice.user_id,
                name,
                MoveOptions {
                    reclaim,
                    rate_limit: RateLimit::Enforce,
                },
                |f: &UserRenameFacts| Ok(*f),
            )
            .await
        }
    };
    let (moved, facts) = rename("alicia", false).await.expect("rename");
    assert_eq!(facts.target_user, alice.user_id);
    assert_eq!(facts.kind, UserKind::Human);
    assert_eq!(
        (moved.old_path.as_str(), moved.new_path.as_str()),
        ("alice", "alicia")
    );
    let mut su = t.superuser().await;
    assert_eq!(
        count(
            &mut su,
            "SELECT count(*) FROM forge.users WHERE username = 'alicia'"
        )
        .await,
        1
    );
    assert_eq!(path_of(&mut su, web.namespace_id).await, "alicia/web");
    assert_eq!(
        count(
            &mut su,
            "SELECT count(*) FROM forge.audit_log WHERE action = 'user.renamed'"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &mut su,
            "SELECT count(*) FROM forge.audit_log WHERE action = 'project.moved'"
        )
        .await,
        1
    );
    // Back without reclaim: the tombstone blocks (ADR-0015).
    let err = rename("alice", false).await.expect_err("tombstoned");
    assert_reason(&err, ErrorCode::NamespacePathTaken, "tombstoned");
    // With reclaim: release + rename in one transaction.
    let (back, _) = rename("alice", true).await.expect("reclaim");
    assert_eq!(back.summary.reclaimed_tombstones, 2);
    let stones: Vec<String> =
        sqlx::query_scalar("SELECT old_path FROM forge.namespace_tombstones ORDER BY old_path")
            .fetch_all(&mut su)
            .await
            .expect("tombstones");
    assert_eq!(stones, vec!["alicia".to_owned(), "alicia/web".to_owned()]);
    assert_tree_consistent(&mut su).await;
    t.drop_now().await;
}

#[tokio::test]
async fn reclaim_never_releases_a_tombstone_of_another_namespace() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let x = project(&t, "x", acme.namespace_id, alice.user_id).await;
    let z = project(&t, "z", acme.namespace_id, alice.user_id).await;
    let reclaim = MoveOptions {
        reclaim: true,
        rate_limit: RateLimit::Enforce,
    };
    move_project(
        &t,
        x.project_id,
        alice.user_id,
        to(Some(acme.namespace_id), "y"),
        reclaim,
    )
    .await
    .expect("x -> y");
    let err = move_project(
        &t,
        z.project_id,
        alice.user_id,
        to(Some(acme.namespace_id), "x"),
        reclaim,
    )
    .await
    .expect_err("x's tombstone is not z's");
    assert_reason(&err, ErrorCode::NamespacePathTaken, "tombstoned");
    // A live sibling blocks too.
    let err = move_project(
        &t,
        z.project_id,
        alice.user_id,
        to(Some(acme.namespace_id), "y"),
        reclaim,
    )
    .await
    .expect_err("y is taken");
    assert_reason(&err, ErrorCode::NamespacePathTaken, "sibling");
    t.drop_now().await;
}

#[tokio::test]
async fn a_tombstone_at_a_descendants_new_path_blocks_the_move() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let beta = group(&t, "beta", None, alice.user_id).await;
    let g = group(&t, "g", Some(acme.namespace_id), alice.user_id).await;
    let _p = project(&t, "p", g.namespace_id, alice.user_id).await;
    t.commit_audited(
        ("group", beta.group_id.get()),
        &[format!(
            "INSERT INTO forge.namespace_tombstones (old_path, kind, cause_type, cause_id) \
             VALUES ('beta/g/p', 'deleted', 'group', {})",
            beta.group_id.get()
        )],
    )
    .await;
    let err = move_group(
        &t,
        g.group_id,
        alice.user_id,
        to(Some(beta.namespace_id), "g"),
    )
    .await
    .expect_err("a descendant's path is tombstoned");
    assert_reason(&err, ErrorCode::NamespacePathTaken, "tombstoned");
    let mut su = t.superuser().await;
    assert_eq!(
        count(
            &mut su,
            "SELECT count(*) FROM forge.audit_log WHERE action = 'group.moved'"
        )
        .await,
        0
    );
    assert_eq!(path_of(&mut su, g.namespace_id).await, "acme/g");
    t.drop_now().await;
}

#[tokio::test]
async fn a_move_breaking_a_subtree_rule_is_transfer_invalid() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let carol = user(&t, "carol").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let sub = group(&t, "sub", Some(acme.namespace_id), alice.user_id).await;
    let web = project(&t, "web", acme.namespace_id, alice.user_id).await;
    let api = project(&t, "api", acme.namespace_id, alice.user_id).await;
    let bot = users::create(
        &t.db,
        &ctx("req-bot"),
        &NewUser {
            kind: UserKind::Bot,
            email: None,
            ..NewUser::human("ci-bot", "CI", "unused@example.test")
        },
    )
    .await
    .expect("bot");
    let invalid = |r: Result<(MoveOutcome, ProjectMoveFacts), ForgeError>, reason: &str| {
        assert_reason(&r.expect_err(reason), ErrorCode::TransferInvalid, reason);
    };
    let d = MoveOptions::default();
    invalid(
        move_project(
            &t,
            web.project_id,
            alice.user_id,
            to(Some(api.namespace_id), "web"),
            d,
        )
        .await,
        "target_kind",
    );
    invalid(
        move_project(&t, web.project_id, alice.user_id, to(None, "web"), d).await,
        "target_kind",
    );
    invalid(
        move_project(
            &t,
            web.project_id,
            alice.user_id,
            to(Some(acme.namespace_id), "web"),
            d,
        )
        .await,
        "same_location",
    );
    invalid(
        move_project(
            &t,
            web.project_id,
            alice.user_id,
            to(Some(bot.namespace_id), "web"),
            d,
        )
        .await,
        "parent_owner_bot",
    );
    let _ = users::disable_with(&t.db, &ctx("req-off"), carol.user_id, users::Guard::Force)
        .await
        .expect("disable carol");
    invalid(
        move_project(
            &t,
            web.project_id,
            alice.user_id,
            to(Some(carol.namespace_id), "web"),
            d,
        )
        .await,
        "parent_owner_inactive",
    );
    let group_invalid = |r: Result<(MoveOutcome, GroupMoveFacts), ForgeError>, reason: &str| {
        assert_reason(&r.expect_err(reason), ErrorCode::TransferInvalid, reason);
    };
    group_invalid(
        move_group(
            &t,
            acme.group_id,
            alice.user_id,
            to(Some(sub.namespace_id), "acme"),
        )
        .await,
        "cycle",
    );
    group_invalid(
        move_group(
            &t,
            acme.group_id,
            alice.user_id,
            to(Some(acme.namespace_id), "x"),
        )
        .await,
        "cycle",
    );
    group_invalid(
        move_group(
            &t,
            sub.group_id,
            alice.user_id,
            to(Some(alice.namespace_id), "sub"),
        )
        .await,
        "target_kind",
    );
    // too_deep: a chain of 18 below `deep`, moved below `sub` (depth 2).
    let deep = group(&t, "deep", None, alice.user_id).await;
    let mut parent = deep.namespace_id;
    for n in 0..18 {
        parent = group(&t, &format!("d{n}"), Some(parent), alice.user_id)
            .await
            .namespace_id;
    }
    group_invalid(
        move_group(
            &t,
            deep.group_id,
            alice.user_id,
            to(Some(sub.namespace_id), "deep"),
        )
        .await,
        "too_deep",
    );
    // path_too_long: a long segment chain moved below a long name.
    let long = "l".repeat(60);
    let tall = group(&t, &format!("{long}1"), None, alice.user_id).await;
    let a = group(
        &t,
        &format!("{long}2"),
        Some(tall.namespace_id),
        alice.user_id,
    )
    .await;
    let b = group(&t, &format!("{long}3"), Some(a.namespace_id), alice.user_id).await;
    let _c = group(&t, &format!("{long}4"), Some(b.namespace_id), alice.user_id).await;
    let other = group(&t, &format!("{long}5"), None, alice.user_id).await;
    group_invalid(
        move_group(
            &t,
            tall.group_id,
            alice.user_id,
            to(Some(other.namespace_id), &format!("{long}1")),
        )
        .await,
        "path_too_long",
    );
    // subtree_too_large: 1000 children below `big` (1001 namespaces).
    let big = group(&t, "big", None, alice.user_id).await;
    t.commit_audited(
        ("group", big.group_id.get()),
        &[
            "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, resource_id, \
             request_id) SELECT 'system', 'probe.changed', 'succeeded', 'group', 900000 + g, \
             'r-fixture' FROM generate_series(1, 1000) AS g"
                .to_owned(),
            format!(
                "INSERT INTO forge.namespaces (parent_id, kind, owner_id, segment, full_path, depth) \
                 SELECT {}, 'group', 900000 + g, 'c' || g, 'big/c' || g, 2 \
                 FROM generate_series(1, 1000) AS g",
                big.namespace_id.get()
            ),
        ],
    )
    .await;
    group_invalid(
        move_group(
            &t,
            big.group_id,
            alice.user_id,
            to(Some(acme.namespace_id), "big"),
        )
        .await,
        "subtree_too_large",
    );
    let mut su = t.superuser().await;
    assert_eq!(
        count(&mut su, "SELECT count(*) FROM forge.namespace_tombstones").await,
        0,
        "nothing written"
    );
    assert_eq!(
        count(
            &mut su,
            "SELECT count(*) FROM forge.audit_log WHERE action IN ('project.moved', 'group.moved')"
        )
        .await,
        0
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_denied_move_writes_nothing_and_decides_before_the_name_rules() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let web = project(&t, "web", acme.namespace_id, alice.user_id).await;
    let err = projects::move_decided(
        &t.db,
        &ctx("req-move"),
        web.project_id,
        alice.user_id,
        &to(Some(acme.namespace_id), "NOT A SEGMENT"),
        MoveOptions::default(),
        |_: &ProjectMoveFacts| -> Result<(), ForgeError> { Err(deny()) },
    )
    .await
    .expect_err("denied");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    let mut su = t.superuser().await;
    assert_eq!(path_of(&mut su, web.namespace_id).await, "acme/web");
    assert_eq!(
        count(&mut su, "SELECT count(*) FROM forge.namespace_tombstones").await,
        0
    );
    // The locks were released: a second (allowed) move goes through.
    move_project(
        &t,
        web.project_id,
        alice.user_id,
        to(Some(acme.namespace_id), "web2"),
        MoveOptions::default(),
    )
    .await
    .expect("move after the denial");
    t.drop_now().await;
}

#[tokio::test]
async fn the_eleventh_move_in_a_day_is_rate_limited() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let web = project(&t, "web", acme.namespace_id, alice.user_id).await;
    for n in 0..MAX_MOVES_PER_DAY {
        move_project(
            &t,
            web.project_id,
            alice.user_id,
            to(Some(acme.namespace_id), &format!("w{n}")),
            MoveOptions::default(),
        )
        .await
        .unwrap_or_else(|e| panic!("move {n}: {e}"));
    }
    let err = move_project(
        &t,
        web.project_id,
        alice.user_id,
        to(Some(acme.namespace_id), "w-late"),
        MoveOptions::default(),
    )
    .await
    .expect_err("rate limited");
    assert_reason(&err, ErrorCode::RenameRateLimited, "moves_per_day");
    assert_eq!(err.code().http_status(), 429);
    // The count comes from the append-only audit log: removing the
    // tombstones (as releases or reclaims do) does not lower it.
    t.commit_audited(
        ("project", web.project_id.get()),
        &["DELETE FROM forge.namespace_tombstones".to_owned()],
    )
    .await;
    let err = move_project(
        &t,
        web.project_id,
        alice.user_id,
        to(Some(acme.namespace_id), "w-late"),
        MoveOptions::default(),
    )
    .await
    .expect_err("still rate limited");
    assert_reason(&err, ErrorCode::RenameRateLimited, "moves_per_day");
    let exempt = MoveOptions {
        reclaim: false,
        rate_limit: RateLimit::Exempt,
    };
    // Only the operator CLI (actor system) is exempt.
    let err = projects::move_decided(
        &t.db,
        &as_user(alice.user_id, "req-user-move"),
        web.project_id,
        alice.user_id,
        &to(Some(acme.namespace_id), "w-user"),
        exempt,
        |f: &ProjectMoveFacts| Ok(*f),
    )
    .await
    .expect_err("a user is never exempt");
    assert_reason(&err, ErrorCode::Internal, "rate_limit_exempt_not_system");
    move_project(
        &t,
        web.project_id,
        alice.user_id,
        to(Some(acme.namespace_id), "w-operator"),
        exempt,
    )
    .await
    .expect("the operator is exempt");
    t.drop_now().await;
}

#[tokio::test]
async fn a_moved_tombstone_is_released_by_its_targets_owner_and_a_deleted_one_names_its_parent() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let web = project(&t, "web", acme.namespace_id, alice.user_id).await;
    move_project(
        &t,
        web.project_id,
        alice.user_id,
        to(Some(acme.namespace_id), "site"),
        MoveOptions::default(),
    )
    .await
    .expect("rename");
    let mut su = t.superuser().await;
    let moved: i64 = sqlx::query_scalar("SELECT id FROM forge.namespace_tombstones")
        .fetch_one(&mut su)
        .await
        .expect("tombstone");
    let release = |viewer: UserId, id: i64| {
        let db = t.db.clone();
        async move {
            tombstones::release_decided(
                &db,
                &ctx("req-release"),
                TombstoneId::new(id),
                viewer,
                |f: &TombstoneReleaseFacts| Ok(*f),
            )
            .await
        }
    };
    let err = tombstones::release_decided(
        &t.db,
        &ctx("req-release"),
        TombstoneId::new(moved),
        bob.user_id,
        |f: &TombstoneReleaseFacts| -> Result<(), ForgeError> {
            // bob has no role on the target.
            assert_eq!(
                f.access,
                TombstoneAccess::Moved {
                    target: web.namespace_id,
                    target_role: None
                }
            );
            Err(deny())
        },
    )
    .await
    .expect_err("bob is denied");
    assert_eq!(err.code(), ErrorCode::Forbidden, "{}", chain(&err));
    let (released, facts) = release(alice.user_id, moved).await.expect("alice releases");
    assert_eq!(released.path, "acme/web");
    assert_eq!(released.kind, TombstoneKind::Moved);
    assert_eq!(
        facts.access,
        TombstoneAccess::Moved {
            target: web.namespace_id,
            target_role: Some(Role::Owner)
        }
    );
    let rows = t
        .json_rows(
            "SELECT resource_type, project_id, details FROM forge.audit_log \
             WHERE action = 'namespace_tombstone.released'",
        )
        .await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["resource_type"], "project");
    assert_eq!(rows[0]["project_id"], web.project_id.get());
    // The path is free again.
    project(&t, "web", acme.namespace_id, alice.user_id).await;
    // A deleted tombstone below acme.
    t.commit_audited(
        ("group", acme.group_id.get()),
        &[format!(
            "INSERT INTO forge.namespace_tombstones (old_path, parent_namespace_id, kind, \
             owner_at_write, cause_type, cause_id) VALUES ('acme/gone', {}, 'deleted', {}, \
             'group', {})",
            acme.namespace_id.get(),
            alice.user_id.get(),
            acme.group_id.get()
        )],
    )
    .await;
    let gone: i64 = sqlx::query_scalar(
        "SELECT id FROM forge.namespace_tombstones WHERE old_path = 'acme/gone'",
    )
    .fetch_one(&mut su)
    .await
    .expect("deleted tombstone");
    let (_, facts) = release(alice.user_id, gone).await.expect("release deleted");
    assert_eq!(
        facts.access,
        TombstoneAccess::Deleted {
            recorded_owner: Some(alice.user_id),
            caller_parent_role: Some(Role::Owner),
            has_parent: true
        }
    );
    let err = release(alice.user_id, gone)
        .await
        .expect_err("already released");
    assert_reason(&err, ErrorCode::NotFound, "absent");
    t.drop_now().await;
}

#[tokio::test]
async fn a_move_that_leaves_no_active_owner_is_refused() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let carol = user(&t, "carol").await;
    let beta = group(&t, "beta", None, alice.user_id).await;
    // carol's project: Owner only through her personal namespace.
    let web = project(&t, "web", carol.namespace_id, carol.user_id).await;
    projects::remove_member(
        &t.db,
        &ctx("req-rm"),
        web.project_id,
        carol.user_id,
        Role::Owner,
    )
    .await
    .expect("carol still inherits Owner");
    // beta's only Owner is disabled: moving web there leaves it ownerless.
    let _ = users::disable_with(&t.db, &ctx("req-off"), alice.user_id, users::Guard::Force)
        .await
        .expect("disable alice");
    let err = move_project(
        &t,
        web.project_id,
        carol.user_id,
        to(Some(beta.namespace_id), "web"),
        MoveOptions::default(),
    )
    .await
    .expect_err("ownerless");
    assert_reason(&err, ErrorCode::TransferInvalid, "no_active_owner");
    // A subgroup without a direct Owner cannot become top level.
    let dave = user(&t, "dave").await;
    let acme = group(&t, "acme", None, dave.user_id).await;
    let sub = group(&t, "sub", Some(acme.namespace_id), dave.user_id).await;
    groups::remove_member(
        &t.db,
        &ctx("req-rm-sub"),
        sub.group_id,
        dave.user_id,
        Role::Owner,
    )
    .await
    .expect("a subgroup may lose its direct Owner");
    let err = move_group(&t, sub.group_id, dave.user_id, to(None, "sub"))
        .await
        .expect_err("no direct Owner at the top level");
    assert_reason(&err, ErrorCode::TransferInvalid, "no_active_owner");
    let mut su = t.superuser().await;
    assert_eq!(
        count(&mut su, "SELECT count(*) FROM forge.namespace_tombstones").await,
        0
    );
    t.drop_now().await;
}
