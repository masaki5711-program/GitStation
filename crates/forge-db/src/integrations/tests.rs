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
use crate::change::{Actor, ChangeContext};
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
    .map(|(outcome, _, ())| outcome)
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
    // One resolving binding per project, whatever the integration (M3 X3:
    // a raw row resolves by default).
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
            "integration_bindings_one_resolving_per_project".into()
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

// ── I1a review ──────────────────────────────────────────────────────────

async fn disable_user(t: &FreshDb, user: UserId) {
    let outcome = users::disable(&t.db, &ctx("req-disable"), user)
        .await
        .expect("disable");
    assert!(
        matches!(outcome, users::DisableOutcome::Disabled),
        "{outcome:?}"
    );
}

async fn set_enabled(t: &FreshDb, integration: IntegrationId, enabled: bool) {
    let outcome = update(
        &t.db,
        &ctx("req-enable"),
        integration,
        &Changes {
            enabled: Some(enabled),
            event_types: None,
        },
    )
    .await
    .expect("update");
    assert!(matches!(outcome, UpdateOutcome::Updated(_)), "{outcome:?}");
}

/// Security review HIGH: disabling an integration stops new bindings (each
/// would grant its service account a Reporter membership).
#[tokio::test]
async fn binding_a_disabled_integration_is_refused() {
    let w = world().await;
    set_enabled(&w.t, w.integration, false).await;
    let refused = bind_as(&w, w.web, w.integration, "P1")
        .await
        .expect_err("disabled");
    assert_eq!(refused.code(), ErrorCode::IntegrationDisabled);
    assert_eq!(
        ctx_of(&refused, "reason").as_deref(),
        Some("integration_disabled")
    );
    assert_eq!(role_of(&w.t, w.web, w.sa).await, None);
    assert!(project_bindings(&w.t.db, w.web)
        .await
        .expect("b")
        .is_empty());
    // Enabled again, it binds; unbinding a disabled one still works (it
    // only takes access away).
    set_enabled(&w.t, w.integration, true).await;
    bound(&w, w.web, w.integration, "P1").await;
    set_enabled(&w.t, w.integration, false).await;
    let unbound = unbind_as(&w, w.web, w.integration).await.expect("unbind");
    assert!(unbound.membership_removed);
    w.t.drop_now().await;
}

/// Security review MEDIUM: no grant to a disabled service account (it would
/// silently come alive when the account is enabled again), and no new
/// integration for one.
#[tokio::test]
async fn binding_for_a_disabled_service_account_is_refused() {
    let w = world().await;
    disable_user(&w.t, w.sa).await;
    let refused = bind_as(&w, w.web, w.integration, "P1")
        .await
        .expect_err("disabled account");
    assert_eq!(refused.code(), ErrorCode::IntegrationDisabled);
    assert_eq!(
        ctx_of(&refused, "reason").as_deref(),
        Some("service_account_disabled")
    );
    assert_eq!(role_of(&w.t, w.web, w.sa).await, None);
    let other = service_account(&w.t, "kanflow-two", w.alice.user_id).await;
    disable_user(&w.t, other).await;
    let created = create(
        &w.t.db,
        &ctx("req-int-disabled"),
        &NewIntegration {
            kind: IntegrationKind::Kanflow,
            service_account: other,
            event_types: None,
        },
        w.alice.user_id,
    )
    .await
    .expect_err("disabled account");
    assert_eq!(created.code(), ErrorCode::RequestInvalid);
    assert_eq!(
        ctx_of(&created, "rule").as_deref(),
        Some("service_account_disabled")
    );
    w.t.drop_now().await;
}

async fn granted(t: &FreshDb, project: ProjectId) -> bool {
    let b = project_bindings(&t.db, project).await.expect("bindings");
    b.first().expect("bound").granted_membership
}

