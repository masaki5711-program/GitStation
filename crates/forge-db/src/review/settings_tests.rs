//! Merge settings, the CODEOWNERS switch and protected-ref merge rules
//! against a migrated database (M3 R6, ADR-0024 §4.1, §8, §9, §12).

use forge_core::{
    CheckName, MergeMethod, MergeRule, MergeSettingsValues, RefName, RefPattern, RequestId, Role,
};
use forge_errors::ErrorCode;

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::projects::{self, CreatedProject, NewProject};
use crate::protected_refs::{self, MergeRuleOutcome};
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

struct World {
    t: FreshDb,
    alice: UserId,
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
        web,
    }
}

impl World {
    async fn count(&self, sql: &str) -> i64 {
        sqlx::query_scalar(sql)
            .bind(self.web.project_id.get())
            .fetch_one(&mut self.t.superuser().await)
            .await
            .expect("count")
    }

    async fn audit_actions(&self, prefix: &str) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT action FROM forge.audit_log WHERE project_id = $1 AND action LIKE $2 \
             ORDER BY id",
        )
        .bind(self.web.project_id.get())
        .bind(format!("{prefix}%"))
        .fetch_all(&mut self.t.superuser().await)
        .await
        .expect("audit")
    }

    async fn suspend(&self, minutes: u32, request: &str) -> StoredMergeSettings {
        toggle_codeowners_mr_decided(
            &self.t.db,
            &as_user(self.alice, request),
            self.web.project_id,
            self.alice,
            CodeownersChange::Suspend { minutes },
            |_| Ok(()),
        )
        .await
        .map(|(_, s, ())| s)
        .expect("suspend")
    }

    async fn rule(&self, pattern: &str) -> forge_core::ProtectedRefId {
        protected_refs::create(
            &self.t.db,
            &as_user(self.alice, &format!("req-rule-{}", pattern.len())),
            self.web.project_id,
            &RefPattern::parse(pattern).expect("pattern"),
            Role::Maintainer,
        )
        .await
        .expect("rule")
    }

    async fn put_rule(
        &self,
        rule: forge_core::ProtectedRefId,
        merge: &MergeRule,
        request: &str,
    ) -> Result<MergeRuleOutcome, forge_errors::ForgeError> {
        protected_refs::put_merge_rule_decided(
            &self.t.db,
            &as_user(self.alice, request),
            self.web.project_id,
            rule,
            self.alice,
            merge,
            |_| Ok(()),
        )
        .await
        .map(|(o, ())| o)
    }
}

/// No row: the defaults (ADR-0024 §4.1, §8.2, §8.3); a put writes one
/// audit row (`merge_settings.updated`, no event); the same values again
/// write nothing.
#[tokio::test]
async fn merge_settings_default_then_put_once() {
    let w = world().await;
    let read = merge_settings(&w.t.db, w.web.project_id)
        .await
        .expect("read");
    assert!(!read.stored);
    assert_eq!(read.values, MergeSettingsValues::default());
    assert!(read.codeowners.enforced());
    let values = MergeSettingsValues {
        allowed_methods: vec![
            MergeMethod::FastForward,
            MergeMethod::Squash,
            MergeMethod::FastForward,
        ],
        default_method: MergeMethod::Squash,
        delete_source_default: true,
        reset_approvals_on_push: false,
        committer_approvals_excluded: false,
    };
    let (outcome, stored, ()) = put_merge_settings_mr_decided(
        &w.t.db,
        &as_user(w.alice, "req-put"),
        w.web.project_id,
        w.alice,
        &values,
        |facts| {
            assert_eq!(facts.highest_protection, None);
            Ok(())
        },
    )
    .await
    .expect("put");
    assert_eq!(outcome, SettingsOutcome::Changed);
    assert_eq!(
        stored.values.allowed_methods,
        [MergeMethod::FastForward, MergeMethod::Squash],
        "duplicates dropped"
    );
    assert!(stored.stored && stored.values.delete_source_default);
    let (again, _, ()) = put_merge_settings_mr_decided(
        &w.t.db,
        &as_user(w.alice, "req-put-again"),
        w.web.project_id,
        w.alice,
        &values,
        |_| Ok(()),
    )
    .await
    .expect("again");
    assert_eq!(again, SettingsOutcome::Unchanged);
    assert_eq!(
        w.audit_actions("merge_settings.").await,
        ["merge_settings.updated"]
    );
    assert_eq!(
        w.count(
            "SELECT count(*) FROM forge.events WHERE project_id = $1 \
             AND event_type LIKE 'merge_settings.%'"
        )
        .await,
        0
    );
    // The CHECK refuses a default the methods do not allow (backstop).
    let bad = MergeSettingsValues {
        allowed_methods: vec![MergeMethod::Squash],
        default_method: MergeMethod::MergeCommit,
        ..values
    };
    let err = put_merge_settings_mr_decided(
        &w.t.db,
        &as_user(w.alice, "req-put-bad"),
        w.web.project_id,
        w.alice,
        &bad,
        |_| Ok(()),
    )
    .await
    .expect_err("check");
    assert_eq!(err.code(), ErrorCode::DbQueryFailed);
    w.t.drop_now().await;
}

