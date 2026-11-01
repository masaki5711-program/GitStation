//! M1 phase 8b MR7a: the writes decided inside their transaction —
//! `projects::{create_decided, update_decided, change_visibility_decided}`,
//! `groups::{create_decided, update_decided, load}`, `resolve::load_parent`
//! and the roles they hand to `decide`.

use forge_core::{ChangedField, EventType, GroupRecord, Visibility};

use super::*;
use crate::groups::{GroupCreateFacts, GroupFields, GroupParent, GroupPatch, GroupUpdateFacts};
use crate::resolve::{load_parent, ParentRecord};

fn deny() -> ForgeError {
    ForgeError::new(ErrorCode::Forbidden).with_context("reason", "test_denied")
}

async fn audit_rows(su: &mut PgConnection, action: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM forge.audit_log WHERE action = $1")
        .bind(action)
        .fetch_one(su)
        .await
        .expect("audit rows")
}

#[tokio::test]
async fn update_decided_hands_the_locked_facts_to_decide_and_writes_one_audit_row() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    groups::add_member(
        &t.db,
        &ctx("req-bob"),
        acme.group_id,
        bob.user_id,
        Role::Maintainer,
    )
    .await
    .expect("bob maintains acme");
    let web = create_ok(&t, &project("web", acme.namespace_id, alice.user_id)).await;
    let patch = ProjectPatch {
        display_name: Some("Web site".into()),
        description: None,
    };
    let (updated, seen) = update_decided(
        &t.db,
        &ctx("req-update"),
        web.project_id,
        bob.user_id,
        &patch,
        |facts: &ProjectUpdateFacts| Ok(*facts),
    )
    .await
    .expect("update");
    assert_eq!(updated, Updated::Changed(vec![ChangedField::DisplayName]));
    // bob's role is the inherited Maintainer, re-read in the transaction.
    assert_eq!(seen.project.project, web.project_id);
    assert_eq!(seen.project.access.role, Some(Role::Maintainer));
    assert_eq!(seen.project.access.visibility, Visibility::Private);
    assert!(!seen.project.access.pending_deletion);
    assert!(seen.account.active && !seen.account.is_admin);
    let mut su = t.superuser().await;
    let row: (String, String, Option<i64>, serde_json::Value) = sqlx::query_as(
        "SELECT p.display_name, a.resource_type, a.project_id, a.details FROM forge.projects p \
         JOIN forge.audit_log a ON a.action = 'project.updated' WHERE p.id = $1",
    )
    .bind(web.project_id.get())
    .fetch_one(&mut su)
    .await
    .expect("updated");
    assert_eq!(
        row,
        (
            "Web site".into(),
            "project".into(),
            Some(web.project_id.get()),
            json!({"project_id": web.project_id.get(), "fields": ["display_name"]})
        )
    );
    // project.updated is audit-only in M1.
    assert_eq!(
        count(
            &mut su,
            "SELECT count(*) FROM forge.events WHERE event_type LIKE 'project.updated%'"
        )
        .await,
        0
    );
    // The same value again: nothing written.
    let (again, _) = update_decided(
        &t.db,
        &ctx("req-again"),
        web.project_id,
        bob.user_id,
        &patch,
        |_| Ok(()),
    )
    .await
    .expect("unchanged");
    assert_eq!(again, Updated::Unchanged);
    assert_eq!(audit_rows(&mut su, "project.updated").await, 1);
    t.drop_now().await;
}

