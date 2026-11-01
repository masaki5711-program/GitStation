//! D5 MR4: the purge's database half — `due`, `start` (phase 1) and
//! `finish` (the rows, the tombstone, the audit row and the one event), and
//! the race with restore.

use forge_core::{EventType, RepositoryState};

use super::delete::{expire, GRACE};
use super::*;
use crate::projects::purge::{self, Finished, Started};

/// Delete `project` as `by` (grace period [`GRACE`]).
async fn delete(t: &FreshDb, project: ProjectId, by: UserId) {
    delete_decided(&t.db, &ctx("req-delete"), project, by, GRACE, |_| Ok(()))
        .await
        .expect("delete");
}

/// A project of alice's, deleted and past its grace period.
async fn expired(t: &FreshDb, alice: &users::Created, segment: &str) -> CreatedProject {
    let made = create_ok(t, &project(segment, alice.namespace_id, alice.user_id)).await;
    delete(t, made.project_id, alice.user_id).await;
    expire(t, made.project_id).await;
    made
}

async fn repo_state(su: &mut PgConnection, project: ProjectId) -> Option<String> {
    sqlx::query_scalar("SELECT state FROM forge.repositories WHERE project_id = $1")
        .bind(project.get())
        .fetch_optional(su)
        .await
        .expect("state")
}

/// Rows of `project` left in the tables the purge deletes from.
async fn left(su: &mut PgConnection, project: ProjectId) -> [i64; 5] {
    let id = project.get();
    [
        count(
            su,
            &format!("SELECT count(*) FROM forge.projects WHERE id = {id}"),
        )
        .await,
        count(
            su,
            &format!(
                "SELECT count(*) FROM forge.namespaces WHERE kind = 'project' AND owner_id = {id}"
            ),
        )
        .await,
        count(
            su,
            &format!("SELECT count(*) FROM forge.repositories WHERE project_id = {id}"),
        )
        .await,
        count(
            su,
            &format!("SELECT count(*) FROM forge.project_members WHERE project_id = {id}"),
        )
        .await,
        count(
            su,
            &format!("SELECT count(*) FROM forge.protected_refs WHERE project_id = {id}"),
        )
        .await,
    ]
}

#[tokio::test]
async fn due_lists_only_projects_past_their_grace_period() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let live = create_ok(&t, &project("live", alice.namespace_id, alice.user_id)).await;
    let waiting = create_ok(&t, &project("waiting", alice.namespace_id, alice.user_id)).await;
    delete(&t, waiting.project_id, alice.user_id).await;
    let old = expired(&t, &alice, "old").await;
    let due = purge::due(&t.db, 10).await.expect("due");
    let ids: Vec<ProjectId> = due.iter().map(|d| d.project_id).collect();
    assert_eq!(ids, [old.project_id]);
    assert_eq!(due[0].repository_id, old.repository_id);
    assert_eq!(due[0].state, RepositoryState::Provisioning);
    assert!(due[0].overdue_secs >= 0);
    // Not expired, not deleted: skipped, nothing written.
    for (made, reason) in [(&waiting, "not_expired"), (&live, "not_deleted")] {
        let started = purge::start(&t.db, &ctx("req-start"), made.project_id)
            .await
            .expect("start");
        assert_eq!(started, Started::Skipped { reason });
    }
    let mut su = t.superuser().await;
    assert_eq!(
        count(
            &mut su,
            "SELECT count(*) FROM forge.repositories WHERE state = 'purging'"
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &mut su,
            "SELECT count(*) FROM forge.audit_log WHERE action = 'repository.purge_started'"
        )
        .await,
        0
    );
    t.drop_now().await;
}

