//! Approvals and approval rules against a migrated database (M3 R5,
//! ADR-0024 §8): the approval row bound to the head and target, its end on
//! a new head, a retarget and a withdrawal (one `merge_request.unapproved`
//! each), the guard, the cap, the rules and the push coverage query.

use forge_core::{ObjectId, RefPattern, RequestId, UnapproveReason};
use forge_errors::ErrorCode;

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::projects::{self, CreatedProject, NewProject};
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

fn oid(c: char) -> ObjectId {
    ObjectId::parse(&c.to_string().repeat(40)).expect("oid")
}

struct World {
    t: FreshDb,
    alice: UserId,
    bob: UserId,
    web: CreatedProject,
    mr: StoredMergeRequest,
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
    let bob = users::create(
        &t.db,
        &system("req-user-bob"),
        &NewUser::human("bob", "bob", "bob@example.test"),
    )
    .await
    .expect("bob");
    let web = projects::create(
        &t.db,
        &system("req-project-web"),
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("web");
    let (mr, ()) = open_mr_decided(
        &t.db,
        &as_user(alice.user_id, "req-open"),
        web.project_id,
        alice.user_id,
        &NewMergeRequest {
            title: "Feature".to_owned(),
            description: String::new(),
            source_branch: RefName::parse("refs/heads/feature").expect("branch"),
            target_branch: RefName::parse("refs/heads/main").expect("branch"),
            draft: false,
        },
        |_| Ok(()),
    )
    .await
    .expect("open");
    record_mr_version(
        &t.db,
        &system("req-v1"),
        mr.id,
        &oid('a'),
        Some(alice.user_id),
    )
    .await
    .expect("v1");
    World {
        t,
        alice: alice.user_id,
        bob: bob.user_id,
        web,
        mr,
    }
}

impl World {
    async fn approve(&self, who: UserId, request: &str) -> (ApproveOutcome, StoredApproval) {
        approve_mr_decided(
            &self.t.db,
            &as_user(who, request),
            (self.web.project_id, self.mr.iid),
            who,
            |_| Ok(()),
            |_, facts, id| {
                assert!(facts.approvals.iter().any(|a| a.approval.id == id));
                (true, None)
            },
        )
        .await
        .map(|(o, a, ())| (o, a))
        .expect("approve")
    }

    async fn events(&self, kind: &str) -> Vec<serde_json::Value> {
        sqlx::query_scalar("SELECT payload FROM forge.events WHERE event_type = $1 ORDER BY id")
            .bind(kind)
            .fetch_all(&mut self.t.superuser().await)
            .await
            .expect("events")
    }

    async fn live(&self) -> Vec<StoredApproval> {
        approval_facts(&self.t.db, self.web.project_id, self.mr.id)
            .await
            .expect("facts")
            .approvals
            .into_iter()
            .map(|a| a.approval)
            .collect()
    }
}

/// ADR-0024 §8.1: the row holds the head and target as locked; approving
/// the same pair again writes nothing; `approved` carries the
/// classification of that moment.
#[tokio::test]
async fn an_approval_is_bound_to_the_head_and_target_and_is_idempotent() {
    let w = world().await;
    let (outcome, stored) = w.approve(w.bob, "req-approve").await;
    assert_eq!(
        outcome,
        ApproveOutcome::Approved {
            counted: true,
            uncounted_reason: None
        }
    );
    assert_eq!(stored.head_sha, oid('a'));
    assert_eq!(stored.target_branch.as_str(), "refs/heads/main");
    let (again, same) = w.approve(w.bob, "req-approve-again").await;
    assert_eq!(again, ApproveOutcome::Unchanged);
    assert_eq!(same.id, stored.id);
    let approved = w.events("merge_request.approved").await;
    assert_eq!(approved.len(), 1, "one event");
    assert_eq!(approved[0]["counted"], true);
    assert_eq!(approved[0]["approver_id"], w.bob.get());
}

/// Plan 11 Q4 / ADR-0024 §8.2: a new head ends every approval
/// (`new_commits`), one event per approval, in the version's transaction.
#[tokio::test]
async fn a_new_head_revokes_the_approvals() {
    let w = world().await;
    assert!(matches!(
        w.approve(w.bob, "req-approve-bob").await.0,
        ApproveOutcome::Approved { .. }
    ));
    assert!(matches!(
        w.approve(w.alice, "req-approve-alice").await.0,
        ApproveOutcome::Approved { .. }
    )); // authz is forge-review's
    record_mr_version(&w.t.db, &system("req-v2"), w.mr.id, &oid('b'), None)
        .await
        .expect("v2");
    assert!(w.live().await.is_empty(), "every approval ended");
    let ended = w.events("merge_request.unapproved").await;
    assert_eq!(ended.len(), 2);
    assert!(ended.iter().all(|e| e["reason"] == "new_commits"));
    // Back to the first head (A -> B -> A): the old approvals stay ended.
    record_mr_version(&w.t.db, &system("req-v3"), w.mr.id, &oid('a'), None)
        .await
        .expect("restore");
    assert!(w.live().await.is_empty());
}

/// ADR-0024 §8.1: a retarget ends the approvals (`target_changed`).
#[tokio::test]
async fn a_retarget_revokes_the_approvals() {
    let w = world().await;
    assert!(matches!(
        w.approve(w.bob, "req-approve").await.0,
        ApproveOutcome::Approved { .. }
    ));
    let (_, _, ()) = edit_mr_decided(
        &w.t.db,
        &as_user(w.alice, "req-retarget"),
        w.web.project_id,
        w.mr.iid,
        w.alice,
        &MergeRequestPatch {
            target_branch: Some(RefName::parse("refs/heads/release").expect("branch")),
            ..MergeRequestPatch::default()
        },
        |_| Ok(()),
    )
    .await
    .expect("retarget");
    assert!(w.live().await.is_empty());
    let ended = w.events("merge_request.unapproved").await;
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0]["reason"], "target_changed");
}

