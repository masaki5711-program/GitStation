//! The prepare job end to end (plan 18 S6, ADR-0037 (f)): the production
//! handlers and subscribers against a migrated database and real git. A
//! version is prepared by one job that writes what the four jobs wrote; a
//! failed step is handed over to its own job only; the merge request
//! pipeline is asked for only when the merged tree has a `.forge-ci.yml`.

use forge_core::MergeabilityStatus;
use forge_db::jobs::Ran;
use forge_review::StoredMergeRequest;

use crate::worker::review::mergeability_tests::{commit_on, oid, World};

/// The (name, status, conclusion) of forge's runs on `head`, by name.
async fn runs(w: &World, head: &str) -> Vec<(String, String, Option<String>)> {
    let mut runs: Vec<(String, String, Option<String>)> =
        forge_db::checks::check_runs(&w.t.db, w.project, &oid(head))
            .await
            .expect("runs")
            .iter()
            .filter(|r| r.reported_by.is_none())
            .map(|r| {
                (
                    r.name.as_str().to_owned(),
                    format!("{:?}", r.status),
                    r.conclusion.map(|c| format!("{c:?}")),
                )
            })
            .collect();
    runs.sort();
    runs
}

/// The kinds of the jobs about `mr` (by merge request id) or its `head`
/// (the head-keyed checks), sorted.
async fn kinds(w: &World, mr: &StoredMergeRequest, head: &str) -> Vec<String> {
    let rows =
        w.t.json_rows(&format!(
            "SELECT kind FROM forge.jobs WHERE payload ->> 'merge_request_id' = '{}' \
             OR payload ->> 'head_sha' = '{head}' ORDER BY kind",
            mr.id.get()
        ))
        .await;
    rows.iter()
        .filter_map(|r| r["kind"].as_str().map(str::to_owned))
        .collect()
}

/// `main` with a README (its commit).
async fn main(w: &mut World) -> String {
    let base = commit_on(
        &w.path,
        None,
        &[(
            "README", "base
",
        )],
    );
    w.push("main", Some(&base)).await;
    base
}

/// A branch `source` one commit ahead of `base`, adding `files`.
async fn branch(w: &mut World, base: &str, source: &str, files: &[(&str, &str)]) -> String {
    let mut all = vec![(
        "README", "base
",
    )];
    all.extend_from_slice(files);
    let head = commit_on(&w.path, Some(base), &all);
    w.push(source, Some(&head)).await;
    head
}

/// One version: one prepare job, and none of the four it replaces; it
/// writes forge's runs and the mergeability decision, and its version
/// event says `prepared`. It writes what the four jobs write for the same
/// change (the version job of a job enqueued before S6 still runs the old
/// way, which is the comparison).
#[tokio::test]
async fn prepare_writes_the_same_check_runs_as_the_four_jobs() {
    let mut w = World::new().await;
    let base = main(&mut w).await;
    let one = branch(
        &mut w,
        &base,
        "one",
        &[(".env", "A=1\n"), ("notes.md", "# One\n")],
    )
    .await;
    let two = branch(
        &mut w,
        &base,
        "two",
        &[(".env", "A=2\n"), ("notes.md", "# Two\n")],
    )
    .await;
    let prepared = w.open("one", "main").await;
    w.drain().await;
    let legacy = w.open("two", "main").await;
    // As if enqueued before S6: the version job, not the prepare job.
    w.t.exec_superuser(&format!(
        "UPDATE forge.jobs SET kind = 'merge_request.refresh', \
         dedup_key = replace(dedup_key, 'mr_prepare:', 'mr_refresh:') \
         WHERE kind = 'merge_request.prepare' AND payload ->> 'merge_request_id' = '{}'",
        legacy.id.get()
    ))
    .await;
    w.drain().await;

    let (mine, theirs) = (runs(&w, &one).await, runs(&w, &two).await);
    assert!(
        mine.iter()
            .any(|(name, _, _)| name == forge_core::checks::RISK_CHECK),
        "{mine:?}"
    );
    assert!(
        mine.iter()
            .any(|(name, _, _)| name == forge_core::checks::SPEC_LINT_CHECK),
        "{mine:?}"
    );
    assert_eq!(mine, theirs, "the same runs and answers");
    assert_eq!(
        w.status(&prepared).await.status,
        MergeabilityStatus::CanBeMerged
    );
    assert_eq!(
        w.status(&legacy).await.status,
        w.status(&prepared).await.status
    );

    let jobs = kinds(&w, &prepared, &one).await;
    for gone in [
        "merge_request.refresh",
        "merge_request.mergeability",
        "check.risk",
        "check.spec_index",
    ] {
        assert!(!jobs.iter().any(|k| k == gone), "{gone}: {jobs:?}");
    }
    assert_eq!(
        jobs.iter()
            .filter(|k| *k == "merge_request.prepare")
            .count(),
        1,
        "{jobs:?}"
    );
    let old = kinds(&w, &legacy, &two).await;
    for kind in [
        "merge_request.refresh",
        "merge_request.mergeability",
        "check.risk",
        "check.spec_index",
    ] {
        assert!(old.iter().any(|k| k == kind), "{kind}: {old:?}");
    }
    let events =
        w.t.json_rows(&format!(
            "SELECT payload FROM forge.events WHERE event_type = 'merge_request.pushed' \
             AND aggregate_id = {} ",
            prepared.id.get()
        ))
        .await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["payload"]["prepared"], true);
    w.t.drop_now().await;
}