/// Purge after expiry: phase 1 moves the repository to `purging` (one
/// audit-only row), phase 3 deletes every row, keeps the path with a
/// `deleted` tombstone owned by the deleter, and writes one `project.purged`
/// audit row and one admin event; running either phase again changes
/// nothing (the event exists once).
#[tokio::test]
async fn a_purge_deletes_the_rows_keeps_the_path_and_publishes_once() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let bob = user(&t, "bob").await;
    let web = expired(&t, &alice, "web").await;
    let id = web.project_id.get();
    t.commit_audited(
        ("project", id),
        &[
            format!(
                "INSERT INTO forge.project_members (project_id, user_id, role) \
                 VALUES ({id}, {}, 'developer')",
                bob.user_id.get()
            ),
            format!(
                "INSERT INTO forge.protected_refs (project_id, pattern, push_min_role) \
                 VALUES ({id}, 'refs/heads/main', 'maintainer')"
            ),
        ],
    )
    .await;
    let started = purge::start(&t.db, &ctx("req-start"), web.project_id)
        .await
        .expect("start");
    assert_eq!(
        started,
        Started::Started {
            repository_id: web.repository_id,
            from: RepositoryState::Provisioning
        }
    );
    let mut su = t.superuser().await;
    assert_eq!(
        repo_state(&mut su, web.project_id).await.as_deref(),
        Some("purging")
    );
    // A second start resumes: nothing new is written.
    let again = purge::start(&t.db, &ctx("req-start-2"), web.project_id)
        .await
        .expect("start again");
    assert_eq!(
        again,
        Started::Resumed {
            repository_id: web.repository_id
        }
    );
    let started_rows: Vec<(Option<i64>, serde_json::Value)> = sqlx::query_as(
        "SELECT project_id, details FROM forge.audit_log WHERE action = 'repository.purge_started'",
    )
    .fetch_all(&mut su)
    .await
    .expect("rows");
    assert_eq!(
        started_rows,
        [(
            Some(id),
            json!({"project_id": id, "repository_id": web.repository_id.get(),
                   "from": "provisioning"})
        )]
    );
    // The project can no longer be restored.
    let err = restore_decided(
        &t.db,
        &ctx("req-restore"),
        web.project_id,
        alice.user_id,
        |_| Ok(()),
    )
    .await
    .expect_err("purging");
    assert_reason(&err, ErrorCode::RestoreUnavailable, "purging");

    let finished = purge::finish(&t.db, &ctx("req-finish"), web.project_id, true)
        .await
        .expect("finish");
    let Finished::Purged(purged) = finished else {
        panic!("purged: {finished:?}");
    };
    assert_eq!(purged.namespace_id, web.namespace_id);
    assert_eq!(purged.redirects_ended, 0);
    assert_eq!(left(&mut su, web.project_id).await, [0; 5]);
    let tomb: (
        i64,
        String,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        String,
        i64,
    ) = sqlx::query_as(
        "SELECT id, kind, namespace_id, parent_namespace_id, owner_at_write, cause_type, \
         cause_id FROM forge.namespace_tombstones WHERE old_path = 'alice/web'",
    )
    .fetch_one(&mut su)
    .await
    .expect("tombstone");
    assert_eq!(
        tomb,
        (
            purged.tombstone_id.get(),
            "deleted".to_owned(),
            None,
            Some(alice.namespace_id.get()),
            Some(alice.user_id.get()),
            "project".to_owned(),
            id
        )
    );
    let audit: (String, Option<i64>, serde_json::Value) = sqlx::query_as(
        "SELECT resource_type, project_id, details FROM forge.audit_log \
         WHERE action = 'project.purged'",
    )
    .fetch_one(&mut su)
    .await
    .expect("audit row");
    assert_eq!(audit.0, "project");
    assert_eq!(audit.1, Some(id));
    assert_eq!(audit.2["members_removed"], json!(2));
    assert_eq!(audit.2["protected_refs_removed"], json!(1));
    assert_eq!(audit.2["storage_removed"], json!(true));
    assert_eq!(audit.2["deleted_by"], json!(alice.user_id.get()));
    assert_eq!(audit.2["tombstone_id"], json!(purged.tombstone_id.get()));
    let events: Vec<(String, Option<i64>, serde_json::Value, String)> = sqlx::query_as(
        "SELECT event_type, project_id, payload, dedup_key FROM forge.events \
         WHERE event_type = 'project.purged'",
    )
    .fetch_all(&mut su)
    .await
    .expect("events");
    assert_eq!(
        events,
        [(
            EventType::ProjectPurged.as_str().to_owned(),
            Some(id),
            audit.2.clone(), // clone: compared below as well
            format!("project_purged:{id}")
        )]
    );
    // Both phases again: the project is gone, nothing is written twice.
    let started = purge::start(&t.db, &ctx("req-start-3"), web.project_id)
        .await
        .expect("start after the purge");
    assert_eq!(started, Started::Skipped { reason: "gone" });
    let finished = purge::finish(&t.db, &ctx("req-finish-2"), web.project_id, false)
        .await
        .expect("finish after the purge");
    assert_eq!(finished, Finished::Gone);
    assert_eq!(
        count(
            &mut su,
            "SELECT count(*) FROM forge.events WHERE event_type = 'project.purged'"
        )
        .await,
        1
    );
    assert!(purge::due(&t.db, 10).await.expect("due").is_empty());
    // The path stays blocked until its tombstone is released (ADR-0015).
    let err = projects_create_err(&t, &project("web", alice.namespace_id, alice.user_id)).await;
    assert_reason(&err, ErrorCode::NamespacePathTaken, "tombstoned");
    t.drop_now().await;
}

async fn projects_create_err(t: &FreshDb, new: &NewProject) -> ForgeError {
    create(&t.db, &ctx("req-again"), new)
        .await
        .expect_err("refused")
}

