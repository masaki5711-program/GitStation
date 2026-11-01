//! The project commands against a migrated database. Inspection uses a
//! superuser connection.

use forge_core::{ProjectAccess, RequestId, Role, UserKind};
use forge_errors::ErrorCode;
use serde_json::json;
use sqlx::postgres::PgConnection;

use super::*;
use crate::audited_tests::{audit, commit};
use crate::change::Actor;
use crate::groups::{self, CreatedGroup, NewGroup};
use crate::resolve::project_access;
use crate::test_support::*;
use crate::users::{self, NewUser};

mod delete;
mod member_writes;
mod members;
mod purge;
mod race;
mod update;

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

async fn count(su: &mut PgConnection, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(su)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// Every row a create writes, counted: projects, project namespaces,
/// repositories, project members, `project.created` audit rows, events and
/// jobs. A refused create leaves all of them at zero.
async fn written(su: &mut PgConnection) -> [i64; 7] {
    [
        count(su, "SELECT count(*) FROM forge.projects").await,
        count(
            su,
            "SELECT count(*) FROM forge.namespaces WHERE kind = 'project'",
        )
        .await,
        count(su, "SELECT count(*) FROM forge.repositories").await,
        count(su, "SELECT count(*) FROM forge.project_members").await,
        count(
            su,
            "SELECT count(*) FROM forge.audit_log WHERE action = 'project.created'",
        )
        .await,
        count(su, "SELECT count(*) FROM forge.events").await,
        count(su, "SELECT count(*) FROM forge.jobs").await,
    ]
}

/// An active human named `name`, with their personal namespace.
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
    groups::create(
        &t.db,
        &ctx(&format!("req-group-{}", &segment[..segment.len().min(24)])),
        &new,
    )
    .await
    .unwrap_or_else(|e| panic!("{segment}: {e}"))
}

fn project(segment: &str, parent: NamespaceId, creator: UserId) -> NewProject {
    NewProject::new(segment, parent, &format!("Project {segment}"), creator)
}

async fn create_ok(t: &FreshDb, new: &NewProject) -> CreatedProject {
    create(
        &t.db,
        &ctx(&format!(
            "req-project-{}",
            new.segment.chars().take(24).collect::<String>()
        )),
        new,
    )
    .await
    .unwrap_or_else(|e| panic!("{}: {e}", new.segment))
}

/// The three rows of one project, read together (ADR-0015 Amendment D4
/// MR1: the 1:1:1 is not a database constraint, the create keeps it).
type Trio = (String, i64, i64, String, String, String, String, bool);

async fn trio(su: &mut PgConnection, project: ProjectId) -> Option<Trio> {
    sqlx::query_as(
        "SELECT n.full_path, n.depth::bigint, r.id, r.kind, r.state, p.visibility, \
           p.display_name, r.ready_at IS NULL \
         FROM forge.projects p \
         JOIN forge.namespaces n ON n.kind = 'project' AND n.owner_id = p.id \
         JOIN forge.repositories r ON r.project_id = p.id \
         WHERE p.id = $1",
    )
    .bind(project.get())
    .fetch_optional(su)
    .await
    .expect("trio")
}

