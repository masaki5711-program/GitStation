//! Check runs, annotations and namespace grants against a migrated
//! database (M3 C1, ADR-0024 §10; migration 20261115100000).

use std::sync::Arc;

use forge_core::checks::{CheckAnnotationLevel, CheckConclusion, CheckStatus};
use forge_core::{CheckName, CheckNamespace, CheckResultsSource, ObjectId, ProjectId, RequestId};
use forge_errors::ErrorCode;

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::projects::{self, CreatedProject, NewProject};
use crate::service_accounts::{self, NewServiceAccount};
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

fn sha(c: char) -> ObjectId {
    ObjectId::parse(&c.to_string().repeat(40)).expect("sha")
}

fn name(raw: &str) -> CheckName {
    CheckName::parse(raw).expect("name")
}

fn report(raw: &str, status: CheckStatus, conclusion: Option<CheckConclusion>) -> NewCheckRun {
    NewCheckRun {
        name: name(raw),
        status,
        conclusion,
        summary: "synthetic summary".to_owned(),
        details_url: Some("https://ci.example.test/run/1".to_owned()),
        annotations: Vec::new(),
    }
}

fn success(raw: &str) -> NewCheckRun {
    report(raw, CheckStatus::Completed, Some(CheckConclusion::Success))
}

fn failure(raw: &str) -> NewCheckRun {
    report(raw, CheckStatus::Completed, Some(CheckConclusion::Failure))
}

fn constraint_of(result: Result<sqlx::postgres::PgQueryResult, sqlx::Error>) -> Option<String> {
    match result {
        Err(sqlx::Error::Database(e)) => e.constraint().map(str::to_owned),
        _ => None,
    }
}

struct World {
    t: FreshDb,
    alice: UserId,
    ci: UserId,
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
    let ci = service_accounts::create(
        &t.db,
        &as_user(alice.user_id, "req-sa-ci"),
        &NewServiceAccount {
            username: "ci-runner".to_owned(),
            display_name: "CI".to_owned(),
            owner: alice.user_id,
            purpose: "synthetic CI reporter".to_owned(),
        },
        alice.user_id,
    )
    .await
    .expect("ci");
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
        ci: ci.user_id,
        web,
    }
}

impl World {
    fn project(&self) -> ProjectId {
        self.web.project_id
    }

    async fn report(
        &self,
        head: &ObjectId,
        new: &NewCheckRun,
        request: &str,
    ) -> Result<Reported, forge_errors::ForgeError> {
        report_check_run_decided(
            &self.t.db,
            &as_user(self.alice, request),
            (self.project(), head),
            self.alice,
            ReportCredential(None),
            new,
            |_| Ok(()),
        )
        .await
        .map(|(r, ())| r)
    }

    async fn count(&self, sql: &str) -> i64 {
        sqlx::query_scalar(sql)
            .bind(self.project().get())
            .fetch_one(&mut self.t.superuser().await)
            .await
            .expect("count")
    }

    async fn events(&self) -> Vec<(String, String)> {
        sqlx::query_as(
            "SELECT event_type, dedup_key FROM forge.events WHERE project_id = $1 \
             AND event_type = 'check.completed' ORDER BY seq",
        )
        .bind(self.project().get())
        .fetch_all(&mut self.t.superuser().await)
        .await
        .expect("events")
    }

    async fn audit_actions(&self) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT action FROM forge.audit_log WHERE project_id = $1 \
             AND (action LIKE 'check%') ORDER BY id",
        )
        .bind(self.project().get())
        .fetch_all(&mut self.t.superuser().await)
        .await
        .expect("audit")
    }
}

