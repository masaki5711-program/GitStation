//! Direct project memberships (D4 MR4): add / change / remove with one
//! audit row and one maintainer-class event each, the last-owner rule with
//! inherited Owners, and the project half of the disable guard.

use std::time::Duration;

use forge_core::{EventType, Role, Scope, VisibilityClass};

use super::*;
use crate::projects::ownership;
use crate::roles::load_project_roles;
use crate::tokens::{self, NewToken};
use crate::users::{DisableOutcome, Guard};

/// (action, resource_type, resource_id, project_id) of a request's audit rows.
async fn audit_rows(su: &mut PgConnection, request: &str) -> Vec<(String, String, i64, i64)> {
    sqlx::query_as(
        "SELECT action, resource_type, resource_id, project_id FROM forge.audit_log \
         WHERE request_id = $1 ORDER BY id",
    )
    .bind(request)
    .fetch_all(su)
    .await
    .expect("audit rows")
}

/// (event_type, aggregate_type, aggregate_id, project_id, payload, has a
/// dedup key) of a request's events.
type EventRow = (String, String, i64, i64, serde_json::Value, bool);

async fn events_of(su: &mut PgConnection, request: &str) -> Vec<EventRow> {
    sqlx::query_as(
        "SELECT event_type, aggregate_type, aggregate_id, project_id, payload, \
           dedup_key IS NOT NULL FROM forge.events WHERE request_id = $1 ORDER BY id",
    )
    .bind(request)
    .fetch_all(su)
    .await
    .expect("events")
}

async fn role_of(su: &mut PgConnection, project: ProjectId, user: UserId) -> Option<String> {
    sqlx::query_scalar(
        "SELECT role FROM forge.project_members WHERE project_id = $1 AND user_id = $2",
    )
    .bind(project.get())
    .bind(user.get())
    .fetch_optional(su)
    .await
    .expect("role")
}

/// `alice/web`, created by alice (her direct Owner, and she inherits Owner
/// as the namespace owner), plus bob.
async fn alice_web(t: &FreshDb) -> (CreatedProject, users::Created, users::Created) {
    let alice = user(t, "alice").await;
    let bob = user(t, "bob").await;
    let web = create_ok(t, &project("web", alice.namespace_id, alice.user_id)).await;
    (web, alice, bob)
}

async fn bot(t: &FreshDb) -> UserId {
    users::create(
        &t.db,
        &ctx("req-bot"),
        &NewUser {
            kind: UserKind::Bot,
            email: None,
            ..NewUser::human("ci-bot", "CI", "unused@example.test")
        },
    )
    .await
    .expect("bot")
    .user_id
}

#[tokio::test]
async fn project_member_add_change_remove_write_one_audit_row_each() {
    let t = FreshDb::migrated().await;
    let (web, _, bob) = alice_web(&t).await;
    let (p, bob) = (web.project_id, bob.user_id);
    add_member(&t.db, &ctx("req-add"), p, bob, Role::Developer)
        .await
        .expect("add");
    let changed = change_member(
        &t.db,
        &ctx("req-change"),
        p,
        bob,
        Role::Developer,
        Role::Maintainer,
    )
    .await
    .expect("change");
    assert_eq!(
        changed,
        MemberChange::Changed {
            from: Role::Developer
        }
    );
    remove_member(&t.db, &ctx("req-remove"), p, bob, Role::Maintainer)
        .await
        .expect("remove");
    let mut su = t.superuser().await;
    for (request, action, details) in [
        (
            "req-add",
            "member.added",
            json!({"user_id": bob.get(), "role": "developer"}),
        ),
        (
            "req-change",
            "member.role_changed",
            json!({"user_id": bob.get(), "from": "developer", "to": "maintainer"}),
        ),
        (
            "req-remove",
            "member.removed",
            json!({"user_id": bob.get(), "role": "maintainer"}),
        ),
    ] {
        // One audit row on the project, naming the project (ADR-0016
        // Amendment D4: the scope rule).
        assert_eq!(
            audit_rows(&mut su, request).await,
            vec![(action.to_owned(), "project".to_owned(), p.get(), p.get())],
            "{request}"
        );
        let stored: serde_json::Value =
            sqlx::query_scalar("SELECT details FROM forge.audit_log WHERE request_id = $1")
                .bind(request)
                .fetch_one(&mut su)
                .await
                .expect("details");
        assert_eq!(stored, details, "{request}");
        // One event of the project, its payload the audit details, no
        // dedup key.
        assert_eq!(
            events_of(&mut su, request).await,
            vec![(
                action.to_owned(),
                "project".to_owned(),
                p.get(),
                p.get(),
                details,
                false
            )],
            "{request}"
        );
    }
    assert_eq!(role_of(&mut su, p, bob).await, None);
    t.drop_now().await;
}

