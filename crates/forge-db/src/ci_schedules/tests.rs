//! Pipeline schedules against a migrated database (M4 C12): create /
//! update / delete with their audit rows and the owner, the per-project
//! limit, the claim (next time from the database's clock, one job per due
//! time, nothing twice), and the fire (the pipeline, the schedule's last
//! run and one `pipeline.scheduled` row; skipped when already run, owned by
//! someone else or inactive; nothing written when the decision refuses).

use forge_core::{ObjectId, RefName, RequestId, UserId};
use forge_errors::{ErrorCode, ForgeError};

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::pipelines::{self, CopyMark, NewCiJob, NewPipeline, PipelineRequest, Planned, Source};
use crate::projects::{self, CreatedProject, NewProject};
use crate::test_support::*;
use crate::users::{self, NewUser};

fn ctx(actor: Actor, request: &str) -> ChangeContext {
    ChangeContext::new(actor, RequestId::parse(request).expect("request id"))
}

struct World {
    t: FreshDb,
    alice: UserId,
    bob: UserId,
    web: CreatedProject,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let alice = users::create(
        &t.db,
        &ctx(Actor::System, "req-user-alice"),
        &NewUser::human("alice", "alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let bob = users::create(
        &t.db,
        &ctx(Actor::System, "req-user-bob"),
        &NewUser::human("bob", "bob", "bob@example.test"),
    )
    .await
    .expect("bob");
    let web = projects::create(
        &t.db,
        &ctx(Actor::System, "req-project-web"),
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("web");
    World {
        t,
        alice: alice.user_id,
        bob: bob.user_id,
        web,
    }
}

fn input(cron: &str) -> ScheduleInput {
    ScheduleInput {
        description: "nightly".to_owned(),
        git_ref: RefName::parse("refs/heads/main").expect("ref"),
        cron: cron.to_owned(),
        utc_offset_minutes: 540,
        active: true,
    }
}

async fn create(w: &World, user: UserId, cron: &str) -> Result<i64, ForgeError> {
    create_schedule_decided(
        &w.t.db,
        &ctx(Actor::User(user), "req-schedule"),
        w.web.project_id,
        user,
        &input(cron),
        |_| Ok(()),
    )
    .await
    .map(|(id, ())| id)
}

async fn audit(w: &World) -> Vec<(String, String, Option<i64>, serde_json::Value)> {
    sqlx::query_as(
        "SELECT action, actor_kind, actor_user_id, details FROM forge.audit_log \
         WHERE (action LIKE 'pipeline_schedule.%' OR action = 'pipeline.scheduled') \
           AND outcome = 'succeeded' ORDER BY id",
    )
    .fetch_all(&mut w.t.superuser().await)
    .await
    .expect("audit")
}

/// Saving makes the saver the owner; each change is one audit row.
#[tokio::test]
async fn a_schedule_is_created_updated_and_deleted_with_its_owner_audited() {
    let w = world().await;
    let p = w.web.project_id;
    let id = create(&w, w.alice, "0 2 * * *").await.expect("create");
    let stored = get(&w.t.db, p, id).await.expect("get").expect("found");
    assert_eq!(stored.owner, w.alice);
    assert_eq!(stored.created_by, w.alice);
    assert_eq!(stored.cron, "0 2 * * *");
    assert!(stored.active);
    assert!(
        stored.next_run_at.is_some(),
        "an active schedule is due later"
    );
    assert_eq!(stored.last_outcome, None);

    let mut changed = input("30 3 * * 1-5");
    changed.active = false;
    update_schedule_decided(
        &w.t.db,
        &ctx(Actor::User(w.bob), "req-schedule-update"),
        (p, id),
        w.bob,
        &changed,
        |_| Ok(()),
    )
    .await
    .expect("update");
    let stored = get(&w.t.db, p, id).await.expect("get").expect("found");
    assert_eq!(stored.owner, w.bob, "whoever saves it owns it");
    assert_eq!(stored.created_by, w.alice);
    assert!(!stored.active);
    assert_eq!(
        stored.next_run_at, None,
        "an inactive schedule is never due"
    );

    let missing = update_schedule_decided(
        &w.t.db,
        &ctx(Actor::User(w.bob), "req-schedule-missing"),
        (p, id + 100),
        w.bob,
        &changed,
        |_| Ok(()),
    )
    .await
    .expect_err("missing");
    assert_eq!(missing.code(), ErrorCode::NotFound);

    let denied = delete_schedule_decided(
        &w.t.db,
        &ctx(Actor::User(w.bob), "req-schedule-denied"),
        (p, id),
        w.bob,
        |_| Err::<(), _>(ForgeError::new(ErrorCode::Forbidden)),
    )
    .await
    .expect_err("denied");
    assert_eq!(denied.code(), ErrorCode::Forbidden);
    assert!(get(&w.t.db, p, id).await.expect("get").is_some());
    delete_schedule_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-schedule-delete"),
        (p, id),
        w.alice,
        |_| Ok(()),
    )
    .await
    .expect("delete");
    assert!(list(&w.t.db, p).await.expect("list").is_empty());

    let rows = audit(&w).await;
    let actions: Vec<(&str, Option<i64>)> = rows.iter().map(|r| (r.0.as_str(), r.2)).collect();
    assert_eq!(
        actions,
        [
            ("pipeline_schedule.created", Some(w.alice.get())),
            ("pipeline_schedule.updated", Some(w.bob.get())),
            ("pipeline_schedule.deleted", Some(w.alice.get())),
        ]
    );
    assert_eq!(rows[0].3["owner_id"], w.alice.get());
    assert_eq!(rows[1].3["owner_id"], w.bob.get());
    assert_eq!(rows[1].3["cron"], "30 3 * * 1-5");
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_project_may_have_at_most_twenty_schedules() {
    let w = world().await;
    for _ in 0..MAX_PER_PROJECT {
        create(&w, w.alice, "0 2 * * *").await.expect("create");
    }
    let err = create(&w, w.alice, "0 2 * * *").await.expect_err("21st");
    assert_eq!(err.code(), ErrorCode::ScheduleLimitReached);
    assert_eq!(
        list(&w.t.db, w.web.project_id).await.expect("list").len(),
        20
    );
    w.t.drop_now().await;
}

async fn make_due(w: &World, id: i64, secs_ago: i64) {
    let _ = sqlx::query(
        "UPDATE forge.ci_pipeline_schedules SET next_run_at = now() - make_interval(secs => $2) \
         WHERE id = $1",
    )
    .bind(id)
    .bind(secs_ago as f64)
    .execute(&mut w.t.superuser().await)
    .await
    .expect("due");
}

async fn fire_jobs(w: &World) -> Vec<FireRequest> {
    let rows: Vec<serde_json::Value> =
        sqlx::query_scalar("SELECT payload FROM forge.jobs WHERE kind = $1 ORDER BY id")
            .bind(SCHEDULE_FIRE_JOB)
            .fetch_all(&mut w.t.superuser().await)
            .await
            .expect("jobs");
    rows.iter()
        .map(|p| fire_payload(p).expect("payload"))
        .collect()
}

/// A due schedule is claimed once: one job for its due time, the next time
/// moved after the database's now; a second claim finds nothing. An
/// inactive schedule is never claimed.
#[tokio::test]
async fn a_due_schedule_is_claimed_once_with_one_job() {
    let w = world().await;
    let id = create(&w, w.alice, "*/5 * * * *").await.expect("create");
    let idle = create(&w, w.alice, "0 2 * * *").await.expect("idle");
    let mut off = input("0 2 * * *");
    off.active = false;
    update_schedule_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-off"),
        (w.web.project_id, idle),
        w.alice,
        &off,
        |_| Ok(()),
    )
    .await
    .expect("off");
    make_due(&w, id, 3600).await;
    let origin = RequestId::generate();
    let claimed = claim_due(&w.t.db, &origin).await.expect("claim");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].schedule, id);
    assert_eq!(fire_jobs(&w).await, claimed);
    let future: bool = sqlx::query_scalar(
        "SELECT next_run_at > now() FROM forge.ci_pipeline_schedules WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&mut w.t.superuser().await)
    .await
    .expect("next");
    assert!(future, "missed times are run once, then the next is ahead");
    assert!(claim_due(&w.t.db, &origin).await.expect("again").is_empty());
    let job = fire_job(&claimed[0], origin).expect("job");
    assert_eq!(
        job.dedup_key.expect("key").as_str(),
        format!("ci_schedule:{id}:{}", claimed[0].due)
    );
    w.t.drop_now().await;
}