/// ADR-0024 §9: a suspension has an expiry (the database's clock), is an
/// event, and enforcing again is one more; enforcing an enforced project
/// writes nothing. An expired suspension reads as enforced at once, and
/// the worker's restore records it once (event deduplicated by the
/// expiry).
#[tokio::test]
async fn a_codeowners_suspension_expires_by_itself_and_is_recorded_once() {
    let w = world().await;
    let s = w.suspend(60, "req-suspend").await;
    let CodeownersEnforcement::Suspended { since, until } = &s.codeowners else {
        panic!("suspended: {s:?}");
    };
    assert!(until > since);
    assert!(!s.codeowners.enforced());
    // A Maintainer enforces it again.
    let (outcome, back, ()) = toggle_codeowners_mr_decided(
        &w.t.db,
        &as_user(w.alice, "req-enforce"),
        w.web.project_id,
        w.alice,
        CodeownersChange::Enforce,
        |_| Ok(()),
    )
    .await
    .expect("enforce");
    assert_eq!(outcome, SettingsOutcome::Changed);
    assert_eq!(back.codeowners, CodeownersEnforcement::Enforced);
    let (unchanged, _, ()) = toggle_codeowners_mr_decided(
        &w.t.db,
        &as_user(w.alice, "req-enforce-again"),
        w.web.project_id,
        w.alice,
        CodeownersChange::Enforce,
        |_| Ok(()),
    )
    .await
    .expect("enforce again");
    assert_eq!(unchanged, SettingsOutcome::Unchanged);
    // Suspended again, then the expiry passes (moved back in time).
    w.suspend(30, "req-suspend-2").await;
    w.t.commit_audited(
        ("project", w.web.project_id.get()),
        &[format!(
            "UPDATE forge.merge_settings SET codeowners_suspended_at = now() - interval '2 hours', \
             codeowners_enforced_off_until = now() - interval '1 hour' WHERE project_id = {}",
            w.web.project_id.get()
        )],
    )
    .await;
    let read = merge_settings(&w.t.db, w.web.project_id)
        .await
        .expect("read");
    assert!(
        matches!(read.codeowners, CodeownersEnforcement::Expired { .. }),
        "{read:?}"
    );
    assert!(read.codeowners.enforced(), "an expired suspension enforces");
    let report = restore_expired_codeowners(&w.t.db, &system("req-restore"))
        .await
        .expect("restore");
    assert_eq!(report.restored, 1);
    let again = restore_expired_codeowners(&w.t.db, &system("req-restore-2"))
        .await
        .expect("restore again");
    assert_eq!(again.restored, 0);
    assert_eq!(
        merge_settings(&w.t.db, w.web.project_id)
            .await
            .expect("read")
            .codeowners,
        CodeownersEnforcement::Enforced
    );
    assert_eq!(
        w.audit_actions("merge_settings.codeowners").await,
        [
            "merge_settings.codeowners_enforcement_changed",
            "merge_settings.codeowners_enforcement_changed",
            "merge_settings.codeowners_enforcement_changed",
            "merge_settings.codeowners_enforcement_restored",
        ]
    );
    assert_eq!(
        w.count(
            "SELECT count(*) FROM forge.events WHERE project_id = $1 \
             AND event_type = 'merge_settings.codeowners_enforcement_restored' \
             AND dedup_key LIKE 'codeowners_enforcement_restored:%'"
        )
        .await,
        1
    );
    w.t.drop_now().await;
}