#[tokio::test]
async fn project_create_writes_namespace_project_repository_owner_job_and_one_audit_row() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let created = create(
        &t.db,
        &ctx("req-web"),
        &project("web", alice.namespace_id, alice.user_id),
    )
    .await
    .expect("create alice/web");
    let mut su = t.superuser().await;
    // Project, namespace and repository exist together.
    assert_eq!(
        trio(&mut su, created.project_id).await,
        Some((
            "alice/web".into(),
            2,
            created.repository_id.get(),
            "system".into(),
            "provisioning".into(),
            "private".into(),
            "Project web".into(),
            true
        ))
    );
    let ns: (i64, String, i64) = sqlx::query_as(
        "SELECT owner_id, segment, parent_id FROM forge.namespaces WHERE id = $1 \
         AND kind = 'project'",
    )
    .bind(created.namespace_id.get())
    .fetch_one(&mut su)
    .await
    .expect("namespace");
    assert_eq!(
        ns,
        (
            created.project_id.get(),
            "web".into(),
            alice.namespace_id.get()
        )
    );
    // The creator is the direct Owner.
    let members: Vec<(i64, String)> =
        sqlx::query_as("SELECT user_id, role FROM forge.project_members WHERE project_id = $1")
            .bind(created.project_id.get())
            .fetch_all(&mut su)
            .await
            .expect("members");
    assert_eq!(members, vec![(alice.user_id.get(), "owner".to_owned())]);
    // One audit row on the project, with its project_id.
    let audit_rows: Vec<(String, String, i64, Option<i64>, serde_json::Value)> = sqlx::query_as(
        "SELECT action, resource_type, resource_id, project_id, details \
             FROM forge.audit_log WHERE request_id = 'req-web'",
    )
    .fetch_all(&mut su)
    .await
    .expect("audit");
    let details = json!({
        "project_id": created.project_id.get(),
        "namespace_id": created.namespace_id.get(),
        "parent_namespace_id": alice.namespace_id.get(),
        "repository_id": created.repository_id.get(),
        "kind": "system",
        "visibility": "private",
        "owner_id": alice.user_id.get(),
    });
    assert_eq!(
        audit_rows,
        vec![(
            "project.created".to_owned(),
            "project".to_owned(),
            created.project_id.get(),
            Some(created.project_id.get()),
            details.clone()
        )]
    );
    // One integration event of the project; its payload is the details.
    let events: Vec<(String, String, i64, Option<i64>, serde_json::Value)> = sqlx::query_as(
        "SELECT event_type, aggregate_type, aggregate_id, project_id, payload \
         FROM forge.events WHERE request_id = 'req-web'",
    )
    .fetch_all(&mut su)
    .await
    .expect("events");
    assert_eq!(
        events,
        vec![(
            "project.created".to_owned(),
            "project".to_owned(),
            created.project_id.get(),
            Some(created.project_id.get()),
            details
        )]
    );
    // The provisioning job, deduplicated on the repository.
    let jobs: Vec<(
        i64,
        String,
        serde_json::Value,
        Option<String>,
        String,
        String,
    )> = sqlx::query_as(
        "SELECT id, kind, payload, dedup_key, origin_request_id, state FROM forge.jobs",
    )
    .fetch_all(&mut su)
    .await
    .expect("jobs");
    assert_eq!(
        jobs,
        vec![(
            created.provision_job.get(),
            "repository.provision".to_owned(),
            json!({ "repo_id": created.repository_id.get() }),
            Some(format!(
                "repository_provision:{}",
                created.repository_id.get()
            )),
            "req-web".to_owned(),
            "queued".to_owned()
        )]
    );
    // All of it in one transaction.
    let txids = count(
        &mut su,
        "SELECT count(DISTINCT txid) FROM (SELECT txid FROM forge.audit_log \
         WHERE request_id = 'req-web' UNION ALL SELECT txid FROM forge.events \
         WHERE request_id = 'req-web') t",
    )
    .await;
    assert_eq!(txids, 1);
    assert_eq!(written(&mut su).await, [1, 1, 1, 1, 1, 1, 1]);
    let access = project_access(&t.db, created.project_id, None)
        .await
        .expect("access");
    assert_eq!(
        access,
        Some(ProjectAccess {
            visibility: Visibility::Private,
            role: None,
            pending_deletion: false
        })
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_project_under_a_group_keeps_its_visibility_and_stores_the_constant_kind() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let api = group(&t, "api", Some(acme.namespace_id), alice.user_id).await;
    let new = NewProject {
        visibility: Visibility::Internal,
        description: "specifications".to_owned(),
        ..project("specs", api.namespace_id, alice.user_id)
    };
    let created = create_ok(&t, &new).await;
    let mut su = t.superuser().await;
    let row = trio(&mut su, created.project_id).await.expect("trio");
    assert_eq!(
        (row.0.as_str(), row.1, row.3.as_str(), row.5.as_str()),
        ("acme/api/specs", 3, "system", "internal")
    );
    assert_eq!(
        project_access(&t.db, created.project_id, Some(alice.user_id))
            .await
            .expect("access"),
        Some(ProjectAccess {
            visibility: Visibility::Internal,
            role: Some(Role::Owner),
            pending_deletion: false
        })
    );
    t.drop_now().await;
}

#[tokio::test]
async fn a_project_cannot_be_created_under_a_project() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let web = create_ok(&t, &project("web", alice.namespace_id, alice.user_id)).await;
    let mut su = t.superuser().await;
    let before = written(&mut su).await;
    let err = create(
        &t.db,
        &ctx("req-nested"),
        &project("inner", web.namespace_id, alice.user_id),
    )
    .await
    .expect_err("projects never nest");
    assert_reason(
        &err,
        ErrorCode::ProjectRequestInvalid,
        "parent_not_owner_namespace",
    );
    assert_eq!(err.code().http_status(), 400);
    assert_eq!(written(&mut su).await, before);
    t.drop_now().await;
}

