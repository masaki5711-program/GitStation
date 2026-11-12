//! The history import allowance against a migrated database (2026-11-12,
//! migration 20261210100000): allowed only on a ready repository, open
//! until it expires or is used, used once with its audit rows.

use forge_core::history_import::HistoryFinding;
use forge_core::{RequestId, UserKind};

use super::*;
use crate::change::Actor;
use crate::projects::{self, CreatedProject, NewProject};
use crate::repositories;
use crate::test_support::*;
use crate::users::{self, NewUser};

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

struct World {
    t: FreshDb,
    owner: forge_core::UserId,
    namespace: forge_core::NamespaceId,
    app: CreatedProject,
}

async fn world(ready: bool) -> World {
    let t = FreshDb::migrated().await;
    let owner = users::create(
        &t.db,
        &system("req-user-ivy"),
        &NewUser::human("ivy", "Ivy", "ivy@example.test"),
    )
    .await
    .expect("user");
    let app = projects::create(
        &t.db,
        &system("req-project-app"),
        &NewProject::new("app", owner.namespace_id, "App", owner.user_id),
    )
    .await
    .expect("project");
    if ready {
        let marked = repositories::mark_ready(&t.db, &system("req-ready"), app.repository_id)
            .await
            .expect("ready");
        assert!(matches!(marked, repositories::MarkReady::Marked { .. }));
    }
    World {
        t,
        owner: owner.user_id,
        namespace: owner.namespace_id,
        app,
    }
}

impl World {
    fn pusher(&self, request: &str) -> Pusher {
        Pusher {
            request_id: RequestId::parse(request).expect("request id"),
            user: self.owner,
            kind: UserKind::Human,
            credential_id: None,
            source_ip: None,
            user_agent: None,
        }
    }

    async fn rows(&self, action: &str) -> Vec<serde_json::Value> {
        self.t
            .json_rows(&format!(
                "SELECT actor_kind, request_id, details FROM forge.audit_log \
                 WHERE action = '{action}' ORDER BY id"
            ))
            .await
    }
}

fn finding(path: &str) -> HistoryFinding {
    HistoryFinding {
        rule: "private_key".to_owned(),
        blob: "b".repeat(40),
        path: path.to_owned(),
        commit: Some("c".repeat(40)),
    }
}

#[tokio::test]
async fn an_allowance_is_open_until_used_once_with_its_audit_rows() {
    let w = world(true).await;
    let repo = w.app.repository_id;
    assert!(!import_allowed(&w.t.db, repo).await.expect("read"));
    let allowed = allow(
        &w.t.db,
        &system("req-allow-1"),
        repo,
        Duration::from_secs(3600),
    )
    .await
    .expect("allowed");
    assert_eq!(allowed.project_id, w.app.project_id);
    assert!(!allowed.replaced);
    assert!(allowed.expires_at.ends_with('Z'), "{allowed:?}");
    assert!(import_allowed(&w.t.db, repo).await.expect("read"));
    let rows = w.rows("repository.history_import_allowed").await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["details"]["ttl_secs"], 3600);

    let used = HistoryImportUse {
        findings: vec![finding("old/id_rsa"), finding("old/key.pem")],
        total: 2,
    };
    let request = RequestId::parse("req-push-1").expect("id");
    assert!(claim_import(&w.t.db, repo, &request).await.expect("claim"));
    // Claimed: no longer open, for anyone.
    assert!(!import_allowed(&w.t.db, repo).await.expect("read"));
    let done =
        forge_core::HistoryImports::record_import(&w.t.db, &w.pusher("req-push-1"), repo, &used)
            .await
            .expect("recorded");
    assert_eq!(done, HistoryImportRecorded::Consumed);
    assert!(!import_allowed(&w.t.db, repo).await.expect("read"));
    let imported = w.rows("repository.history_imported").await;
    assert_eq!(imported.len(), 1, "{imported:?}");
    assert_eq!(imported[0]["actor_kind"], "user");
    assert_eq!(imported[0]["request_id"], "req-push-1");
    assert_eq!(imported[0]["details"]["history_findings"], 2);
    assert_eq!(imported[0]["details"]["consumed"], true);
    let secrets = w.rows("repository.history_secret_imported").await;
    assert_eq!(secrets.len(), 2, "{secrets:?}");
    assert_eq!(secrets[0]["details"]["path"], "old/id_rsa");
    assert_eq!(secrets[0]["details"]["rule"], "private_key");
    assert_eq!(secrets[0]["details"]["severity"], "warning");
    assert_eq!(secrets[0]["request_id"], "req-push-1");

    // A record by a push that holds no claim: still audited, not consumed.
    let again = record_import(
        &w.t.db,
        &system("req-push-2"),
        repo,
        &HistoryImportUse::default(),
    )
    .await
    .expect("recorded");
    assert_eq!(again, HistoryImportRecorded::AlreadyUsed);
    // A new allowance replaces the used row.
    let renewed = allow(
        &w.t.db,
        &system("req-allow-2"),
        repo,
        Duration::from_secs(60),
    )
    .await
    .expect("allowed");
    assert!(renewed.replaced);
    assert!(import_allowed(&w.t.db, repo).await.expect("read"));
}