/// A risk step that fails is handed over to its own `check.risk` job; the
/// prepare job still succeeds on its first attempt, the other steps are
/// done, and the risk job alone writes `forge/risk` once it can.
#[tokio::test]
async fn a_failed_risk_step_retries_only_risk() {
    let mut w = World::new().await;
    let base = main(&mut w).await;
    let head = branch(&mut w, &base, "feature", &[("notes.md", "# Notes\n")]).await;
    let mr = w.open("feature", "main").await;
    w.t.exec_superuser(
        "CREATE FUNCTION forge.s6_refuse_risk() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN IF NEW.name = 'forge/risk' THEN RAISE EXCEPTION 'synthetic s6 failure'; \
         END IF; RETURN NEW; END $$",
    )
    .await;
    w.t.exec_superuser(
        "CREATE TRIGGER s6_refuse_risk BEFORE INSERT OR UPDATE ON forge.check_runs \
         FOR EACH ROW EXECUTE FUNCTION forge.s6_refuse_risk()",
    )
    .await;
    let ran = w.runner.run_once().await.expect("runner").expect("ran");
    assert!(matches!(ran, Ran::Succeeded(_)), "{ran:?}");
    let attempts = w
        .t
        .json_rows("SELECT attempts, state FROM forge.jobs WHERE kind = 'merge_request.prepare'")
        .await;
    assert_eq!(attempts.len(), 1, "{attempts:?}");
    assert_eq!(attempts[0]["attempts"], 1, "{attempts:?}");
    assert_eq!(attempts[0]["state"], "succeeded");
    let jobs = kinds(&w, &mr, &head).await;
    assert_eq!(
        jobs.iter().filter(|k| *k == "check.risk").count(),
        1,
        "handed over: {jobs:?}"
    );
    assert!(!jobs.iter().any(|k| k == "check.spec_index"), "{jobs:?}");
    let before = runs(&w, &head).await;
    assert!(
        !before
            .iter()
            .any(|(name, _, _)| name == forge_core::checks::RISK_CHECK),
        "{before:?}"
    );
    assert!(
        before
            .iter()
            .any(|(name, _, _)| name == forge_core::checks::SPEC_LINT_CHECK),
        "the spec step went on: {before:?}"
    );
    assert_eq!(w.status(&mr).await.status, MergeabilityStatus::CanBeMerged);
    w.t.exec_superuser("DROP TRIGGER s6_refuse_risk ON forge.check_runs")
        .await;
    w.drain().await;
    let after = runs(&w, &head).await;
    assert!(
        after
            .iter()
            .any(|(name, _, _)| name == forge_core::checks::RISK_CHECK),
        "the risk job wrote it: {after:?}"
    );
    let prepares =
        w.t.json_rows("SELECT count(*) AS n FROM forge.jobs WHERE kind = 'merge_request.prepare'")
            .await;
    assert_eq!(prepares[0]["n"], 1, "the prepare job did not run again");
    w.t.drop_now().await;
}

