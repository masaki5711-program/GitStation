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
