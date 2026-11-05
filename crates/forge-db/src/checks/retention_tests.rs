//! The retention of check runs against a migrated database: old runs go
//! with their annotations, the current head of an open merge request and
//! recent runs stay, one `check_run.retention` audit row per project and
//! batch, batches are bounded, and nothing deadlocks with a report or the
//! merge gate.

use std::sync::Arc;
use std::time::Duration;

use forge_core::checks::{CheckAnnotationLevel, CheckConclusion, CheckStatus};
use forge_core::{CheckName, NamespaceId, ObjectId, ProjectId, RequestId, UserId};
use forge_errors::{ErrorCode, ForgeError};

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::checks::{
    report_check_run_decided, CheckAnnotation, NewCheckRun, ReportCredential, Reported,
};
use crate::projects::{self, CreatedProject, NewProject};
use crate::test_support::*;
use crate::users::{self, NewUser};

const DAYS: u32 = 180;

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

fn sha(c: char) -> ObjectId {
    ObjectId::parse(&c.to_string().repeat(40)).expect("sha")
}

fn nth_sha(base: char, i: u8) -> ObjectId {
    ObjectId::parse(&format!("{base}{i:039x}")).expect("sha")
}

fn run(name: &str, annotations: usize) -> NewCheckRun {
    NewCheckRun {
        name: CheckName::parse(name).expect("name"),
        status: CheckStatus::Completed,
        conclusion: Some(CheckConclusion::Success),
        summary: "synthetic".to_owned(),
        details_url: None,
        annotations: (0..annotations)
            .map(|i| CheckAnnotation {
                path: format!("src/file{i}.rs"),
                start_line: 1,
                end_line: 2,
                level: CheckAnnotationLevel::Notice,
                message: "synthetic".to_owned(),
                title: None,
            })
            .collect(),
    }
}

struct World {
    t: FreshDb,
    alice: UserId,
    alice_namespace: NamespaceId,
    web: CreatedProject,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let alice = users::create(
        &t.db,
        &system("req-user-alice"),
        &NewUser::human("alice", "alice", "alice@example.test"),
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
    World {
        t,
        alice: alice.user_id,
        alice_namespace: alice.namespace_id,
        web,
    }
}

impl World {
    fn project(&self) -> ProjectId {
        self.web.project_id
    }

    async fn second_project(&self) -> ProjectId {
        projects::create(
            &self.t.db,
            &system("req-project-api"),
            &NewProject::new("api", self.alice_namespace, "Api", self.alice),
        )
        .await
        .expect("api")
        .project_id
    }

    async fn report_in(
        &self,
        project: ProjectId,
        head: &ObjectId,
        new: &NewCheckRun,
        request: &str,
    ) -> Result<Reported, ForgeError> {
        report_check_run_decided(
            &self.t.db,
            &ChangeContext::new(
                Actor::User(self.alice),
                RequestId::parse(request).expect("request id"),
            ),
            (project, head),
            self.alice,
            ReportCredential(None),
            new,
            |_| Ok(()),
        )
        .await
        .map(|(r, ())| r)
    }

    /// Report a run on the first project, then make it `days` days old.
    async fn aged_run(&self, head: &ObjectId, name: &str, annotations: usize, days: u32) -> i64 {
        self.aged_run_in(self.project(), head, name, annotations, days)
            .await
    }

    async fn aged_run_in(
        &self,
        project: ProjectId,
        head: &ObjectId,
        name: &str,
        annotations: usize,
        days: u32,
    ) -> i64 {
        let id = self
            .report_in(project, head, &run(name, annotations), "req-aged")
            .await
            .expect("report")
            .run
            .id;
        self.t
            .commit_audited(
                ("project", project.get()),
                &[format!(
                    "UPDATE forge.check_runs SET updated_at = now() - interval '{days} days', \
                     revision = revision + 1 WHERE id = {id}"
                )],
            )
            .await;
        id
    }