#[tokio::test]
async fn a_refusing_decision_or_an_invalid_patch_writes_nothing() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let web = create_ok(&t, &project("web", alice.namespace_id, alice.user_id)).await;
    let patch = ProjectPatch {
        display_name: None,
        description: Some("new".into()),
    };
    let err = update_decided(
        &t.db,
        &ctx("req-denied"),
        web.project_id,
        alice.user_id,
        &patch,
        |_| Err::<(), _>(deny()),
    )
    .await
    .expect_err("denied");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    let err = update_decided(
        &t.db,
        &ctx("req-empty"),
        web.project_id,
        alice.user_id,
        &ProjectPatch::default(),
        |_| Ok(()),
    )
    .await
    .expect_err("empty");
    assert_reason(&err, ErrorCode::ProjectRequestInvalid, "empty_update");
    let long = ProjectPatch {
        display_name: Some("x".repeat(256)),
        description: None,
    };
    let err = update_decided(
        &t.db,
        &ctx("req-long"),
        web.project_id,
        alice.user_id,
        &long,
        |_| Ok(()),
    )
    .await
    .expect_err("too long");
    assert_reason(
        &err,
        ErrorCode::ProjectRequestInvalid,
        "display_name_length",
    );
    let err = update_decided(
        &t.db,
        &ctx("req-absent"),
        ProjectId::new(9999),
        alice.user_id,
        &patch,
        |_| Ok(()),
    )
    .await
    .expect_err("absent");
    assert_reason(&err, ErrorCode::NotFound, "absent");
    let mut su = t.superuser().await;
    assert_eq!(audit_rows(&mut su, "project.updated").await, 0);
    t.drop_now().await;
}

#[tokio::test]
async fn a_visibility_change_publishes_one_maintainer_event() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let web = create_ok(&t, &project("web", alice.namespace_id, alice.user_id)).await;
    let (outcome, seen) = change_visibility_decided(
        &t.db,
        &ctx("req-vis"),
        web.project_id,
        alice.user_id,
        Visibility::Internal,
        |facts| Ok(facts.project.access),
    )
    .await
    .expect("change");
    assert_eq!(
        outcome,
        VisibilityOutcome::Changed {
            from: Visibility::Private
        }
    );
    // alice owns the personal namespace the project is in.
    assert_eq!(seen.role, Some(Role::Owner));
    let mut su = t.superuser().await;
    let event: (String, Option<i64>, serde_json::Value) = sqlx::query_as(
        "SELECT event_type, project_id, payload FROM forge.events \
         WHERE event_type = 'project.visibility_changed'",
    )
    .fetch_one(&mut su)
    .await
    .expect("event");
    let payload = json!({"project_id": web.project_id.get(), "from": "private", "to": "internal"});
    assert_eq!(
        event,
        (
            EventType::ProjectVisibilityChanged.as_str().to_owned(),
            Some(web.project_id.get()),
            payload.clone()
        )
    );
    let details: serde_json::Value = sqlx::query_scalar(
        "SELECT details FROM forge.audit_log WHERE action = 'project.visibility_changed'",
    )
    .fetch_one(&mut su)
    .await
    .expect("audit");
    assert_eq!(details, payload);
    let access = project_access(&t.db, web.project_id, None)
        .await
        .expect("access")
        .expect("exists");
    assert_eq!(access.visibility, Visibility::Internal);
    // Already internal: nothing written.
    let (outcome, ()) = change_visibility_decided(
        &t.db,
        &ctx("req-vis-again"),
        web.project_id,
        alice.user_id,
        Visibility::Internal,
        |_| Ok(()),
    )
    .await
    .expect("unchanged");
    assert_eq!(outcome, VisibilityOutcome::Unchanged);
    assert_eq!(audit_rows(&mut su, "project.visibility_changed").await, 1);
    // A refusal rolls back.
    let err = change_visibility_decided(
        &t.db,
        &ctx("req-vis-denied"),
        web.project_id,
        alice.user_id,
        Visibility::Private,
        |_| Err::<(), _>(deny()),
    )
    .await
    .expect_err("denied");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    assert_eq!(audit_rows(&mut su, "project.visibility_changed").await, 1);
    t.drop_now().await;
}

