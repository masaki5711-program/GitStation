//! `record_push` against a migrated database (M1 phase 9 MR2): one audit row
//! and one `repository.pushed` event per push, deduplicated by request id
//! and updates, with explicit truncation. Inspection uses a superuser
//! connection.

use forge_core::{
    EventType, ObjectId, PushRecorder, PushReplay, PushUpdateKind, PushedRef, RefName, UserId,
    VisibilityClass,
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
        total_commits_partial: false,
        commits,
    }
}

fn pusher(user: UserId, request: &str) -> Pusher {
    Pusher {
        request_id: rid(request),
        user,
        kind: forge_core::UserKind::Human,
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
    // Each update classified; force and delete first, then create and
    // fast-forward.
    assert_eq!(
        refs,
        [
            ("refs/heads/rewrite", "force"),
            ("refs/heads/old", "delete"),
            ("refs/tags/v1", "create"),
            ("refs/heads/main", "fast_forward"),
        ]
    );
    for (key, n) in [
        ("created", 1),
        ("fast_forward", 1),
        ("forced", 1),
        ("deleted", 1),
    ] {
        assert_eq!(details[key], n, "{key}");
    }
    assert_eq!(details["replayed_from"], serde_json::Value::Null);
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
    // Small push: the payload is the details (a complete commit listing:
    // no `push_commits_incomplete` marker, M3 R5 round 4).
    assert_eq!(payload, details);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM forge.push_commits")
        .fetch_one(&mut t.superuser().await)
        .await
        .expect("rows");
    assert_eq!(rows, 2, "the listed new commits, one row each (b and e)");
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
            total_commits_partial: false,
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

/// The runbook replay: the actor is the operator (`system` with the CLI's
/// label), the logged pusher is `replayed_from`; a logged project or pusher
/// that does not match the database is refused before anything is written.
#[tokio::test]
async fn a_replay_names_the_operator_and_checks_the_log_against_the_database() {
    let t = FreshDb::migrated().await;
    let (p, fred) = project(&t, "fred").await;
    let (other, _) = project(&t, "gina").await;
    let push = PushRecord {
        repository_id: p.repository_id,
        updates: vec![update(
            "refs/heads/main",
            'a',
            'b',
            PushUpdateKind::FastForward,
        )],
    };
    let mut ctx = ChangeContext::new(Actor::System, rid("req-push-logged"));
    ctx.actor_label = Some("forge-cli:operator".to_owned());
    let who = PushReplay {
        pusher: fred,
        credential_id: Some(42),
    };
    // Another project than the repository's, an unknown pusher.
    for (project_id, replay, field) in [
        (other.project_id, who, "project_id"),
        (
            p.project_id,
            PushReplay {
                pusher: UserId::new(987_654),
                credential_id: None,
            },
            "user_id",
        ),
    ] {
        let err = check_replay(&t.db, project_id, replay, &push)
            .await
            .expect_err(field);
        assert_reason(&err, ErrorCode::PushRecordFailed, "push_log_invalid");
        assert!(err.to_string().contains(&format!("field={field}")), "{err}");
        let err = replay_push(&t.db, &ctx, project_id, replay, &push)
            .await
            .expect_err(field);
        assert!(err.to_string().contains(&format!("field={field}")), "{err}");
    }
    let mut su = t.superuser().await;
    assert!(audit_rows(&mut su).await.is_empty());
    check_replay(&t.db, p.project_id, who, &push)
        .await
        .expect("matches");
    let done = replay_push(&t.db, &ctx, p.project_id, who, &push)
        .await
        .expect("replayed");
    assert_eq!(done, PushRecorded::Recorded);
    let audits = audit_rows(&mut su).await;
    assert_eq!(audits.len(), 1);
    let (actor_kind, actor_user, _, _, _, request, details) = &audits[0];
    assert_eq!(actor_kind, "system");
    assert_eq!(*actor_user, None);
    assert_eq!(request, "req-push-logged");
    assert_eq!(
        details["replayed_from"],
        serde_json::json!({ "user_id": fred.get(), "credential_id": 42 })
    );
    let events = event_rows(&mut su).await;
    assert_eq!(events[0].7["replayed_from"]["user_id"], fred.get());
    // The live push of the same request id and updates is the same key.
    let live = push_context(&pusher(fred, "req-push-logged"));
    assert_eq!(
        record_push(&t.db, &live, &push).await.expect("live"),
        PushRecorded::Deduped
    );
    t.drop_now().await;
}

/// R5 review round 5: the largest listing a push records (the cap) is
/// written, audit trigger included, within a third of the push path's
/// record budget; a listing past the cap keeps the cap's rows and is
/// marked incomplete.
#[tokio::test]
async fn the_largest_listing_is_recorded_within_a_third_of_the_budget() {
    let t = FreshDb::migrated().await;
    let (p, alice) = project(&t, "alice").await;
    let push = PushRecord {
        repository_id: p.repository_id,
        updates: vec![update("refs/heads/main", '0', 'b', PushUpdateKind::Create)],
    };
    let cap = forge_core::MAX_PUSHED_COMMITS;
    let all = |n: usize| PushedCommits {
        commits: (0..n)
            .map(|i| ObjectId::parse(&format!("{i:040x}")).expect("oid"))
            .collect(),
        complete: true,
    };
    let started = std::time::Instant::now();
    let done = record_push_with_commits(
        &t.db,
        &push_context(&pusher(alice, "req-push-cap")),
        &push,
        &all(cap),
    )
    .await
    .expect("recorded");
    let took = started.elapsed();
    eprintln!("push_commits: {cap} rows recorded in {took:?}");
    assert_eq!(done, PushRecorded::Recorded);
    assert!(
        took < forge_core::PUSH_RECORD_BUDGET / 3,
        "{cap} rows took {took:?}"
    );
    let mut su = t.superuser().await;
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM forge.push_commits")
        .fetch_one(&mut su)
        .await
        .expect("rows");
    assert_eq!(rows, cap as i64);
    let audits = audit_rows(&mut su).await;
    assert_eq!(audits[0].6.get("push_commits_incomplete"), None);

    // Past the cap: the cap's rows, the marker.
    let (q, bob) = project(&t, "bob").await;
    let over = PushRecord {
        repository_id: q.repository_id,
        ..push
    };
    let mut more = all(cap + 3);
    more.commits.rotate_left(3); // other ids first: they are the ones written
    let over_done = record_push_with_commits(
        &t.db,
        &push_context(&pusher(bob, "req-push-over")),
        &over,
        &more,
    )
    .await
    .expect("recorded");
    assert_eq!(over_done, PushRecorded::Recorded);
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM forge.push_commits WHERE repository_id = $1")
            .bind(q.repository_id.get())
            .fetch_one(&mut su)
            .await
            .expect("rows");
    assert_eq!(rows, cap as i64);
    let audits = audit_rows(&mut su).await;
    assert_eq!(audits[1].6["push_commits_incomplete"], true);
    t.drop_now().await;
}

/// R5 review round 5: the runbook's approved backfill (merge-blocked,
/// Mitigate 2b), run verbatim, writes the rows with their audit row in one
/// transaction; without the audit row the same insert is refused.
#[tokio::test]
async fn the_runbook_backfill_of_push_commits_writes_rows_with_their_audit_row() {
    let t = FreshDb::migrated().await;
    let (p, alice) = project(&t, "alice").await;
    let runbook = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../ops/runbooks/merge-blocked.md"),
    )
    .expect("runbook");
    let begin = runbook
        .find("-- push_commits backfill (begin)")
        .expect("begin marker");
    let end = runbook
        .find("-- push_commits backfill (end)")
        .expect("end marker");
    // psql's interpolation: `:'name'` a quoted literal, `:name` the value.
    let shas = format!("{{{},{}}}", "a".repeat(40), "b".repeat(40));
    let quoted = |v: &str| format!("'{}'", v.replace('\'', "''"));
    let mut sql = runbook[begin..end].to_owned();
    for (name, value) in [
        ("operator", "ops-o'brien"),
        ("change_id", "chg-backfill-1"),
        ("approver", "synthetic approver"),
        ("evidence", "synthetic log"),
        ("shas", shas.as_str()),
    ] {
        sql = sql.replace(&format!(":'{name}'"), &quoted(value));
    }
    for (name, value) in [
        ("project_id", p.project_id.get()),
        ("repo_id", p.repository_id.get()),
        ("pusher_id", alice.get()),
    ] {
        sql = sql.replace(&format!(":{name}"), &value.to_string());
    }
    assert!(!sql.contains(":'"), "every variable is set: {sql}");
    let mut su = t.superuser().await;
    sqlx::raw_sql(&sql)
        .execute(&mut su)
        .await
        .expect("backfill");
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT commit_sha, pusher_id FROM forge.push_commits WHERE request_id = 'chg-backfill-1' \
         ORDER BY commit_sha",
    )
    .fetch_all(&mut su)
    .await
    .expect("rows");
    assert_eq!(
        rows,
        [("a".repeat(40), alice.get()), ("b".repeat(40), alice.get())]
    );
    let details: serde_json::Value = sqlx::query_scalar(
        "SELECT details FROM forge.audit_log WHERE action = 'repository.push_commits_backfilled'",
    )
    .fetch_one(&mut su)
    .await
    .expect("audit row");
    assert_eq!(details["commits"], 2);
    assert_eq!(details["operator"], "ops-o'brien");
    let label: String = sqlx::query_scalar(
        "SELECT actor_label FROM forge.audit_log WHERE action = 'repository.push_commits_backfilled'",
    )
    .fetch_one(&mut su)
    .await
    .expect("label");
    assert_eq!(label, "merge-blocked 2b by ops-o'brien");
    // The insert alone (no audit row in its transaction) is refused.
    let alone = sql[sql.find("INSERT INTO forge.push_commits").expect("insert")..]
        .replace("COMMIT;", "")
        .replace("chg-backfill-1", "chg-backfill-2")
        .replace(&"a".repeat(40), &"c".repeat(40));
    let refused = sqlx::raw_sql(&alone)
        .execute(&mut su)
        .await
        .expect_err("an unaudited backfill must be refused");
    assert!(refused.to_string().contains("no audit row"), "{refused}");
    t.drop_now().await;
}

