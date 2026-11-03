//! Integrations and bindings against a migrated database (M3 I1a): the
//! schema's backstops by constraint name (raw statements as superuser,
//! rolled back: the deferred audit trigger would refuse any raw INSERT at
//! COMMIT, so the name of the violated constraint is what proves the
//! rule), and the commands with a `decide` that allows (the decision
//! itself is forge-hosting's and the matrix's). Synthetic data only.

use forge_core::ids::{ProjectId, UserId};
use forge_core::{RequestId, Role};
use forge_errors::ErrorCode;
use sqlx::postgres::PgConnection;
use sqlx::Connection as _;

use super::*;
use crate::change::Actor;
use crate::projects::{self, NewProject};
use crate::service_accounts::{self, NewServiceAccount};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

async fn human(t: &FreshDb, name: &str) -> users::Created {
    users::create(
        &t.db,
        &ctx(&format!("req-user-{name}")),
        &NewUser::human(name, name, &format!("{name}@example.test")),
    )
    .await
    .expect("create user")
}

async fn service_account(t: &FreshDb, username: &str, owner: UserId) -> UserId {
    service_accounts::create(
        &t.db,
        &ctx(&format!("req-sa-{username}")),
        &NewServiceAccount {
            username: username.to_owned(),
            display_name: username.to_owned(),
            owner,
            purpose: "Kanflow reads the event feed of bound projects".to_owned(),
        },
        owner,
    )
    .await
    .expect("service account")
    .user_id
}

async fn project(t: &FreshDb, owner: &users::Created, segment: &str) -> ProjectId {
    projects::create(
        &t.db,
        &ctx(&format!("req-p-{segment}")),
        &NewProject::new(segment, owner.namespace_id, segment, owner.user_id),
    )
    .await
    .expect("project")
    .project_id
}

async fn kanflow(t: &FreshDb, sa: UserId, by: UserId) -> IntegrationId {
    create(
        &t.db,
        &ctx("req-int"),
        &NewIntegration {
            kind: IntegrationKind::Kanflow,
            service_account: sa,
            event_types: None,
        },
        by,
    )
    .await
    .expect("integration")
}

struct World {
    t: FreshDb,
    alice: users::Created,
    sa: UserId,
    integration: IntegrationId,
    web: ProjectId,
}

/// A person, her service account, its kanflow integration and her project.
async fn world() -> World {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice").await;
    let sa = service_account(&t, "kanflow-sync", alice.user_id).await;
    let integration = kanflow(&t, sa, alice.user_id).await;
    let web = project(&t, &alice, "web").await;
    World {
        t,
        alice,
        sa,
        integration,
        web,
    }
}

async fn bind_as(
    w: &World,
    project: ProjectId,
    integration: IntegrationId,
    external: &str,
) -> Result<BindOutcome, ForgeError> {
    bind_decided(
        &w.t.db,
        &ctx("req-bind"),
        project,
        w.alice.user_id,
        integration,
        external,
        |_| Ok(()),
    )
    .await
    .map(|(outcome, ())| outcome)
}

async fn unbind_as(
    w: &World,
    project: ProjectId,
    integration: IntegrationId,
) -> Result<UnbindOutcome, ForgeError> {
    unbind_decided(
        &w.t.db,
        &ctx("req-unbind"),
        project,
        w.alice.user_id,
        integration,
        |_| Ok(()),
    )
    .await
    .map(|(outcome, ())| outcome)
}

/// Bind, expecting a new binding.
async fn bound(w: &World, project: ProjectId, integration: IntegrationId, external: &str) {
    let outcome = bind_as(w, project, integration, external)
        .await
        .unwrap_or_else(|e| panic!("bind {}: {e:?}", project.get()));
    assert!(matches!(outcome, BindOutcome::Bound { .. }), "{outcome:?}");
}

async fn role_of(t: &FreshDb, project: ProjectId, user: UserId) -> Option<String> {
    sqlx::query_scalar(
        "SELECT role FROM forge.project_members WHERE project_id = $1 AND user_id = $2",
    )
    .bind(project.get())
    .bind(user.get())
    .fetch_optional(t.db.write_pool())
    .await
    .expect("role")
}

async fn audit_actions(t: &FreshDb, project: ProjectId) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT action FROM forge.audit_log WHERE resource_type = 'project' \
         AND resource_id = $1 AND (action LIKE 'integration.%' OR action LIKE 'member.%') \
         ORDER BY id",
    )
    .bind(project.get())
    .fetch_all(t.db.write_pool())
    .await
    .expect("audit")
}

/// The value of `key` in the error's context.
fn ctx_of(e: &ForgeError, key: &str) -> Option<String> {
    e.context()
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.clone())
}