#[tokio::test]
async fn create_decided_sees_the_parent_and_refuses_before_writing() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    groups::add_member(
        &t.db,
        &ctx("req-bob"),
        acme.group_id,
        bob.user_id,
        Role::Developer,
    )
    .await
    .expect("bob develops in acme");
    let new = project("web", acme.namespace_id, bob.user_id);
    let err = create_decided(&t.db, &ctx("req-denied"), &new, |facts| {
        assert_eq!(
            facts.parent,
            ParentAccess::Group {
                namespace: acme.namespace_id,
                group: acme.group_id,
                role: Some(Role::Developer),
            }
        );
        Err::<(), _>(deny())
    })
    .await
    .expect_err("denied");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    let mut su = t.superuser().await;
    assert_eq!(written(&mut su).await, [0; 7]);
    let mine = project("own", alice.namespace_id, alice.user_id);
    let (created, parent) = create_decided(&t.db, &ctx("req-own"), &mine, |f| Ok(f.parent))
        .await
        .expect("own namespace");
    assert_eq!(
        parent,
        ParentAccess::User {
            namespace: alice.namespace_id,
            owner: alice.user_id
        }
    );
    assert!(trio(&mut su, created.project_id).await.is_some());
    t.drop_now().await;
}

#[tokio::test]
async fn group_create_and_update_decided_and_the_group_record() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let fields = GroupFields {
        segment: "acme".into(),
        display_name: "Acme".into(),
        description: String::new(),
    };
    let (acme, facts) = groups::create_decided(
        &t.db,
        &ctx("req-top"),
        GroupParent::TopLevel,
        &fields,
        alice.user_id,
        |f: &GroupCreateFacts| Ok(*f),
    )
    .await
    .expect("top level");
    assert_eq!(facts.parent, None);
    assert!(facts.account.active);
    let sub = GroupFields {
        segment: "web".into(),
        display_name: "Web".into(),
        description: "synthetic".into(),
    };
    // bob has no role on acme: decide sees None and refuses.
    let err = groups::create_decided(
        &t.db,
        &ctx("req-sub-denied"),
        GroupParent::Group(acme.group_id),
        &sub,
        bob.user_id,
        |f| {
            assert_eq!(f.parent.map(|p| p.role), Some(None));
            Err::<(), _>(deny())
        },
    )
    .await
    .expect_err("denied");
    assert_eq!(err.code(), ErrorCode::Forbidden);
    let (web, facts) = groups::create_decided(
        &t.db,
        &ctx("req-sub"),
        GroupParent::Group(acme.group_id),
        &sub,
        alice.user_id,
        |f| Ok(*f),
    )
    .await
    .expect("subgroup");
    assert_eq!(
        facts.parent.map(|p| (p.group, p.namespace, p.role)),
        Some((acme.group_id, acme.namespace_id, Some(Role::Owner)))
    );
    let record: GroupRecord = groups::load(&t.db, web.group_id, Some(bob.user_id))
        .await
        .expect("load")
        .expect("exists");
    assert_eq!(record.path.full_path(), "acme/web");
    assert_eq!(record.parent_id, Some(acme.group_id));
    assert_eq!(record.role, None);
    assert_eq!(record.namespace_id, web.namespace_id);
    assert_eq!(record.description, "synthetic");
    let owner_view = groups::load(&t.db, web.group_id, Some(alice.user_id))
        .await
        .expect("load")
        .expect("exists");
    assert_eq!(owner_view.role, Some(Role::Owner));
    assert_eq!(
        groups::load(&t.db, GroupId::new(9999), None)
            .await
            .expect("absent"),
        None
    );
    let patch = GroupPatch {
        display_name: Some("Web team".into()),
        description: Some("synthetic".into()),
    };
    let (updated, facts) = groups::update_decided(
        &t.db,
        &ctx("req-group-update"),
        web.group_id,
        alice.user_id,
        &patch,
        |f: &GroupUpdateFacts| Ok(*f),
    )
    .await
    .expect("update");
    // The description did not change: only the display name is named.
    assert_eq!(updated, Updated::Changed(vec![ChangedField::DisplayName]));
    assert_eq!(facts.group.role, Some(Role::Owner));
    let err = groups::update_decided(
        &t.db,
        &ctx("req-group-empty"),
        web.group_id,
        alice.user_id,
        &GroupPatch::default(),
        |_| Ok(()),
    )
    .await
    .expect_err("empty");
    assert_reason(&err, ErrorCode::GroupRequestInvalid, "empty_update");
    let mut su = t.superuser().await;
    let details: serde_json::Value =
        sqlx::query_scalar("SELECT details FROM forge.audit_log WHERE action = 'group.updated'")
            .fetch_one(&mut su)
            .await
            .expect("one group.updated row");
    assert_eq!(
        details,
        json!({"group_id": web.group_id.get(), "fields": ["display_name"]})
    );
    t.drop_now().await;
}

