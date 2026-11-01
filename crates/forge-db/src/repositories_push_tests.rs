//! `record_push` against a migrated database (M1 phase 9 MR2): one audit row
//! and one `repository.pushed` event per push, deduplicated by request id
//! and updates, with explicit truncation. Inspection uses a superuser
//! connection.

use forge_core::{
    EventType, ObjectId, PushRecorder, PushUpdateKind, PushedRef, RefName, UserId, VisibilityClass,
};
use forge_errors::ErrorCode;
use sqlx::postgres::PgConnection;

use super::*;
use crate::projects::{self, CreatedProject, NewProject};
use crate::test_support::*;
use crate::users::{self, NewUser};

/// actor_kind, actor_user_id, resource_type, resource_id, project_id,
/// request_id, details.
type AuditRow = (
    String,
    Option<i64>,
    String,
    i64,
    Option<i64>,
    String,
    serde_json::Value,
);
/// event_type, aggregate_type, aggregate_id, project_id, actor_user_id,
/// request_id, dedup_key, payload.
type EventRow = (
    String,
    String,
    i64,
    Option<i64>,
    Option<i64>,
    String,
    Option<String>,
    serde_json::Value,
);

fn rid(raw: &str) -> RequestId {
    RequestId::parse(raw).expect("request id")
}

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, rid(request))
}

/// A project of a new user `name` and that user.
async fn project(t: &FreshDb, name: &str) -> (CreatedProject, UserId) {
    let user = users::create(
        &t.db,
        &system(&format!("req-user-{name}")),
        &NewUser::human(name, name, &format!("{name}@example.test")),
    )
    .await
    .expect("user");
    let created = projects::create(
        &t.db,
        &system(&format!("req-project-{name}")),
        &NewProject::new("app", user.namespace_id, "App", user.user_id),
    )
    .await
    .expect("project");
    (created, user.user_id)
}

fn oid(c: char) -> ObjectId {
    ObjectId::parse(&c.to_string().repeat(40)).expect("oid")
}

fn update(name: &str, old: char, new: char, kind: PushUpdateKind) -> PushedRef {
    let new = oid(new);
    let commits = if kind == PushUpdateKind::Delete {
        Vec::new()
    } else {
        vec![new.clone()]
    };
    PushedRef {
        name: RefName::parse(name).expect("ref"),
        old: oid(old),
        new,
        kind,
        total_commits: commits.len() as u64,
        commits,
    }
}

fn pusher(user: UserId, request: &str) -> Pusher {
    Pusher {
        request_id: rid(request),
        user,
        credential_id: None,
        source_ip: Some("192.0.2.10".parse().expect("ip")),
        user_agent: Some("git/2.47.0".to_owned()),
    }
}

async fn audit_rows(su: &mut PgConnection) -> Vec<AuditRow> {
    sqlx::query_as(
        "SELECT actor_kind, actor_user_id, resource_type, resource_id, project_id, request_id, \
         details FROM forge.audit_log WHERE action = 'repository.pushed' ORDER BY id",
    )
    .fetch_all(su)
    .await
    .expect("audit rows")
}

async fn event_rows(su: &mut PgConnection) -> Vec<EventRow> {
    sqlx::query_as(
        "SELECT event_type, aggregate_type, aggregate_id, project_id, actor_user_id, request_id, \
         dedup_key, payload FROM forge.events WHERE event_type = 'repository.pushed' ORDER BY id",
    )
    .fetch_all(su)
    .await
    .expect("event rows")
}