/// The table refuses a suspension without an expiry or longer than 24
/// hours (the backstop of `authorize()`'s range).
#[tokio::test]
async fn the_table_refuses_a_suspension_beyond_a_day() {
    let w = world().await;
    let err = toggle_codeowners_mr_decided(
        &w.t.db,
        &as_user(w.alice, "req-suspend-long"),
        w.web.project_id,
        w.alice,
        CodeownersChange::Suspend {
            minutes: 24 * 60 + 1,
        },
        |_| Ok(()),
    )
    .await
    .expect_err("too long");
    assert_eq!(err.code(), ErrorCode::DbQueryFailed);
    assert!(
        !merge_settings(&w.t.db, w.web.project_id)
            .await
            .expect("read")
            .stored
    );
    w.t.drop_now().await;
}

/// ADR-0024 §12: a merge rule on a protection rule joins the push and
/// merge snapshot (strictest of the matching rules), is audited, is
/// listed, goes with its rule's deletion, and refuses a tag pattern and
/// an `ext/` check (FG-REVIEW-0016).
#[tokio::test]
async fn a_merge_rule_joins_the_snapshot_and_goes_with_its_rule() {
    let w = world().await;
    let main = w.rule("refs/heads/main").await;
    let tags = w.rule("refs/tags/v*").await;
    let gates = CheckName::parse("ci/gates").expect("name");
    let merge = MergeRule {
        required_approvals: 2,
        required_checks: vec![gates.clone()],
        require_resolved_discussions: true,
        direct_push_forbidden: true,
    };
    assert_eq!(
        w.put_rule(main, &merge, "req-mr-put").await.expect("put"),
        MergeRuleOutcome::Created
    );
    assert_eq!(
        w.put_rule(main, &merge, "req-mr-put-2").await.expect("put"),
        MergeRuleOutcome::Unchanged
    );
    let snapshot = protected_refs::rules_for(&w.t.db, w.web.project_id)
        .await
        .expect("snapshot");
    let target = RefName::parse("refs/heads/main").expect("ref");
    assert_eq!(snapshot.merge_rule(&target), merge);
    assert!(snapshot
        .merge_rule(&RefName::parse("refs/heads/dev").expect("ref"))
        .is_empty());
    let listed = protected_refs::list(&w.t.db, w.web.project_id)
        .await
        .expect("list");
    assert_eq!(listed[0].merge.as_ref(), Some(&merge));
    assert_eq!(listed[1].merge, None);
    // A tag pattern has no merge requests.
    let err = w
        .put_rule(tags, &merge, "req-mr-tag")
        .await
        .expect_err("tag");
    assert_eq!(err.code(), ErrorCode::ProtectedRefRuleInvalid);
    // `ext/` is never required.
    let ext = MergeRule {
        required_checks: vec![CheckName::parse("ext/sonar").expect("name")],
        ..MergeRule::default()
    };
    let err = w.put_rule(main, &ext, "req-mr-ext").await.expect_err("ext");
    assert_eq!(err.code(), ErrorCode::RequiredCheckNamespaceNotAllowed);
    // Deleting the rule deletes its merge rule in the same change.
    protected_refs::delete(
        &w.t.db,
        &as_user(w.alice, "req-rule-del"),
        w.web.project_id,
        main,
    )
    .await
    .expect("delete");
    assert_eq!(
        w.count("SELECT count(*) FROM forge.protected_ref_merge_rules WHERE project_id = $1")
            .await,
        0
    );
    assert_eq!(
        w.audit_actions("protected_ref.merge_rule").await,
        ["protected_ref.merge_rule_put"]
    );
    w.t.drop_now().await;
}

