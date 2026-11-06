//! M3 X3 (plan 03 §7.1, plan 02 §2, ADR-0016 Amendment X3): a kanflow
//! project with several repositories, a shared spec repository in several
//! kanflow projects, one resolving binding per repository, the binding's
//! setting and the membership a shared binding granted. Synthetic data
//! only.

use super::*;

async fn bind_with(
    w: &World,
    project: ProjectId,
    external: &str,
    resolves: bool,
) -> Result<(BindOutcome, Binding), ForgeError> {
    bind_as_decided(
        &w.t.db,
        &ctx("req-bind-x3"),
        project,
        w.alice.user_id,
        &NewBinding {
            integration: w.integration,
            external_project_id: external,
            resolves_work_items: resolves,
        },
        None,
        |_| Ok(()),
    )
    .await
    .map(|(outcome, binding, ())| (outcome, binding))
}

async fn shared(w: &World, project: ProjectId, external: &str) -> Binding {
    let (outcome, binding) = bind_with(w, project, external, false)
        .await
        .unwrap_or_else(|e| panic!("shared bind {external}: {e:?}"));
    assert!(matches!(outcome, BindOutcome::Bound { .. }), "{outcome:?}");
    binding
}

async fn unbind_one(
    w: &World,
    project: ProjectId,
    binding: i64,
) -> Result<UnbindOutcome, ForgeError> {
    unbind_binding_decided(
        &w.t.db,
        &ctx("req-unbind-x3"),
        project,
        w.alice.user_id,
        w.integration,
        binding,
        None,
        |_| Ok(()),
    )
    .await
    .map(|(outcome, ())| outcome)
}

async fn set_resolving(
    w: &World,
    project: ProjectId,
    binding: i64,
    resolves: bool,
) -> Result<(BindingUpdate, Binding), ForgeError> {
    update_binding_decided(
        &w.t.db,
        &ctx("req-binding-change"),
        project,
        w.alice.user_id,
        w.integration,
        binding,
        resolves,
        None,
        |_| Ok(()),
    )
    .await
    .map(|(outcome, binding, ())| (outcome, binding))
}

async fn granted_of(t: &FreshDb, binding: i64) -> bool {
    sqlx::query_scalar(
        "SELECT granted_membership FROM forge.integration_bindings WHERE generation = $1",
    )
    .bind(binding)
    .fetch_one(t.db.write_pool())
    .await
    .expect("granted")
}