#[tokio::test]
async fn a_bot_cannot_create_or_own_a_project() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
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
    .expect("bot")
    .user_id;
    // As creator (and so first Owner): the same answer as for groups.
    let err = create(
        &t.db,
        &ctx("req-bot-web"),
        &project("web", alice.namespace_id, bot),
    )
    .await
    .expect_err("bot creator");
    assert_reason(&err, ErrorCode::MembershipConflict, "bot_member");
    // A writer that skips forge-db cannot make it an Owner either.
    let web = create_ok(&t, &project("web", alice.namespace_id, alice.user_id)).await;
    let err = commit(
        &t,
        &[
            audit("project", web.project_id.get(), "succeeded"),
            format!(
                "INSERT INTO forge.project_members (project_id, user_id, role) \
                 VALUES ({}, {}, 'owner')",
                web.project_id.get(),
                bot.get()
            ),
        ],
    )
    .await
    .expect_err("trigger");
    assert!(
        chain(&err).contains("project_members_no_bot"),
        "{}",
        chain(&err)
    );
    // In a bot's personal namespace (every user kind has one): refused, so
    // the bot never inherits Owner of a project there.
    let bot_ns: i64 =
        sqlx::query_scalar("SELECT id FROM forge.namespaces WHERE kind = 'user' AND owner_id = $1")
            .bind(bot.get())
            .fetch_one(&mut t.superuser().await)
            .await
            .expect("bot namespace");
    let err = create(
        &t.db,
        &ctx("req-in-bot"),
        &project("tools", NamespaceId::new(bot_ns), alice.user_id),
    )
    .await
    .expect_err("bot namespace");
    assert_reason(&err, ErrorCode::MembershipConflict, "bot_member");
    let mut su = t.superuser().await;
    assert_eq!(written(&mut su).await, [1, 1, 1, 1, 1, 1, 1]);
    t.drop_now().await;
}

#[tokio::test]
async fn a_disabled_users_personal_namespace_takes_no_project() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    assert_eq!(
        users::disable(&t.db, &ctx("req-disable"), bob.user_id)
            .await
            .expect("disable"),
        users::DisableOutcome::Disabled
    );
    let err = create(
        &t.db,
        &ctx("req-in-bob"),
        &project("web", bob.namespace_id, alice.user_id),
    )
    .await
    .expect_err("disabled owner");
    assert_reason(
        &err,
        ErrorCode::ProjectRequestInvalid,
        "parent_owner_inactive",
    );
    let mut su = t.superuser().await;
    assert_eq!(written(&mut su).await, [0; 7]);
    t.drop_now().await;
}

/// The creator is the acting user (or the operator, `Actor::System`).
#[tokio::test]
async fn the_creator_must_be_the_acting_user() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let as_actor = |actor: Actor, request: &str| {
        ChangeContext::new(actor, RequestId::parse(request).expect("request id"))
    };
    let new = project("web", alice.namespace_id, alice.user_id);
    let via = Actor::UserViaServiceAccount {
        user: bob.user_id,
        via: alice.user_id,
        credential_id: 1,
        act_as_claimed: "hmac".to_owned(),
        act_as_key_id: 1,
        via_assertion_id: None,
    };
    for (actor, name) in [
        (Actor::User(bob.user_id), "user"),
        (Actor::ServiceAccount(bob.user_id), "service_account"),
        (via, "via"),
        (Actor::Anonymous, "anonymous"),
    ] {
        let err = create(&t.db, &as_actor(actor, &format!("req-{name}")), &new)
            .await
            .expect_err(name);
        assert_reason(&err, ErrorCode::ProjectRequestInvalid, "creator_not_actor");
    }
    let mut su = t.superuser().await;
    assert_eq!(written(&mut su).await, [0; 7]);
    // The acting user themselves, and a person acting through kanflow.
    create(
        &t.db,
        &as_actor(Actor::User(alice.user_id), "req-self"),
        &new,
    )
    .await
    .expect("alice creates her own");
    let for_bob = Actor::UserViaServiceAccount {
        user: bob.user_id,
        via: alice.user_id,
        credential_id: 1,
        act_as_claimed: "hmac".to_owned(),
        act_as_key_id: 1,
        via_assertion_id: None,
    };
    create(
        &t.db,
        &as_actor(for_bob, "req-for-bob"),
        &project("api", bob.namespace_id, bob.user_id),
    )
    .await
    .expect("bob through a service account");
    t.drop_now().await;
}