/// Removing a merge rule: audited; none to remove is FG-REPO-0005
/// `resource=merge_rule`.
#[tokio::test]
async fn a_merge_rule_is_removed_once() {
    let w = world().await;
    let main = w.rule("refs/heads/main").await;
    let merge = MergeRule {
        required_approvals: 1,
        ..MergeRule::default()
    };
    assert_eq!(
        w.put_rule(main, &merge, "req-mr-put").await.expect("put"),
        MergeRuleOutcome::Created
    );
    let (first, second) = (
        as_user(w.alice, "req-mr-rm"),
        as_user(w.alice, "req-mr-rm-2"),
    );
    let remove = |ctx| {
        protected_refs::remove_merge_rule_decided(
            &w.t.db,
            ctx,
            w.web.project_id,
            main,
            w.alice,
            |_| Ok(()),
        )
    };
    assert_eq!(
        remove(&first).await.expect("remove").0,
        MergeRuleOutcome::Removed
    );
    let err = remove(&second).await.expect_err("none");
    assert_eq!(err.code(), ErrorCode::NotFound);
    assert!(err
        .context()
        .iter()
        .any(|(k, v)| *k == "resource" && v == "merge_rule"));
    w.t.drop_now().await;
}

// ── R6 review ──

impl World {
    /// Move the project's suspension into the past (expired an hour ago).
    async fn expire(&self) {
        self.t
            .commit_audited(
                ("project", self.web.project_id.get()),
                &[format!(
                    "UPDATE forge.merge_settings SET codeowners_suspended_at = now() - interval '2 hours', \
                     codeowners_enforced_off_until = now() - interval '1 hour' WHERE project_id = {}",
                    self.web.project_id.get()
                )],
            )
            .await;
    }
}

/// R6 review (MEDIUM): a suspension is never extended — suspending while
/// suspended is `Unchanged` and keeps the expiry in force.
#[tokio::test]
async fn a_suspension_cannot_be_extended() {
    let w = world().await;
    let first = w.suspend(30, "req-suspend").await;
    let (outcome, again, ()) = toggle_codeowners_mr_decided(
        &w.t.db,
        &as_user(w.alice, "req-suspend-again"),
        w.web.project_id,
        w.alice,
        CodeownersChange::Suspend { minutes: 1440 },
        |_| Ok(()),
    )
    .await
    .expect("again");
    assert_eq!(outcome, SettingsOutcome::Unchanged);
    assert_eq!(again.codeowners, first.codeowners, "the expiry in force");
    assert_eq!(
        w.audit_actions("merge_settings.codeowners").await,
        ["merge_settings.codeowners_enforcement_changed"]
    );
    w.t.drop_now().await;
}

/// R6 review (MEDIUM): a new suspension after an expired one first records
/// the expiry (audit row and deduplicated event), so the restore is never
/// lost; the worker then finds nothing left.
#[tokio::test]
async fn a_new_suspension_after_an_expired_one_records_the_restore_first() {
    let w = world().await;
    w.suspend(30, "req-suspend").await;
    w.expire().await;
    let renewed = w.suspend(60, "req-suspend-2").await;
    assert!(matches!(
        renewed.codeowners,
        CodeownersEnforcement::Suspended { .. }
    ));
    assert_eq!(
        w.audit_actions("merge_settings.codeowners").await,
        [
            "merge_settings.codeowners_enforcement_changed",
            "merge_settings.codeowners_enforcement_changed",
            "merge_settings.codeowners_enforcement_restored",
        ]
    );
    assert_eq!(
        w.count(
            "SELECT count(*) FROM forge.events WHERE project_id = $1 \
             AND event_type = 'merge_settings.codeowners_enforcement_restored'"
        )
        .await,
        1
    );
    let report = restore_expired_codeowners(&w.t.db, &system("req-restore"))
        .await
        .expect("restore");
    assert_eq!(report.found, 0);
    w.t.drop_now().await;
}