fn scheduled(w: &World, owner: UserId) -> NewPipeline {
    NewPipeline {
        request: PipelineRequest {
            project: w.web.project_id,
            git_ref: RefName::parse("refs/heads/main").expect("ref"),
            sha: ObjectId::parse(&"c".repeat(40)).expect("sha"),
            source: Source::Schedule,
            merge_request: None,
            triggered_by: Some(owner),
        },
        request_id: "req-fire".to_owned(),
        definition_hash: Some(pipelines::definition_hash(b"version: 1")),
        protected: false,
        copy_mark: CopyMark::Open,
        planned: Planned::Jobs(vec![NewCiJob {
            name: "nightly".to_owned(),
            stage: "test".to_owned(),
            wave: 0,
            needs: Vec::new(),
            image: format!("registry.internal/rust@sha256:{}", "a".repeat(64)),
            timeout_s: 600,
            tags: Vec::new(),
            secrets: Vec::new(),
            manual: false,
        }]),
        ai_eval_job: None,
    }
}

/// The fire writes the pipeline (`schedule`, the owner as `triggered_by`,
/// never protected), the schedule's last run and one `pipeline.scheduled`
/// row by `system`; the same due time again is skipped; a refused decision
/// writes nothing; a schedule saved by someone else since is skipped.
#[tokio::test]
async fn a_fire_writes_the_pipeline_once_per_due_time() {
    let w = world().await;
    let id = create(&w, w.alice, "0 2 * * *").await.expect("create");
    let request = FireRequest {
        schedule: id,
        project: w.web.project_id,
        due: 1_794_589_200,
    };
    let system = ctx(Actor::System, "req-fire");
    let refused = fire_schedule_decided(
        &w.t.db,
        &system,
        &request,
        w.alice,
        &scheduled(&w, w.alice),
        |_| Err::<(), _>(ForgeError::new(ErrorCode::ScheduleOwnerNotAllowed)),
    )
    .await
    .expect_err("refused");
    assert_eq!(refused.code(), ErrorCode::ScheduleOwnerNotAllowed);
    assert!(pipelines::list(&w.t.db, w.web.project_id, None, None, 10)
        .await
        .expect("list")
        .is_empty());

    let (fired, decided) = fire_schedule_decided(
        &w.t.db,
        &system,
        &request,
        w.alice,
        &scheduled(&w, w.alice),
        |_| Ok(()),
    )
    .await
    .expect("fire");
    let Fired::Created(pipeline) = fired else {
        panic!("created: {fired:?}")
    };
    assert_eq!(decided, Some(()));
    let (stored, jobs) = pipelines::get(&w.t.db, w.web.project_id, pipeline)
        .await
        .expect("get")
        .expect("found");
    assert_eq!(stored.source, Source::Schedule);
    assert_eq!(stored.triggered_by, Some(w.alice));
    assert!(!stored.protected && !stored.secrets_allowed());
    assert_eq!(jobs.len(), 1);
    let schedule = get(&w.t.db, w.web.project_id, id)
        .await
        .expect("get")
        .expect("found");
    assert_eq!(schedule.last_pipeline_id, Some(pipeline));
    assert_eq!(schedule.last_outcome, Some(LastOutcome::Created));
    assert_eq!(schedule.last_run_epoch, Some(request.due));

    let (again, _) = fire_schedule_decided(
        &w.t.db,
        &system,
        &request,
        w.alice,
        &scheduled(&w, w.alice),
        |_| Ok(()),
    )
    .await
    .expect("again");
    assert_eq!(again, Fired::Skipped("already_ran"));
    let later = FireRequest {
        due: request.due + 86_400,
        ..request
    };
    let (other, _) = fire_schedule_decided(
        &w.t.db,
        &system,
        &later,
        w.bob,
        &scheduled(&w, w.bob),
        |_| Ok(()),
    )
    .await
    .expect("other owner");
    assert_eq!(other, Fired::Skipped("owner_changed"));

    let rows = audit(&w).await;
    let fires: Vec<_> = rows
        .iter()
        .filter(|r| r.0 == "pipeline.scheduled")
        .collect();
    assert_eq!(fires.len(), 1);
    assert_eq!(fires[0].1, "system");
    assert_eq!(fires[0].3["owner_id"], w.alice.get());
    assert_eq!(fires[0].3["schedule_id"], id);
    assert_eq!(fires[0].3["pipeline_id"], pipeline);

    record_outcome(
        &w.t.db,
        &later,
        LastOutcome::Refused,
        Some(ErrorCode::ScheduleOwnerNotAllowed),
    )
    .await
    .expect("outcome");
    let schedule = get(&w.t.db, w.web.project_id, id)
        .await
        .expect("get")
        .expect("found");
    assert_eq!(schedule.last_outcome, Some(LastOutcome::Refused));
    assert_eq!(schedule.last_error_code.as_deref(), Some("FG-CI-0026"));
    assert_eq!(schedule.last_pipeline_id, None);
    w.t.drop_now().await;
}