#[tokio::test]
async fn load_parent_names_groups_users_and_refuses_projects() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let acme = group(&t, "acme", None, alice.user_id).await;
    let web = create_ok(&t, &project("web", alice.namespace_id, alice.user_id)).await;
    assert_eq!(
        load_parent(&t.db, acme.namespace_id, Some(alice.user_id))
            .await
            .expect("group"),
        Some(ParentRecord::Group {
            namespace: acme.namespace_id,
            group: acme.group_id,
            role: Some(Role::Owner)
        })
    );
    assert_eq!(
        load_parent(&t.db, alice.namespace_id, None)
            .await
            .expect("user"),
        Some(ParentRecord::User {
            namespace: alice.namespace_id,
            owner: alice.user_id
        })
    );
    assert_eq!(
        load_parent(&t.db, web.namespace_id, Some(alice.user_id))
            .await
            .expect("project"),
        Some(ParentRecord::Project)
    );
    assert_eq!(
        load_parent(&t.db, NamespaceId::new(9999), None)
            .await
            .expect("absent"),
        None
    );
    t.drop_now().await;
}

/// `FOR UPDATE NOWAIT` on the project row from another connection.
async fn row_free(su: &mut PgConnection, id: i64) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(&format!(
        "BEGIN; SELECT id FROM forge.projects WHERE id = {id} FOR UPDATE NOWAIT; ROLLBACK"
    ))
    .execute(&mut *su)
    .await
    .map(|_| ())
}

/// A denial and an unchanged update roll back explicitly (`ChangeTx::settle`):
/// the project row is free the moment the call returns. A dropped
/// transaction would roll back later, from the pool's return task (sqlx
/// 0.8; handoff D4), and `FOR UPDATE NOWAIT` could fail here.
#[tokio::test]
async fn a_denial_or_an_unchanged_update_releases_the_row_lock_at_once() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let web = create_ok(&t, &project("web", alice.namespace_id, alice.user_id)).await;
    let mut su = t.superuser().await;
    let id = web.project_id.get();
    let patch = ProjectPatch {
        display_name: Some("Project web".into()),
        description: None,
    };
    for round in 0..20 {
        let denied = update_decided(
            &t.db,
            &ctx(&format!("req-deny-{round}")),
            web.project_id,
            alice.user_id,
            &patch,
            |_| Err::<(), _>(deny()),
        )
        .await;
        assert!(denied.is_err());
        row_free(&mut su, id)
            .await
            .expect("the denial released the row");
        let (unchanged, ()) = update_decided(
            &t.db,
            &ctx(&format!("req-same-{round}")),
            web.project_id,
            alice.user_id,
            &patch,
            |_| Ok(()),
        )
        .await
        .expect("unchanged");
        assert_eq!(unchanged, Updated::Unchanged);
        row_free(&mut su, id)
            .await
            .expect("the unchanged update released the row");
        let (same, ()) = change_visibility_decided(
            &t.db,
            &ctx(&format!("req-vis-{round}")),
            web.project_id,
            alice.user_id,
            Visibility::Private,
            |_| Ok(()),
        )
        .await
        .expect("unchanged");
        assert_eq!(same, VisibilityOutcome::Unchanged);
        row_free(&mut su, id)
            .await
            .expect("the unchanged visibility released the row");
    }
    t.drop_now().await;
}
