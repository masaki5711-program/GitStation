//! CI variables against a migrated database (M4 C6): set and replace with
//! their audit rows, delete, the per-project limit, the read pool that
//! cannot read a ciphertext, the masked values of a job (only for a
//! protected push), and the claim that fails a job with a code.

use forge_core::ci_secret::SealedValue;
use forge_core::{ObjectId, RequestId, UserId};
use forge_errors::ErrorCode;

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::ci_runners::lease;
use crate::ci_runners::{register, NewRunner};
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
        web,
    }
}

async fn put(
    db: &Db,
    user: UserId,
    project: forge_core::ProjectId,
    v: &NewValue,
) -> Result<(StoredVariable, bool), forge_errors::ForgeError> {
    put_variable_decided(
        db,
        &ctx(Actor::User(user), "req-ci-var"),
        project,
        v,
        |_| Ok(()),
    )
    .await
}

async fn delete(
    db: &Db,
    c: &ChangeContext,
    project: forge_core::ProjectId,
    name: &str,
) -> Result<bool, forge_errors::ForgeError> {
    delete_variable_decided(db, c, project, name, |_| Ok(())).await
}

fn value(name: &str, byte: u8, masked: bool) -> NewValue {
    NewValue {
        name: name.to_owned(),
        sealed: SealedValue {
            key_id: 1,
            ciphertext: vec![byte; 40],
        },
        value_bytes: 12,
        protected_only: true,
        masked,
    }
}

impl World {
    async fn put(&self, v: &NewValue) -> (StoredVariable, bool) {
        put(&self.t.db, self.alice, self.web.project_id, v)
            .await
            .expect("put")
    }

    async fn audit_actions(&self) -> Vec<String> {
        let mut su = self.t.superuser().await;
        sqlx::query_scalar(
            "SELECT action FROM forge.audit_log WHERE action LIKE 'ci_variable.%' ORDER BY id",
        )
        .fetch_all(&mut su)
        .await
        .expect("audit")
    }

    async fn pipeline(&self, protected: bool, source: Source, secrets: &[&str]) -> (i64, i64) {
        let new = NewPipeline {
            request: PipelineRequest {
                project: self.web.project_id,
                git_ref: forge_core::RefName::parse("refs/heads/main").expect("ref"),
                sha: ObjectId::parse(&(if protected { "a" } else { "b" }).repeat(40)).expect("sha"),
                source,
                merge_request: None,
                triggered_by: None,
            },
            request_id: "req-plan".to_owned(),
            definition_hash: Some(pipelines::definition_hash(b"version: 1")),
            protected,
            copy_mark: CopyMark::Open,
            planned: Planned::Jobs(vec![NewCiJob {
                name: "deploy".to_owned(),
                stage: "deploy".to_owned(),
                wave: 0,
                needs: Vec::new(),
                image: format!("registry.internal/rust@sha256:{}", "a".repeat(64)),
                timeout_s: 600,
                tags: Vec::new(),
                secrets: secrets.iter().map(|s| (*s).to_owned()).collect(),
            }]),
        };
        let pipeline = pipelines::create(&self.t.db, &new)
            .await
            .expect("pipeline")
            .pipeline_id;
        let mut su = self.t.superuser().await;
        let job: i64 = sqlx::query_scalar("SELECT id FROM forge.ci_jobs WHERE pipeline_id = $1")
            .bind(pipeline)
            .fetch_one(&mut su)
            .await
            .expect("job");
        (pipeline, job)
    }
}

#[tokio::test]
async fn a_variable_is_set_replaced_and_deleted_with_an_audit_row_each() {
    let w = world().await;
    let (stored, created) = w.put(&value("DEPLOY_TOKEN", 1, true)).await;
    assert!(created);
    assert_eq!(stored.name, "DEPLOY_TOKEN");
    assert!(stored.masked && stored.protected_only);
    let (again, created) = w.put(&value("DEPLOY_TOKEN", 2, false)).await;
    assert!(!created);
    assert!(!again.masked);
    let listed = list(&w.t.db, w.web.project_id).await.expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].value_bytes, 12);
    let ctx = ctx(Actor::User(w.alice), "req-ci-var-del");
    assert!(delete(&w.t.db, &ctx, w.web.project_id, "DEPLOY_TOKEN")
        .await
        .expect("delete"));
    assert!(!delete(&w.t.db, &ctx, w.web.project_id, "DEPLOY_TOKEN")
        .await
        .expect("nothing left"));
    assert_eq!(
        w.audit_actions().await,
        vec![
            "ci_variable.created",
            "ci_variable.updated",
            "ci_variable.deleted"
        ]
    );
    // The audit row never holds the ciphertext.
    let mut su = w.t.superuser().await;
    let details: Vec<String> = sqlx::query_scalar(
        "SELECT details::text FROM forge.audit_log WHERE action LIKE 'ci_variable.%'",
    )
    .fetch_all(&mut su)
    .await
    .expect("details");
    assert!(
        details.iter().all(|d| !d.contains("ciphertext")),
        "{details:?}"
    );
}