#[tokio::test]
async fn an_expired_allowance_is_ignored() {
    let w = world(true).await;
    let repo = w.app.repository_id;
    allow(
        &w.t.db,
        &system("req-allow-x"),
        repo,
        Duration::from_secs(1),
    )
    .await
    .expect("allowed");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!import_allowed(&w.t.db, repo).await.expect("read"));
}

#[tokio::test]
async fn an_allowance_needs_a_ready_repository_and_a_bounded_ttl() {
    let w = world(false).await;
    let repo = w.app.repository_id;
    let err = allow(
        &w.t.db,
        &system("req-allow-p"),
        repo,
        Duration::from_secs(60),
    )
    .await
    .expect_err("provisioning");
    assert_reason(
        &err,
        ErrorCode::HistoryImportRefused,
        "repository_not_ready",
    );
    for ttl in [Duration::ZERO, MAX_TTL + Duration::from_secs(1)] {
        let err = allow(&w.t.db, &system("req-allow-t"), repo, ttl)
            .await
            .expect_err("ttl");
        assert_reason(&err, ErrorCode::HistoryImportRefused, "ttl_out_of_range");
    }
    let err = allow(
        &w.t.db,
        &system("req-allow-u"),
        RepoId::new(987_654),
        Duration::from_secs(60),
    )
    .await
    .expect_err("unknown");
    assert_eq!(err.code(), ErrorCode::NotFound);
    assert!(w.rows("repository.history_import_allowed").await.is_empty());
}

/// Review 2026-11-12 (HIGH): of concurrent claims exactly one wins; a
/// released claim is open again, a claim kept (record failed after refs
/// landed) never re-arms, and only a new allowance clears it.
#[tokio::test]
async fn concurrent_claims_have_exactly_one_winner() {
    let w = world(true).await;
    let repo = w.app.repository_id;
    allow(
        &w.t.db,
        &system("req-allow-c"),
        repo,
        Duration::from_secs(3600),
    )
    .await
    .expect("allowed");
    let ids: Vec<RequestId> = (0..8)
        .map(|i| RequestId::parse(&format!("req-claim-{i}")).expect("id"))
        .collect();
    let mut tasks = Vec::new();
    for id in &ids {
        let (db, id) = (w.t.db.clone(), id.clone()); // clone: each task owns its handle
        tasks.push(tokio::spawn(
            async move { claim_import(&db, repo, &id).await },
        ));
    }
    let mut claims = Vec::new();
    for task in tasks {
        claims.push(task.await.expect("task"));
    }
    let won: Vec<&RequestId> = claims
        .iter()
        .zip(&ids)
        .filter(|(c, _)| *c.as_ref().expect("claim"))
        .map(|(_, id)| id)
        .collect();
    assert_eq!(won.len(), 1, "{claims:?}");
    let winner = won[0];
    // A loser's release changes nothing; the winner's reopens it.
    let loser = ids.iter().find(|id| *id != winner).expect("a loser");
    release_import(&w.t.db, repo, loser).await.expect("release");
    assert!(!import_allowed(&w.t.db, repo).await.expect("read"));
    release_import(&w.t.db, repo, winner)
        .await
        .expect("release");
    assert!(import_allowed(&w.t.db, repo).await.expect("read"));
    // A loser cannot consume it either.
    assert!(claim_import(&w.t.db, repo, winner).await.expect("claim"));
    let ctx = system(loser.as_str());
    let done = record_import(&w.t.db, &ctx, repo, &HistoryImportUse::default())
        .await
        .expect("recorded");
    assert_eq!(done, HistoryImportRecorded::AlreadyUsed);
    // Claimed and never consumed (a record that failed): not open, and no
    // second claim, until an operator allows anew.
    assert!(!claim_import(&w.t.db, repo, loser).await.expect("claim"));
    allow(
        &w.t.db,
        &system("req-allow-d"),
        repo,
        Duration::from_secs(60),
    )
    .await
    .expect("allowed");
    assert!(claim_import(&w.t.db, repo, loser).await.expect("claim"));
}

/// Review 2026-11-12 (MEDIUM): the stored project is the repository's.
#[tokio::test]
async fn the_allowance_project_must_be_the_repository_project() {
    let w = world(true).await;
    let other = projects::create(
        &w.t.db,
        &system("req-project-other"),
        &NewProject::new("other", w.namespace, "Other", w.owner),
    )
    .await
    .expect("project");
    let mut conn = w.t.superuser().await;
    let err = sqlx::query(
        "INSERT INTO forge.repository_history_imports (repository_id, project_id, expires_at) \
         VALUES ($1, $2, now() + interval '1 hour')",
    )
    .bind(w.app.repository_id.get())
    .bind(other.project_id.get())
    .execute(&mut conn)
    .await
    .expect_err("another project");
    let constraint = err
        .as_database_error()
        .and_then(|d| d.constraint())
        .map(str::to_owned);
    assert_eq!(
        constraint.as_deref(),
        Some("repository_history_imports_repository")
    );
}