#[test]
fn the_fire_payload_round_trips_and_refuses_anything_else() {
    let request = FireRequest {
        schedule: 3,
        project: forge_core::ProjectId::new(7),
        due: 1_794_589_200,
    };
    let job = fire_job(&request, RequestId::generate()).expect("job");
    assert_eq!(job.kind.as_str(), SCHEDULE_FIRE_JOB);
    assert_eq!(fire_payload(&job.payload).expect("parse"), request);
    for bad in [
        serde_json::json!({}),
        serde_json::json!({"schedule_id": 0, "project_id": 7, "due": 1}),
        serde_json::json!({"schedule_id": 3, "project_id": "7", "due": 1}),
    ] {
        assert!(fire_payload(&bad).is_err(), "{bad}");
    }
}

/// M4 C12 review: a due run starts nothing while the schedule's previous
/// pipeline is pending or running (`busy`), nor past the hourly cap of
/// scheduled pipelines per project (`capped`).
#[tokio::test]
async fn a_due_run_is_skipped_while_busy_or_past_the_hourly_cap() {
    let w = world().await;
    let id = create(&w, w.alice, "0 2 * * *").await.expect("create");
    let system = ctx(Actor::System, "req-fire");
    let at = |due: i64| FireRequest {
        schedule: id,
        project: w.web.project_id,
        due,
    };
    let fire = |due: i64| {
        let db = w.t.db.clone();
        let system = system.clone();
        let new = scheduled(&w, w.alice);
        let alice = w.alice;
        let request = at(due);
        async move {
            fire_schedule_decided(&db, &system, &request, alice, &new, |_| Ok(()))
                .await
                .expect("fire")
                .0
        }
    };
    let Fired::Created(first) = fire(1_794_589_200).await else {
        panic!("created")
    };
    assert_eq!(
        fire(1_794_589_500).await,
        Fired::Busy,
        "the first still pending"
    );
    let _ = sqlx::query("UPDATE forge.ci_pipelines SET status = 'succeeded' WHERE id = $1")
        .bind(first)
        .execute(&mut w.t.superuser().await)
        .await
        .expect("finish");
    // Eleven more scheduled pipelines of the project in the last hour.
    for _ in 0..(MAX_PROJECT_PER_HOUR - 1) {
        let mut done = scheduled(&w, w.alice);
        done.planned = Planned::Jobs(Vec::new());
        pipelines::create(&w.t.db, &done).await.expect("scheduled");
    }
    assert_eq!(
        fire(1_794_589_800).await,
        Fired::Capped("project", MAX_PROJECT_PER_HOUR)
    );
    w.t.drop_now().await;
}