/// Security / database review MEDIUM: a person removes the granted
/// membership and gives it again by hand; unbinding then keeps the human
/// grant (the removal cleared `granted_membership`), on both removal paths.
#[tokio::test]
async fn removing_the_granted_membership_clears_the_grant() {
    let w = world().await;
    bound(&w, w.web, w.integration, "P1").await;
    assert!(granted(&w.t, w.web).await);
    projects::remove_member(&w.t.db, &ctx("req-rm"), w.web, w.sa, Role::Reporter)
        .await
        .expect("remove");
    assert!(!granted(&w.t, w.web).await);
    projects::add_member(&w.t.db, &ctx("req-readd"), w.web, w.sa, Role::Reporter)
        .await
        .expect("re-add");
    let unbound = unbind_as(&w, w.web, w.integration).await.expect("unbind");
    assert!(!unbound.membership_removed);
    assert_eq!(
        role_of(&w.t, w.web, w.sa).await.as_deref(),
        Some("reporter")
    );
    // The decided removal path too.
    let api = project(&w.t, &w.alice, "api").await;
    bound(&w, api, w.integration, "P1").await;
    projects::remove_member_decided(
        &w.t.db,
        &ctx("req-rm-decided"),
        api,
        w.alice.user_id,
        w.sa,
        |_| Ok(()),
    )
    .await
    .expect("remove decided");
    assert!(!granted(&w.t, api).await);
    // The binding change is audited by the removal's own project row.
    let actions = audit_actions(&w.t, api).await;
    assert_eq!(actions.last().map(String::as_str), Some("member.removed"));
    w.t.drop_now().await;
}

#[tokio::test]
async fn event_type_entries_are_checked_by_the_schema() {
    let t = FreshDb::migrated().await;
    let mut su = t.superuser().await;
    for bad in [
        "ARRAY[NULL]::text[]",
        "ARRAY['']",
        "ARRAY[' a']",
        "ARRAY['merge_request.opened', ' ']",
        "ARRAY['Merge']",
        "ARRAY[['*'], ['*']]",
    ] {
        assert_eq!(
            refusal(&mut su, PEOPLE, &integration_row(1, 902, "kanflow", bad)).await,
            Some(("23514".into(), "integrations_event_types_entries".into())),
            "{bad}"
        );
    }
    for ok in [
        "ARRAY['*']",
        "ARRAY['merge_request.*', 'repository.pushed']",
    ] {
        assert_eq!(
            refusal(&mut su, PEOPLE, &integration_row(1, 902, "kanflow", ok)).await,
            None,
            "{ok}"
        );
    }
    t.drop_now().await;
}

/// Database / rust review MEDIUM: two binds at 199 bindings, at once:
/// exactly one wins, the other is FG-INT-0012 (the count is taken under
/// the integration's row lock).
#[tokio::test]
async fn two_binds_at_199_bindings_let_exactly_one_through() {
    let w = world().await;
    bound(&w, w.web, w.integration, "P1").await;
    for i in 1..MAX_BINDINGS - 1 {
        let p = project(&w.t, &w.alice, &format!("p{i}")).await;
        bound(&w, p, w.integration, "P1").await;
    }
    let a = project(&w.t, &w.alice, "race-a").await;
    let b = project(&w.t, &w.alice, "race-b").await;
    let (ra, rb) = tokio::join!(
        bind_as(&w, a, w.integration, "P1"),
        bind_as(&w, b, w.integration, "P1")
    );
    let wins = [&ra, &rb].iter().filter(|r| r.is_ok()).count();
    assert_eq!(wins, 1, "{ra:?} {rb:?}");
    let lost = [ra, rb]
        .into_iter()
        .find_map(Result::err)
        .expect("one refused");
    assert_eq!(lost.code(), ErrorCode::IntegrationLimitReached);
    let got = get(&w.t.db, w.integration)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(got.bindings, MAX_BINDINGS);
    w.t.drop_now().await;
}