#[tokio::test]
async fn project_member_events_are_maintainer_class() {
    let t = FreshDb::migrated().await;
    let (web, alice, bob) = alice_web(&t).await;
    let (p, bob) = (web.project_id, bob.user_id);
    add_member(&t.db, &ctx("req-add"), p, bob, Role::Reporter)
        .await
        .expect("add");
    let _ = change_member(
        &t.db,
        &ctx("req-change"),
        p,
        bob,
        Role::Reporter,
        Role::Developer,
    )
    .await
    .expect("change");
    remove_member(&t.db, &ctx("req-remove"), p, bob, Role::Developer)
        .await
        .expect("remove");
    let mut su = t.superuser().await;
    let stored: Vec<String> = sqlx::query_scalar(
        "SELECT event_type FROM forge.events WHERE event_type LIKE 'member.%' ORDER BY id",
    )
    .fetch_all(&mut su)
    .await
    .expect("events");
    assert_eq!(
        stored,
        ["member.added", "member.role_changed", "member.removed"]
    );
    for name in &stored {
        let t = EventType::parse(name).expect("a known type");
        assert_eq!(t.visibility_class(), VisibilityClass::Maintainer, "{name}");
    }
    // Group member changes stay audit-only (Q8): no event.
    let acme = group(&t, "acme", None, alice.user_id).await;
    groups::add_member(&t.db, &ctx("req-g-add"), acme.group_id, bob, Role::Guest)
        .await
        .expect("group add");
    assert!(events_of(&mut su, "req-g-add").await.is_empty());
    t.drop_now().await;
}

#[tokio::test]
async fn changing_a_project_member_to_the_same_role_writes_nothing() {
    let t = FreshDb::migrated().await;
    let (web, _, bob) = alice_web(&t).await;
    let (p, bob) = (web.project_id, bob.user_id);
    add_member(&t.db, &ctx("req-add"), p, bob, Role::Reporter)
        .await
        .expect("add");
    let same = change_member(
        &t.db,
        &ctx("req-same"),
        p,
        bob,
        Role::Reporter,
        Role::Reporter,
    )
    .await
    .expect("same role");
    assert_eq!(same, MemberChange::Unchanged);
    let mut su = t.superuser().await;
    assert!(audit_rows(&mut su, "req-same").await.is_empty());
    assert!(events_of(&mut su, "req-same").await.is_empty());
    t.drop_now().await;
}

#[tokio::test]
async fn a_bot_cannot_be_a_project_member() {
    let t = FreshDb::migrated().await;
    let (web, alice, bob) = alice_web(&t).await;
    let p = web.project_id;
    let bot = bot(&t).await;
    let err = add_member(&t.db, &ctx("req-bot-add"), p, bot, Role::Developer)
        .await
        .expect_err("bots are never members");
    assert_reason(&err, ErrorCode::MembershipConflict, "bot_member");
    assert_eq!(err.code().http_status(), 409);
    // The other conflicts, as for groups.
    let err = add_member(&t.db, &ctx("req-again"), p, alice.user_id, Role::Guest)
        .await
        .expect_err("alice is the owner");
    assert_reason(&err, ErrorCode::MembershipConflict, "already_member");
    let err = change_member(
        &t.db,
        &ctx("req-change"),
        p,
        bob.user_id,
        Role::Guest,
        Role::Guest,
    )
    .await
    .expect_err("bob is no member");
    assert_reason(&err, ErrorCode::MembershipConflict, "not_member");
    let err = remove_member(&t.db, &ctx("req-remove"), p, bob.user_id, Role::Guest)
        .await
        .expect_err("bob is no member");
    assert_reason(&err, ErrorCode::MembershipConflict, "not_member");
    let err = add_member(
        &t.db,
        &ctx("req-noproject"),
        ProjectId::new(9_999),
        bob.user_id,
        Role::Guest,
    )
    .await
    .expect_err("unknown project");
    assert_reason(&err, ErrorCode::NotFound, "absent");
    assert!(err.to_string().contains("resource=project"), "{err}");
    let err = add_member(
        &t.db,
        &ctx("req-nouser"),
        p,
        UserId::new(9_999),
        Role::Guest,
    )
    .await
    .expect_err("unknown user");
    assert_reason(&err, ErrorCode::NotFound, "absent");
    assert!(err.to_string().contains("resource=user"), "{err}");
    let mut su = t.superuser().await;
    for request in [
        "req-bot-add",
        "req-again",
        "req-change",
        "req-remove",
        "req-noproject",
        "req-nouser",
    ] {
        assert!(audit_rows(&mut su, request).await.is_empty(), "{request}");
        assert!(events_of(&mut su, request).await.is_empty(), "{request}");
    }
    t.drop_now().await;
}