/// R6 review (MEDIUM): one row the worker cannot record does not stop the
/// batch; it is reported with its code and moves to the back.
#[tokio::test]
async fn a_failing_restore_does_not_stop_the_batch() {
    let w = world().await;
    let bob = users::create(
        &w.t.db,
        &system("req-user-bob"),
        &NewUser::human("bob", "bob", "bob@example.test"),
    )
    .await
    .expect("bob");
    let api = projects::create(
        &w.t.db,
        &system("req-project-api"),
        &NewProject::new("api", bob.namespace_id, "Api", bob.user_id),
    )
    .await
    .expect("api");
    for (project, user, request) in [
        (w.web.project_id, w.alice, "req-s-web"),
        (api.project_id, bob.user_id, "req-s-api"),
    ] {
        let (outcome, _, ()) = toggle_codeowners_mr_decided(
            &w.t.db,
            &as_user(user, request),
            project,
            user,
            CodeownersChange::Suspend { minutes: 30 },
            |_| Ok(()),
        )
        .await
        .expect("suspend");
        assert_eq!(outcome, SettingsOutcome::Changed);
    }
    for project in [w.web.project_id, api.project_id] {
        w.t.commit_audited(
            ("project", project.get()),
            &[format!(
                "UPDATE forge.merge_settings SET codeowners_suspended_at = now() - interval '2 hours', \
                 codeowners_enforced_off_until = now() - interval '1 hour' WHERE project_id = {}",
                project.get()
            )],
        )
        .await;
    }
    // The web project's restore fails (a synthetic trigger refuses it).
    let mut su = w.t.superuser().await;
    for sql in [
        "CREATE FUNCTION forge.test_refuse_restore() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN RAISE EXCEPTION 'synthetic refusal'; END $$"
            .to_owned(),
        format!(
            "CREATE TRIGGER test_refuse_restore BEFORE UPDATE OF codeowners_enforced \
             ON forge.merge_settings FOR EACH ROW WHEN (NEW.project_id = {}) \
             EXECUTE FUNCTION forge.test_refuse_restore()",
            w.web.project_id.get()
        ),
    ] {
        sqlx::query(&sql).execute(&mut su).await.expect("trigger");
    }
    let report = restore_expired_codeowners(&w.t.db, &system("req-restore"))
        .await
        .expect("restore");
    assert_eq!(report.found, 2);
    assert_eq!(report.restored, 1, "the other project is recorded");
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].0, w.web.project_id);
    let seen: Option<String> = sqlx::query_scalar(
        "SELECT codeowners_restore_seen_at::text FROM forge.merge_settings WHERE project_id = $1",
    )
    .bind(w.web.project_id.get())
    .fetch_one(&mut su)
    .await
    .expect("seen");
    assert!(seen.is_some(), "moved to the back of the next run");
    w.t.drop_now().await;
}

/// R6 review (LOW): the allowed methods are a set; loosening is decided
/// against the stored values.
#[tokio::test]
async fn the_method_order_is_no_change_and_loosening_is_known() {
    let w = world().await;
    let put = |values: MergeSettingsValues, request: &'static str| {
        let ctx = as_user(w.alice, request);
        let db = w.t.db.clone();
        let (project, alice) = (w.web.project_id, w.alice);
        async move {
            put_merge_settings_mr_decided(&db, &ctx, project, alice, &values, |facts| {
                Ok(loosens(&facts.current.values, &values))
            })
            .await
            .map(|(o, _, l)| (o, l))
            .expect("put")
        }
    };
    let swapped = MergeSettingsValues {
        allowed_methods: vec![MergeMethod::Squash, MergeMethod::MergeCommit],
        ..MergeSettingsValues::default()
    };
    assert_eq!(
        put(swapped, "req-swap").await,
        (SettingsOutcome::Unchanged, false)
    );
    let loose = MergeSettingsValues {
        committer_approvals_excluded: false,
        ..MergeSettingsValues::default()
    };
    assert_eq!(
        put(loose, "req-loose").await,
        (SettingsOutcome::Changed, true)
    );
    let tight = MergeSettingsValues::default();
    assert_eq!(
        put(tight, "req-tight").await,
        (SettingsOutcome::Changed, false)
    );
    w.t.drop_now().await;
}