/// Run `setup` then `probe` as superuser in a transaction that is rolled
/// back: the SQLSTATE and constraint name of the probe's failure.
async fn refusal(su: &mut PgConnection, setup: &str, probe: &str) -> Option<(String, String)> {
    let mut tx = su.begin().await.expect("begin");
    if !setup.is_empty() {
        sqlx::raw_sql(setup).execute(&mut *tx).await.expect(setup);
    }
    let result = sqlx::raw_sql(probe).execute(&mut *tx).await;
    tx.rollback().await.expect("rollback");
    let e = result.err()?;
    let db = e.as_database_error().expect("a database error");
    Some((
        db.code().map(|c| c.into_owned()).unwrap_or_default(),
        db.constraint().unwrap_or_default().to_owned(),
    ))
}

const PEOPLE: &str = "\
    INSERT INTO forge.users (id, kind, username, display_name, email) OVERRIDING SYSTEM VALUE \
    VALUES (901, 'human', 'p901', 'P', 'p901@example.test'); \
    INSERT INTO forge.users (id, kind, username, display_name) OVERRIDING SYSTEM VALUE \
    VALUES (902, 'service_account', 's902', 'S'), (903, 'bot', 'b903', 'B'), \
    (904, 'service_account', 's904', 'S');";

fn integration_row(id: i64, sa: i64, kind: &str, types: &str) -> String {
    format!(
        "INSERT INTO forge.integrations (id, kind, service_account_id, event_types) \
         OVERRIDING SYSTEM VALUE VALUES ({id}, '{kind}', {sa}, {types})"
    )
}

#[tokio::test]
async fn integration_checks_refuse_raw_rows_by_constraint_name() {
    let t = FreshDb::migrated().await;
    let mut su = t.superuser().await;
    let all = "ARRAY['*']";
    // The account must be a service account (not a person, not a bot).
    for user in [901, 903] {
        assert_eq!(
            refusal(&mut su, PEOPLE, &integration_row(1, user, "kanflow", all)).await,
            Some(("23514".into(), "integrations_service_account_kind".into())),
            "user {user}"
        );
    }
    assert_eq!(
        refusal(&mut su, PEOPLE, &integration_row(1, 902, "gitlab", all)).await,
        Some(("23514".into(), "integrations_kind_known".into()))
    );
    assert_eq!(
        refusal(&mut su, PEOPLE, &integration_row(1, 902, "kanflow", "'{}'")).await,
        Some(("23514".into(), "integrations_event_types_count".into()))
    );
    let two = format!("{PEOPLE} {};", integration_row(1, 902, "kanflow", all));
    assert_eq!(
        refusal(&mut su, &two, &integration_row(2, 902, "kanflow", all)).await,
        Some((
            "23505".into(),
            "integrations_one_per_service_account".into()
        ))
    );
    // The service account never changes, even to another service account.
    assert_eq!(
        refusal(
            &mut su,
            &two,
            "UPDATE forge.integrations SET service_account_id = 904 WHERE id = 1"
        )
        .await,
        Some(("23514".into(), "integrations_service_account_fixed".into()))
    );
    // A valid row passes (only the deferred audit trigger, at COMMIT, is left).
    assert_eq!(
        refusal(&mut su, PEOPLE, &integration_row(1, 902, "kanflow", all)).await,
        None
    );
    t.drop_now().await;
}