#[tokio::test]
async fn removing_the_last_direct_owner_is_allowed_when_an_owner_is_inherited() {
    let t = FreshDb::migrated().await;
    // Personal namespace: alice is web's only direct Owner and inherits
    // Owner as the namespace owner, so she may leave the direct list.
    let (web, alice, bob) = alice_web(&t).await;
    remove_member(
        &t.db,
        &ctx("req-rm-alice"),
        web.project_id,
        alice.user_id,
        Role::Owner,
    )
    .await
    .expect("alice still inherits Owner");
    let roles = load_project_roles(&t.db, web.project_id, alice.user_id)
        .await
        .expect("roles");
    assert_eq!((roles.direct, roles.native), (None, Some(Role::Owner)));
    // Group: bob creates acme/api (its only direct Owner); acme's Owner
    // alice inherits, so bob may be demoted and removed.
    let acme = group(&t, "acme", None, alice.user_id).await;
    let api = create_ok(&t, &project("api", acme.namespace_id, bob.user_id)).await;
    let changed = change_member(
        &t.db,
        &ctx("req-demote-bob"),
        api.project_id,
        bob.user_id,
        Role::Owner,
        Role::Developer,
    )
    .await
    .expect("alice inherits Owner of acme/api");
    assert_eq!(changed, MemberChange::Changed { from: Role::Owner });
    remove_member(
        &t.db,
        &ctx("req-rm-bob"),
        api.project_id,
        bob.user_id,
        Role::Developer,
    )
    .await
    .expect("a non-owner leaves");
    let roles = load_project_roles(&t.db, api.project_id, alice.user_id)
        .await
        .expect("roles");
    assert_eq!(roles.native, Some(Role::Owner));
    t.drop_now().await;
}

#[tokio::test]
async fn removing_the_last_owner_without_inheritance_is_refused() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let carol = user(&t, "carol").await;
    // The operator (Actor::System) creates alice/web with bob as creator:
    // bob is its direct Owner and alice inherits Owner.
    let web = create_ok(&t, &project("web", alice.namespace_id, bob.user_id)).await;
    let p = web.project_id;
    // carol is a direct Owner too, but disabled: she does not count.
    add_member(&t.db, &ctx("req-add-carol"), p, carol.user_id, Role::Owner)
        .await
        .expect("carol owner");
    let disabled = users::disable(&t.db, &ctx("req-disable-carol"), carol.user_id)
        .await
        .expect("bob and alice remain");
    assert_eq!(disabled, DisableOutcome::Disabled);
    // alice (the inherited Owner) may be disabled: bob is a direct Owner.
    let disabled = users::disable(&t.db, &ctx("req-disable-alice"), alice.user_id)
        .await
        .expect("bob remains");
    assert_eq!(disabled, DisableOutcome::Disabled);
    for err in [
        remove_member(&t.db, &ctx("req-rm-bob"), p, bob.user_id, Role::Owner)
            .await
            .expect_err("bob is the last active owner"),
        change_member(
            &t.db,
            &ctx("req-demote-bob"),
            p,
            bob.user_id,
            Role::Owner,
            Role::Guest,
        )
        .await
        .expect_err("bob is the last active owner"),
    ] {
        assert_reason(&err, ErrorCode::MembershipConflict, "last_owner");
        assert!(
            err.to_string().contains(&format!("project_id={}", p.get())),
            "{err}"
        );
    }
    let mut su = t.superuser().await;
    assert_eq!(
        role_of(&mut su, p, bob.user_id).await.as_deref(),
        Some("owner")
    );
    for request in ["req-rm-bob", "req-demote-bob"] {
        assert!(audit_rows(&mut su, request).await.is_empty(), "{request}");
        assert!(events_of(&mut su, request).await.is_empty(), "{request}");
    }
    // Another active direct Owner lets bob go.
    let dave = user(&t, "dave").await;
    add_member(&t.db, &ctx("req-add-dave"), p, dave.user_id, Role::Owner)
        .await
        .expect("dave owner");
    remove_member(&t.db, &ctx("req-rm-bob-2"), p, bob.user_id, Role::Owner)
        .await
        .expect("dave remains");
    t.drop_now().await;
}