#[tokio::test]
async fn the_read_pool_never_reads_a_ciphertext_and_the_table_refuses_reserved_names() {
    let w = world().await;
    w.put(&value("TOKEN_A", 1, true)).await;
    let e = sqlx::query("SELECT ciphertext FROM forge.ci_variables")
        .execute(w.t.db.pool().executor())
        .await
        .expect_err("no ciphertext for forge_ro")
        .to_string()
        .to_ascii_lowercase();
    assert!(e.contains("permission denied"), "{e}");
    for name in ["FORGE_X", "CI_X", "PATH", "lower"] {
        let mut v = value(name, 1, true);
        v.name = name.to_owned();
        let e = put(&w.t.db, w.alice, w.web.project_id, &v)
            .await
            .expect_err(name);
        assert_eq!(e.code(), ErrorCode::DbQueryFailed, "{name}: {e}");
    }
}

#[tokio::test]
async fn a_project_holds_at_most_its_limit_of_variables() {
    let w = world().await;
    // Fill up to the limit, then one more is refused.
    for i in 0..MAX_VARIABLES_PER_PROJECT {
        w.put(&value(&format!("V{i}"), 1, false)).await;
    }
    let e = put(
        &w.t.db,
        w.alice,
        w.web.project_id,
        &value("ONE_MORE", 1, false),
    )
    .await
    .expect_err("over the limit");
    assert_eq!(e.code(), ErrorCode::RequestInvalid);
    assert!(e.to_string().contains("too_many_variables"), "{e}");
    // Replacing an existing one is still allowed.
    w.put(&value("V0", 3, false)).await;
}

#[tokio::test]
async fn a_jobs_masks_are_the_snapshot_of_its_claim_and_survive_a_change() {
    let w = world().await;
    w.put(&value("MASKED_ONE", 1, true)).await;
    w.put(&value("PLAIN_ONE", 2, false)).await;
    let (_, job) = w
        .pipeline(true, Source::Push, &["MASKED_ONE", "PLAIN_ONE"])
        .await;
    // Before a release: nothing to mask, not released.
    let (project, released, sealed) = masked_for_job(&w.t.db, job)
        .await
        .expect("read")
        .expect("job");
    assert_eq!(project, w.web.project_id);
    assert!(!released && sealed.is_empty());
    let runner = register(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-reg-r1"),
        &NewRunner {
            name: "r1".to_owned(),
            description: String::new(),
            tags: Vec::new(),
        },
    )
    .await
    .expect("runner")
    .runner
    .id;
    let claimed = lease::claim(&w.t.db, runner)
        .await
        .expect("claim")
        .expect("job");
    let released_now = sealed_for(&w.t.db, w.web.project_id, &claimed.requested_secrets)
        .await
        .expect("sealed");
    assert!(record_release(
        &w.t.db,
        (w.web.project_id, job, claimed.attempt),
        &released_now
    )
    .await
    .expect("record"));
    // The variable is replaced, then deleted: the claim's value stays masked.
    w.put(&value("MASKED_ONE", 9, true)).await;
    delete(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-ci-var-del"),
        w.web.project_id,
        "MASKED_ONE",
    )
    .await
    .expect("delete");
    let (_, released, sealed) = masked_for_job(&w.t.db, job)
        .await
        .expect("read")
        .expect("job");
    assert!(released);
    let names: Vec<&str> = sealed.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(names, ["MASKED_ONE"], "masked only, from the snapshot");
    assert_eq!(sealed[0].sealed.ciphertext, vec![1u8; 40]);
    // A job that is not running records nothing.
    assert!(
        !record_release(&w.t.db, (w.web.project_id, 999_999, 1), &[])
            .await
            .expect("record")
    );
    assert!(masked_for_job(&w.t.db, 999_999)
        .await
        .expect("read")
        .is_none());
}

#[tokio::test]
async fn a_claimed_job_failed_for_its_secrets_keeps_the_code_and_loses_its_token() {
    let w = world().await;
    let (pipeline, job) = w.pipeline(false, Source::Push, &["TOKEN"]).await;
    let runner = register(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-reg-r1"),
        &NewRunner {
            name: "r1".to_owned(),
            description: String::new(),
            tags: Vec::new(),
        },
    )
    .await
    .expect("runner")
    .runner
    .id;
    let claimed = lease::claim(&w.t.db, runner)
        .await
        .expect("claim")
        .expect("a job");
    assert_eq!(claimed.job_id, job);
    assert!(!claimed.secrets_allowed);
    assert!(claimed.secret_names.is_empty());
    assert_eq!(claimed.requested_secrets, ["TOKEN"]);
    let done = lease::fail_claimed(
        &w.t.db,
        runner,
        job,
        claimed.attempt,
        ErrorCode::SecretOnUnprotectedRef,
    )
    .await
    .expect("failed");
    assert_eq!(done.status, "failed");
    assert_eq!(done.pipeline_id, pipeline);
    let mut su = w.t.superuser().await;
    let (status, code): (String, Option<String>) =
        sqlx::query_as("SELECT status, error_code FROM forge.ci_jobs WHERE id = $1")
            .bind(job)
            .fetch_one(&mut su)
            .await
            .expect("job");
    assert_eq!(status, "failed");
    assert_eq!(code.as_deref(), Some("FG-CI-0017"));
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM forge.ci_job_tokens WHERE job_id = $1 AND revoked_at IS NULL",
    )
    .bind(job)
    .fetch_one(&mut su)
    .await
    .expect("tokens");
    assert_eq!(live, 0);
}