#[tokio::test]
async fn an_approver_withdraws_only_their_own_approval() {
    let w = world().await;
    assert!(matches!(
        w.approve(w.bob, "req-approve-bob").await.0,
        ApproveOutcome::Approved { .. }
    ));
    assert!(matches!(
        w.approve(w.alice, "req-approve-alice").await.0,
        ApproveOutcome::Approved { .. }
    ));
    let (outcome, ended, ()) = unapprove_mr_decided(
        &w.t.db,
        &as_user(w.bob, "req-unapprove"),
        (w.web.project_id, w.mr.iid),
        w.bob,
        |_| Ok(()),
    )
    .await
    .expect("unapprove");
    assert_eq!(outcome, ApproveOutcome::Withdrawn);
    assert_eq!(
        ended.expect("ended").revoked_reason,
        Some(UnapproveReason::Unapproved)
    );
    let live = w.live().await;
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].approver, w.alice);
    let (again, none, ()) = unapprove_mr_decided(
        &w.t.db,
        &as_user(w.bob, "req-unapprove-again"),
        (w.web.project_id, w.mr.iid),
        w.bob,
        |_| Ok(()),
    )
    .await
    .expect("unapprove again");
    assert_eq!(again, ApproveOutcome::Unchanged);
    assert!(none.is_none());
}

/// The guard: an approval's identity never changes and an ended one is
/// final.
#[tokio::test]
async fn an_ended_approval_never_changes() {
    let w = world().await;
    let (_, stored) = w.approve(w.bob, "req-approve").await;
    let mut conn = w.t.superuser().await;
    let moved = sqlx::query("UPDATE forge.merge_request_approvals SET head_sha = $2 WHERE id = $1")
        .bind(stored.id.get())
        .bind(oid('c').as_str())
        .execute(&mut conn)
        .await;
    assert!(moved.is_err(), "the identity is fixed");
    record_mr_version(&w.t.db, &system("req-v2"), w.mr.id, &oid('b'), None)
        .await
        .expect("v2");
    let revived = sqlx::query(
        "UPDATE forge.merge_request_approvals SET revoked_at = NULL, revoked_reason = NULL \
         WHERE id = $1",
    )
    .bind(stored.id.get())
    .execute(&mut conn)
    .await;
    assert!(revived.is_err(), "an ended approval is final");
}