#[tokio::test]
async fn every_push_writes_one_audit_row_and_one_event() {
    let t = FreshDb::migrated().await;
    let (p, alice) = project(&t, "alice").await;
    let repo = p.repository_id;
    let push = PushRecord {
        repository_id: repo,
        updates: vec![
            update("refs/tags/v1", '0', 'b', PushUpdateKind::Create),
            update("refs/heads/main", 'a', 'b', PushUpdateKind::FastForward),
            update("refs/heads/old", 'c', '0', PushUpdateKind::Delete),
            update("refs/heads/rewrite", 'd', 'e', PushUpdateKind::Force),
        ],
    };
    let who = pusher(alice, "req-push-1");
    // Through the trait forge-web uses.
    let recorder: &dyn PushRecorder = &t.db;
    let done = recorder.record_push(&who, &push).await.expect("recorded");
    assert_eq!(done, PushRecorded::Recorded);

    let mut su = t.superuser().await;
    let audits = audit_rows(&mut su).await;
    assert_eq!(audits.len(), 1, "{audits:?}");
    let (actor_kind, actor_user, resource_type, resource_id, project_id, request, details) =
        &audits[0];
    assert_eq!(actor_kind, "user");
    assert_eq!(*actor_user, Some(alice.get()));
    assert_eq!(resource_type, "project");
    assert_eq!(*resource_id, p.project_id.get());
    assert_eq!(*project_id, Some(p.project_id.get()));
    assert_eq!(request, "req-push-1");
    assert_eq!(details["total_refs"], 4);
    assert_eq!(details["refs_truncated"], false);
    assert_eq!(details["total_commits"], 3);
    assert_eq!(details["commits_truncated"], false);
    let refs: Vec<(&str, &str)> = details["refs"]
        .as_array()
        .expect("refs")
        .iter()
        .map(|r| {
            (
                r["ref"].as_str().unwrap_or_default(),
                r["kind"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    // Sorted by ref name, each update classified.
    assert_eq!(
        refs,
        [
            ("refs/heads/main", "fast_forward"),
            ("refs/heads/old", "delete"),
            ("refs/heads/rewrite", "force"),
            ("refs/tags/v1", "create"),
        ]
    );
    let source: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT host(source_ip), user_agent FROM forge.audit_log WHERE action = 'repository.pushed'",
    )
    .fetch_one(&mut su)
    .await
    .expect("source");
    assert_eq!(
        source,
        (Some("192.0.2.10".to_owned()), Some("git/2.47.0".to_owned()))
    );

    let events = event_rows(&mut su).await;
    assert_eq!(events.len(), 1, "{events:?}");
    let (
        event_type,
        aggregate,
        aggregate_id,
        event_project,
        event_actor,
        event_request,
        key,
        payload,
    ) = &events[0];
    assert_eq!(event_type, "repository.pushed");
    assert_eq!(
        EventType::RepositoryPushed.visibility_class(),
        VisibilityClass::Integration
    );
    assert_eq!(aggregate, "repository");
    assert_eq!(*aggregate_id, repo.get());
    assert_eq!(*event_project, Some(p.project_id.get()));
    assert_eq!(*event_actor, Some(alice.get()));
    assert_eq!(event_request, "req-push-1");
    let expected = push_dedup_key(&rid("req-push-1"), &push).expect("key");
    assert_eq!(key.as_deref(), Some(expected.as_str()));
    let prefix = format!("repository_pushed:{}:", repo.get());
    let hash = expected.as_str().strip_prefix(&prefix).expect("prefix");
    assert_eq!(hash.len(), 64);
    assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()));
    // Small push: the payload is the details.
    assert_eq!(payload, details);
    t.drop_now().await;
}

#[tokio::test]
async fn replayed_push_is_deduped() {
    let t = FreshDb::migrated().await;
    let (p, bob) = project(&t, "bob").await;
    let main = update("refs/heads/main", 'a', 'b', PushUpdateKind::FastForward);
    let tag = update("refs/tags/v2", '0', 'b', PushUpdateKind::Create);
    let push = PushRecord {
        repository_id: p.repository_id,
        updates: vec![main.clone(), tag.clone()],
    };
    let ctx = push_context(&pusher(bob, "req-push-replay"));
    assert_eq!(
        record_push(&t.db, &ctx, &push).await.expect("first"),
        PushRecorded::Recorded
    );
    // A retry, and the runbook replay (another order, no IP: same key).
    assert_eq!(
        record_push(&t.db, &ctx, &push).await.expect("retry"),
        PushRecorded::Deduped
    );
    let replay = PushRecord {
        repository_id: p.repository_id,
        updates: vec![tag, main],
    };
    let mut replay_ctx = push_context(&Pusher {
        source_ip: None,
        user_agent: None,
        ..pusher(bob, "req-push-replay")
    });
    replay_ctx.actor_label = Some("forge-cli:operator".to_owned());
    assert_eq!(
        record_push(&t.db, &replay_ctx, &replay)
            .await
            .expect("replay"),
        PushRecorded::Deduped
    );
    let mut su = t.superuser().await;
    assert_eq!(audit_rows(&mut su).await.len(), 1);
    assert_eq!(event_rows(&mut su).await.len(), 1);
    t.drop_now().await;
}

#[tokio::test]
async fn aba_pushes_are_three_events() {
    let t = FreshDb::migrated().await;
    let (p, cleo) = project(&t, "cleo").await;
    let repo = p.repository_id;
    let pushes = [
        (
            "req-aba-1",
            update("refs/heads/main", 'a', 'b', PushUpdateKind::FastForward),
        ),
        (
            "req-aba-2",
            update("refs/heads/main", 'b', 'a', PushUpdateKind::Force),
        ),
        (
            "req-aba-3",
            update("refs/heads/main", 'a', 'b', PushUpdateKind::FastForward),
        ),
    ];
    for (request, u) in &pushes {
        let push = PushRecord {
            repository_id: repo,
            updates: vec![u.clone()],
        };
        let done = record_push(&t.db, &push_context(&pusher(cleo, request)), &push)
            .await
            .expect("recorded");
        assert_eq!(done, PushRecorded::Recorded, "{request}");
    }
    let mut su = t.superuser().await;
    let events = event_rows(&mut su).await;
    assert_eq!(events.len(), 3);
    let keys: std::collections::HashSet<_> = events.iter().map(|e| e.6.clone()).collect();
    assert_eq!(keys.len(), 3, "{keys:?}");
    // The third push repeats the first one's updates: only its request id
    // tells them apart.
    assert_eq!(events[0].7, events[2].7);
    assert_eq!(audit_rows(&mut su).await.len(), 3);
    t.drop_now().await;
}

#[tokio::test]
async fn payload_truncation_is_explicit() {
    let t = FreshDb::migrated().await;
    let (p, dana) = project(&t, "dana").await;
    let wide = |c: char| ObjectId::parse(&c.to_string().repeat(64)).expect("oid");
    // The largest push MR4 lets through: 1000 refs with long names, SHA-256
    // ids and 20 listed commits each.
    let updates: Vec<PushedRef> = (0..forge_core::push::MAX_PUSH_UPDATES)
        .map(|i| PushedRef {
            name: RefName::parse(&format!("refs/heads/{}-{i:04}", "x".repeat(240))).expect("ref"),
            old: wide('a'),
            new: wide('b'),
            kind: PushUpdateKind::Force,
            commits: vec![wide('c'); forge_core::push::MAX_LISTED_COMMITS],
            total_commits: 40,
        })
        .collect();
    let push = PushRecord {
        repository_id: p.repository_id,
        updates,
    };
    let done = record_push(&t.db, &push_context(&pusher(dana, "req-push-big")), &push)
        .await
        .expect("a large push is recorded, not refused");
    assert_eq!(done, PushRecorded::Recorded);
    let mut su = t.superuser().await;
    let (details, details_bytes): (serde_json::Value, i32) = sqlx::query_as(
        "SELECT details, pg_column_size(details) FROM forge.audit_log \
         WHERE action = 'repository.pushed'",
    )
    .fetch_one(&mut su)
    .await
    .expect("details");
    let (payload, payload_bytes): (serde_json::Value, i32) = sqlx::query_as(
        "SELECT payload, pg_column_size(payload) FROM forge.events \
         WHERE event_type = 'repository.pushed'",
    )
    .fetch_one(&mut su)
    .await
    .expect("payload");
    assert!(details_bytes <= 8192, "{details_bytes}");
    assert!(payload_bytes <= 65536, "{payload_bytes}");
    for v in [&details, &payload] {
        assert_eq!(v["total_refs"], 1000);
        assert_eq!(v["refs_truncated"], true);
        assert_eq!(v["total_commits"], 40_000);
        assert_eq!(v["commits_truncated"], true);
        for r in v["refs"].as_array().expect("refs") {
            assert_eq!(r["total_commits"], 40);
            assert_eq!(r["commits_truncated"], true);
        }
    }
    let listed = |v: &serde_json::Value| v["refs"].as_array().map_or(0, Vec::len);
    assert!(listed(&details) >= 1);
    assert!(listed(&payload) > listed(&details), "the event lists more");
    assert!(listed(&payload) <= 100);
    t.drop_now().await;
}

#[tokio::test]
async fn an_invalid_or_unknown_push_writes_nothing() {
    let t = FreshDb::migrated().await;
    let (p, eve) = project(&t, "eve").await;
    let ctx = push_context(&pusher(eve, "req-push-bad"));
    let bad = PushRecord {
        repository_id: p.repository_id,
        updates: vec![update(
            "refs/heads/main",
            '0',
            'b',
            PushUpdateKind::FastForward,
        )],
    };
    let err = record_push(&t.db, &ctx, &bad)
        .await
        .expect_err("inconsistent");
    assert_reason(&err, ErrorCode::ChangeRejected, "invalid_push_record");
    let unknown = PushRecord {
        repository_id: RepoId::new(987_654),
        updates: vec![update(
            "refs/heads/main",
            'a',
            'b',
            PushUpdateKind::FastForward,
        )],
    };
    let err = record_push(&t.db, &ctx, &unknown)
        .await
        .expect_err("unknown");
    assert_eq!(err.code(), ErrorCode::NotFound);
    let mut su = t.superuser().await;
    assert!(audit_rows(&mut su).await.is_empty());
    assert!(event_rows(&mut su).await.is_empty());
    t.drop_now().await;
}
