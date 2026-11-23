//! The search index against a migrated database (plan 20 K2): the
//! subscriber refreshes entries from their source rows, a purge removes a
//! project's rows, a failed step is retried (never skipped), the rebuild
//! is idempotent, and the reader ranks, narrows and times out as promised.

use forge_core::search::{CandidateFilter, SourceKind};
use forge_core::search_text::SearchQuery;
use forge_core::{ProjectId, RefName, RequestId, UserId};

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::projects::{self, CreatedProject, NewProject};
use crate::review::{edit_mr_decided, open_mr_decided, MergeRequestPatch, NewMergeRequest};
use crate::sequencer::{self, AuditSequencing, BatchSize};
use crate::subscribers::{self, Progress, Start};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

fn as_user(user: UserId, request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::User(user),
        RequestId::parse(request).expect("request id"),
    )
}

fn branch(name: &str) -> RefName {
    RefName::parse(&format!("refs/heads/{name}")).expect("branch")
}

/// alice and her project `alice/<name>` titled `title`.
async fn project(t: &FreshDb, user: &str, name: &str, title: &str) -> (UserId, CreatedProject) {
    let owner = users::create(
        &t.db,
        &system(&format!("req-user-{user}")),
        &NewUser::human(user, user, &format!("{user}@example.test")),
    )
    .await
    .expect("user");
    let p = projects::create(
        &t.db,
        &system(&format!("req-project-{name}")),
        &NewProject::new(name, owner.namespace_id, title, owner.user_id),
    )
    .await
    .expect("project");
    (owner.user_id, p)
}

async fn open(
    t: &FreshDb,
    author: UserId,
    p: ProjectId,
    title: &str,
    description: &str,
    source: &str,
) {
    let new = NewMergeRequest {
        title: title.to_owned(),
        description: description.to_owned(),
        source_branch: branch(source),
        target_branch: branch("main"),
        draft: false,
    };
    open_mr_decided(&t.db, &as_user(author, "req-open"), p, author, &new, |_| {
        Ok(())
    })
    .await
    .expect("open");
}

/// Number the events, then run `search-index` until it caught up.
async fn drain(t: &FreshDb) {
    sequencer::run_once(&t.db, BatchSize::MAX, AuditSequencing::Skip)
        .await
        .expect("sequencer");
    loop {
        match subscribers::run_once(&t.db, &SearchIndex)
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
    }
}

async fn register(t: &FreshDb) {
    subscribers::register(&t.db, &SearchIndex, Start::Beginning)
        .await
        .expect("register");
}

/// Write one event of `kind` for `project` with `payload` (and its audit
/// row), then number it.
async fn raw_event(t: &FreshDb, kind: &str, project: ProjectId, payload: &str) {
    let p = project.get();
    t.exec_superuser(&format!(
        "BEGIN; \
         INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, resource_id, \
           request_id) VALUES ('system', 'probe.search', 'succeeded', 'project', {p}, 'r-search'); \
         INSERT INTO forge.events (event_id, event_type, schema_version, aggregate_type, \
           aggregate_id, project_id, actor_kind, request_id, payload) VALUES \
           (gen_random_uuid(), '{kind}', 1, 'project', {p}, {p}, 'system', 'r-search', \
           '{payload}'::jsonb); \
         COMMIT"
    ))
    .await;
    sequencer::run_once(&t.db, BatchSize::MAX, AuditSequencing::Skip)
        .await
        .expect("sequencer");
}

async fn titles(t: &FreshDb, query: &str) -> Vec<(String, String)> {
    let q = SearchQuery::parse(query).expect("query");
    candidates(
        &t.db,
        &q.tsquery_literal(),
        &CandidateFilter::default(),
        0,
        50,
    )
    .await
    .expect("candidates")
    .into_iter()
    .map(|c| (c.kind.as_str().to_owned(), c.title))
    .collect()
}

async fn snapshot(t: &FreshDb) -> Vec<serde_json::Value> {
    t.json_rows(
        "SELECT source_kind, project_id, source_key, title, tokens::text AS tokens \n         FROM forge.search_entries ORDER BY source_kind, source_key",
    )
    .await
}

async fn entry_count(t: &FreshDb, project: ProjectId) -> i64 {
    let rows = t
        .json_rows(&format!(
            "SELECT count(*) AS n FROM forge.search_entries WHERE project_id = {}",
            project.get()
        ))
        .await;
    rows[0]["n"].as_i64().expect("count")
}

