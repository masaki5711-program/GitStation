#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The development status service (M3 I1d; plan 02 §5.2, §11.2) against
//! the development database (ADR-0011), through `Feed` with its decision
//! (`prove_visible`): the paged walk of every work item (one `as_of_seq` on
//! every page, a visible-set change is 410, another integration's or a feed
//! cursor is 400), a reference of no bound kanflow project (404), and the
//! `feed_cursor` it answers (the feed's bound). Merge requests and their
//! links are forge-db's tests (`feed::tests`: open / merged / closed, no
//! checks vs success, 02 §8.2) — the review commands are forge-review's to
//! call. Bindings and work items are fixtures. Synthetic data only.

use std::sync::Arc;
use std::time::Duration;

use forge_auth::{
    prove, AccountKind, Action, Authorized, Credential, IntegrationFeedCtx, Principal, Scope,
};
use forge_core::{
    CursorKeys, DevStatusPosition, FeedPosition, IntegrationId, IntegrationKind, ProjectId,
    RequestId, UserId, WorkItemRef,
};
use forge_db::change::{Actor, ChangeContext};
use forge_db::integrations::{self, NewIntegration};
use forge_db::projects::{self, CreatedProject, NewProject};
use forge_db::service_accounts::{self, NewServiceAccount};
use forge_db::subscribers::{self, Progress, Start};
use forge_db::test_support::*;
use forge_db::users::{self, NewUser};
use forge_db::work_items::WorkItemSync;
use forge_errors::{ErrorCode, ForgeError};
use forge_integ::{DevStatus, Feed, MrStateFilter, PageQuery, WorkItemQuery};

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

struct World {
    t: FreshDb,
    alice: UserId,
    namespace: forge_core::NamespaceId,
    web: CreatedProject,
    sa: UserId,
    integration: IntegrationId,
    feed: Feed,
    keys: Arc<CursorKeys>,
}

async fn account(t: &FreshDb, owner: UserId, name: &str) -> UserId {
    service_accounts::create(
        &t.db,
        &system(&format!("req-sa-{name}")),
        &NewServiceAccount {
            username: name.to_owned(),
            display_name: name.to_owned(),
            owner,
            purpose: "Kanflow reads the development status".to_owned(),
        },
        owner,
    )
    .await
    .expect("service account")
    .user_id
}