/// FG-REVIEW-0015: more than 50 live approvals on one merge request.
#[tokio::test]
async fn an_approval_past_the_cap_is_refused() {
    let w = world().await;
    // 50 live approvals by synthetic users, written in one transaction
    // with the project's audit row (setup only).
    let mut ids = Vec::new();
    for n in 0..MAX_APPROVALS {
        let name = format!("approver{n}");
        let u = users::create(
            &w.t.db,
            &system(&format!("req-user-{n}")),
            &NewUser::human(&name, &name, &format!("{name}@example.test")),
        )
        .await
        .expect("user");
        ids.push(u.user_id.get());
    }
    let mut conn = w.t.superuser().await;
    sqlx::query("BEGIN")
        .execute(&mut conn)
        .await
        .expect("begin");
    sqlx::query(
        "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, resource_id,          project_id, request_id) VALUES ('system', 'merge_request.approved', 'succeeded',          'project', $1, $1, 'req-setup')",
    )
    .bind(w.web.project_id.get())
    .execute(&mut conn)
    .await
    .expect("audit");
    for user in ids {
        sqlx::query(
            "INSERT INTO forge.merge_request_approvals (merge_request_id, project_id,              approver_id, head_sha, target_branch) VALUES ($1, $2, $3, $4, 'refs/heads/main')",
        )
        .bind(w.mr.id.get())
        .bind(w.web.project_id.get())
        .bind(user)
        .bind(oid('a').as_str())
        .execute(&mut conn)
        .await
        .expect("approval");
    }
    sqlx::query("COMMIT")
        .execute(&mut conn)
        .await
        .expect("commit");
    let e = approve_mr_decided(
        &w.t.db,
        &as_user(w.bob, "req-approve"),
        (w.web.project_id, w.mr.iid),
        w.bob,
        |_| Ok(()),
        |_, _, _| (true, None),
    )
    .await
    .expect_err("full");
    assert_eq!(e.code(), ErrorCode::ApprovalLimitReached);
}

#[tokio::test]
async fn approval_rules_are_put_replaced_and_removed() {
    let w = world().await;
    let rule = |required, users: Vec<UserId>| NewApprovalRule {
        name: "backend".to_owned(),
        target_pattern: Some(RefPattern::parse("refs/heads/main").expect("pattern")),
        required_approvals: required,
        users,
        groups: Vec::new(),
    };
    let put = |new: NewApprovalRule, request: &'static str| {
        let db = w.t.db.clone();
        let project = w.web.project_id;
        let alice = w.alice;
        async move {
            put_approval_rule_mr_decided(
                &db,
                &as_user(alice, request),
                project,
                alice,
                &new,
                |_| Ok(()),
            )
            .await
            .map(|(o, r, ())| (o, r))
        }
    };
    let (created, stored) = put(rule(1, vec![w.bob]), "req-rule-1")
        .await
        .expect("create");
    assert_eq!(created, RuleOutcome::Created);
    assert_eq!(stored.users, vec![w.bob]);
    let (same, _) = put(rule(1, vec![w.bob]), "req-rule-2").await.expect("same");
    assert_eq!(same, RuleOutcome::Unchanged);
    let (replaced, stored) = put(rule(2, Vec::new()), "req-rule-3")
        .await
        .expect("replace");
    assert_eq!(replaced, RuleOutcome::Replaced);
    assert_eq!(stored.required_approvals, 2);
    assert!(stored.users.is_empty());
    let unknown = put(rule(1, vec![UserId::new(999_999)]), "req-rule-4")
        .await
        .expect_err("unknown user");
    assert_eq!(unknown.code(), ErrorCode::RequestInvalid);
    let rules = approval_rules(&w.t.db, w.web.project_id)
        .await
        .expect("rules");
    assert_eq!(rules.len(), 1);
    let (removed, ()) = remove_approval_rule_mr_decided(
        &w.t.db,
        &as_user(w.alice, "req-rule-rm"),
        w.web.project_id,
        w.alice,
        "backend",
        |_| Ok(()),
    )
    .await
    .expect("remove");
    assert_eq!(removed, RuleOutcome::Removed);
    let gone = remove_approval_rule_mr_decided(
        &w.t.db,
        &as_user(w.alice, "req-rule-rm-2"),
        w.web.project_id,
        w.alice,
        "backend",
        |_| Ok(()),
    )
    .await
    .expect_err("gone");
    assert_eq!(gone.code(), ErrorCode::NotFound);
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM forge.audit_log WHERE action LIKE 'approval_rule.%'",
    )
    .fetch_one(&mut w.t.superuser().await)
    .await
    .expect("audit");
    assert_eq!(
        audited, 3,
        "put, replace and remove are audited; unchanged is not"
    );
}