    async fn merge_request(&self, iid: i64, head: &ObjectId, state: &str) {
        let p = self.project().get();
        let (closed_at, closed_by) = if state == "closed" {
            ("now()".to_owned(), self.alice.get().to_string())
        } else {
            ("NULL".to_owned(), "NULL".to_owned())
        };
        self.t
            .commit_audited(
                ("project", p),
                &[
                    format!(
                        "INSERT INTO forge.merge_request_counters (project_id, last_iid) \
                         VALUES ({p}, {iid}) ON CONFLICT (project_id) DO UPDATE SET last_iid = {iid}"
                    ),
                    format!(
                        "INSERT INTO forge.merge_requests (project_id, iid, title, author_id, \
                         source_branch, target_branch, state, head_sha, closed_at, closed_by) \
                         VALUES ({p}, {iid}, 'synthetic', {}, 'refs/heads/f{iid}', \
                         'refs/heads/main', '{state}', '{}', {closed_at}, {closed_by})",
                        self.alice.get(),
                        head.as_str()
                    ),
                ],
            )
            .await;
    }

    async fn run_ids(&self) -> Vec<i64> {
        self.t
            .json_rows("SELECT id FROM forge.check_runs ORDER BY id")
            .await
            .iter()
            .map(|r| r["id"].as_i64().expect("id"))
            .collect()
    }

    async fn count(&self, table: &str) -> i64 {
        self.t
            .json_rows(&format!("SELECT count(*) AS n FROM forge.{table}"))
            .await[0]["n"]
            .as_i64()
            .expect("count")
    }