/// A stored role outside the five (changed outside forge-db) is
/// `FG-DB-0018` `unknown_role` with `resource=project`.
#[tokio::test]
async fn an_unknown_stored_project_role_is_membership_data_invalid() {
    let t = FreshDb::migrated().await;
    let (web, _, bob) = alice_web(&t).await;
    let (p, bob) = (web.project_id, bob.user_id);
    add_member(&t.db, &ctx("req-add"), p, bob, Role::Developer)
        .await
        .expect("add");
    // The constraint and the audit triggers are dropped / off for the tamper
    // only (this database is dropped).
    t.exec_superuser(&format!(
        "ALTER TABLE forge.project_members DROP CONSTRAINT project_members_role; \
         ALTER TABLE forge.project_members DISABLE TRIGGER USER; \
         UPDATE forge.project_members SET role = 'superuser' \
           WHERE project_id = {} AND user_id = {}; \
         ALTER TABLE forge.project_members ENABLE TRIGGER USER",
        p.get(),
        bob.get()
    ))
    .await;
    for err in [
        change_member(
            &t.db,
            &ctx("req-change"),
            p,
            bob,
            Role::Developer,
            Role::Guest,
        )
        .await
        .expect_err("change"),
        remove_member(&t.db, &ctx("req-remove"), p, bob, Role::Developer)
            .await
            .expect_err("remove"),
        add_member(&t.db, &ctx("req-add-again"), p, bob, Role::Guest)
            .await
            .expect_err("add"),
    ] {
        assert_reason(&err, ErrorCode::MembershipDataInvalid, "unknown_role");
        let text = err.to_string();
        assert!(text.contains("resource=project"), "{text}");
        assert!(text.contains("raw=superuser"), "{text}");
    }
    t.drop_now().await;
}

// ── The project half of the disable guard (ADR-0019 Amendment D4 MR4) ──

async fn token_for(t: &FreshDb, user: UserId, request: &str) {
    tokens::issue(
        &t.db,
        &ctx(request),
        &NewToken {
            user,
            name: "laptop".to_owned(),
            scopes: vec![Scope::ReadApi],
            expires_in: None,
            is_agent: false,
        },
    )
    .await
    .expect("issue");
}

async fn is_active(su: &mut PgConnection, user: UserId) -> bool {
    let state: String = sqlx::query_scalar("SELECT state FROM forge.users WHERE id = $1")
        .bind(user.get())
        .fetch_one(su)
        .await
        .expect("state");
    state == "active"
}