/// S0 review, carried into M3 I1b: a service account's push is audited as
/// what wrote it (`actor_kind = 'service_account'`), on the audit row and
/// the `repository.pushed` event, and its commits name it as the pusher.
#[tokio::test]
async fn a_service_accounts_push_is_audited_as_service_account() {
    let t = FreshDb::migrated().await;
    let (p, alice) = project(&t, "alice").await;
    let bot = crate::service_accounts::create(
        &t.db,
        &system("req-sa-ci"),
        &crate::service_accounts::NewServiceAccount {
            username: "ci-bot".to_owned(),
            display_name: "CI".to_owned(),
            owner: alice,
            purpose: "Pushes release tags".to_owned(),
        },
        alice,
    )
    .await
    .expect("service account")
    .user_id;
    let mut push = PushRecord {
        repository_id: p.repository_id,
        updates: vec![update(
            "refs/heads/main",
            'a',
            'b',
            PushUpdateKind::FastForward,
        )],
    };
    push.updates[0].commits = vec![oid('b')];
    push.updates[0].total_commits = 1;
    let who = Pusher {
        kind: forge_core::UserKind::ServiceAccount,
        ..pusher(bot, "req-push-sa")
    };
    let recorder: &dyn PushRecorder = &t.db;
    let done = recorder.record_push(&who, &push).await.expect("recorded");
    assert_eq!(done, PushRecorded::Recorded);
    let mut su = t.superuser().await;
    let audit: (String, Option<i64>) = sqlx::query_as(
        "SELECT actor_kind, actor_user_id FROM forge.audit_log \
         WHERE action = 'repository.pushed' AND request_id = 'req-push-sa'",
    )
    .fetch_one(&mut su)
    .await
    .expect("audit row");
    assert_eq!(audit, ("service_account".to_owned(), Some(bot.get())));
    let event: (String, Option<i64>) = sqlx::query_as(
        "SELECT actor_kind, actor_user_id FROM forge.events \
         WHERE event_type = 'repository.pushed' AND request_id = 'req-push-sa'",
    )
    .fetch_one(&mut su)
    .await
    .expect("event");
    assert_eq!(event, ("service_account".to_owned(), Some(bot.get())));
    let pushers: Vec<i64> = sqlx::query_scalar(
        "SELECT pusher_id FROM forge.push_commits WHERE request_id = 'req-push-sa'",
    )
    .fetch_all(&mut su)
    .await
    .expect("push commits");
    assert_eq!(pushers, [bot.get()]);
    drop(su);
    t.drop_now().await;
}