/// M4 C12 review: a refused owner stops the schedule, audited by `system`;
/// a description or pause/resume edit keeps the owner, a rule change or a
/// take-over after a refusal makes the editor the owner.
#[tokio::test]
async fn a_refused_owner_stops_the_schedule_and_only_a_real_change_moves_the_owner() {
    let w = world().await;
    let p = w.web.project_id;
    let id = create(&w, w.alice, "0 2 * * *").await.expect("create");
    // A pause by bob keeps alice as the owner.
    let mut paused = input("0 2 * * *");
    paused.active = false;
    paused.description = "nightly (paused)".to_owned();
    update_schedule_decided(
        &w.t.db,
        &ctx(Actor::User(w.bob), "req-pause"),
        (p, id),
        w.bob,
        &paused,
        |_| Ok(()),
    )
    .await
    .expect("pause");
    let stored = get(&w.t.db, p, id).await.expect("get").expect("found");
    assert_eq!(stored.owner, w.alice, "a toggle keeps the owner");
    // Resumed by alice, then refused at a due run: stopped.
    update_schedule_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-resume"),
        (p, id),
        w.alice,
        &input("0 2 * * *"),
        |_| Ok(()),
    )
    .await
    .expect("resume");
    let request = FireRequest {
        schedule: id,
        project: p,
        due: 1_794_589_200,
    };
    let system = ctx(Actor::System, "req-refused");
    assert!(
        deactivate_refused(&w.t.db, &system, &request, w.alice, "FG-AUTH-0003")
            .await
            .expect("deactivate")
    );
    assert!(
        !deactivate_refused(&w.t.db, &system, &request, w.alice, "FG-AUTH-0003")
            .await
            .expect("again"),
        "already stopped: nothing written"
    );
    let stored = get(&w.t.db, p, id).await.expect("get").expect("found");
    assert!(!stored.active);
    assert_eq!(stored.next_run_at, None);
    assert_eq!(stored.last_outcome, Some(LastOutcome::Refused));
    let rows = audit(&w).await;
    let stopped: Vec<_> = rows
        .iter()
        .filter(|r| r.0 == "pipeline_schedule.deactivated")
        .collect();
    assert_eq!(stopped.len(), 1);
    assert_eq!(stopped[0].1, "system");
    assert_eq!(stopped[0].3["cause_code"], "FG-AUTH-0003");
    // bob saves it again (the same settings): he takes it over.
    update_schedule_decided(
        &w.t.db,
        &ctx(Actor::User(w.bob), "req-take-over"),
        (p, id),
        w.bob,
        &input("0 2 * * *"),
        |_| Ok(()),
    )
    .await
    .expect("take over");
    let stored = get(&w.t.db, p, id).await.expect("get").expect("found");
    assert_eq!(stored.owner, w.bob);
    assert!(stored.active);
    w.t.drop_now().await;
}