#[tokio::test]
async fn project_and_merge_request_events_index_their_rows() {
    let t = FreshDb::migrated().await;
    register(&t).await;
    let (alice, web) = project(&t, "alice", "plant", "設備仕様の管理").await;
    open(
        &t,
        alice,
        web.project_id,
        "ポンプの承認手順",
        "PLC データ収集",
        "pump",
    )
    .await;
    drain(&t).await;
    assert_eq!(
        titles(&t, "設備").await,
        vec![("project".to_owned(), "設備仕様の管理".to_owned())]
    );
    assert_eq!(
        titles(&t, "ﾃﾞｰﾀ").await,
        vec![("merge_request".to_owned(), "ポンプの承認手順".to_owned())]
    );
    // The path is searchable too (weight B).
    assert_eq!(titles(&t, "alice plant").await.len(), 1);
}

#[tokio::test]
async fn mr_title_edit_reindexed() {
    let t = FreshDb::migrated().await;
    register(&t).await;
    let (alice, web) = project(&t, "alice", "web", "Web").await;
    open(&t, alice, web.project_id, "古い題名", "", "feature").await;
    drain(&t).await;
    assert_eq!(titles(&t, "古い").await.len(), 1);
    let patch = MergeRequestPatch {
        title: Some("新しい題名".to_owned()),
        ..MergeRequestPatch::default()
    };
    let (_edited, _, ()) = edit_mr_decided(
        &t.db,
        &as_user(alice, "req-edit"),
        web.project_id,
        1,
        alice,
        &patch,
        |_| Ok(()),
    )
    .await
    .expect("edit");
    drain(&t).await;
    assert!(titles(&t, "古い").await.is_empty());
    assert_eq!(
        titles(&t, "新しい").await,
        vec![("merge_request".to_owned(), "新しい題名".to_owned())]
    );
}

#[tokio::test]
async fn deleted_project_rows_removed() {
    let t = FreshDb::migrated().await;
    register(&t).await;
    let (alice, web) = project(&t, "alice", "web", "Pump station").await;
    open(&t, alice, web.project_id, "Pump fix", "", "fix").await;
    drain(&t).await;
    assert_eq!(entry_count(&t, web.project_id).await, 2);
    raw_event(&t, "project.purged", web.project_id, "{}").await;
    drain(&t).await;
    assert_eq!(entry_count(&t, web.project_id).await, 0);
}

#[tokio::test]
async fn subscriber_failure_is_retried_not_dropped() {
    let t = FreshDb::migrated().await;
    register(&t).await;
    let (_alice, web) = project(&t, "alice", "web", "Web").await;
    drain(&t).await;
    // A merge request event without its iid cannot be processed.
    raw_event(&t, "merge_request.updated", web.project_id, "{}").await;
    for _ in 0..2 {
        match subscribers::run_once(&t.db, &SearchIndex)
            .await
            .expect("step")
        {
            Progress::Failed(_) => {}
            other @ (Progress::Busy
            | Progress::Disabled
            | Progress::CaughtUp
            | Progress::Handled { .. }
            | Progress::Stuck(_)
            | Progress::Quarantined(_)) => panic!("expected a counted failure, got {other:?}"),
        }
    }
    let rows = t
        .json_rows(
            "SELECT s.last_seq, (SELECT max(seq) FROM forge.events) AS high \
             FROM forge.event_subscribers s WHERE s.name = 'search-index'",
        )
        .await;
    // The cursor stays before the event: it is tried again, never skipped.
    assert!(rows[0]["last_seq"].as_i64() < rows[0]["high"].as_i64());
}

#[tokio::test]
async fn reindex_is_idempotent() {
    let t = FreshDb::migrated().await;
    let (alice, web) = project(&t, "alice", "web", "Pump station").await;
    open(&t, alice, web.project_id, "Pump fix", "valve", "fix").await;
    open(&t, alice, web.project_id, "Valve fix", "pump", "valve").await;
    // An orphan row of a project that does not exist.
    t.exec_superuser(
        "INSERT INTO forge.search_entries (source_kind, project_id, source_key, title, tokens, \
           source_updated_at) VALUES ('project', 999999, '', 'Ghost', 'ghost'::tsvector, now())",
    )
    .await;
    let request = RequestId::parse("req-reindex").expect("request id");
    let first = reindex(&t.db, &request, None).await.expect("first");
    assert_eq!(first.projects, 1);
    assert_eq!(first.merge_requests, 2);
    assert_eq!(first.orphans_removed, 1);
    assert!(first.failed.is_empty());
    let before = snapshot(&t).await;
    let second = reindex(&t.db, &request, Some(web.project_id))
        .await
        .expect("second");
    assert_eq!(second.projects, 1);
    assert_eq!(snapshot(&t).await, before);
    assert_eq!(before.len(), 3);
    let state = t
        .json_rows("SELECT state, attempts FROM forge.search_index_state")
        .await;
    assert_eq!(state[0]["state"], "ok");
    // Ranking: a title (A) beats a body (C).
    let ranked = titles(&t, "pump").await;
    assert_eq!(ranked.len(), 3);
    assert_eq!(
        ranked.last().map(|(_, title)| title.as_str()),
        Some("Valve fix")
    );
}