/// Database / rust review MEDIUM: an unbind racing a removal of the granted
/// membership ends consistent in either order and at once (both lock the
/// project row first).
#[tokio::test]
async fn unbind_racing_a_membership_removal_ends_consistent() {
    let w = world().await;
    for round in 0..3 {
        let p = project(&w.t, &w.alice, &format!("rm{round}")).await;
        bound(&w, p, w.integration, "P1").await;
        let rm_ctx = ctx("req-rm");
        let remove = projects::remove_member(&w.t.db, &rm_ctx, p, w.sa, Role::Reporter);
        let (unbound, removed) = match round {
            0 => {
                let u = unbind_as(&w, p, w.integration).await;
                (u, remove.await)
            }
            1 => {
                let r = remove.await;
                (unbind_as(&w, p, w.integration).await, r)
            }
            _ => tokio::join!(unbind_as(&w, p, w.integration), remove),
        };
        let unbound = unbound.expect("unbind always succeeds");
        match removed {
            // The removal came first: nothing left for the unbind.
            Ok(()) => assert!(!unbound.membership_removed, "round {round}"),
            // The unbind removed it first: the removal finds no member.
            Err(e) => {
                assert_eq!(e.code(), ErrorCode::MembershipConflict, "round {round}");
                assert!(unbound.membership_removed, "round {round}");
            }
        }
        assert_eq!(role_of(&w.t, p, w.sa).await, None, "round {round}");
        assert!(project_bindings(&w.t.db, p).await.expect("b").is_empty());
    }
    w.t.drop_now().await;
}

/// The same with a role change of the granted membership (Reporter to
/// Developer): the unbind keeps a changed role, or the change finds no
/// member.
#[tokio::test]
async fn unbind_racing_a_role_change_ends_consistent() {
    let w = world().await;
    for round in 0..3 {
        let p = project(&w.t, &w.alice, &format!("ch{round}")).await;
        bound(&w, p, w.integration, "P1").await;
        let ch_ctx = ctx("req-ch");
        let change =
            projects::change_member(&w.t.db, &ch_ctx, p, w.sa, Role::Reporter, Role::Developer);
        let (unbound, changed) = match round {
            0 => {
                let u = unbind_as(&w, p, w.integration).await;
                (u, change.await)
            }
            1 => {
                let c = change.await;
                (unbind_as(&w, p, w.integration).await, c)
            }
            _ => tokio::join!(unbind_as(&w, p, w.integration), change),
        };
        let unbound = unbound.expect("unbind always succeeds");
        match changed {
            Ok(_) => {
                assert!(!unbound.membership_removed, "round {round}");
                assert_eq!(
                    role_of(&w.t, p, w.sa).await.as_deref(),
                    Some("developer"),
                    "round {round}"
                );
            }
            Err(e) => {
                assert_eq!(e.code(), ErrorCode::MembershipConflict, "round {round}");
                assert!(unbound.membership_removed, "round {round}");
                assert_eq!(role_of(&w.t, p, w.sa).await, None, "round {round}");
            }
        }
    }
    w.t.drop_now().await;
}

/// Rust review LOW: two creates for one service account at once — the
/// loser is FG-WEB-0006 `service_account_in_use` whether it lost on the
/// check or on the unique index (SQLSTATE 23505).
#[tokio::test]
async fn concurrent_creates_for_one_account_refuse_the_second_as_in_use() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice").await;
    let sa = service_account(&t, "kanflow-sync", alice.user_id).await;
    let new = NewIntegration {
        kind: IntegrationKind::Kanflow,
        service_account: sa,
        event_types: None,
    };
    let (ctx_a, ctx_b) = (ctx("req-int-a"), ctx("req-int-b"));
    let (a, b) = tokio::join!(
        create(&t.db, &ctx_a, &new, alice.user_id),
        create(&t.db, &ctx_b, &new, alice.user_id)
    );
    assert_eq!(
        [&a, &b].iter().filter(|r| r.is_ok()).count(),
        1,
        "{a:?} {b:?}"
    );
    let lost = [a, b]
        .into_iter()
        .find_map(Result::err)
        .expect("one refused");
    assert_eq!(lost.code(), ErrorCode::RequestInvalid, "{lost:?}");
    assert_eq!(
        ctx_of(&lost, "rule").as_deref(),
        Some("service_account_in_use")
    );
    t.drop_now().await;
}