/// The merge request pipeline: a document change whose merged tree has no
/// `.forge-ci.yml` gets no `ci.pipeline_create` job; a change with one gets
/// exactly one.
#[tokio::test]
async fn a_doc_mr_without_forge_ci_yml_gets_no_pipeline_job() {
    let mut w = World::new().await;
    let base = main(&mut w).await;
    let _doc = branch(&mut w, &base, "doc", &[("notes.md", "# Notes\n")]).await;
    let _ci = branch(
        &mut w,
        &base,
        "ci",
        &[(".forge-ci.yml", "version: 1\nstages: [build]\njobs: {}\n")],
    )
    .await;
    let doc = w.open("doc", "main").await;
    let ran = w.runner.run_once().await.expect("runner").expect("ran");
    assert!(matches!(ran, Ran::Succeeded(_)), "{ran:?}");
    let ci = w.open("ci", "main").await;
    let ran = w.runner.run_once().await.expect("runner").expect("ran");
    assert!(matches!(ran, Ran::Succeeded(_)), "{ran:?}");
    let pipelines = |mr: &StoredMergeRequest| {
        format!(
            "SELECT count(*) AS n FROM forge.jobs WHERE kind = 'ci.pipeline_create' \
             AND payload ->> 'merge_request_id' = '{}'",
            mr.id.get()
        )
    };
    let none = w.t.json_rows(&pipelines(&doc)).await;
    assert_eq!(none[0]["n"], 0, "no definition, no job");
    assert_eq!(w.status(&doc).await.status, MergeabilityStatus::CanBeMerged);
    let one = w.t.json_rows(&pipelines(&ci)).await;
    assert_eq!(one[0]["n"], 1, "a definition, one job");
    w.t.drop_now().await;
}

/// A trigger refusing writes of `table` rows matching `when` (on `NEW`).
async fn refuse(w: &World, name: &str, table: &str, when: &str) {
    w.t.exec_superuser(&format!(
        "CREATE FUNCTION forge.{name}() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN IF {when} THEN RAISE EXCEPTION 'synthetic s6 failure'; \
         END IF; RETURN NEW; END $$"
    ))
    .await;
    w.t.exec_superuser(&format!(
        "CREATE TRIGGER {name} BEFORE INSERT OR UPDATE ON {table} \
         FOR EACH ROW EXECUTE FUNCTION forge.{name}()"
    ))
    .await;
}

async fn allow(w: &World, name: &str, table: &str) {
    w.t.exec_superuser(&format!("DROP TRIGGER {name} ON {table}"))
        .await;
}

fn has(runs: &[(String, String, Option<String>)], name: &str) -> bool {
    runs.iter().any(|(n, _, _)| n == name)
}

/// The mergeability step fails (its result cannot be stored): the waiting
/// mergeability job is woken (ready now) and the prepare job still
/// succeeds with the other steps done; that job alone then decides.
#[tokio::test]
async fn a_failed_mergeability_step_wakes_its_own_job() {
    let mut w = World::new().await;
    let base = main(&mut w).await;
    let head = branch(&mut w, &base, "feature", &[("notes.md", "# Notes\n")]).await;
    let mr = w.open("feature", "main").await;
    refuse(
        &w,
        "s6_refuse_decision",
        "forge.merge_request_mergeability",
        "NEW.status = 'can_be_merged'",
    )
    .await;
    let ran = w.runner.run_once().await.expect("runner").expect("ran");
    assert!(matches!(ran, Ran::Succeeded(_)), "{ran:?}");
    let woken =
        w.t.json_rows(&format!(
            "SELECT run_after <= now() AS ready FROM forge.jobs \
             WHERE kind = 'merge_request.mergeability' AND state = 'queued' \
             AND payload ->> 'merge_request_id' = '{}'",
            mr.id.get()
        ))
        .await;
    assert_eq!(woken.len(), 1, "{woken:?}");
    assert_eq!(woken[0]["ready"], true, "woken, not waiting");
    let runs_now = runs(&w, &head).await;
    assert!(
        has(&runs_now, forge_core::checks::RISK_CHECK),
        "{runs_now:?}"
    );
    allow(&w, "s6_refuse_decision", "forge.merge_request_mergeability").await;
    w.drain().await;
    assert_eq!(w.status(&mr).await.status, MergeabilityStatus::CanBeMerged);
    w.t.drop_now().await;
}