/// A job already holding the new repository's dedup key is an
/// inconsistency, never absorbed: the whole create fails.
#[tokio::test]
async fn a_held_provisioning_dedup_key_fails_the_create() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    // The fresh database gives the first repository id 1.
    let job = crate::repositories::provision_job(
        RepoId::new(1),
        &RequestId::parse("req-held").expect("request id"),
    )
    .expect("job");
    crate::jobs::enqueue(&t.db, &job).await.expect("held key");
    let err = create(
        &t.db,
        &ctx("req-web"),
        &project("web", alice.namespace_id, alice.user_id),
    )
    .await
    .expect_err("deduped job");
    assert_reason(&err, ErrorCode::Internal, "unexpected_job_dedup");
    let mut su = t.superuser().await;
    assert_eq!(written(&mut su).await, [0, 0, 0, 0, 0, 0, 1]);
    t.drop_now().await;
}

#[tokio::test]
async fn an_invalid_project_request_is_project_request_invalid() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let ns = alice.namespace_id;
    let me = alice.user_id;
    let cases = [
        (
            NewProject {
                display_name: String::new(),
                ..project("p1", ns, me)
            },
            "display_name_length",
        ),
        (
            NewProject {
                display_name: "x".repeat(256),
                ..project("p2", ns, me)
            },
            "display_name_length",
        ),
        (
            NewProject {
                description: "x".repeat(2001),
                ..project("p3", ns, me)
            },
            "description_length",
        ),
        (
            NewProject {
                description: "a\0b".into(),
                ..project("p4", ns, me)
            },
            "nul_character",
        ),
    ];
    for (new, reason) in cases {
        let err = create(&t.db, &ctx(&format!("req-{}", new.segment)), &new)
            .await
            .expect_err(reason);
        assert_reason(&err, ErrorCode::ProjectRequestInvalid, reason);
    }
    // 255 and 2000 characters are fine (the CHECKs count characters).
    create_ok(
        &t,
        &NewProject {
            display_name: "é".repeat(255),
            description: "é".repeat(2000),
            ..project("edge", ns, me)
        },
    )
    .await;
    // A bad segment is the path code.
    let err = create(&t.db, &ctx("req-bad"), &project("Bad", ns, me))
        .await
        .expect_err("uppercase");
    assert_eq!(err.code(), ErrorCode::NamespacePathInvalid, "{err}");
    // Unknown parent and creator.
    let err = create(
        &t.db,
        &ctx("req-p5"),
        &project("p5", NamespaceId::new(9_999), me),
    )
    .await
    .expect_err("unknown parent");
    assert_reason(&err, ErrorCode::NotFound, "absent");
    assert!(err.to_string().contains("resource=namespace"), "{err}");
    let err = create(
        &t.db,
        &ctx("req-p6"),
        &project("p6", ns, UserId::new(9_999)),
    )
    .await
    .expect_err("unknown creator");
    assert_reason(&err, ErrorCode::NotFound, "absent");
    assert!(err.to_string().contains("resource=user"), "{err}");
    // An inactive creator.
    let bob = user(&t, "bob").await;
    assert_eq!(
        users::disable(&t.db, &ctx("req-disable"), bob.user_id)
            .await
            .expect("disable"),
        users::DisableOutcome::Disabled
    );
    let err = create(&t.db, &ctx("req-p7"), &project("p7", ns, bob.user_id))
        .await
        .expect_err("inactive creator");
    assert_reason(&err, ErrorCode::ProjectRequestInvalid, "creator_inactive");
    let mut su = t.superuser().await;
    assert_eq!(written(&mut su).await, [1, 1, 1, 1, 1, 1, 1], "only `edge`");
    t.drop_now().await;
}