#[tokio::test]
async fn disabling_the_last_project_owner_is_refused_and_force_records_it() {
    let t = FreshDb::migrated().await;
    let (web, alice, bob) = alice_web(&t).await;
    let healthy = ownerless(&t.db).await.expect("read").expect("migrated");
    assert_eq!(healthy.count, 0);
    healthy.check().expect("every project has an owner");
    // alice is web's only Owner, direct and inherited.
    let err = users::disable(&t.db, &ctx("req-disable-alice"), alice.user_id)
        .await
        .expect_err("alice is web's last owner");
    assert_reason(&err, ErrorCode::AdministrationConflict, "last_owner");
    let text = err.to_string();
    assert!(
        text.contains(&format!("project_id={}", web.project_id.get())),
        "{text}"
    );
    assert!(text.contains("ownerless_projects=1"), "{text}");
    assert!(text.contains("ownerless_groups=0"), "{text}");
    let mut su = t.superuser().await;
    assert!(is_active(&mut su, alice.user_id).await);
    assert!(audit_rows(&mut su, "req-disable-alice").await.is_empty());

    // An inherited Owner elsewhere does not save web; a group Owner saves
    // the group's projects: alice owns acme with bob, and acme/api (alice
    // its direct Owner) keeps bob as inherited Owner.
    let acme = group(&t, "acme", None, alice.user_id).await;
    groups::add_member(
        &t.db,
        &ctx("req-g-bob"),
        acme.group_id,
        bob.user_id,
        Role::Owner,
    )
    .await
    .expect("bob owner of acme");
    let _api = create_ok(&t, &project("api", acme.namespace_id, alice.user_id)).await;

    let outcome = users::disable_with(&t.db, &ctx("req-force"), alice.user_id, Guard::Force)
        .await
        .expect("forced");
    assert_eq!(
        outcome,
        DisableOutcome::DisabledPastGuard {
            ownerless_groups: 0,
            ownerless_projects: 1,
            left_without_admin: false,
        }
    );
    let details: serde_json::Value =
        sqlx::query_scalar("SELECT details FROM forge.audit_log WHERE request_id = 'req-force'")
            .fetch_one(&mut su)
            .await
            .expect("details");
    assert_eq!(
        details,
        json!({
            "kind": "human",
            "revoked_tokens": 0,
            "revoked_sessions": 0,
            "forced": true,
            "ownerless_groups": 0,
            "ownerless_projects": 1,
            "left_without_admin": false,
        })
    );
    // What forge doctor will report (wired into users.administration).
    let found = ownerless(&t.db).await.expect("read").expect("migrated");
    assert_eq!(found.count, 1);
    assert_eq!(found.project_ids, vec![web.project_id]);
    let err = found.check().expect_err("ownerless");
    assert_reason(&err, ErrorCode::AdministrationOrphaned, "ownerless_project");
    let text = err.to_string();
    assert!(
        text.contains(&format!("project_ids={}", web.project_id.get())),
        "{text}"
    );
    // A project pending deletion is not counted.
    commit(
        &t,
        &[
            audit("project", web.project_id.get(), "succeeded"),
            format!(
                "UPDATE forge.projects SET deleted_at = now(), \
                 purge_after = now() + interval '30 days' WHERE id = {}",
                web.project_id.get()
            ),
        ],
    )
    .await
    .expect("mark pending deletion");
    let found = ownerless(&t.db).await.expect("read").expect("migrated");
    assert_eq!(found.count, 0);
    t.drop_now().await;
}

/// The project guard counts inherited Owners on both sides: disabling an
/// ancestor group's Owner is refused when that group's projects rely on
/// them alone — here a subgroup project whose top-level Owner was forced
/// out earlier.
#[tokio::test]
async fn disabling_the_only_inherited_owner_of_a_project_is_refused() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let team = group(&t, "team", Some(acme.namespace_id), alice.user_id).await;
    groups::add_member(
        &t.db,
        &ctx("req-g-bob"),
        team.group_id,
        bob.user_id,
        Role::Owner,
    )
    .await
    .expect("bob owner of team");
    // bob creates acme/team/api; then leaves its direct list (alice and he
    // inherit).
    let api = create_ok(&t, &project("api", team.namespace_id, bob.user_id)).await;
    remove_member(
        &t.db,
        &ctx("req-rm-bob"),
        api.project_id,
        bob.user_id,
        Role::Owner,
    )
    .await
    .expect("inherited owners remain");
    // alice is forced out (acme is then ownerless); bob, team's Owner, is
    // api's only Owner left.
    let _ = users::disable_with(&t.db, &ctx("req-force-alice"), alice.user_id, Guard::Force)
        .await
        .expect("forced");
    let err = users::disable(&t.db, &ctx("req-disable-bob"), bob.user_id)
        .await
        .expect_err("bob is api's last owner");
    assert_reason(&err, ErrorCode::AdministrationConflict, "last_owner");
    assert!(
        err.to_string()
            .contains(&format!("project_id={}", api.project_id.get())),
        "{err}"
    );
    t.drop_now().await;
}

