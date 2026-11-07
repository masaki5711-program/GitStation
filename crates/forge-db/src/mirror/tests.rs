//! The push mirror against a migrated database (M3 O2; migration
//! 20261130100000): the decided set / remove with their audit rows, the
//! subscriber's waiting mark and job, the job's start / success / failure
//! bookkeeping, the lag sweep, the constraints and the read pool.

use std::time::Duration;

use forge_core::ids::{ProjectId, RepoId, UserId};
use forge_core::{
    ObjectId, PushRecord, PushUpdateKind, PushedRef, Pusher, RefName, RequestId, UserKind,
};
use forge_errors::ErrorCode;

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::projects::{self, CreatedProject, NewProject};
use crate::repositories::{mark_ready, push_context, record_push};
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

fn setting(url: &str, enabled: bool) -> MirrorSetting {
    MirrorSetting {
        url: url.to_owned(),
        origin: "https://gitlab.example.invalid:443".to_owned(),
        credential_name: "gitlab".to_owned(),
        enabled,
    }
}

const URL: &str = "https://gitlab.example.invalid/ymma/app.git";

struct World {
    t: FreshDb,
    admin: UserId,
    app: CreatedProject,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let admin = users::create(
        &t.db,
        &system("req-user-mira"),
        &NewUser::human("mira", "Mira", "mira@example.test"),
    )
    .await
    .expect("admin");
    let app = projects::create(
        &t.db,
        &system("req-project-app"),
        &NewProject::new("app", admin.namespace_id, "App", admin.user_id),
    )
    .await
    .expect("app");
    let marked = mark_ready(&t.db, &system("req-ready"), app.repository_id)
        .await
        .expect("ready");
    assert!(matches!(
        marked,
        crate::repositories::MarkReady::Marked { .. }
    ));
    subscribers::register(&t.db, &PushMirrorSubscriber, Start::Head)
        .await
        .expect("register");
    World {
        t,
        admin: admin.user_id,
        app,
    }
}

impl World {
    fn project(&self) -> ProjectId {
        self.app.project_id
    }

    fn repo(&self) -> RepoId {
        self.app.repository_id
    }

    async fn set(&self, request: &str, s: &MirrorSetting) -> SetOutcome {
        set_decided(
            &self.t.db,
            &as_user(self.admin, request),
            self.project(),
            self.admin,
            s,
            |_| Ok(()),
        )
        .await
        .map(|(o, ())| o)
        .expect("set")
    }

    async fn scalar(&self, sql: &str) -> i64 {
        sqlx::query_scalar(sql)
            .fetch_one(&mut self.t.superuser().await)
            .await
            .expect(sql)
    }

    async fn jobs(&self) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT dedup_key FROM forge.jobs WHERE kind = 'repository.push_mirror' ORDER BY id",
        )
        .fetch_all(&mut self.t.superuser().await)
        .await
        .expect("jobs")
    }

    async fn push(&self, request: &str, c: char) {
        let oid = |c: char| ObjectId::parse(&c.to_string().repeat(40)).expect("oid");
        let new = oid(c);
        let record = PushRecord {
            repository_id: self.repo(),
            updates: vec![PushedRef {
                name: RefName::parse("refs/heads/main").expect("ref"),
                old: oid('0'),
                new: new.clone(), // clone: also the commit list
                kind: PushUpdateKind::Create,
                total_commits: 1,
                total_commits_partial: false,
                commits: vec![new],
            }],
        };
        let pusher = Pusher {
            request_id: RequestId::parse(request).expect("id"),
            user: self.admin,
            kind: UserKind::Human,
            credential_id: None,
            source_ip: None,
            user_agent: None,
        };
        let recorded = record_push(&self.t.db, &push_context(&pusher), &record)
            .await
            .expect("push");
        assert_eq!(recorded, forge_core::PushRecorded::Recorded);
    }

    /// Number the events, then run the subscriber until it caught up.
    async fn drain(&self) {
        crate::sequencer::run_once(
            &self.t.db,
            crate::sequencer::BatchSize::MAX,
            crate::sequencer::AuditSequencing::Skip,
        )
        .await
        .expect("sequence");
        loop {
            match subscribers::run_once(&self.t.db, &PushMirrorSubscriber)
                .await
                .expect("run")
            {
                Progress::Handled { .. } => {}
                Progress::CaughtUp => return,
                other @ (Progress::Busy
                | Progress::Disabled
                | Progress::Failed(_)
                | Progress::Stuck(_)
                | Progress::Quarantined(_)) => panic!("unexpected {other:?}"),
            }
        }
    }
}