#[tokio::test]
async fn project_paths_obey_depth_and_length_limits() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    // A project below a group at depth 19 is at depth 20; at depth 20, 21.
    let mut parent = None;
    let mut groups_made = Vec::new();
    for level in 1..=20 {
        let g = group(&t, &format!("l{level}"), parent, alice.user_id).await;
        parent = Some(g.namespace_id);
        groups_made.push(g);
    }
    let at_19 = groups_made[18].namespace_id;
    let deepest = create_ok(&t, &project("p", at_19, alice.user_id)).await;
    let mut su = t.superuser().await;
    assert_eq!(trio(&mut su, deepest.project_id).await.expect("trio").1, 20);
    let at_20 = groups_made[19].namespace_id;
    let err = create(&t.db, &ctx("req-deep"), &project("p", at_20, alice.user_id))
        .await
        .expect_err("depth 21");
    assert_reason(&err, ErrorCode::NamespacePathInvalid, "too_deep");
    // A full path over 255 characters: 3 × 64 + 2 slashes, then 64 more.
    let long = "a".repeat(64);
    let mut parent = None;
    for level in 0..3 {
        let g = group(&t, &format!("{long}{level}")[1..], parent, alice.user_id).await;
        parent = Some(g.namespace_id);
    }
    let parent = parent.expect("three groups");
    let err = create(
        &t.db,
        &ctx("req-long"),
        &project(&long, parent, alice.user_id),
    )
    .await
    .expect_err("path over 255");
    assert_reason(&err, ErrorCode::NamespacePathInvalid, "path_too_long");
    assert!(err.to_string().contains("len=259"), "{err}");
    t.drop_now().await;
}

#[tokio::test]
async fn a_project_path_taken_or_tombstoned_is_rejected() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    create_ok(&t, &project("web", alice.namespace_id, alice.user_id)).await;
    // A subgroup and a project share the sibling space of their parent.
    group(&t, "team", Some(acme.namespace_id), alice.user_id).await;
    commit(
        &t,
        &[
            audit("project", 999, "succeeded"),
            "INSERT INTO forge.namespace_tombstones (old_path, kind, cause_type, cause_id) \
             VALUES ('alice/ghost', 'deleted', 'project', 999)"
                .to_owned(),
        ],
    )
    .await
    .expect("seed tombstone");
    let mut su = t.superuser().await;
    let before = written(&mut su).await;
    let cases = [
        ("web", alice.namespace_id, "sibling"),
        ("team", acme.namespace_id, "sibling"),
        ("ghost", alice.namespace_id, "tombstoned"),
    ];
    for (name, parent, reason) in cases {
        let err = create(
            &t.db,
            &ctx(&format!("req-{name}")),
            &project(name, parent, alice.user_id),
        )
        .await
        .expect_err(name);
        assert_reason(&err, ErrorCode::NamespacePathTaken, reason);
        assert!(!chain(&err).contains(name), "{name}: {}", chain(&err));
    }
    assert_eq!(written(&mut su).await, before);
    t.drop_now().await;
}

/// Two creates of one path at once: the path lock serializes them, one wins
/// and the loser leaves nothing behind (no project, repository or job).
#[tokio::test]
async fn two_concurrent_creates_of_one_project_path_make_one_project() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let (ctx_a, ctx_b) = (ctx("req-a"), ctx("req-b"));
    let new_a = project("web", alice.namespace_id, alice.user_id);
    let new_b = project("web", alice.namespace_id, alice.user_id);
    let (a, b) = tokio::join!(create(&t.db, &ctx_a, &new_a), create(&t.db, &ctx_b, &new_b));
    let errs: Vec<_> = [a, b].into_iter().filter_map(Result::err).collect();
    assert_eq!(errs.len(), 1, "exactly one create fails: {errs:?}");
    assert_reason(&errs[0], ErrorCode::NamespacePathTaken, "sibling");
    let mut su = t.superuser().await;
    assert_eq!(written(&mut su).await, [1, 1, 1, 1, 1, 1, 1]);
    t.drop_now().await;
}

#[tokio::test]
async fn access_reports_visibility_pending_deletion_and_absence() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let web = create_ok(&t, &project("web", alice.namespace_id, alice.user_id)).await;
    commit(
        &t,
        &[
            audit("project", web.project_id.get(), "succeeded"),
            format!(
                "UPDATE forge.projects SET deleted_at = now(), \
                 purge_after = now() + interval '7 days' WHERE id = {}",
                web.project_id.get()
            ),
        ],
    )
    .await
    .expect("mark deleted");
    assert_eq!(
        project_access(&t.db, web.project_id, None)
            .await
            .expect("access"),
        Some(ProjectAccess {
            visibility: Visibility::Private,
            role: None,
            pending_deletion: true
        })
    );
    // Absent is None, not an error (ProjectAccessLoader).
    assert_eq!(
        project_access(&t.db, ProjectId::new(9_999), None)
            .await
            .expect("absent"),
        None
    );
    t.drop_now().await;
}