#[tokio::test]
async fn a_shared_spec_project_binds_to_two_kanflow_projects() {
    let w = world().await;
    let first = shared(&w, w.web, "P1").await;
    let second = shared(&w, w.web, "P2").await;
    assert!(first.id < second.id);
    assert!(!first.resolves_work_items);
    // One Reporter membership, granted by the first binding only.
    assert_eq!(
        role_of(&w.t, w.web, w.sa).await.as_deref(),
        Some("reporter")
    );
    assert!(first.granted_membership);
    assert!(!second.granted_membership);
    // The same pair again is unchanged; with the other setting refused.
    let (again, found) = bind_with(&w, w.web, "P1", false).await.expect("again");
    assert_eq!(again, BindOutcome::Unchanged);
    assert_eq!(found.id, first.id);
    let other = bind_with(&w, w.web, "P1", true)
        .await
        .expect_err("other setting");
    assert_eq!(other.code(), ErrorCode::BindingConflict);
    assert_eq!(
        ctx_of(&other, "reason").as_deref(),
        Some("bound_with_other_setting")
    );
    // A resolving binding is refused next to shared ones.
    let resolving = bind_with(&w, w.web, "P3", true)
        .await
        .expect_err("resolving");
    assert_eq!(resolving.code(), ErrorCode::BindingConflict);
    assert_eq!(
        ctx_of(&resolving, "reason").as_deref(),
        Some("shared_bindings_present")
    );
    // The kanflow project lists its repositories with their paths.
    let api = project(&w.t, &w.alice, "api").await;
    bound(&w, api, w.integration, "P1").await;
    let p1 = external_bindings(&w.t.db, w.integration, Some("P1"))
        .await
        .expect("P1");
    let paths: Vec<_> = p1
        .iter()
        .map(|b| b.path.clone().unwrap_or_default())
        .collect();
    assert_eq!(paths, vec!["alice/api".to_owned(), "alice/web".to_owned()]);
    assert_eq!(
        p1.iter().filter(|b| b.binding.resolves_work_items).count(),
        1
    );
    assert_eq!(
        external_bindings(&w.t.db, w.integration, None)
            .await
            .expect("all")
            .len(),
        3
    );
    assert_eq!(
        binding(&w.t.db, w.integration, second.id)
            .await
            .expect("one"),
        Some(second)
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_repository_with_code_refuses_a_second_kanflow_project() {
    let w = world().await;
    bound(&w, w.web, w.integration, "P1").await;
    for resolves in [true, false] {
        let e = bind_with(&w, w.web, "P2", resolves)
            .await
            .expect_err("second kanflow project");
        assert_eq!(e.code(), ErrorCode::BindingConflict);
        assert_eq!(
            ctx_of(&e, "reason").as_deref(),
            Some("bound_to_other_external_project"),
            "resolves={resolves}"
        );
    }
    assert_eq!(project_bindings(&w.t.db, w.web).await.expect("b").len(), 1);
    w.t.drop_now().await;
}

#[tokio::test]
async fn unbinding_one_shared_binding_keeps_the_membership_until_the_last() {
    let w = world().await;
    let first = shared(&w, w.web, "P1").await;
    let second = shared(&w, w.web, "P2").await;
    let out = unbind_one(&w, w.web, first.id).await.expect("unbind first");
    assert!(!out.membership_removed);
    // The grant moved to the remaining binding; the Reporter stays.
    assert!(granted_of(&w.t, second.id).await);
    assert_eq!(
        role_of(&w.t, w.web, w.sa).await.as_deref(),
        Some("reporter")
    );
    // Another integration's binding id, or one already gone: not bound.
    let gone = unbind_one(&w, w.web, first.id).await.expect_err("gone");
    assert_eq!(gone.code(), ErrorCode::NotFound);
    let out = unbind_one(&w, w.web, second.id).await.expect("unbind last");
    assert!(out.membership_removed);
    assert_eq!(role_of(&w.t, w.web, w.sa).await, None);
    let actions = audit_actions(&w.t, w.web).await;
    assert_eq!(
        actions
            .iter()
            .filter(|a| *a == "integration.unbound")
            .count(),
        2,
        "{actions:?}"
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_shared_binding_becomes_resolving_only_when_alone_and_never_back() {
    let w = world().await;
    let first = shared(&w, w.web, "P1").await;
    let second = shared(&w, w.web, "P2").await;
    let e = set_resolving(&w, w.web, first.id, true)
        .await
        .expect_err("not alone");
    assert_eq!(
        ctx_of(&e, "reason").as_deref(),
        Some("shared_bindings_present")
    );
    unbind_one(&w, w.web, second.id).await.expect("unbind");
    let (outcome, changed) = set_resolving(&w, w.web, first.id, true)
        .await
        .expect("now alone");
    assert_eq!(outcome, BindingUpdate::Updated);
    assert!(changed.resolves_work_items);
    let (outcome, _) = set_resolving(&w, w.web, first.id, true)
        .await
        .expect("same value");
    assert_eq!(outcome, BindingUpdate::Unchanged);
    let e = set_resolving(&w, w.web, first.id, false)
        .await
        .expect_err("fixed");
    assert_eq!(e.code(), ErrorCode::BindingConflict);
    assert_eq!(ctx_of(&e, "reason").as_deref(), Some("resolution_fixed"));
    let absent = set_resolving(&w, w.web, first.id + 1000, true)
        .await
        .expect_err("absent");
    assert_eq!(absent.code(), ErrorCode::NotFound);
    assert!(audit_actions(&w.t, w.web)
        .await
        .contains(&"integration.binding_changed".to_owned()));
    w.t.drop_now().await;
}

#[tokio::test]
async fn work_items_resolve_only_through_the_resolving_binding() {
    let w = world().await;
    shared(&w, w.web, "P1").await;
    let mut conn = w.t.db.write_pool().acquire().await.expect("conn");
    assert_eq!(
        crate::work_items::binding_of(&mut conn, w.web)
            .await
            .expect("binding"),
        None,
        "a shared spec project detects no work items"
    );
    let api = project(&w.t, &w.alice, "api").await;
    bound(&w, api, w.integration, "P1").await;
    let resolving = crate::work_items::binding_of(&mut conn, api)
        .await
        .expect("binding")
        .expect("resolving");
    assert_eq!(resolving.external_project_id, "P1");
    drop(conn);
    w.t.drop_now().await;
}