/// A failed spec step is handed over to its own `check.spec_index` job
/// only; risk is done.
#[tokio::test]
async fn a_failed_spec_step_is_handed_over_to_its_own_job() {
    let mut w = World::new().await;
    let base = main(&mut w).await;
    let head = branch(&mut w, &base, "feature", &[("notes.md", "# Notes\n")]).await;
    let mr = w.open("feature", "main").await;
    refuse(
        &w,
        "s6_refuse_spec",
        "forge.check_runs",
        "NEW.name = 'forge/spec-lint'",
    )
    .await;
    let ran = w.runner.run_once().await.expect("runner").expect("ran");
    assert!(matches!(ran, Ran::Succeeded(_)), "{ran:?}");
    let jobs = kinds(&w, &mr, &head).await;
    assert_eq!(
        jobs.iter().filter(|k| *k == "check.spec_index").count(),
        1,
        "{jobs:?}"
    );
    assert!(!jobs.iter().any(|k| k == "check.risk"), "{jobs:?}");
    allow(&w, "s6_refuse_spec", "forge.check_runs").await;
    w.drain().await;
    let after = runs(&w, &head).await;
    assert!(
        has(&after, forge_core::checks::SPEC_LINT_CHECK),
        "{after:?}"
    );
    w.t.drop_now().await;
}

/// A failed risk step whose job key is held by a finished job (the head
/// was evaluated before under that key, nothing will run) is answered
/// `error` at once: never left without an answer.
#[tokio::test]
async fn a_hand_over_held_by_a_finished_job_answers_error() {
    let mut w = World::new().await;
    let base = main(&mut w).await;
    let head = branch(&mut w, &base, "feature", &[("notes.md", "# Notes\n")]).await;
    let mr = w.open("feature", "main").await;
    w.t.exec_superuser(&format!(
        "INSERT INTO forge.jobs (kind, payload, state, max_attempts, timeout_ms, dedup_key, \
         origin_request_id, finished_at) VALUES ('check.risk', '{{}}'::jsonb, 'succeeded', 5, \
         90000, 'risk:{p}:{head}:mr{mr}', 'req-old', now())",
        p = w.project.get(),
        mr = mr.id.get(),
    ))
    .await;
    refuse(
        &w,
        "s6_refuse_risk_answer",
        "forge.check_runs",
        "NEW.name = 'forge/risk' AND NEW.conclusion IS DISTINCT FROM 'error'",
    )
    .await;
    let ran = w.runner.run_once().await.expect("runner").expect("ran");
    assert!(matches!(ran, Ran::Succeeded(_)), "{ran:?}");
    let answered = forge_db::checks::check_runs(&w.t.db, w.project, &oid(&head))
        .await
        .expect("runs");
    let risk = answered
        .iter()
        .find(|r| r.name.as_str() == forge_core::checks::RISK_CHECK)
        .expect("forge/risk answered");
    assert_eq!(risk.conclusion, Some(forge_core::CheckConclusion::Error));
    allow(&w, "s6_refuse_risk_answer", "forge.check_runs").await;
    w.t.drop_now().await;
}