#[tokio::test]
async fn binding_checks_refuse_raw_rows_by_constraint_name() {
    let w = world().await;
    let mut su = w.t.superuser().await;
    let row = |external: &str, project: i64| {
        format!(
            "INSERT INTO forge.integration_bindings (integration_id, project_id, \
             external_project_id, granted_membership) VALUES ({}, {project}, '{external}', false)",
            w.integration.get()
        )
    };
    for bad in ["", "a b", "a/b", &"x".repeat(101)] {
        assert_eq!(
            refusal(&mut su, "", &row(bad, w.web.get())).await,
            Some((
                "23514".into(),
                "integration_bindings_external_project_id_format".into()
            )),
            "{bad:?}"
        );
    }
    // One binding per project, whatever the integration.
    let other_sa = service_account(&w.t, "kanflow-two", w.alice.user_id).await;
    let other = kanflow(&w.t, other_sa, w.alice.user_id).await;
    let first = row("P1", w.web.get());
    let second = format!(
        "INSERT INTO forge.integration_bindings (integration_id, project_id, \
         external_project_id, granted_membership) VALUES ({}, {}, 'P2', false)",
        other.get(),
        w.web.get()
    );
    assert_eq!(
        refusal(&mut su, &first, &second).await,
        Some((
            "23505".into(),
            "integration_bindings_one_per_project".into()
        ))
    );
    // A raw row that passes every check is refused at COMMIT by the
    // deferred audit trigger (no audit row in its transaction).
    let committed = sqlx::raw_sql(&first).execute(&mut su).await;
    let e = committed.expect_err("no audit row");
    assert_eq!(
        e.as_database_error().and_then(|d| d.code()).as_deref(),
        Some("XF002"),
        "{e}"
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn binding_grants_and_unbinding_removes_reporter_membership() {
    let w = world().await;
    assert_eq!(role_of(&w.t, w.web, w.sa).await, None);
    let bound = bind_as(&w, w.web, w.integration, "P1").await.expect("bind");
    assert_eq!(
        bound,
        BindOutcome::Bound {
            granted_membership: true
        }
    );
    assert_eq!(
        role_of(&w.t, w.web, w.sa).await.as_deref(),
        Some("reporter")
    );
    let listed = project_bindings(&w.t.db, w.web).await.expect("bindings");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].integration, w.integration);
    assert_eq!(listed[0].external_project_id, "P1");
    assert!(listed[0].granted_membership);
    // The same binding again changes nothing.
    assert_eq!(
        bind_as(&w, w.web, w.integration, "P1")
            .await
            .expect("again"),
        BindOutcome::Unchanged
    );
    // One audit row for the binding and one for the membership it granted,
    // with the member.added event of the project.
    assert_eq!(
        audit_actions(&w.t, w.web).await,
        vec!["integration.bound", "member.added"]
    );
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM forge.events WHERE project_id = $1 AND event_type = 'member.added' \
         AND payload->>'user_id' = $2",
    )
    .bind(w.web.get())
    .bind(w.sa.get().to_string())
    .fetch_one(w.t.db.write_pool())
    .await
    .expect("events");
    assert_eq!(events, 1);

    let unbound = unbind_as(&w, w.web, w.integration).await.expect("unbind");
    assert_eq!(
        unbound,
        UnbindOutcome {
            membership_removed: true
        }
    );
    assert_eq!(role_of(&w.t, w.web, w.sa).await, None);
    assert!(project_bindings(&w.t.db, w.web)
        .await
        .expect("bindings")
        .is_empty());
    assert_eq!(
        audit_actions(&w.t, w.web).await,
        vec![
            "integration.bound",
            "member.added",
            "integration.unbound",
            "member.removed"
        ]
    );
    // Unbinding again: nothing is bound.
    let gone = unbind_as(&w, w.web, w.integration)
        .await
        .expect_err("not bound");
    assert_eq!(gone.code(), ErrorCode::NotFound);
    assert_eq!(
        ctx_of(&gone, "resource").as_deref(),
        Some("integration_binding")
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn unbinding_keeps_a_membership_the_binding_did_not_grant() {
    let w = world().await;
    // Already a Developer before the binding: the binding grants nothing.
    projects::add_member(&w.t.db, &ctx("req-m"), w.web, w.sa, Role::Developer)
        .await
        .expect("member");
    assert_eq!(
        bind_as(&w, w.web, w.integration, "P1").await.expect("bind"),
        BindOutcome::Bound {
            granted_membership: false
        }
    );
    assert_eq!(
        unbind_as(&w, w.web, w.integration).await.expect("unbind"),
        UnbindOutcome {
            membership_removed: false
        }
    );
    assert_eq!(
        role_of(&w.t, w.web, w.sa).await.as_deref(),
        Some("developer")
    );
    // A granted Reporter someone promoted meanwhile is kept too.
    let api = project(&w.t, &w.alice, "api").await;
    bound(&w, api, w.integration, "P1").await;
    let promoted = projects::change_member(
        &w.t.db,
        &ctx("req-promote"),
        api,
        w.sa,
        Role::Reporter,
        Role::Developer,
    )
    .await
    .expect("promote");
    assert!(matches!(
        promoted,
        crate::groups::MemberChange::Changed { .. }
    ));
    assert_eq!(
        unbind_as(&w, api, w.integration).await.expect("unbind api"),
        UnbindOutcome {
            membership_removed: false
        }
    );
    assert_eq!(role_of(&w.t, api, w.sa).await.as_deref(), Some("developer"));
    w.t.drop_now().await;
}

#[tokio::test]
async fn binding_second_kanflow_project_is_refused() {
    let w = world().await;
    bound(&w, w.web, w.integration, "P1").await;
    // The same integration, another external project.
    let moved = bind_as(&w, w.web, w.integration, "P2")
        .await
        .expect_err("second external project");
    assert_eq!(moved.code(), ErrorCode::BindingConflict);
    assert_eq!(
        ctx_of(&moved, "reason").as_deref(),
        Some("bound_to_other_external_project")
    );
    // Another kanflow integration.
    let other_sa = service_account(&w.t, "kanflow-two", w.alice.user_id).await;
    let other = kanflow(&w.t, other_sa, w.alice.user_id).await;
    let elsewhere = bind_as(&w, w.web, other, "P1")
        .await
        .expect_err("second integration");
    assert_eq!(elsewhere.code(), ErrorCode::BindingConflict);
    assert_eq!(
        ctx_of(&elsewhere, "reason").as_deref(),
        Some("bound_to_other_integration")
    );
    assert_eq!(
        ctx_of(&elsewhere, "bound_integration_id").as_deref(),
        Some(w.integration.get().to_string().as_str())
    );
    // Nothing was written by the refusals: one binding, no membership of
    // the other service account.
    assert_eq!(project_bindings(&w.t.db, w.web).await.expect("b").len(), 1);
    assert_eq!(role_of(&w.t, w.web, other_sa).await, None);
    // A kanflow project may hold several forge projects (02 §2).
    let api = project(&w.t, &w.alice, "api").await;
    bound(&w, api, w.integration, "P1").await;
    w.t.drop_now().await;
}

#[tokio::test]
async fn binding_limit_200_is_int_0012() {
    let w = world().await;
    // The world's project plus 199 more: 200 bindings.
    bound(&w, w.web, w.integration, "P1").await;
    for i in 1..MAX_BINDINGS {
        let p = project(&w.t, &w.alice, &format!("p{i}")).await;
        bound(&w, p, w.integration, "P1").await;
    }
    let extra = project(&w.t, &w.alice, "extra").await;
    let refused = bind_as(&w, extra, w.integration, "P1")
        .await
        .expect_err("201st binding");
    assert_eq!(refused.code(), ErrorCode::IntegrationLimitReached);
    assert_eq!(ctx_of(&refused, "limit").as_deref(), Some("200"));
    assert_eq!(ctx_of(&refused, "reason").as_deref(), Some("binding_limit"));
    assert_eq!(role_of(&w.t, extra, w.sa).await, None);
    let listed = get(&w.t.db, w.integration)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(listed.bindings, MAX_BINDINGS);
    // Unbinding one makes room again.
    unbind_as(&w, w.web, w.integration).await.expect("unbind");
    bound(&w, extra, w.integration, "P1").await;
    w.t.drop_now().await;
}

#[tokio::test]
async fn binding_refused_for_pending_deletion_project() {
    let w = world().await;
    projects::delete_decided(
        &w.t.db,
        &ctx("req-delete"),
        w.web,
        w.alice.user_id,
        std::time::Duration::from_secs(86_400),
        |_| Ok(()),
    )
    .await
    .expect("delete");
    // Even with a `decide` that lets it pass, the command refuses it.
    let refused = bind_as(&w, w.web, w.integration, "P1")
        .await
        .expect_err("pending deletion");
    assert_eq!(refused.code(), ErrorCode::PendingDeletion);
    assert_eq!(role_of(&w.t, w.web, w.sa).await, None);
    w.t.drop_now().await;
}

#[tokio::test]
async fn binding_an_absent_integration_or_project_is_not_found() {
    let w = world().await;
    let absent = bind_as(&w, w.web, IntegrationId::new(9_999), "P1")
        .await
        .expect_err("absent integration");
    assert_eq!(absent.code(), ErrorCode::NotFound);
    assert_eq!(ctx_of(&absent, "resource").as_deref(), Some("integration"));
    let nowhere = bind_as(&w, ProjectId::new(9_999), w.integration, "P1")
        .await
        .expect_err("absent project");
    assert_eq!(nowhere.code(), ErrorCode::NotFound);
    assert_eq!(ctx_of(&nowhere, "resource").as_deref(), Some("project"));
    let bad = bind_as(&w, w.web, w.integration, "a/b")
        .await
        .expect_err("bad external id");
    assert_eq!(bad.code(), ErrorCode::RequestInvalid);
    w.t.drop_now().await;
}

#[tokio::test]
async fn decide_sees_the_locked_project_and_its_refusal_writes_nothing() {
    let w = world().await;
    let refused = bind_decided(
        &w.t.db,
        &ctx("req-bind"),
        w.web,
        w.alice.user_id,
        w.integration,
        "P1",
        |facts| {
            assert_eq!(facts.project.project, w.web);
            assert_eq!(facts.project.access.role, Some(Role::Owner));
            assert!(facts.account.active);
            Err::<(), _>(ForgeError::new(ErrorCode::Forbidden))
        },
    )
    .await
    .expect_err("refused");
    assert_eq!(refused.code(), ErrorCode::Forbidden);
    assert!(project_bindings(&w.t.db, w.web)
        .await
        .expect("b")
        .is_empty());
    assert_eq!(role_of(&w.t, w.web, w.sa).await, None);
    w.t.drop_now().await;
}

#[tokio::test]
async fn create_and_update_integrations_with_one_audit_row_each() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice").await;
    let sa = service_account(&t, "kanflow-sync", alice.user_id).await;
    let id = kanflow(&t, sa, alice.user_id).await;
    let got = get(&t.db, id).await.expect("get").expect("present");
    assert_eq!(got.kind, IntegrationKind::Kanflow);
    assert_eq!(got.service_account, sa);
    assert_eq!(got.service_account_username, "kanflow-sync");
    assert!(got.enabled);
    assert_eq!(got.event_types, vec!["*".to_owned()]);
    assert_eq!(got.bindings, 0);
    assert_eq!(got.created_by, Some(alice.user_id));
    // One integration per service account; a person is no service account.
    let twice = create(
        &t.db,
        &ctx("req-int-2"),
        &NewIntegration {
            kind: IntegrationKind::Kanflow,
            service_account: sa,
            event_types: None,
        },
        alice.user_id,
    )
    .await
    .expect_err("second integration of one account");
    assert_eq!(twice.code(), ErrorCode::RequestInvalid);
    assert_eq!(
        ctx_of(&twice, "rule").as_deref(),
        Some("service_account_in_use")
    );
    let person = create(
        &t.db,
        &ctx("req-int-3"),
        &NewIntegration {
            kind: IntegrationKind::Kanflow,
            service_account: alice.user_id,
            event_types: None,
        },
        alice.user_id,
    )
    .await
    .expect_err("a person");
    assert_eq!(
        ctx_of(&person, "rule").as_deref(),
        Some("not_service_account")
    );
    let unknown = create(
        &t.db,
        &ctx("req-int-4"),
        &NewIntegration {
            kind: IntegrationKind::Kanflow,
            service_account: UserId::new(9_999),
            event_types: None,
        },
        alice.user_id,
    )
    .await
    .expect_err("absent");
    assert_eq!(
        ctx_of(&unknown, "rule").as_deref(),
        Some("not_service_account")
    );
    let bad_types = create(
        &t.db,
        &ctx("req-int-5"),
        &NewIntegration {
            kind: IntegrationKind::Kanflow,
            service_account: sa,
            event_types: Some(vec!["member.added".to_owned()]),
        },
        alice.user_id,
    )
    .await
    .expect_err("maintainer class type");
    assert_eq!(bad_types.code(), ErrorCode::RequestInvalid);
    assert_eq!(
        ctx_of(&bad_types, "rule").as_deref(),
        Some("unknown_event_type")
    );

    // Update: the named fields; an equal update writes nothing.
    let changed = update(
        &t.db,
        &ctx("req-int-up"),
        id,
        &Changes {
            enabled: Some(false),
            event_types: Some(vec![
                "repository.pushed".to_owned(),
                "merge_request.*".to_owned(),
            ]),
        },
    )
    .await
    .expect("update");
    assert_eq!(
        changed,
        UpdateOutcome::Updated(vec![
            IntegrationField::Enabled,
            IntegrationField::EventTypes
        ])
    );
    let got = get(&t.db, id).await.expect("get").expect("present");
    assert!(!got.enabled);
    assert_eq!(
        got.event_types,
        vec!["merge_request.*", "repository.pushed"]
    );
    let same = update(
        &t.db,
        &ctx("req-int-same"),
        id,
        &Changes {
            enabled: Some(false),
            event_types: None,
        },
    )
    .await
    .expect("same");
    assert_eq!(same, UpdateOutcome::Unchanged);
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM forge.audit_log WHERE resource_type = 'integration' \
         AND resource_id = $1 ORDER BY id",
    )
    .bind(id.get())
    .fetch_all(t.db.write_pool())
    .await
    .expect("audit");
    assert_eq!(rows, vec!["integration.created", "integration.updated"]);
    let absent = update(
        &t.db,
        &ctx("req-int-x"),
        IntegrationId::new(9_999),
        &Changes::default(),
    )
    .await
    .expect_err("absent");
    assert_eq!(absent.code(), ErrorCode::NotFound);
    let listed = list(&t.db).await.expect("list");
    assert_eq!(listed.items.len(), 1);
    assert!(!listed.truncated);
    t.drop_now().await;
}