    async fn retention_rows(&self) -> Vec<serde_json::Value> {
        self.t
            .json_rows(
                "SELECT actor_kind, resource_type, resource_id, outcome, details \
                 FROM forge.audit_log WHERE action = 'check_run.retention' ORDER BY id",
            )
            .await
    }
}

#[tokio::test]
async fn old_runs_go_with_their_annotations_and_recent_ones_stay() {
    let w = world().await;
    let old = w.aged_run(&sha('a'), "ci/old", 3, 181).await;
    let edge = w.aged_run(&sha('b'), "ci/edge", 1, 179).await;
    let recent = w.aged_run(&sha('c'), "ci/recent", 2, 1).await;
    assert_eq!(w.count("check_annotations").await, 6);
    let done = expire_old_runs(&w.t.db, &system("req-retention"), DAYS, RETENTION_BATCH)
        .await
        .expect("expire");
    assert_eq!(
        done,
        Expired {
            runs: 1,
            annotations: 3,
            projects: 1
        }
    );
    assert_eq!(w.run_ids().await, vec![edge, recent], "old {old} is gone");
    assert_eq!(w.count("check_annotations").await, 3);
    w.t.drop_now().await;
}

#[tokio::test]
async fn the_current_head_of_an_open_merge_request_is_kept() {
    let w = world().await;
    let kept = w.aged_run(&sha('a'), "ci/gates", 2, 400).await;
    let same_head = w.aged_run(&sha('a'), "ci/lint", 0, 400).await;
    w.aged_run(&sha('b'), "ci/gates", 1, 400).await;
    w.aged_run(&sha('c'), "ci/gates", 1, 400).await;
    w.merge_request(1, &sha('a'), "open").await;
    // A closed merge request protects nothing.
    w.merge_request(2, &sha('b'), "closed").await;
    // An open one whose head moved on: `c` is no longer current.
    w.merge_request(3, &sha('d'), "open").await;
    let done = expire_old_runs(&w.t.db, &system("req-retention"), DAYS, RETENTION_BATCH)
        .await
        .expect("expire");
    assert_eq!(done.runs, 2, "{done:?}");
    assert_eq!(w.run_ids().await, vec![kept, same_head]);
    assert_eq!(w.count("check_annotations").await, 2);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_batch_writes_one_counts_only_audit_row_per_project() {
    let w = world().await;
    let api = w.second_project().await;
    w.aged_run(&sha('a'), "ci/a", 2, 200).await;
    w.aged_run(&sha('b'), "ci/b", 0, 200).await;
    w.aged_run_in(api, &sha('c'), "ci/c", 1, 200).await;
    let done = expire_old_runs(&w.t.db, &system("req-retention"), DAYS, RETENTION_BATCH)
        .await
        .expect("expire");
    assert_eq!(
        done,
        Expired {
            runs: 3,
            annotations: 3,
            projects: 2
        }
    );
    let rows = w.retention_rows().await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    for row in &rows {
        assert_eq!(row["actor_kind"], "system");
        assert_eq!(row["resource_type"], "project");
        assert_eq!(row["outcome"], "succeeded");
    }
    let find = |p: i64| {
        rows.iter()
            .find(|r| r["resource_id"].as_i64() == Some(p))
            .unwrap_or_else(|| panic!("no row for {p}: {rows:?}"))
    };
    assert_eq!(
        find(w.project().get())["details"],
        serde_json::json!({
            "project_id": w.project().get(), "runs": 2, "annotations": 2,
            "older_than_days": 180
        })
    );
    assert_eq!(find(api.get())["details"]["runs"], 1);
    w.t.drop_now().await;
}

#[tokio::test]
async fn nothing_due_writes_nothing() {
    let w = world().await;
    w.aged_run(&sha('a'), "ci/a", 1, 5).await;
    let done = expire_old_runs(&w.t.db, &system("req-retention"), DAYS, RETENTION_BATCH)
        .await
        .expect("expire");
    assert_eq!(done, Expired::default());
    assert!(w.retention_rows().await.is_empty());
    assert_eq!(w.run_ids().await.len(), 1);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_batch_is_bounded_and_takes_the_oldest_first() {
    let w = world().await;
    let mut ids = Vec::new();
    for (i, days) in [500u32, 400, 300, 250, 200].into_iter().enumerate() {
        let head = nth_sha('a', u8::try_from(i).expect("i"));
        ids.push(w.aged_run(&head, "ci/a", 1, days).await);
    }
    let mut sizes = Vec::new();
    for pass in 0..4 {
        let done = expire_old_runs(&w.t.db, &system(&format!("req-pass-{pass}")), DAYS, 2)
            .await
            .expect("expire");
        sizes.push((done.runs, done.annotations));
        if pass == 0 {
            assert_eq!(w.run_ids().await, ids[2..].to_vec(), "oldest two first");
        }
    }
    assert_eq!(sizes, vec![(2, 2), (2, 2), (1, 1), (0, 0)]);
    assert!(w.run_ids().await.is_empty());
    assert_eq!(
        w.retention_rows().await.len(),
        3,
        "one row per batch that deleted"
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_batch_or_window_out_of_range_is_refused() {
    let w = world().await;
    for (days, batch) in [(DAYS, 0), (DAYS, RETENTION_BATCH + 1), (0, 10)] {
        let err = expire_old_runs(&w.t.db, &system("req-bad"), days, batch)
            .await
            .expect_err("refused");
        assert_eq!(err.code(), ErrorCode::Internal, "{days} {batch}");
    }
    w.t.drop_now().await;
}

/// A report holds the run's row: the retention skips that row instead of
/// waiting for it.
#[tokio::test]
async fn a_run_a_report_holds_is_skipped_not_waited_for() {
    let w = world().await;
    let held = w.aged_run(&sha('a'), "ci/a", 1, 300).await;
    w.aged_run(&sha('b'), "ci/a", 0, 300).await;
    let mut holder = w.t.superuser().await;
    let mut tx = sqlx::Connection::begin(&mut holder).await.expect("begin");
    sqlx::query("SELECT id FROM forge.check_runs WHERE id = $1 FOR UPDATE")
        .bind(held)
        .execute(&mut *tx)
        .await
        .expect("lock");
    let done = tokio::time::timeout(
        Duration::from_secs(10),
        expire_old_runs(&w.t.db, &system("req-retention"), DAYS, RETENTION_BATCH),
    )
    .await
    .expect("never waits for a run in a report")
    .expect("expire");
    assert_eq!(done.runs, 1);
    tx.rollback().await.expect("rollback");
    assert_eq!(w.run_ids().await, vec![held]);
    w.t.drop_now().await;
}

/// The merge gate holds the project row `FOR UPDATE` and reads the runs:
/// the retention does not queue behind it.
#[tokio::test]
async fn the_retention_does_not_wait_behind_the_merge_gate() {
    let w = world().await;
    w.aged_run(&sha('a'), "ci/a", 1, 300).await;
    let mut gate = w.t.superuser().await;
    let mut tx = sqlx::Connection::begin(&mut gate).await.expect("begin");
    sqlx::query("SELECT id FROM forge.projects WHERE id = $1 FOR UPDATE")
        .bind(w.project().get())
        .execute(&mut *tx)
        .await
        .expect("gate lock");
    let during = tokio::time::timeout(
        Duration::from_secs(10),
        expire_old_runs(&w.t.db, &system("req-during"), DAYS, RETENTION_BATCH),
    )
    .await
    .expect("never waits for the gate")
    .expect("expire");
    assert_eq!(during, Expired::default());
    assert_eq!(w.run_ids().await.len(), 1, "kept while the gate runs");
    tx.rollback().await.expect("rollback");
    let after = expire_old_runs(&w.t.db, &system("req-after"), DAYS, RETENTION_BATCH)
        .await
        .expect("expire");
    assert_eq!(after.runs, 1);
    w.t.drop_now().await;
}

/// Reports (re-reports of old names and new commits) run while batches
/// delete: no deadlock, no error.
#[tokio::test]
async fn concurrent_reports_and_batches_do_not_deadlock() {
    let w = Arc::new(world().await);
    for i in 0..6u8 {
        w.aged_run(&nth_sha('a', i), "ci/a", 1, 300).await;
    }
    let mut reports = Vec::new();
    let mut batches = Vec::new();
    for i in 0..6u8 {
        let r = Arc::clone(&w);
        reports.push(tokio::spawn(async move {
            r.report_in(
                r.project(),
                &nth_sha('a', i),
                &run("ci/a", 1),
                &format!("req-again-{i}"),
            )
            .await?;
            r.report_in(
                r.project(),
                &nth_sha('f', i),
                &run("ci/a", 0),
                &format!("req-fresh-{i}"),
            )
            .await
            .map(|_| ())
        }));
        let b = Arc::clone(&w);
        batches.push(tokio::spawn(async move {
            expire_old_runs(&b.t.db, &system(&format!("req-batch-{i}")), DAYS, 2)
                .await
                .map(|_| ())
        }));
    }
    let all = async {
        for t in reports.into_iter().chain(batches) {
            t.await.expect("task").expect("no error");
        }
    };
    tokio::time::timeout(Duration::from_secs(60), all)
        .await
        .expect("no deadlock");
    let w = Arc::try_unwrap(w).unwrap_or_else(|_| panic!("tasks done"));
    assert!(w.count("check_runs").await >= 6, "fresh runs are kept");
    w.t.drop_now().await;
}

/// The doctor's backlog: runs past the window plus a day of slack that are
/// not an open merge request's head.
#[tokio::test]
async fn the_backlog_counts_what_the_worker_is_late_with() {
    let w = world().await;
    assert_eq!(
        retention_backlog(&w.t.db, DAYS).await.expect("empty"),
        RetentionBacklog::default()
    );
    w.aged_run(&sha('a'), "ci/a", 0, 180).await; // inside the day of slack
    w.aged_run(&sha('b'), "ci/a", 0, 190).await;
    w.aged_run(&sha('c'), "ci/a", 0, 400).await;
    w.aged_run(&sha('d'), "ci/a", 0, 400).await; // an open merge request's head
    w.merge_request(1, &sha('d'), "open").await;
    let backlog = retention_backlog(&w.t.db, DAYS).await.expect("backlog");
    assert_eq!(
        backlog,
        RetentionBacklog {
            due: 2,
            oldest_age_days: Some(400)
        }
    );
    expire_old_runs(&w.t.db, &system("req-late"), DAYS, RETENTION_BATCH)
        .await
        .expect("expire");
    assert_eq!(
        retention_backlog(&w.t.db, DAYS).await.expect("after"),
        RetentionBacklog::default()
    );
    w.t.drop_now().await;
}