/// A `moved` tombstone naming the purged project's namespace loses its
/// target (`ON DELETE SET NULL`): the redirect ends, the old path stays
/// blocked, and the count is recorded.
#[tokio::test]
async fn a_purge_ends_the_redirects_of_the_projects_old_paths() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let web = expired(&t, &alice, "web").await;
    let id = web.project_id.get();
    t.commit_audited(
        ("project", id),
        &[format!(
            "INSERT INTO forge.namespace_tombstones (old_path, parent_namespace_id, \
             namespace_id, kind, owner_at_write, cause_type, cause_id) VALUES \
             ('alice/old-web', {}, {}, 'moved', {}, 'project', {id})",
            alice.namespace_id.get(),
            web.namespace_id.get(),
            alice.user_id.get()
        )],
    )
    .await;
    let started = purge::start(&t.db, &ctx("req-start"), web.project_id)
        .await
        .expect("start");
    assert!(matches!(started, Started::Started { .. }), "{started:?}");
    let Finished::Purged(purged) = purge::finish(&t.db, &ctx("req-finish"), web.project_id, false)
        .await
        .expect("finish")
    else {
        panic!("purged");
    };
    assert_eq!(purged.redirects_ended, 1);
    let mut su = t.superuser().await;
    let moved: (String, Option<i64>) = sqlx::query_as(
        "SELECT kind, namespace_id FROM forge.namespace_tombstones WHERE old_path = 'alice/old-web'",
    )
    .fetch_one(&mut su)
    .await
    .expect("moved tombstone kept");
    assert_eq!(moved, ("moved".to_owned(), None));
    let err = projects_create_err(&t, &project("old-web", alice.namespace_id, alice.user_id)).await;
    assert_reason(&err, ErrorCode::NamespacePathTaken, "tombstoned");
    t.drop_now().await;
}

/// Phase 3 without phase 1 is a bug of the caller: refused, nothing deleted.
#[tokio::test]
async fn finishing_a_repository_that_is_not_purging_is_refused() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let web = expired(&t, &alice, "web").await;
    let err = purge::finish(&t.db, &ctx("req-finish"), web.project_id, false)
        .await
        .expect_err("not purging");
    assert_reason(&err, ErrorCode::Internal, "not_purging");
    let mut su = t.superuser().await;
    assert_eq!(left(&mut su, web.project_id).await, [1, 1, 1, 1, 0]);
    t.drop_now().await;
}

/// A restore in flight holds the project row: the purge start waits on it
/// (same lock order), re-reads the committed restore and skips — no guard
/// error, the repository untouched.
#[tokio::test]
async fn a_purge_start_waits_for_a_restore_in_flight_and_skips() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let web = expired(&t, &alice, "web").await;
    let restore = t
        .hold_audited(
            ("project", web.project_id.get()),
            &[format!(
                "UPDATE forge.projects SET deleted_at = NULL, purge_after = NULL, \
                 deleted_by = NULL WHERE id = {}",
                web.project_id.get()
            )],
        )
        .await;
    let start_ctx = ctx("req-start");
    let starting = purge::start(&t.db, &start_ctx, web.project_id);
    let committing = async {
        t.until_waiting("coalesce(purge_after <= clock_timestamp(), false)")
            .await;
        restore.commit().await;
    };
    let (started, ()) = tokio::join!(starting, committing);
    assert_eq!(
        started.expect("start"),
        Started::Skipped {
            reason: "not_deleted"
        }
    );
    let mut su = t.superuser().await;
    assert_eq!(
        repo_state(&mut su, web.project_id).await.as_deref(),
        Some("provisioning")
    );
    t.drop_now().await;
}

/// Restore and purge start at the same time, many times: exactly one wins
/// each time, and the loser changes nothing.
#[tokio::test]
async fn restore_and_purge_start_racing_have_exactly_one_winner() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    for n in 0..8 {
        let made = expired(&t, &alice, &format!("race-{n}")).await;
        let (restore_ctx, start_ctx) = (ctx("req-restore"), ctx("req-start"));
        let restoring =
            restore_decided(&t.db, &restore_ctx, made.project_id, alice.user_id, |_| {
                Ok(())
            });
        let starting = purge::start(&t.db, &start_ctx, made.project_id);
        let (restored, started) = tokio::join!(restoring, starting);
        let started = started.expect("start never fails");
        let mut su = t.superuser().await;
        let state = repo_state(&mut su, made.project_id).await;
        let pending: bool =
            sqlx::query_scalar("SELECT deleted_at IS NOT NULL FROM forge.projects WHERE id = $1")
                .bind(made.project_id.get())
                .fetch_one(&mut su)
                .await
                .expect("project");
        match restored {
            Ok(_) => {
                assert_eq!(
                    started,
                    Started::Skipped {
                        reason: "not_deleted"
                    },
                    "{n}"
                );
                assert!(!pending, "{n}");
                assert_eq!(state.as_deref(), Some("provisioning"), "{n}");
            }
            Err(e) => {
                assert_reason(&e, ErrorCode::RestoreUnavailable, "purging");
                assert!(
                    matches!(started, Started::Started { .. }),
                    "{n}: {started:?}"
                );
                assert!(pending, "{n}");
                assert_eq!(state.as_deref(), Some("purging"), "{n}");
            }
        }
    }
    t.drop_now().await;
}