/// R6 review (LOW): the table's check names follow `CheckName::parse`.
#[tokio::test]
async fn the_table_refuses_check_names_check_name_refuses() {
    let w = world().await;
    let main = w.rule("refs/heads/main").await;
    let insert = |checks: &str| {
        format!(
            "INSERT INTO forge.protected_ref_merge_rules (protected_ref_id, project_id, \
             required_checks) VALUES ({}, {}, ARRAY[{checks}]::text[])",
            main.get(),
            w.web.project_id.get()
        )
    };
    let mut su = w.t.superuser().await;
    for bad in [
        "'ci/a/'",
        "'ci/a//b'",
        "'ci/a b'",
        "'ext/x'",
        "'ci/-x'",
        "'ci/'",
        // R6 review round 2: one element holding two names would pass a
        // space-joined match and then block every merge.
        "'ci/a ci/b'",
        "'ci/a', 'ci/b forge/c'",
        // R6 re-review: an empty name, a leading or trailing space, a NULL
        // element (alone or next to a valid name).
        "''",
        "' ci/a'",
        "'ci/a '",
        "NULL",
        "'ci/a', NULL",
    ] {
        let err = sqlx::query(&insert(bad)).execute(&mut su).await;
        assert_eq!(
            constraint_of(err),
            Some("protected_ref_merge_rules_required_checks".to_owned()),
            "{bad} must be refused by the CHECK"
        );
    }
    let long = format!("'ci/{}'", "a".repeat(101));
    assert_eq!(
        constraint_of(sqlx::query(&insert(&long)).execute(&mut su).await),
        Some("protected_ref_merge_rules_required_checks".to_owned())
    );
    // The CHECK accepts well-formed names (the audit trigger, deferred to
    // the commit, is not reached: the transaction is rolled back).
    let mut tx = sqlx::Connection::begin(&mut su).await.expect("begin");
    sqlx::query(&insert("'ci/a', 'forge/b.c', 'kanflow/x/y'"))
        .execute(&mut *tx)
        .await
        .expect("well-formed names pass the CHECK");
    tx.rollback().await.expect("rollback");
    // No required check (the column's default) passes too.
    let mut tx = sqlx::Connection::begin(&mut su).await.expect("begin");
    sqlx::query(&insert(""))
        .execute(&mut *tx)
        .await
        .expect("an empty array passes the CHECK");
    tx.rollback().await.expect("rollback");
    w.t.drop_now().await;
}

/// The constraint a failed statement violated.
fn constraint_of(result: Result<sqlx::postgres::PgQueryResult, sqlx::Error>) -> Option<String> {
    match result {
        Err(sqlx::Error::Database(e)) => e.constraint().map(str::to_owned),
        _ => None,
    }
}

/// R6 review round 2 (LOW): the scan-order mark is written only on a
/// suspension that is still expired — a row whose switch changed after the
/// scan (enforced again, or a new suspension) keeps no stale mark.
#[tokio::test]
async fn the_restore_mark_is_written_only_on_a_row_still_expired() {
    let w = world().await;
    w.suspend(30, "req-suspend").await;
    // A suspension still in force (not expired).
    mark_restore_seen(&w.t.db, w.web.project_id)
        .await
        .expect("mark");
    assert_eq!(restore_seen(&w).await, None, "not expired: no mark");
    // Expired: marked.
    w.expire().await;
    mark_restore_seen(&w.t.db, w.web.project_id)
        .await
        .expect("mark");
    assert!(restore_seen(&w).await.is_some(), "expired: marked");
    w.t.drop_now().await;
}