/// Rust review MEDIUM: the bind answers with the binding it wrote (or the
/// one it found), never a re-read after the commit.
#[tokio::test]
async fn bind_returns_the_binding_it_wrote() {
    let w = world().await;
    let (outcome, binding, ()) = bind_decided(
        &w.t.db,
        &ctx("req-bind"),
        w.web,
        w.alice.user_id,
        w.integration,
        "P1",
        |_| Ok(()),
    )
    .await
    .expect("bind");
    assert!(matches!(outcome, BindOutcome::Bound { .. }));
    assert_eq!(binding.integration, w.integration);
    assert_eq!(binding.project, w.web);
    assert_eq!(binding.kind, IntegrationKind::Kanflow);
    assert_eq!(binding.external_project_id, "P1");
    assert!(binding.granted_membership);
    assert_eq!(binding.created_by, Some(w.alice.user_id));
    let stored = project_bindings(&w.t.db, w.web).await.expect("b");
    assert_eq!(stored, vec![binding.clone()]);
    let (again, same, ()) = bind_decided(
        &w.t.db,
        &ctx("req-bind-2"),
        w.web,
        w.alice.user_id,
        w.integration,
        "P1",
        |_| Ok(()),
    )
    .await
    .expect("again");
    assert_eq!(again, BindOutcome::Unchanged);
    assert_eq!(same, binding);
    w.t.drop_now().await;
}

// ── I1a re-review ───────────────────────────────────────────────────────

/// Database re-review MEDIUM: forge_app may change a binding's
/// `granted_membership` and nothing else (column-level UPDATE,
/// 20261114100200). Each statement runs as forge_app in a transaction that
/// is rolled back (the deferred audit trigger is not reached).
#[tokio::test]
async fn forge_app_updates_only_granted_membership_of_a_binding() {
    let w = world().await;
    bound(&w, w.web, w.integration, "P1").await;
    let id = w.web.get();
    for (column, value) in [
        ("external_project_id", "'P2'"),
        ("integration_id", "integration_id"),
        ("project_id", "project_id"),
        ("created_by", "NULL"),
        ("created_at", "now()"),
    ] {
        let mut tx = w.t.db.write_pool().begin().await.expect("begin");
        let result = sqlx::raw_sql(&format!(
            "UPDATE forge.integration_bindings SET {column} = {value} WHERE project_id = {id}"
        ))
        .execute(&mut *tx)
        .await;
        tx.rollback().await.expect("rollback");
        let e = result.expect_err(column);
        assert_eq!(
            e.as_database_error().and_then(|d| d.code()).as_deref(),
            Some("42501"),
            "{column}: {e}"
        );
    }
    let mut tx = w.t.db.write_pool().begin().await.expect("begin");
    let result = sqlx::raw_sql(&format!(
        "UPDATE forge.integration_bindings SET granted_membership = false WHERE project_id = {id}"
    ))
    .execute(&mut *tx)
    .await;
    tx.rollback().await.expect("rollback");
    result.expect("granted_membership is forge_app's to change");
    w.t.drop_now().await;
}