/// The version step decides the job's fate: a database failure while
/// recording the version is retried; a malformed payload is dead at once.
#[tokio::test]
async fn a_failed_refresh_step_is_retried_and_a_malformed_payload_is_dead() {
    let mut w = World::new().await;
    let base = main(&mut w).await;
    let _head = branch(&mut w, &base, "feature", &[("notes.md", "# Notes\n")]).await;
    let _mr = w.open("feature", "main").await;
    refuse(
        &w,
        "s6_refuse_version",
        "forge.merge_request_versions",
        "true",
    )
    .await;
    let ran = w.runner.run_once().await.expect("runner").expect("ran");
    assert!(matches!(ran, Ran::Retrying { .. }), "{ran:?}");
    allow(&w, "s6_refuse_version", "forge.merge_request_versions").await;
    w.t.exec_superuser(
        "INSERT INTO forge.jobs (kind, payload, state, max_attempts, timeout_ms, \
         origin_request_id) VALUES ('merge_request.prepare', '{\"merge_request_id\": 0}'::jsonb, \
         'queued', 5, 420000, 'req-bad')",
    )
    .await;
    let ran = w.runner.run_once().await.expect("runner").expect("ran");
    assert!(matches!(ran, Ran::Dead { .. }), "{ran:?}");
    w.t.drop_now().await;
}

/// Review of S6 (HIGH): a prepare job that dies after recording its
/// version leaves the head's checks to no one (the version says
/// `prepared`); the `prepare_sweep` answers them `error` once.
#[tokio::test]
async fn a_dead_prepare_job_after_its_version_gets_its_checks_answered() {
    let mut w = World::new().await;
    let base = main(&mut w).await;
    let head = branch(&mut w, &base, "feature", &[("notes.md", "# Notes\n")]).await;
    let mr = w.open("feature", "main").await;
    // The version step ran; then the job died (its worker was killed).
    let refs = forge_review::VersionRefs::new(
        w.storage.git.clone(),  // clone: the refs own their handles
        w.storage.root.clone(), // clone: as git
    );
    let report = forge_review::refresh_prepared(
        &w.t.db,
        &refs,
        forge_db::review::RefreshRequest {
            merge_request: mr.id,
            pushed_by: None,
        },
        false,
        &forge_core::RequestId::parse("req-died").expect("id"),
    )
    .await
    .expect("version");
    assert_eq!(report.outcome, "added");
    w.t.exec_superuser(
        "UPDATE forge.jobs SET state = 'dead', finished_at = now(), \
         last_error_code = 'FG-DB-0007' WHERE kind = 'merge_request.prepare'",
    )
    .await;
    assert!(runs(&w, &head).await.is_empty());
    let cursor = std::sync::atomic::AtomicI64::new(0);
    let next = crate::worker::prepare_sweep::sweep(&w.t.db, &cursor, 50)
        .await
        .expect("sweep");
    assert_eq!(next, crate::worker::supervise::Next::AfterPeriod);
    let answered = forge_db::checks::check_runs(&w.t.db, w.project, &oid(&head))
        .await
        .expect("runs");
    for name in [
        forge_core::checks::RISK_CHECK,
        forge_core::checks::SPEC_LINT_CHECK,
        forge_core::checks::SPEC_TRACE_CHECK,
        forge_core::checks::DOC_LINT_CHECK,
        forge_core::checks::CI_LINT_CHECK,
    ] {
        let run = answered
            .iter()
            .find(|r| r.name.as_str() == name)
            .unwrap_or_else(|| panic!("{name} answered: {answered:?}"));
        assert_eq!(run.conclusion, Some(forge_core::CheckConclusion::Error));
        assert!(run.summary.contains("FG-DB-0007"), "{}", run.summary);
    }
    // Answered: the next sweep writes nothing more.
    crate::worker::prepare_sweep::sweep(&w.t.db, &cursor, 50)
        .await
        .expect("again");
    let again = forge_db::checks::check_runs(&w.t.db, w.project, &oid(&head))
        .await
        .expect("runs");
    assert!(again.iter().all(|r| r.revision == 1), "{again:?}");
    w.t.drop_now().await;
}