// ── Pinned races: a project member removal against a disable ───────────
//
// One interleaving per test, real commands on separate connections: a
// blocker transaction (superuser) holds a row lock that one command needs
// late in its change, so that command stays open holding its own locks
// while the other runs (the style of `users/tests/race.rs`).

const DISABLE_ROW_WAIT: &str = "is_admin FROM forge.users WHERE id = $1 FOR UPDATE";
const DISABLE_TOKENS_WAIT: &str = "UPDATE forge.tokens SET revoked_at";
const OWNER_SHARE_WAIT: &str = "FOR SHARE OF u";
const MEMBER_DELETE_WAIT: &str = "DELETE FROM forge.project_members";

/// Wait (bounded) until a backend of this test's database runs a statement
/// containing `statement` and waits on a lock.
async fn until_waiting(t: &FreshDb, statement: &str) {
    let mut su = t.superuser().await;
    for _ in 0..500 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity WHERE wait_event_type = 'Lock' \
             AND datname = current_database() AND strpos(query, $1) > 0",
        )
        .bind(statement)
        .fetch_one(&mut su)
        .await
        .expect("pg_stat_activity");
        if waiting > 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no statement containing {statement:?} waited on a lock within 10 s");
}

/// A superuser transaction holding `lock_sql` until it is rolled back.
async fn blocker(t: &FreshDb, lock_sql: &str) -> PgConnection {
    let mut su = t.superuser().await;
    sqlx::raw_sql(&format!("BEGIN; {lock_sql}"))
        .execute(&mut su)
        .await
        .unwrap_or_else(|e| panic!("{lock_sql}: {e}"));
    su
}

async fn release(mut blocker: PgConnection) {
    sqlx::raw_sql("ROLLBACK")
        .execute(&mut blocker)
        .await
        .expect("rollback the blocker");
}

/// `alice/web` with alice (direct and inherited) and bob (direct) as its
/// Owners; alice holds a token (its row keeps a disable open).
async fn two_project_owners(t: &FreshDb) -> (ProjectId, UserId, UserId) {
    let (web, alice, bob) = alice_web(t).await;
    add_member(
        &t.db,
        &ctx("req-add-bob"),
        web.project_id,
        bob.user_id,
        Role::Owner,
    )
    .await
    .expect("bob owner");
    token_for(t, alice.user_id, "req-token-alice").await;
    (web.project_id, alice.user_id, bob.user_id)
}

/// Pinned, removal first: the removal of bob holds the project row and
/// alice's user row `FOR SHARE` and is kept open before its DELETE by a
/// blocker on bob's membership row. The disable of alice must wait for her
/// row, then see bob gone and refuse (`last_owner`). Without the
/// `FOR SHARE OF u` of the remaining Owners the disable would not wait,
/// would still count bob, and both would commit, leaving web ownerless.
#[tokio::test]
async fn a_disable_waits_for_a_project_owner_removal_and_then_refuses() {
    let t = FreshDb::migrated().await;
    let (p, alice, bob) = two_project_owners(&t).await;
    let hold = blocker(
        &t,
        &format!(
            "SELECT 1 FROM forge.project_members WHERE project_id = {} AND user_id = {} \
             FOR UPDATE",
            p.get(),
            bob.get()
        ),
    )
    .await;
    let (rm_ctx, dis_ctx) = (ctx("req-rm-bob"), ctx("req-disable-alice"));
    let removing = remove_member(&t.db, &rm_ctx, p, bob, Role::Owner);
    let disabling = async {
        until_waiting(&t, MEMBER_DELETE_WAIT).await;
        users::disable(&t.db, &dis_ctx, alice).await
    };
    let releasing = async {
        until_waiting(&t, MEMBER_DELETE_WAIT).await;
        until_waiting(&t, DISABLE_ROW_WAIT).await;
        release(hold).await;
    };
    let (removed, disabled, ()) = tokio::join!(removing, disabling, releasing);
    removed.expect("alice is an active owner while bob leaves");
    let err = disabled.expect_err("bob is gone: alice is the last owner");
    assert_reason(&err, ErrorCode::AdministrationConflict, "last_owner");
    let mut su = t.superuser().await;
    assert!(is_active(&mut su, alice).await, "web keeps an owner");
    t.drop_now().await;
}