#[tokio::test]
async fn setting_a_mirror_audits_it_marks_it_waiting_and_enqueues_one_push() {
    let w = world().await;
    assert_eq!(
        w.set("req-set", &setting(URL, true)).await,
        SetOutcome::Created
    );
    let got = get(&w.t.db, w.project())
        .await
        .expect("get")
        .expect("mirror");
    assert_eq!(got.repo, w.repo());
    assert_eq!(got.url, URL);
    assert_eq!(got.credential_name, "gitlab");
    assert!(got.enabled);
    assert!(
        got.status.pending_since.is_some(),
        "behind until pushed once"
    );
    assert_eq!(
        w.jobs().await,
        ["push_mirror:1:set:req-set".replace(":1:", &format!(":{}:", w.repo().get()))]
    );
    assert_eq!(
        w.scalar(
            "SELECT count(*) FROM forge.audit_log WHERE action = 'push_mirror.set' \
             AND details->>'created' = 'true' AND details->>'origin' = 'https://gitlab.example.invalid:443' \
             AND details::text NOT LIKE '%ymma%'"
        )
        .await,
        1,
        "audited with the origin, never the full url"
    );
    // The same values again: nothing written.
    assert_eq!(
        w.set("req-set-again", &setting(URL, true)).await,
        SetOutcome::Unchanged
    );
    assert_eq!(w.jobs().await.len(), 1);
    // Disabling clears the waiting mark (no lag alert for a disabled mirror).
    assert_eq!(
        w.set("req-off", &setting(URL, false)).await,
        SetOutcome::Updated
    );
    let got = get(&w.t.db, w.project())
        .await
        .expect("get")
        .expect("mirror");
    assert!(!got.enabled);
    assert!(got.status.pending_since.is_none());
    assert_eq!(w.jobs().await.len(), 1, "a disabled mirror gets no job");
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_refused_decision_or_a_project_without_a_ready_repository_writes_nothing() {
    let w = world().await;
    let e = set_decided(
        &w.t.db,
        &as_user(w.admin, "req-denied"),
        w.project(),
        w.admin,
        &setting(URL, true),
        |_| -> Result<(), ForgeError> { Err(ForgeError::new(ErrorCode::Forbidden)) },
    )
    .await
    .expect_err("denied");
    assert_eq!(e.code(), ErrorCode::Forbidden);
    // A second project whose repository is still provisioning.
    let user = users::create(
        &w.t.db,
        &system("req-user-2"),
        &NewUser::human("u2", "u2", "u2@example.test"),
    )
    .await
    .expect("user");
    let p2 = projects::create(
        &w.t.db,
        &system("req-p2"),
        &NewProject::new("p2", user.namespace_id, "P2", user.user_id),
    )
    .await
    .expect("p2");
    let e = set_decided(
        &w.t.db,
        &as_user(w.admin, "req-norepo"),
        p2.project_id,
        w.admin,
        &setting(URL, true),
        |_| Ok(()),
    )
    .await
    .expect_err("no repo");
    assert_eq!(e.code(), ErrorCode::PushMirrorTargetInvalid);
    assert!(e
        .context()
        .iter()
        .any(|(k, v)| *k == "reason" && v == "no_repository"));
    assert_eq!(w.scalar("SELECT count(*) FROM forge.push_mirrors").await, 0);
    assert!(w.jobs().await.is_empty());
    w.t.drop_now().await;
}

#[tokio::test]
async fn each_push_marks_the_mirror_waiting_and_enqueues_its_own_job() {
    let w = world().await;
    // Without a mirror a push enqueues nothing.
    w.push("req-push-0", 'a').await;
    w.drain().await;
    assert!(w.jobs().await.is_empty());
    w.set("req-set", &setting(URL, true)).await;
    w.push("req-push-1", 'b').await;
    w.push("req-push-2", 'c').await;
    w.drain().await;
    let jobs = w.jobs().await;
    assert_eq!(jobs.len(), 3, "the set's push and one per push: {jobs:?}");
    assert!(jobs[1].contains(":push:"), "{jobs:?}");
    let pending = w
        .scalar("SELECT pending_seq FROM forge.push_mirror_status")
        .await;
    assert!(pending > 0);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_success_covers_what_it_read_and_a_newer_push_keeps_it_waiting() {
    let w = world().await;
    w.set("req-set", &setting(URL, true)).await;
    w.push("req-push-1", 'b').await;
    w.drain().await;
    let target = start(&w.t.db, w.repo())
        .await
        .expect("start")
        .expect("target");
    assert_eq!(target.url, URL);
    assert_eq!(target.credential_name, "gitlab");
    // A push arrives while the job runs.
    w.push("req-push-2", 'c').await;
    w.drain().await;
    succeeded(&w.t.db, w.repo(), target.covers_seq)
        .await
        .expect("ok");
    let got = get(&w.t.db, w.project()).await.expect("get").expect("m");
    assert!(
        got.status.pending_since.is_some(),
        "the newer push still waits"
    );
    assert_eq!(got.status.mirrored_seq, target.covers_seq);
    assert!(got.status.last_success_at.is_some());
    // The next job covers it.
    let next = start(&w.t.db, w.repo())
        .await
        .expect("start")
        .expect("target");
    succeeded(&w.t.db, w.repo(), next.covers_seq)
        .await
        .expect("ok");
    let got = get(&w.t.db, w.project()).await.expect("get").expect("m");
    assert!(got.status.pending_since.is_none(), "up to date");
    w.t.drop_now().await;
}

#[tokio::test]
async fn failures_count_up_and_a_success_clears_them() {
    let w = world().await;
    w.set("req-set", &setting(URL, true)).await;
    start(&w.t.db, w.repo()).await.expect("start").expect("t");
    assert_eq!(
        failed(&w.t.db, w.repo(), ErrorCode::PushMirrorFailed, "auth")
            .await
            .expect("f"),
        1
    );
    assert_eq!(
        failed(
            &w.t.db,
            w.repo(),
            ErrorCode::PushMirrorFailed,
            "Not A Reason!"
        )
        .await
        .expect("f"),
        2
    );
    let got = get(&w.t.db, w.project()).await.expect("get").expect("m");
    assert_eq!(got.status.failures, 2);
    assert_eq!(got.status.last_error_code.as_deref(), Some("FG-INT-0013"));
    assert_eq!(
        got.status.last_reason.as_deref(),
        Some("git"),
        "an unfit reason is `git`"
    );
    let rep = report(&w.t.db, Duration::from_secs(900))
        .await
        .expect("report");
    assert_eq!((rep.mirrors, rep.enabled, rep.failing.len()), (1, 1, 1));
    succeeded(&w.t.db, w.repo(), 0).await.expect("ok");
    let got = get(&w.t.db, w.project()).await.expect("get").expect("m");
    assert_eq!(got.status.failures, 0);
    assert!(got.status.last_error_code.is_none());
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_disabled_mirror_or_a_purging_repository_starts_nothing() {
    let w = world().await;
    assert!(
        start(&w.t.db, w.repo()).await.expect("start").is_none(),
        "no mirror"
    );
    w.set("req-set", &setting(URL, false)).await;
    assert!(
        start(&w.t.db, w.repo()).await.expect("start").is_none(),
        "disabled"
    );
    let e = enqueue_now(
        &w.t.db,
        w.repo(),
        &RequestId::parse("req-admin").expect("id"),
    )
    .await
    .expect_err("disabled");
    assert_eq!(e.code(), ErrorCode::NotFound);
    w.set("req-on", &setting(URL, true)).await;
    assert!(matches!(
        enqueue_now(
            &w.t.db,
            w.repo(),
            &RequestId::parse("req-admin").expect("id")
        )
        .await
        .expect("enqueue"),
        Enqueued::Queued(_)
    ));
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_mirror_behind_the_threshold_is_int_0014() {
    let w = world().await;
    w.set("req-set", &setting(URL, true)).await;
    w.t.exec_superuser(
        "UPDATE forge.push_mirror_status SET pending_since = now() - interval '20 minutes'",
    )
    .await;
    let lag = lagging(&w.t.db, Duration::from_secs(900))
        .await
        .expect("lag");
    assert_eq!(lag.len(), 1);
    assert_eq!(lag[0].repo, w.repo());
    assert!(lag[0].lag_secs >= 1200);
    // Alerted once an hour, not every sweep.
    assert!(lagging(&w.t.db, Duration::from_secs(900))
        .await
        .expect("lag")
        .is_empty());
    // Doctor sees it whatever the alert time.
    let rep = report(&w.t.db, Duration::from_secs(900))
        .await
        .expect("report");
    assert_eq!(rep.lagging.len(), 1);
    // Below the threshold: nothing.
    let rep = report(&w.t.db, Duration::from_secs(3600))
        .await
        .expect("report");
    assert!(rep.lagging.is_empty());
    w.t.drop_now().await;
}

#[tokio::test]
async fn removing_a_mirror_deletes_its_status_and_is_audited() {
    let w = world().await;
    w.set("req-set", &setting(URL, true)).await;
    remove_decided(
        &w.t.db,
        &as_user(w.admin, "req-rm"),
        w.project(),
        w.admin,
        |_| Ok(()),
    )
    .await
    .expect("remove");
    assert!(get(&w.t.db, w.project()).await.expect("get").is_none());
    assert_eq!(
        w.scalar("SELECT count(*) FROM forge.push_mirror_status")
            .await,
        0
    );
    assert_eq!(
        w.scalar("SELECT count(*) FROM forge.audit_log WHERE action = 'push_mirror.removed'")
            .await,
        1
    );
    let e = remove_decided(
        &w.t.db,
        &as_user(w.admin, "req-rm2"),
        w.project(),
        w.admin,
        |_| Ok(()),
    )
    .await
    .expect_err("gone");
    assert_eq!(e.code(), ErrorCode::NotFound);
    // The queued job finds nothing.
    assert!(start(&w.t.db, w.repo()).await.expect("start").is_none());
    w.t.drop_now().await;
}

#[tokio::test]
async fn mirror_checks_refuse_raw_rows_and_the_read_pool_never_reads_the_url() {
    let w = world().await;
    w.set("req-set", &setting(URL, true)).await;
    let mut su = w.t.superuser().await;
    for (sql, constraint) in [
        (
            "UPDATE forge.push_mirrors SET url = 'ssh://x/y'",
            "push_mirrors_url",
        ),
        (
            "UPDATE forge.push_mirrors SET url = 'https://u:p@x/y'",
            "push_mirrors_url",
        ),
        (
            "UPDATE forge.push_mirrors SET credential_name = '../x'",
            "push_mirrors_credential_name",
        ),
        (
            "UPDATE forge.push_mirror_status SET failures = 1",
            "push_mirror_status_failure_shape",
        ),
        (
            "UPDATE forge.push_mirror_status SET mirrored_seq = pending_seq + 1",
            "push_mirror_status_mirrored_le_pending",
        ),
    ] {
        let e = sqlx::raw_sql(sql).execute(&mut su).await.expect_err(sql);
        let Some(d) = e.as_database_error() else {
            panic!("{sql}: {e:?}");
        };
        let name = d.constraint().map(str::to_owned);
        assert_eq!(name.as_deref(), Some(constraint), "{sql}");
    }
    sqlx::raw_sql("SET ROLE forge_ro")
        .execute(&mut su)
        .await
        .expect("role");
    let state = match sqlx::raw_sql("SELECT url FROM forge.push_mirrors")
        .execute(&mut su)
        .await
    {
        Err(sqlx::Error::Database(e)) => e.code().map(|c| c.into_owned()),
        other => panic!("{other:?}"),
    };
    assert_eq!(state.as_deref(), Some("42501"));
    sqlx::raw_sql("SELECT origin, credential_name FROM forge.push_mirrors")
        .execute(&mut su)
        .await
        .expect("origin is readable");
    drop(su);
    w.t.drop_now().await;
}