/// The project's `codeowners_restore_seen_at`.
async fn restore_seen(w: &World) -> Option<String> {
    sqlx::query_scalar(
        "SELECT codeowners_restore_seen_at::text FROM forge.merge_settings WHERE project_id = $1",
    )
    .bind(w.web.project_id.get())
    .fetch_one(&mut w.t.superuser().await)
    .await
    .expect("seen")
}

// ── U2 review: `updated_at` is the optimistic-concurrency token ──

fn settings_values(method: MergeMethod) -> MergeSettingsValues {
    MergeSettingsValues {
        allowed_methods: vec![MergeMethod::MergeCommit, MergeMethod::Squash],
        default_method: method,
        ..MergeSettingsValues::default()
    }
}

/// A settings write that waited on the project row while another change
/// committed stamps a later `updated_at` than that change (`now()` would
/// stamp the waiting transaction's start, before it): the token never goes
/// backwards, so a reader of the newer change is never taken for stale and
/// a reader of the older one never passes.
#[tokio::test]
async fn updated_at_follows_the_commit_order() {
    let w = world().await;
    let id = w.web.project_id.get();
    let (_, first, ()) = put_merge_settings_mr_decided(
        &w.t.db,
        &as_user(w.alice, "req-put-1"),
        w.web.project_id,
        w.alice,
        &settings_values(MergeMethod::Squash),
        |_| Ok(()),
    )
    .await
    .expect("first put");
    let mut held =
        w.t.hold_audited(
            ("project", id),
            &[format!(
                "SELECT 1 FROM forge.projects WHERE id = {id} FOR UPDATE"
            )],
        )
        .await;
    let db = w.t.db.clone();
    let (alice, project) = (w.alice, w.web.project_id);
    let waiting = tokio::spawn(async move {
        put_merge_settings_mr_decided(
            &db,
            &as_user(alice, "req-put-2"),
            project,
            alice,
            &settings_values(MergeMethod::MergeCommit),
            |_| Ok(()),
        )
        .await
        .map(|(_, s, ())| s)
    });
    w.t.until_waiting("FROM forge.projects").await;
    // Another change commits while the write waits (its transaction began
    // before this one).
    let between = held
        .fetch_text(&format!(
            "UPDATE forge.merge_settings SET updated_at = clock_timestamp() \
             WHERE project_id = {id} \
             RETURNING to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')"
        ))
        .await;
    held.commit().await;
    let second = waiting.await.expect("join").expect("second put");
    let first = first.updated_at.expect("stored");
    let second = second.updated_at.expect("stored");
    assert!(between > first, "{between} > {first}");
    assert!(
        second > between,
        "the waiting write is later: {second} > {between}"
    );
    w.t.drop_now().await;
}

/// The expiry worker's restore locks the project row first, as every
/// settings writer does: it waits for a write holding it, so a write that
/// checked `updated_at` cannot overwrite the restore's change.
#[tokio::test]
async fn the_restore_waits_for_the_project_row() {
    let w = world().await;
    let id = w.web.project_id.get();
    w.suspend(30, "req-suspend").await;
    w.t.commit_audited(
        ("project", id),
        &[format!(
            "UPDATE forge.merge_settings SET codeowners_suspended_at = now() - interval '2 hours', \
             codeowners_enforced_off_until = now() - interval '1 hour' WHERE project_id = {id}"
        )],
    )
    .await;
    let held =
        w.t.hold_audited(
            ("project", id),
            &[format!(
                "SELECT 1 FROM forge.projects WHERE id = {id} FOR UPDATE"
            )],
        )
        .await;
    let db = w.t.db.clone();
    let restore =
        tokio::spawn(async move { restore_expired_codeowners(&db, &system("req-restore")).await });
    w.t.until_waiting("FROM forge.projects").await;
    assert!(
        !restore.is_finished(),
        "the restore waits for the project row"
    );
    held.commit().await;
    let report = restore.await.expect("join").expect("restore");
    assert_eq!(report.restored, 1);
    assert_eq!(
        merge_settings(&w.t.db, w.web.project_id)
            .await
            .expect("read")
            .codeowners,
        CodeownersEnforcement::Enforced
    );
    w.t.drop_now().await;
}