/// Pinned, disable first: the disable of alice has locked and updated her
/// row and is kept open before its token revoke by a blocker on her token.
/// The removal of bob must wait for alice's row (the remaining Owners are
/// locked `FOR SHARE`), then re-check the committed row, no longer count
/// alice (`state = 'active'`) and refuse (FG-REPO-0010 `last_owner`).
#[tokio::test]
async fn a_project_owner_removal_waits_for_a_disable_and_then_refuses() {
    let t = FreshDb::migrated().await;
    let (p, alice, bob) = two_project_owners(&t).await;
    let hold = blocker(
        &t,
        &format!(
            "SELECT 1 FROM forge.tokens WHERE user_id = {} FOR UPDATE",
            alice.get()
        ),
    )
    .await;
    let (dis_ctx, rm_ctx) = (ctx("req-disable-alice"), ctx("req-rm-bob"));
    let disabling = users::disable(&t.db, &dis_ctx, alice);
    let removing = async {
        until_waiting(&t, DISABLE_TOKENS_WAIT).await;
        remove_member(&t.db, &rm_ctx, p, bob, Role::Owner).await
    };
    let releasing = async {
        until_waiting(&t, DISABLE_TOKENS_WAIT).await;
        until_waiting(&t, OWNER_SHARE_WAIT).await;
        release(hold).await;
    };
    let (disabled, removed, ()) = tokio::join!(disabling, removing, releasing);
    assert_eq!(
        disabled.expect("bob still owns web"),
        DisableOutcome::Disabled
    );
    let err = removed.expect_err("alice is disabled: bob is the last owner");
    assert_reason(&err, ErrorCode::MembershipConflict, "last_owner");
    let mut su = t.superuser().await;
    assert_eq!(role_of(&mut su, p, bob).await.as_deref(), Some("owner"));
    t.drop_now().await;
}

/// The role a caller authorized against is a precondition (D4 review): a
/// promotion to Owner committed between `authorize()` (which saw
/// Developer) and the demotion or removal makes both `role_changed`.
#[tokio::test]
async fn a_project_role_changed_after_authorization_is_role_changed() {
    let t = FreshDb::migrated().await;
    let (web, _, bob) = alice_web(&t).await;
    let (p, bob) = (web.project_id, bob.user_id);
    add_member(&t.db, &ctx("req-add"), p, bob, Role::Developer)
        .await
        .expect("add");
    let authorized = Role::Developer;
    let _ = change_member(
        &t.db,
        &ctx("req-promote"),
        p,
        bob,
        Role::Developer,
        Role::Owner,
    )
    .await
    .expect("promote");
    for err in [
        change_member(&t.db, &ctx("req-demote"), p, bob, authorized, Role::Guest)
            .await
            .expect_err("demote"),
        remove_member(&t.db, &ctx("req-remove"), p, bob, authorized)
            .await
            .expect_err("remove"),
    ] {
        assert_reason(&err, ErrorCode::MembershipConflict, "role_changed");
        assert!(err.to_string().contains("actual=owner"), "{err}");
    }
    let mut su = t.superuser().await;
    assert_eq!(role_of(&mut su, p, bob).await.as_deref(), Some("owner"));
    for request in ["req-demote", "req-remove"] {
        assert!(audit_rows(&mut su, request).await.is_empty(), "{request}");
        assert!(events_of(&mut su, request).await.is_empty(), "{request}");
    }
    t.drop_now().await;
}

const PROJECT_LOCK_WAIT: &str = "FOR UPDATE OF p";

