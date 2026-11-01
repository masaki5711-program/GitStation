//! `forge admin project purge --dry-run | --now` (step D5 MR4, ADR-0015
//! Amendment D5; runbook repository-purge).
//!
//! `--dry-run` lists the projects whose grace period is over (at most
//! `worker::purge::BATCH`, oldest `purge_after` first) and changes nothing.
//! `--now` runs one batch of the worker's purge (`worker::purge::purge_batch`:
//! the same phases, locks, logs and `FG-REPO-0020` per failed project) at
//! once instead of waiting for the next run of `forge worker`. A mutating
//! operator command (in production an approved runbook step): the operator
//! is the trust root (actor `system`, `actor_label` naming the CLI and the
//! OS account), outside `authorize()` as the other operator commands
//! (ADR-0019 §8); the grace period is never shortened.
//!
//! Each project's failure is logged once by the batch; the command prints
//! one line per project and a summary (`failed=<n>`), so a failure is never
//! silent, and the exit code is 0 only when nothing failed.

use forge_core::config::Config;
use forge_core::RequestId;
use forge_db::projects::purge as db_purge;
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::StorageRoot;

use crate::admin_token::{context, with_db};
use crate::worker::purge::{purge_batch, Outcome, Purger, BATCH};

pub fn purge(cfg: &Config, request_id: &RequestId, now: bool) -> Result<String, ForgeError> {
    if !now {
        return with_db(&cfg.database, async |db| dry_run(db).await);
    }
    let root = StorageRoot::open(cfg.storage_root()?)?;
    with_db(&cfg.database, async |db| {
        purge_now(db, root, request_id).await
    })
}

/// The due projects, one tab-separated line each, then `due=<n>`.
pub(crate) async fn dry_run(db: &Db) -> Result<String, ForgeError> {
    let due = db_purge::due(db, BATCH).await?;
    let mut out = String::from("project_id\trepo_id\tstate\tpurge_after\toverdue_s\n");
    for d in &due {
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\n",
            d.project_id.get(),
            d.repository_id.get(),
            d.state.as_str(),
            d.purge_after,
            d.overdue_secs
        ));
    }
    out.push_str(&format!("due={}\n", due.len()));
    Ok(out)
}

/// One batch now; the per-project lines and the summary. Failed projects
/// were logged by the batch; the command then fails with `FG-REPO-0020`
/// `reason=batch_failed` naming how many (the summary error the CLI
/// boundary logs, not a second copy of each failure).
pub(crate) async fn purge_now(
    db: &Db,
    root: StorageRoot,
    request_id: &RequestId,
) -> Result<String, ForgeError> {
    let mut purger = Purger::new(db, root);
    if let Some(label) = context(request_id).actor_label {
        purger.actor_label = label;
    }
    let done = purge_batch(&purger, BATCH).await?;
    let mut out = String::from("project_id\trepo_id\toutcome\tdetail\trequest_id\n");
    let (mut purged, mut failed) = (0_usize, 0_usize);
    for p in &done {
        let detail = match &p.outcome {
            Outcome::Purged { storage_removed } => {
                purged += 1;
                if *storage_removed {
                    "storage_removed"
                } else {
                    "storage_absent"
                }
            }
            Outcome::Skipped(reason) => reason,
            Outcome::Deferred => "provisioning_running",
            Outcome::Failed(code) => {
                failed += 1;
                code.id()
            }
        };
        out.push_str(&format!(
            "{}\t{}\t{}\t{detail}\t{}\n",
            p.project_id.get(),
            p.repo_id.get(),
            p.outcome.as_str(),
            p.request_id
        ));
    }
    out.push_str(&format!(
        "purged={purged} failed={failed} other={}\n",
        done.len() - purged - failed
    ));
    tracing::info!(
        target: "forge_cli",
        event = "forge.admin.project.purge",
        request_id = %request_id,
        projects = done.len(),
        purged,
        failed,
        "purge batch"
    );
    if failed > 0 {
        // The lines are still shown: stdout first, then the failure.
        crate::emit(&out)?;
        return Err(ForgeError::new(ErrorCode::RepositoryPurgeStuck)
            .with_context("reason", "batch_failed")
            .with_context("failed", failed)
            .with_context("purged", purged));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use forge_db::test_support::FreshDb;

    use super::*;
    use crate::worker::provision::tests::storage;
    use crate::worker::purge::tests::{delete, expire, project, provision};

    /// `--dry-run` lists the due project and changes nothing; `--now`
    /// purges it with the CLI as the audit label and prints its line.
    #[tokio::test]
    async fn dry_run_lists_and_now_purges_the_due_projects() {
        let t = FreshDb::migrated().await;
        let (_dir, storage) = storage();
        let (alice, web) = project(&t, "alice").await;
        provision(&t, &storage).await;
        delete(&t, web.project_id, alice).await;
        expire(&t, web.project_id).await;
        let listed = dry_run(&t.db).await.expect("dry run");
        let line = format!(
            "{}\t{}\tready\t",
            web.project_id.get(),
            web.repository_id.get()
        );
        assert!(listed.contains(&line), "{listed}");
        assert!(listed.ends_with("due=1\n"), "{listed}");
        assert!(
            storage.root.existing(web.repository_id).is_ok(),
            "unchanged"
        );
        let rid = RequestId::parse("req-cli-purge").expect("request id");
        let out = purge_now(&t.db, storage.root.clone(), &rid) // clone: the purger's own handle
            .await
            .expect("purged");
        assert!(out.contains("\tpurged\tstorage_removed\t"), "{out}");
        assert!(out.ends_with("purged=1 failed=0 other=0\n"), "{out}");
        assert!(storage.root.existing(web.repository_id).is_err());
        let labels = t
            .json_rows("SELECT actor_label FROM forge.audit_log WHERE action = 'project.purged'")
            .await;
        let label = labels[0]["actor_label"].as_str().expect("label");
        assert!(label.starts_with("forge-cli:"), "{label}");
        let listed = dry_run(&t.db).await.expect("dry run");
        assert!(listed.ends_with("due=0\n"), "{listed}");
        t.drop_now().await;
    }

    /// A failed project makes `--now` fail with `FG-REPO-0020
    /// reason=batch_failed` (after its line was printed).
    #[tokio::test]
    async fn now_fails_when_a_project_could_not_be_purged() {
        let t = FreshDb::migrated().await;
        let (_dir, storage) = storage();
        let (alice, web) = project(&t, "alice").await;
        delete(&t, web.project_id, alice).await;
        expire(&t, web.project_id).await;
        let path = storage.root.path_for(web.repository_id);
        std::fs::create_dir_all(path.parent().expect("shard")).expect("shard");
        std::fs::write(&path, b"x").expect("a file at the path");
        let rid = RequestId::parse("req-cli-fail").expect("request id");
        let err = purge_now(&t.db, storage.root.clone(), &rid) // clone: the purger's own handle
            .await
            .expect_err("failed");
        assert_eq!(err.code(), ErrorCode::RepositoryPurgeStuck);
        let text = err.to_string();
        assert!(
            text.contains("reason=batch_failed") && text.contains("failed=1"),
            "{text}"
        );
        t.drop_now().await;
    }
}