#[tokio::test]
async fn a_report_creates_one_row_and_a_second_updates_it() {
    let w = world().await;
    let head = sha('a');
    let first = w
        .report(
            &head,
            &report("ci/gates", CheckStatus::InProgress, None),
            "req-1",
        )
        .await
        .expect("first");
    assert!(first.created);
    assert!(!first.completed_event);
    assert_eq!(first.run.revision, 1);
    assert_eq!(first.run.status, CheckStatus::InProgress);
    assert_eq!(first.run.completed_at, None);
    assert_eq!(first.run.reported_by_username.as_deref(), Some("alice"));
    let second = w
        .report(&head, &success("ci/gates"), "req-2")
        .await
        .expect("second");
    assert!(!second.created);
    assert_eq!(second.run.id, first.run.id);
    assert_eq!(second.run.revision, 2);
    assert_eq!(second.run.conclusion, Some(CheckConclusion::Success));
    assert!(second.run.completed_at.is_some());
    assert_eq!(
        w.count("SELECT count(*) FROM forge.check_runs WHERE project_id = $1")
            .await,
        1
    );
    let listed = check_runs(&w.t.db, w.project(), &head).await.expect("list");
    assert_eq!(listed, vec![second.run.clone()]);
    w.t.drop_now().await;
}

/// ADR-0024 §10: `check.completed` only when the conclusion changes, with
/// the dedup key of the run's revision; every other report is audit only.
#[tokio::test]
async fn check_completed_is_published_only_when_the_conclusion_changes() {
    let w = world().await;
    let head = sha('a');
    let steps = [
        (report("ci/gates", CheckStatus::Queued, None), false),
        (success("ci/gates"), true),
        // The same conclusion again: nothing new for the integrations.
        (success("ci/gates"), false),
        (failure("ci/gates"), true),
        (report("ci/gates", CheckStatus::InProgress, None), false),
        // A rerun that ends as before: the conclusion went none -> failure.
        (failure("ci/gates"), true),
    ];
    for (i, (new, event)) in steps.iter().enumerate() {
        let r = w
            .report(&head, new, &format!("req-step-{i}"))
            .await
            .expect("report");
        assert_eq!(r.completed_event, *event, "step {i}");
    }
    let id = check_runs(&w.t.db, w.project(), &head).await.expect("list")[0].id;
    let events = w.events().await;
    assert_eq!(
        events,
        vec![
            (
                "check.completed".to_owned(),
                format!("check_completed:{id}:2")
            ),
            (
                "check.completed".to_owned(),
                format!("check_completed:{id}:4")
            ),
            (
                "check.completed".to_owned(),
                format!("check_completed:{id}:6")
            ),
        ]
    );
    assert_eq!(
        w.audit_actions().await,
        [
            "check_run.reported",
            "check.completed",
            "check_run.reported",
            "check.completed",
            "check_run.reported",
            "check.completed"
        ]
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn annotations_are_replaced_with_the_run() {
    let w = world().await;
    let head = sha('a');
    let annotation = |path: &str, line: u32| CheckAnnotation {
        path: path.to_owned(),
        start_line: line,
        end_line: line + 1,
        level: CheckAnnotationLevel::Warning,
        message: "synthetic".to_owned(),
        title: Some("lint".to_owned()),
    };
    let mut new = failure("ci/lint");
    new.annotations = vec![annotation("src/a.rs", 1), annotation("src/b.rs", 5)];
    let r = w.report(&head, &new, "req-1").await.expect("report");
    assert_eq!(r.run.annotations, new.annotations);
    new.annotations = vec![annotation("src/c.rs", 9)];
    let r = w.report(&head, &new, "req-2").await.expect("report");
    assert_eq!(r.run.annotations, new.annotations);
    assert_eq!(
        w.count("SELECT count(*) FROM forge.check_annotations WHERE project_id = $1")
            .await,
        1
    );
    w.t.drop_now().await;
}

/// The plan's concurrency row: two reports of one name on one commit at
/// once leave exactly one row (they serialize on the commit's lock).
#[tokio::test]
async fn concurrent_reports_of_one_name_leave_one_row() {
    let w = Arc::new(world().await);
    let head = sha('c');
    let mut tasks = Vec::new();
    for i in 0..8 {
        let w = Arc::clone(&w);
        let head = head.clone(); // clone: each task owns its commit
        tasks.push(tokio::spawn(async move {
            let new = if i % 2 == 0 {
                success("ci/gates")
            } else {
                failure("ci/gates")
            };
            w.report(&head, &new, &format!("req-race-{i}")).await
        }));
    }
    let mut created = 0;
    for task in tasks {
        let r = task.await.expect("join").expect("every report succeeds");
        created += usize::from(r.created);
    }
    assert_eq!(created, 1, "exactly one report created the row");
    assert_eq!(
        w.count("SELECT count(*) FROM forge.check_runs WHERE project_id = $1")
            .await,
        1
    );
    assert_eq!(
        w.count("SELECT max(revision) FROM forge.check_runs WHERE project_id = $1")
            .await,
        8
    );
    match Arc::try_unwrap(w) {
        Ok(w) => w.t.drop_now().await,
        Err(_) => panic!("tasks finished"),
    }
}

#[tokio::test]
async fn a_new_name_beyond_the_limit_is_check_run_limit_reached() {
    let w = world().await;
    let head = sha('d');
    // 100 runs written directly (with one fixture audit row).
    w.t.commit_audited(
        ("project", w.project().get()),
        &[format!(
            "INSERT INTO forge.check_runs (project_id, head_sha, name, status, reported_by, \
             request_id) SELECT {}, '{}', 'ci/n' || g, 'queued', {}, 'req-fill' \
             FROM generate_series(1, 100) g",
            w.project().get(),
            head.as_str(),
            w.alice.get()
        )],
    )
    .await;
    let err = w
        .report(&head, &success("ci/one-more"), "req-over")
        .await
        .expect_err("over the cap");
    assert_reason(&err, ErrorCode::CheckRunLimitReached, "too_many_runs");
    // A name the commit already has still updates its run.
    w.report(&head, &success("ci/n7"), "req-known")
        .await
        .expect("a known name updates");
    // Another commit is not affected.
    w.report(&sha('e'), &success("ci/one-more"), "req-other")
        .await
        .expect("another commit");
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_stored_run_this_build_cannot_read_is_check_run_data_invalid() {
    let w = world().await;
    let head = sha('a');
    w.report(
        &head,
        &report("ci/gates", CheckStatus::Queued, None),
        "req-1",
    )
    .await
    .expect("report");
    // A dropped CHECK lets a manual change store a status no build writes.
    w.t.exec_superuser("ALTER TABLE forge.check_runs DROP CONSTRAINT check_runs_status")
        .await;
    w.t.commit_audited(
        ("project", w.project().get()),
        &["UPDATE forge.check_runs SET status = 'done', revision = revision + 1".to_owned()],
    )
    .await;
    for err in [
        w.t.db
            .runs_on(w.project(), &head)
            .await
            .expect_err("source"),
        check_runs(&w.t.db, w.project(), &head)
            .await
            .expect_err("list"),
    ] {
        assert_reason(
            &err,
            ErrorCode::CheckRunDataInvalid,
            "stored_status_invalid",
        );
    }
    w.t.drop_now().await;
}

#[tokio::test]
async fn the_source_reads_the_runs_of_one_commit_only() {
    let w = world().await;
    w.report(&sha('a'), &success("ci/gates"), "req-1")
        .await
        .expect("a");
    w.report(&sha('b'), &failure("ci/gates"), "req-2")
        .await
        .expect("b");
    let runs = w.t.db.runs_on(w.project(), &sha('a')).await.expect("runs");
    let run = runs.run(&sha('a'), &name("ci/gates")).expect("run");
    assert!(run.succeeded());
    assert_eq!(runs.len(), 1);
    let none = w.t.db.runs_on(w.project(), &sha('f')).await.expect("none");
    assert!(none.is_empty());
    w.t.drop_now().await;
}

/// The name CHECK is CheckName::parse's rule (refused by the constraint
/// itself, by name); statuses, conclusions, sizes and the URL scheme too.
#[tokio::test]
async fn the_tables_refuse_what_the_api_refuses_by_constraint_name() {
    let w = world().await;
    let mut su = w.t.superuser().await;
    let p = w.project().get();
    let a = w.alice.get();
    // `rest`: one more `column=value` (or empty).
    let insert = |name: &str, status: &str, conclusion: &str, completed: &str, rest: &str| {
        let (column, value) = match rest.split_once('=') {
            Some((c, v)) => (format!(", {c}"), format!(", {v}")),
            None => (String::new(), String::new()),
        };
        format!(
            "INSERT INTO forge.check_runs (project_id, head_sha, name, status, conclusion, \
             completed_at, reported_by, request_id{column}) VALUES ({p}, repeat('a', 40), \
             {name}, {status}, {conclusion}, {completed}, {a}, 'req-x'{value})"
        )
    };
    let cases: Vec<(String, &str)> = vec![
        (
            insert("'ci/a/'", "'queued'", "NULL", "NULL", ""),
            "check_runs_name",
        ),
        (
            insert("'ci/a//b'", "'queued'", "NULL", "NULL", ""),
            "check_runs_name",
        ),
        (
            insert("'ci/A'", "'queued'", "NULL", "NULL", ""),
            "check_runs_name",
        ),
        (
            insert("'ci/-x'", "'queued'", "NULL", "NULL", ""),
            "check_runs_name",
        ),
        (
            insert("'other/x'", "'queued'", "NULL", "NULL", ""),
            "check_runs_name",
        ),
        (
            insert("'ci/a b'", "'queued'", "NULL", "NULL", ""),
            "check_runs_name",
        ),
        (
            insert(
                &format!("'ci/{}'", "a".repeat(101)),
                "'queued'",
                "NULL",
                "NULL",
                "",
            ),
            "check_runs_name",
        ),
        (
            insert("'ci/a'", "'done'", "NULL", "NULL", ""),
            "check_runs_status",
        ),
        (
            insert("'ci/a'", "'completed'", "'passed'", "now()", ""),
            "check_runs_conclusion",
        ),
        (
            insert("'ci/a'", "'completed'", "NULL", "now()", ""),
            "check_runs_conclusion_when_completed",
        ),
        (
            insert("'ci/a'", "'queued'", "'success'", "NULL", ""),
            "check_runs_conclusion_when_completed",
        ),
        (
            insert("'ci/a'", "'completed'", "'success'", "NULL", ""),
            "check_runs_conclusion_when_completed",
        ),
        (
            insert(
                "'ci/a'",
                "'queued'",
                "NULL",
                "NULL",
                "summary=repeat('x', 65537)",
            ),
            "check_runs_summary_length",
        ),
        (
            insert(
                "'ci/a'",
                "'queued'",
                "NULL",
                "NULL",
                "details_url='ftp://x.test/'",
            ),
            "check_runs_details_url",
        ),
        (
            insert(
                "'ci/a'",
                "'queued'",
                "NULL",
                "NULL",
                "details_url='javascript:alert(1)'",
            ),
            "check_runs_details_url",
        ),
        (
            insert(
                "'ci/a'",
                "'queued'",
                "NULL",
                "NULL",
                "details_url='https://x.test/' || repeat('a', 2048)",
            ),
            "check_runs_details_url",
        ),
    ];
    for (sql, constraint) in &cases {
        assert_eq!(
            constraint_of(sqlx::query(sql).execute(&mut su).await),
            Some((*constraint).to_owned()),
            "{sql}"
        );
    }
    // Well-formed rows pass every CHECK (the audit trigger is deferred to a
    // commit that never comes).
    let mut tx = sqlx::Connection::begin(&mut su).await.expect("begin");
    for ok in [
        insert("'ci/a'", "'queued'", "NULL", "NULL", ""),
        insert("'forge/b.c'", "'in_progress'", "NULL", "NULL", ""),
        insert("'kanflow/x/y'", "'completed'", "'neutral'", "now()", ""),
        insert(
            &format!("'ext/{}'", "a".repeat(100)),
            "'completed'",
            "'action_required'",
            "now()",
            "details_url='http://ci.example.test/1'",
        ),
    ] {
        sqlx::query(&ok)
            .execute(&mut *tx)
            .await
            .expect("well-formed");
    }
    // One row per (project, commit, name).
    let dup = sqlx::query(&insert("'ci/a'", "'queued'", "NULL", "NULL", ""))
        .execute(&mut *tx)
        .await;
    assert_eq!(
        constraint_of(dup),
        Some("check_runs_one_per_name".to_owned())
    );
    tx.rollback().await.expect("rollback");
    w.t.drop_now().await;
}

#[tokio::test]
async fn annotations_are_bounded_relative_and_well_formed_by_constraint_name() {
    let w = world().await;
    let r = w
        .report(&sha('a'), &success("ci/gates"), "req-1")
        .await
        .expect("run");
    let mut su = w.t.superuser().await;
    let (run, p) = (r.run.id, w.project().get());
    let insert = |position: i32, path: &str, start: i32, end: i32, level: &str, message: &str| {
        format!(
            "INSERT INTO forge.check_annotations (check_run_id, project_id, position, path, \
             start_line, end_line, level, message) VALUES ({run}, {p}, {position}, {path}, \
             {start}, {end}, {level}, {message})"
        )
    };
    let cases = [
        (
            insert(50, "'a.rs'", 1, 1, "'notice'", "'m'"),
            "check_annotations_position",
        ),
        (
            insert(-1, "'a.rs'", 1, 1, "'notice'", "'m'"),
            "check_annotations_position",
        ),
        (
            insert(0, "'/etc/passwd'", 1, 1, "'notice'", "'m'"),
            "check_annotations_path",
        ),
        (
            insert(0, "'../a.rs'", 1, 1, "'notice'", "'m'"),
            "check_annotations_path",
        ),
        (
            insert(0, "'src/../../a.rs'", 1, 1, "'notice'", "'m'"),
            "check_annotations_path",
        ),
        (
            insert(0, "'src/..'", 1, 1, "'notice'", "'m'"),
            "check_annotations_path",
        ),
        (
            insert(0, "E'src\\\\a.rs'", 1, 1, "'notice'", "'m'"),
            "check_annotations_path",
        ),
        (
            insert(0, "E'src/a\\n.rs'", 1, 1, "'notice'", "'m'"),
            "check_annotations_path",
        ),
        (
            insert(0, "''", 1, 1, "'notice'", "'m'"),
            "check_annotations_path",
        ),
        (
            insert(0, "'a.rs'", 0, 1, "'notice'", "'m'"),
            "check_annotations_start_line",
        ),
        (
            insert(0, "'a.rs'", 3, 2, "'notice'", "'m'"),
            "check_annotations_lines",
        ),
        (
            insert(0, "'a.rs'", 1, 1, "'error'", "'m'"),
            "check_annotations_level",
        ),
        (
            insert(0, "'a.rs'", 1, 1, "'notice'", "''"),
            "check_annotations_message_length",
        ),
        (
            insert(0, "'a.rs'", 1, 1, "'notice'", "repeat('m', 4097)"),
            "check_annotations_message_length",
        ),
    ];
    for (sql, constraint) in &cases {
        assert_eq!(
            constraint_of(sqlx::query(sql).execute(&mut su).await),
            Some((*constraint).to_owned()),
            "{sql}"
        );
    }
    let mut tx = sqlx::Connection::begin(&mut su).await.expect("begin");
    for ok in [
        insert(0, "'a.rs'", 1, 1, "'notice'", "'m'"),
        insert(49, "'src/..x/..y.rs'", 2, 9, "'failure'", "'m'"),
        insert(7, "'a..b/c.rs'", 1, 1, "'warning'", "'m'"),
    ] {
        sqlx::query(&ok)
            .execute(&mut *tx)
            .await
            .expect("well-formed");
    }
    tx.rollback().await.expect("rollback");
    w.t.drop_now().await;
}

#[tokio::test]
async fn grants_go_to_service_accounts_and_are_audited_on_the_account() {
    let w = world().await;
    let grant = |user: UserId, ns: CheckNamespace, on: bool, req: &'static str| {
        let db = w.t.db.clone(); // clone: the future owns its handle
        let admin = w.alice;
        async move {
            change_check_namespace_grant_decided(
                &db,
                &as_user(admin, req),
                (user, ns, on),
                admin,
                |_| Ok(()),
            )
            .await
            .map(|(o, ())| o)
        }
    };
    assert_eq!(
        grant(w.ci, CheckNamespace::Ci, true, "req-g1")
            .await
            .expect("grant"),
        GrantOutcome::Changed
    );
    assert_eq!(
        grant(w.ci, CheckNamespace::Ci, true, "req-g2")
            .await
            .expect("again"),
        GrantOutcome::Unchanged
    );
    assert_eq!(
        grants_of(&w.t.db, w.ci).await.expect("grants"),
        [CheckNamespace::Ci]
    );
    assert_eq!(
        reported_namespaces(&w.t.db).await.expect("reported"),
        [CheckNamespace::Ci]
    );
    // A person cannot hold a grant: the guard refuses it by name.
    let err = grant(w.alice, CheckNamespace::Kanflow, true, "req-g3")
        .await
        .expect_err("a person");
    assert!(
        chain(&err).contains("check_namespace_grants_service_account"),
        "{err}"
    );
    // forge/ and ext/ are never granted.
    for ns in [CheckNamespace::Forge, CheckNamespace::Ext] {
        let err = grant(w.ci, ns, true, "req-g4")
            .await
            .expect_err("not grantable");
        assert_reason(
            &err,
            ErrorCode::CheckPayloadInvalid,
            "namespace_not_grantable",
        );
    }
    assert_eq!(
        grant(w.ci, CheckNamespace::Ci, false, "req-g5")
            .await
            .expect("revoke"),
        GrantOutcome::Changed
    );
    assert!(grants_of(&w.t.db, w.ci).await.expect("grants").is_empty());
    let rows: Vec<(String, String, Option<i64>)> = sqlx::query_as(
        "SELECT action, resource_type, resource_id FROM forge.audit_log \
         WHERE action LIKE 'check_namespace.%' ORDER BY id",
    )
    .fetch_all(&mut w.t.superuser().await)
    .await
    .expect("audit");
    assert_eq!(
        rows,
        [
            (
                "check_namespace.granted".to_owned(),
                "user".to_owned(),
                Some(w.ci.get())
            ),
            (
                "check_namespace.revoked".to_owned(),
                "user".to_owned(),
                Some(w.ci.get())
            ),
        ]
    );
    w.t.drop_now().await;
}

/// The check_runs guard: a run's identity never changes; every change adds
/// one to its revision.
#[tokio::test]
async fn a_run_keeps_its_identity_and_counts_its_writes() {
    let w = world().await;
    w.report(&sha('a'), &success("ci/gates"), "req-1")
        .await
        .expect("run");
    let mut su = w.t.superuser().await;
    for sql in [
        "UPDATE forge.check_runs SET name = 'ci/other', revision = revision + 1",
        "UPDATE forge.check_runs SET head_sha = repeat('b', 40), revision = revision + 1",
        "UPDATE forge.check_runs SET summary = 'x'",
    ] {
        let err = sqlx::query(sql).execute(&mut su).await.expect_err(sql);
        let code = err
            .as_database_error()
            .and_then(|d| d.code())
            .map(|c| c.into_owned());
        assert_eq!(code.as_deref(), Some("XF001"), "{sql}");
    }
    w.t.drop_now().await;
}