/// Pinned: the last two direct Owners (no inheritance: the personal
/// namespace's owner is disabled) are removed at once. The removal of bob
/// holds the project row and is kept open before its DELETE by a blocker on
/// bob's membership row; the removal of carol must wait on the project row
/// (`FOR UPDATE OF p`), then see bob gone and refuse (`last_owner`).
/// Mutation-checked: without `FOR UPDATE OF p` in `lock_project` nobody
/// waits on the project row (this test panics in `until_waiting`), and
/// unpinned both removals could commit, leaving web without an Owner.
#[tokio::test]
async fn removing_the_last_two_direct_owners_at_once_keeps_one() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let carol = user(&t, "carol").await;
    let web = create_ok(&t, &project("web", alice.namespace_id, bob.user_id)).await;
    let p = web.project_id;
    add_member(&t.db, &ctx("req-add-carol"), p, carol.user_id, Role::Owner)
        .await
        .expect("carol owner");
    let disabled = users::disable(&t.db, &ctx("req-disable-alice"), alice.user_id)
        .await
        .expect("bob and carol remain");
    assert_eq!(disabled, DisableOutcome::Disabled);
    let (bob, carol) = (bob.user_id, carol.user_id);
    let hold = blocker(
        &t,
        &format!(
            "SELECT 1 FROM forge.project_members WHERE project_id = {} AND user_id = {} \
             FOR UPDATE",
            p.get(),
            bob.get()
        ),
    )
    .await;
    let (ctx_b, ctx_c) = (ctx("req-rm-bob"), ctx("req-rm-carol"));
    let removing_bob = remove_member(&t.db, &ctx_b, p, bob, Role::Owner);
    let removing_carol = async {
        until_waiting(&t, MEMBER_DELETE_WAIT).await;
        remove_member(&t.db, &ctx_c, p, carol, Role::Owner).await
    };
    let releasing = async {
        until_waiting(&t, MEMBER_DELETE_WAIT).await;
        until_waiting(&t, PROJECT_LOCK_WAIT).await;
        release(hold).await;
    };
    let (b, c, ()) = tokio::join!(removing_bob, removing_carol, releasing);
    b.expect("carol is an owner while bob leaves");
    let err = c.expect_err("bob is gone: carol is the last owner");
    assert_reason(&err, ErrorCode::MembershipConflict, "last_owner");
    let mut su = t.superuser().await;
    assert_eq!(role_of(&mut su, p, carol).await.as_deref(), Some("owner"));
    t.drop_now().await;
}

/// The ownerless read stays set-based with hundreds of projects (D4
/// review): the right count and ids, and no per-row subplan in either
/// query's plan.
#[tokio::test]
async fn ownerless_reads_hundreds_of_projects_set_based() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let acme = group(&t, "acme", None, bob.user_id).await;
    let mut mine = Vec::new();
    for n in 0..300 {
        // Two thirds in alice's namespace (she is their only Owner), the
        // rest in acme (bob inherits).
        let parent = if n % 3 == 2 {
            acme.namespace_id
        } else {
            alice.namespace_id
        };
        let created = create_ok(&t, &project(&format!("p{n}"), parent, alice.user_id)).await;
        if n % 3 != 2 {
            mine.push(created.project_id);
        }
    }
    let err = users::disable(&t.db, &ctx("req-disable-alice"), alice.user_id)
        .await
        .expect_err("alice is the last owner of 200 projects");
    assert_reason(&err, ErrorCode::AdministrationConflict, "last_owner");
    let text = err.to_string();
    assert!(text.contains("ownerless_projects=200"), "{text}");
    assert!(
        text.contains(&format!("project_id={}", mine[0].get())),
        "{text}"
    );
    let _ = users::disable_with(&t.db, &ctx("req-force"), alice.user_id, Guard::Force)
        .await
        .expect("forced");
    let found = ownerless(&t.db).await.expect("read").expect("migrated");
    assert_eq!(found.count, 200);
    assert_eq!(found.project_ids, mine[..10].to_vec());
    let mut su = t.superuser().await;
    for (name, sql, binds) in [
        (
            "AFTER_DISABLE",
            ownership::AFTER_DISABLE,
            [bob.user_id.get(), bob.user_id.get(), 20, 11],
        ),
        ("EVERY", ownership::EVERY, [10, 0, 20, 0]),
    ] {
        let explain = format!("EXPLAIN {sql}");
        let mut q = sqlx::query_scalar::<_, String>(&explain);
        let used = if name == "EVERY" { 3 } else { 4 };
        for (i, v) in binds.iter().enumerate().take(used) {
            q = if i == 2 {
                q.bind(i32::try_from(*v).expect("cap"))
            } else {
                q.bind(*v)
            };
        }
        let plan: Vec<String> = q.fetch_all(&mut su).await.expect("explain");
        let plan = plan.join("\n");
        assert!(
            !plan.contains("SubPlan"),
            "{name}: a per-row subplan:\n{plan}"
        );
    }
    t.drop_now().await;
}