#[tokio::test]
async fn oversized_description_is_truncated_and_logged() {
    let t = FreshDb::migrated().await;
    let (_alice, web) = project(&t, "alice", "web", "Web").await;
    // No source column holds this much today (documents will, K4): the
    // writer itself is exercised.
    let long = format!("{} needle", "設備の説明 ".repeat(30_000));
    assert!(long.len() > MAX_BODY_BYTES);
    let mut conn = t.superuser().await;
    let origin = Origin::of_request(&RequestId::parse("req-trunc").expect("request id"));
    let entry = Entry {
        kind: SourceKind::Project,
        project: web.project_id,
        key: String::new(),
        title: "Web",
        ids: String::new(),
        body: &long,
    };
    upsert(&mut conn, &origin, &entry, "2026-11-23T00:00:00Z")
        .await
        .expect("upsert");
    let rows = t
        .json_rows(
            "SELECT truncated, octet_length(body_text) AS bytes FROM forge.search_entries \
             WHERE source_kind = 'project'",
        )
        .await;
    assert_eq!(rows[0]["truncated"], true);
    assert!(
        rows[0]["bytes"].as_i64().expect("bytes") <= i64::try_from(MAX_BODY_BYTES).expect("fits")
    );
    // The text past the cut is not searchable.
    assert!(titles(&t, "needle").await.is_empty());
    assert_eq!(titles(&t, "設備").await.len(), 1);
}

#[tokio::test]
async fn candidates_narrow_by_kind_project_and_confidential() {
    let t = FreshDb::migrated().await;
    let (alice, web) = project(&t, "alice", "web", "Pump").await;
    let (_bob, other) = project(&t, "bob", "other", "Pump two").await;
    open(&t, alice, web.project_id, "Pump fix", "", "fix").await;
    let request = RequestId::parse("req-reindex").expect("request id");
    reindex(&t.db, &request, None).await.expect("reindex");
    t.exec_superuser(&format!(
        "UPDATE forge.search_entries SET classification = 'confidential' \
         WHERE project_id = {} AND source_kind = 'project'",
        other.project_id.get()
    ))
    .await;
    let q = SearchQuery::parse("pump").expect("query").tsquery_literal();
    let count = |filter: CandidateFilter| {
        let q = q.clone();
        let t = &t;
        async move {
            candidates(&t.db, &q, &filter, 0, 50)
                .await
                .expect("candidates")
                .len()
        }
    };
    assert_eq!(count(CandidateFilter::default()).await, 3);
    let mrs = CandidateFilter {
        kinds: vec![SourceKind::MergeRequest],
        ..CandidateFilter::default()
    };
    assert_eq!(count(mrs).await, 1);
    let one = CandidateFilter {
        project: Some(web.project_id),
        ..CandidateFilter::default()
    };
    assert_eq!(count(one).await, 2);
    let agent = CandidateFilter {
        exclude_confidential: true,
        ..CandidateFilter::default()
    };
    assert_eq!(count(agent).await, 2);
    // Paging by offset.
    let page = candidates(&t.db, &q, &CandidateFilter::default(), 2, 50)
        .await
        .expect("page");
    assert_eq!(page.len(), 1);
    let refused = candidates(
        &t.db,
        &q,
        &CandidateFilter::default(),
        0,
        MAX_CANDIDATES + 1,
    )
    .await
    .expect_err("too many");
    assert_eq!(refused.code(), forge_errors::ErrorCode::Internal);
}

#[test]
fn set_timeout_matches_the_constant() {
    assert_eq!(
        read::SET_TIMEOUT_SQL,
        format!(
            "SET LOCAL statement_timeout = {}",
            SEARCH_STATEMENT_TIMEOUT.as_millis()
        )
    );
}

#[test]
fn cut_keeps_character_boundaries() {
    assert_eq!(cut("設備", 4), ("設", true));
    assert_eq!(cut("abc", 3), ("abc", false));
}