async fn integration_for(t: &FreshDb, sa: UserId, by: UserId) -> IntegrationId {
    integrations::create(
        &t.db,
        &system(&format!("req-int-{}", sa.get())),
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

/// alice's project `web` bound to kanflow project `kf-1`; work-item-sync
/// registered at the head and caught up.
async fn world() -> World {
    let t = FreshDb::migrated().await;
    let alice = users::create(
        &t.db,
        &system("req-user-alice"),
        &NewUser::human("alice", "Alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let web = projects::create(
        &t.db,
        &system("req-project-web"),
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("web");
    let sa = account(&t, alice.user_id, "kanflow-sync").await;
    let integration = integration_for(&t, sa, alice.user_id).await;
    let keys = Arc::new(CursorKeys::parse(&format!("5 {}\n", "3c".repeat(32))).expect("keys"));
    let feed = Feed::new(t.db.clone(), keys.clone());
    let w = World {
        t,
        alice: alice.user_id,
        namespace: alice.namespace_id,
        web,
        sa,
        integration,
        feed,
        keys,
    };
    bind(&w, w.integration, w.sa, w.web.project_id, "kf-1").await;
    subscribers::register(&w.t.db, &WorkItemSync, Start::Head)
        .await
        .expect("register");
    drain(&w.t).await;
    w
}

/// A binding as the bind command writes it (row + Reporter membership).
async fn bind(
    w: &World,
    integration: IntegrationId,
    sa: UserId,
    project: ProjectId,
    external: &str,
) {
    w.t.commit_audited(
        ("project", project.get()),
        &[
            format!(
                "INSERT INTO forge.integration_bindings (integration_id, project_id, \
                 external_project_id, granted_membership, created_by) \
                 VALUES ({}, {}, '{external}', true, {})",
                integration.get(),
                project.get(),
                w.alice.get()
            ),
            format!(
                "INSERT INTO forge.project_members (project_id, user_id, role) \
                 VALUES ({}, {}, 'reporter') ON CONFLICT DO NOTHING",
                project.get(),
                sa.get()
            ),
        ],
    )
    .await;
}

async fn unbind(w: &World, project: ProjectId) {
    w.t.commit_audited(
        ("project", project.get()),
        &[
            format!(
                "DELETE FROM forge.integration_bindings WHERE project_id = {}",
                project.get()
            ),
            format!(
                "DELETE FROM forge.project_members WHERE project_id = {} AND user_id = {}",
                project.get(),
                w.sa.get()
            ),
        ],
    )
    .await;
}

async fn drain(t: &FreshDb) {
    let sequence = || async {
        forge_db::sequencer::run_once(
            &t.db,
            forge_db::sequencer::BatchSize::MAX,
            forge_db::sequencer::AuditSequencing::Skip,
        )
        .await
        .expect("sequencer")
    };
    sequence().await;
    loop {
        match subscribers::run_once(&t.db, &WorkItemSync)
            .await
            .expect("step")
        {
            Progress::Handled { .. } => {}
            Progress::CaughtUp => break,
            other @ (Progress::Busy
            | Progress::Disabled
            | Progress::Failed(_)
            | Progress::Stuck(_)
            | Progress::Quarantined(_)) => panic!("unexpected {other:?}"),
        }
        sequence().await;
    }
}

/// Work items `PBI-<n>` of `external` (as detection creates them; no link).
async fn work_items(w: &World, external: &str, numbers: std::ops::RangeInclusive<u32>) {
    let values: Vec<String> = numbers
        .map(|n| format!("({}, '{external}', 'PBI-{n}')", w.integration.get()))
        .collect();
    w.t.exec_superuser(&format!(
        "INSERT INTO forge.work_items (integration_id, external_project_id, ref) VALUES {}",
        values.join(", ")
    ))
    .await;
}

fn token(sa: UserId) -> Principal {
    Principal::authenticated(
        sa,
        false,
        true,
        Credential::Token(vec![Scope::ReadApi]),
        AccountKind::ServiceAccount,
    )
}

async fn proof(w: &World, action: Action) -> Authorized<IntegrationFeedCtx> {
    let who = token(w.sa);
    let loaded = w
        .feed
        .target(w.integration)
        .await
        .expect("target")
        .expect("integration");
    prove(
        &who,
        action,
        IntegrationFeedCtx::new(loaded, who.user(), false),
    )
    .expect("proof")
}

async fn status_of(
    w: &World,
    reference: &str,
    project: Option<&str>,
) -> Result<DevStatus, ForgeError> {
    let p = proof(w, Action::IntegrationDevStatusRead).await;
    w.feed
        .work_item_dev_status(
            &p,
            &WorkItemQuery {
                reference: WorkItemRef::parse(reference).expect("ref"),
                project_id: project.map(str::to_owned),
                state: MrStateFilter::All,
            },
        )
        .await
}

async fn page(w: &World, cursor: Option<&str>, limit: usize) -> Result<DevStatus, ForgeError> {
    let p = proof(w, Action::IntegrationDevStatusRead).await;
    w.feed
        .dev_status_page(
            &p,
            &PageQuery {
                cursor: cursor.map(str::to_owned),
                limit,
                state: MrStateFilter::All,
            },
        )
        .await
}

fn reason_of(err: &ForgeError) -> String {
    err.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

const AGE: Duration = Duration::from_secs(3600);

fn feed_position(w: &World, cursor: &str) -> FeedPosition {
    FeedPosition::open(&w.keys, w.integration, cursor, AGE).expect("feed cursor")
}

/// Plan 11 I1 test column: the paged walk returns every work item of the
/// bound kanflow projects exactly once in id order (an unbound kanflow
/// project's never), every page names the first page's `as_of_seq` and
/// `feed_cursor`, and the last page has no next cursor. A binding added
/// between two pages is 410 `visible_set_changed`; a feed cursor is no page
/// cursor (400).
#[tokio::test]
async fn bulk_dev_status_paginates_all() {
    let w = world().await;
    work_items(&w, "kf-1", 1..=7).await;
    work_items(&w, "kf-unbound", 1..=2).await;
    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut as_of: Option<i64> = None;
    let mut pages = 0;
    loop {
        let got = page(&w, cursor.as_deref(), 3).await.expect("page");
        pages += 1;
        if let Some(first) = as_of {
            assert_eq!(
                got.as_of_seq, first,
                "every page names the first page's position"
            );
        }
        as_of = Some(got.as_of_seq);
        assert_eq!(feed_position(&w, &got.feed_cursor).seq, got.as_of_seq);
        for item in &got.work_items {
            assert_eq!(item.project_id, "kf-1");
            assert!(item.merge_requests.is_empty());
            seen.push(item.reference.clone());
        }
        assert_eq!(got.has_more, got.next_cursor.is_some());
        match got.next_cursor {
            Some(next) => {
                let at = DevStatusPosition::open(&w.keys, w.integration, &next, AGE)
                    .expect("page cursor");
                assert_eq!(at.feed.seq, got.as_of_seq);
                cursor = Some(next);
            }
            None => break,
        }
    }
    assert_eq!(pages, 3);
    assert_eq!(
        seen,
        (1..=7).map(|n| format!("PBI-{n}")).collect::<Vec<_>>()
    );
    // A new binding between two pages changes the visible set: 410.
    let first = page(&w, None, 3).await.expect("first page");
    let api = projects::create(
        &w.t.db,
        &system("req-project-api"),
        &NewProject::new("api", w.namespace, "Api", w.alice),
    )
    .await
    .expect("api");
    bind(&w, w.integration, w.sa, api.project_id, "kf-2").await;
    let gone = page(&w, first.next_cursor.as_deref(), 3)
        .await
        .expect_err("visible set changed");
    assert_eq!(gone.code(), ErrorCode::FeedCursorExpired);
    assert_eq!(reason_of(&gone), "visible_set_changed");
    // A feed cursor is no page cursor.
    let bad = page(&w, Some(&first.feed_cursor), 3)
        .await
        .expect_err("a feed cursor");
    assert_eq!(bad.code(), ErrorCode::FeedCursorInvalid);
    w.t.drop_now().await;
}

/// Plan 11 I1 test column `unbound_ref_is_404`: a reference of a kanflow
/// project no visible binding has is FG-REPO-0005 `reason=unbound_ref`; a
/// bound reference nobody named yet is an empty status (not a 404); once
/// the project is unbound, every reference is 404.
#[tokio::test]
async fn unbound_ref_is_404() {
    let w = world().await;
    let other = status_of(&w, "PBI-1", Some("kf-other"))
        .await
        .expect_err("unbound kanflow project");
    assert_eq!(other.code(), ErrorCode::NotFound);
    assert_eq!(reason_of(&other), "unbound_ref");
    let unnamed = status_of(&w, "PBI-404", Some("kf-1"))
        .await
        .expect("bound, nothing named");
    assert_eq!(unnamed.work_items.len(), 1);
    assert_eq!(unnamed.work_items[0].reference, "PBI-404");
    assert!(unnamed.work_items[0].merge_requests.is_empty());
    unbind(&w, w.web.project_id).await;
    let gone = status_of(&w, "PBI-404", None)
        .await
        .expect_err("no binding");
    assert_eq!(gone.code(), ErrorCode::NotFound);
    w.t.drop_now().await;
}

/// The status names the feed's bound (`min(high_seq, work-item-sync)`) as
/// `as_of_seq`, and `feed_cursor` is the head cursor's position then: the
/// two answers agree; one work item per bound kanflow project without
/// `project_id`.
#[tokio::test]
async fn the_status_answers_the_feed_position_it_reflects() {
    let w = world().await;
    let api = projects::create(
        &w.t.db,
        &system("req-project-api"),
        &NewProject::new("api", w.namespace, "Api", w.alice),
    )
    .await
    .expect("api");
    bind(&w, w.integration, w.sa, api.project_id, "kf-2").await;
    drain(&w.t).await;
    let status = status_of(&w, "PBI-9", None).await.expect("status");
    let projects: Vec<&str> = status
        .work_items
        .iter()
        .map(|i| i.project_id.as_str())
        .collect();
    assert_eq!(projects, ["kf-1", "kf-2"]);
    let head = {
        let p = proof(&w, Action::IntegrationEventCursorRead).await;
        w.feed.head_cursor(&p).await.expect("head")
    };
    let head = feed_position(&w, &head);
    let at = feed_position(&w, &status.feed_cursor);
    assert_eq!(at, head, "the same position as GET …/events/cursor");
    assert_eq!(at.seq, status.as_of_seq);
    // Another integration's account never reaches this integration.
    let other_sa = account(&w.t, w.alice, "kanflow-other").await;
    let _other = integration_for(&w.t, other_sa, w.alice).await;
    let loaded = w
        .feed
        .target(w.integration)
        .await
        .expect("target")
        .expect("integration");
    let foreign = token(other_sa);
    let refused = prove(
        &foreign,
        Action::IntegrationDevStatusRead,
        IntegrationFeedCtx::new(loaded, foreign.user(), false),
    )
    .expect_err("another integration's account");
    assert_eq!(refused.code(), ErrorCode::NotFound);
    w.t.drop_now().await;
}