/// Security re-review LOW: a person changing the granted membership's role
/// (any change) makes it theirs: the grant is cleared in the same change,
/// so an unbind keeps the membership (both change paths).
#[tokio::test]
async fn a_role_change_of_the_granted_membership_clears_the_grant() {
    let w = world().await;
    bound(&w, w.web, w.integration, "P1").await;
    let changed = projects::change_member(
        &w.t.db,
        &ctx("req-promote"),
        w.web,
        w.sa,
        Role::Reporter,
        Role::Developer,
    )
    .await
    .expect("promote");
    assert!(matches!(
        changed,
        crate::groups::MemberChange::Changed { .. }
    ));
    assert!(!granted(&w.t, w.web).await);
    // Back to Reporter by hand: still the person's, the unbind keeps it.
    let back = projects::change_member(
        &w.t.db,
        &ctx("req-demote"),
        w.web,
        w.sa,
        Role::Developer,
        Role::Reporter,
    )
    .await
    .expect("demote");
    assert!(matches!(back, crate::groups::MemberChange::Changed { .. }));
    let unbound = unbind_as(&w, w.web, w.integration).await.expect("unbind");
    assert!(!unbound.membership_removed);
    assert_eq!(
        role_of(&w.t, w.web, w.sa).await.as_deref(),
        Some("reporter")
    );
    // The decided change path too.
    let api = project(&w.t, &w.alice, "api").await;
    bound(&w, api, w.integration, "P1").await;
    let (decided, ()) = projects::change_member_decided(
        &w.t.db,
        &ctx("req-promote-decided"),
        api,
        w.alice.user_id,
        w.sa,
        Role::Guest,
        |_| Ok(()),
    )
    .await
    .expect("change decided");
    assert!(matches!(
        decided,
        crate::groups::MemberChange::Changed { .. }
    ));
    assert!(!granted(&w.t, api).await);
    w.t.drop_now().await;
}

/// Security re-review LOW: an integration whose service account is
/// disabled is not enabled again (the rule of `create`); disabling it, or
/// changing its event types, still works.
#[tokio::test]
async fn enabling_an_integration_of_a_disabled_account_is_refused() {
    let w = world().await;
    set_enabled(&w.t, w.integration, false).await;
    disable_user(&w.t, w.sa).await;
    let refused = update(
        &w.t.db,
        &ctx("req-enable"),
        w.integration,
        &Changes {
            enabled: Some(true),
            event_types: None,
        },
    )
    .await
    .expect_err("disabled account");
    assert_eq!(refused.code(), ErrorCode::RequestInvalid);
    assert_eq!(
        ctx_of(&refused, "rule").as_deref(),
        Some("service_account_disabled")
    );
    let got = get(&w.t.db, w.integration)
        .await
        .expect("get")
        .expect("present");
    assert!(!got.enabled);
    let types = update(
        &w.t.db,
        &ctx("req-types"),
        w.integration,
        &Changes {
            enabled: Some(false),
            event_types: Some(vec!["merge_request.*".to_owned()]),
        },
    )
    .await
    .expect("event types of a disabled integration");
    assert_eq!(
        types,
        UpdateOutcome::Updated(vec![IntegrationField::EventTypes])
    );
    w.t.drop_now().await;
}

/// M3 X3 (plan 03 §7.1, ADR-0016 Amendment X3): a binding that does not
/// resolve work items (a shared spec repository) may be one of several of
/// its project; one that resolves is the only resolving binding of its
/// project; one external project once per (integration, project).
#[tokio::test]
async fn binding_rows_share_a_spec_project_but_resolve_once() {
    let w = world().await;
    let mut su = w.t.superuser().await;
    let row = |external: &str, resolves: bool| {
        format!(
            "INSERT INTO forge.integration_bindings (integration_id, project_id, \
             external_project_id, granted_membership, resolves_work_items) \
             VALUES ({}, {}, '{external}', false, {resolves})",
            w.integration.get(),
            w.web.get()
        )
    };
    let shared = format!("{}; {}", row("P1", false), row("P2", false));
    assert_eq!(refusal(&mut su, "", &shared).await, None);
    assert_eq!(
        refusal(&mut su, &row("P1", true), &row("P2", true)).await,
        Some((
            "23505".into(),
            "integration_bindings_one_resolving_per_project".into()
        ))
    );
    assert_eq!(
        refusal(&mut su, &row("P1", false), &row("P1", true)).await,
        Some((
            "23505".into(),
            "integration_bindings_one_per_external_project".into()
        ))
    );
    w.t.drop_now().await;
}

#[path = "multi_tests.rs"]
mod multi;
