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
            None,
            &crate::review::ApproveAs::own_unknown(),
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
        None,
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
        None,
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
        None,
        &crate::review::ApproveAs::own_unknown(),
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
        selectors: Vec::new(),
    };
    let put = |new: NewApprovalRule, request: &'static str| {
        let db = w.t.db.clone();
        let project = w.web.project_id;
        let alice = w.alice;
        async move {
            // U2 follow-up: each put names the rule as it reads it now.
            let read = approval_rules(&db, project).await?;
            let expected = read
                .iter()
                .find(|r| r.name == new.name)
                .map(|r| r.updated_at.clone());
            put_approval_rule_mr_decided(
                &db,
                &as_user(alice, request),
                project,
                alice,
                &new,
                expected.as_deref(),
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
    // R5 review: a user named twice is one approver (no new audit row).
    let (twice, stored_twice) = put(rule(1, vec![w.bob, w.bob]), "req-rule-2b")
        .await
        .expect("twice");
    assert_eq!(twice, RuleOutcome::Unchanged);
    assert_eq!(stored_twice.users, vec![w.bob]);
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

/// M3 R6 (ADR-0024 §8.2): with `reset_approvals_on_push = false` and the
/// change allowed by forge-review (a [`CarryPlan`] for the recorded head),
/// a new head carries the live approvals whose approved head the plan
/// allows (old row `carried`, new row with `carried_from`, one
/// `merge_request.approved` each); the default setting, a protected
/// target, a plan for another head than the recorded one, or (R6 review
/// round 2) an approved head the plan does not name end them with
/// `new_commits`.
#[tokio::test]
async fn approvals_are_carried_only_when_every_condition_holds() {
    let w = world().await;
    let (first, _) = w.approve(w.bob, "req-approve").await;
    assert!(matches!(first, ApproveOutcome::Approved { .. }));
    // The default (reset on push): the carry asked for is not taken.
    record_mr_version_carrying(
        &w.t.db,
        &system("req-v2"),
        w.mr.id,
        &oid('b'),
        None,
        Some(&CarryPlan {
            from: oid('a'),
            approved_heads: vec![oid('a')],
        }),
    )
    .await
    .expect("v2");
    assert!(w.live().await.is_empty(), "the default ends them");
    // The project keeps approvals on push.
    let keep = forge_core::MergeSettingsValues {
        reset_approvals_on_push: false,
        ..forge_core::MergeSettingsValues::default()
    };
    let (changed, _, ()) = put_merge_settings_mr_decided(
        &w.t.db,
        &as_user(w.alice, "req-settings"),
        w.web.project_id,
        w.alice,
        &keep,
        |_| Ok(()),
    )
    .await
    .expect("settings");
    assert_eq!(changed, SettingsOutcome::Changed);
    let (second, _) = w.approve(w.bob, "req-approve-b").await;
    assert!(matches!(second, ApproveOutcome::Approved { .. }));
    // A carry asked for another head than the recorded one: ended.
    record_mr_version_carrying(
        &w.t.db,
        &system("req-v3"),
        w.mr.id,
        &oid('c'),
        None,
        Some(&CarryPlan {
            from: oid('a'),
            approved_heads: vec![oid('a')],
        }),
    )
    .await
    .expect("v3");
    assert!(w.live().await.is_empty(), "a stale carry_from ends them");
    let approved = w.approve(w.bob, "req-approve-c").await.1;
    record_mr_version_carrying(
        &w.t.db,
        &system("req-v4"),
        w.mr.id,
        &oid('d'),
        None,
        Some(&CarryPlan {
            from: oid('c'),
            approved_heads: vec![oid('c')],
        }),
    )
    .await
    .expect("v4");
    let live = w.live().await;
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].head_sha, oid('d'));
    assert_eq!(live[0].approver, w.bob);
    let carried = w.events("merge_request.approved").await;
    let last = carried.last().expect("event");
    assert_eq!(last["carried_from"], approved.id.get());
    assert_eq!(last["uncounted_reason"], "carried");
    let reason: String = sqlx::query_scalar(
        "SELECT revoked_reason FROM forge.merge_request_approvals WHERE id = $1",
    )
    .bind(approved.id.get())
    .fetch_one(&mut w.t.superuser().await)
    .await
    .expect("old row");
    assert_eq!(reason, "carried");
    let heads = approved_heads(&w.t.db, w.mr.id)
        .await
        .expect("approved heads");
    assert_eq!(heads.len(), 1);
    assert_eq!(heads[0].approval, live[0].id);
    assert_eq!(
        heads[0].approved_head,
        Some(oid('c')),
        "followed back to the approval"
    );
    // R6 review round 2: the carried approval's approved head is still
    // `c`, not the recorded head `d` — a plan allowing only `d` (the
    // change from the previous head) ends it.
    record_mr_version_carrying(
        &w.t.db,
        &system("req-v5"),
        w.mr.id,
        &oid('e'),
        None,
        Some(&CarryPlan {
            from: oid('d'),
            approved_heads: vec![oid('d')],
        }),
    )
    .await
    .expect("v5");
    assert!(
        w.live().await.is_empty(),
        "an approval is measured from the head it was given on"
    );
    let (again, _) = w.approve(w.bob, "req-approve-e").await;
    assert!(matches!(again, ApproveOutcome::Approved { .. }));
    // A protected target: never carried.
    crate::protected_refs::create(
        &w.t.db,
        &as_user(w.alice, "req-protect"),
        w.web.project_id,
        &RefPattern::parse("refs/heads/main").expect("pattern"),
        forge_core::Role::Maintainer,
    )
    .await
    .expect("protect");
    record_mr_version_carrying(
        &w.t.db,
        &system("req-v6"),
        w.mr.id,
        &oid('f'),
        None,
        Some(&CarryPlan {
            from: oid('e'),
            approved_heads: vec![oid('e')],
        }),
    )
    .await
    .expect("v6");
    assert!(w.live().await.is_empty(), "a protected target ends them");
    w.t.drop_now().await;
}

impl World {
    /// The project keeps approvals on push (`reset_approvals_on_push = false`).
    async fn keep_approvals(&self) {
        let keep = forge_core::MergeSettingsValues {
            reset_approvals_on_push: false,
            ..forge_core::MergeSettingsValues::default()
        };
        let (changed, _, ()) = put_merge_settings_mr_decided(
            &self.t.db,
            &as_user(self.alice, "req-keep"),
            self.web.project_id,
            self.alice,
            &keep,
            |_| Ok(()),
        )
        .await
        .expect("settings");
        assert_eq!(changed, SettingsOutcome::Changed);
    }

    /// Record `to` as the head, carrying the approvals given on `approved`
    /// (the plan made for the recorded head `from`).
    async fn carry(&self, request: &str, (from, to): (char, char), approved: &[char]) {
        record_mr_version_carrying(
            &self.t.db,
            &system(request),
            self.mr.id,
            &oid(to),
            None,
            Some(&CarryPlan {
                from: oid(from),
                approved_heads: approved.iter().map(|c| oid(*c)).collect(),
            }),
        )
        .await
        .expect("version");
    }
}

/// R6 re-review (MEDIUM): the `carried_from` walk is bounded — a chain
/// longer than the cap has no approved head (`None`: never carried); one
/// within it is followed back through every ended, carried row.
#[tokio::test]
async fn the_approved_head_walk_is_bounded() {
    let w = world().await;
    w.keep_approvals().await;
    assert!(matches!(
        w.approve(w.bob, "req-approve").await.0,
        ApproveOutcome::Approved { .. }
    ));
    // Three carried pushes: a -> b -> c -> d.
    w.carry("req-v2", ('a', 'b'), &['a']).await;
    w.carry("req-v3", ('b', 'c'), &['a']).await;
    w.carry("req-v4", ('c', 'd'), &['a']).await;
    let mut su = w.t.superuser().await;
    let within = super::approvals::approved_heads_capped_on(&mut su, w.mr.id, 3)
        .await
        .expect("within the cap");
    assert_eq!(within.len(), 1);
    assert_eq!(within[0].approved_head, Some(oid('a')), "three hops back");
    let over = super::approvals::approved_heads_capped_on(&mut su, w.mr.id, 2)
        .await
        .expect("over the cap");
    assert_eq!(over.len(), 1, "the approval is still listed");
    assert_eq!(over[0].approved_head, None, "past the cap: never carried");
    w.t.drop_now().await;
}

impl World {
    /// `(action, approver_id)` of the audit rows of `request`, by id.
    async fn audit_of(&self, request: &str) -> Vec<(String, Option<i64>)> {
        sqlx::query_as(
            "SELECT action, (details ->> 'approver_id')::bigint FROM forge.audit_log \
             WHERE request_id = $1 ORDER BY id",
        )
        .bind(request)
        .fetch_all(&mut self.t.superuser().await)
        .await
        .expect("audit")
    }

    /// `(event_type, approver_id)` of the events of `request`, by id.
    async fn events_of(&self, request: &str) -> Vec<(String, Option<i64>)> {
        sqlx::query_as(
            "SELECT event_type, (payload ->> 'approver_id')::bigint FROM forge.events \
             WHERE request_id = $1 ORDER BY id",
        )
        .bind(request)
        .fetch_all(&mut self.t.superuser().await)
        .await
        .expect("events")
    }
}

/// R6 re-review (MEDIUM): one carry keeps the approval whose approved head
/// the plan allows — found through a multi-hop chain of ended, carried
/// rows — and ends the other (`new_commits`); every live approval yields
/// exactly one audit row and one event in that change (plus the push's
/// own).
#[tokio::test]
async fn a_carry_keeps_or_ends_each_approval_by_its_approved_head() {
    let w = world().await;
    w.keep_approvals().await;
    assert!(matches!(
        w.approve(w.bob, "req-approve-bob").await.0,
        ApproveOutcome::Approved { .. }
    ));
    w.carry("req-v2", ('a', 'b'), &['a']).await;
    w.carry("req-v3", ('b', 'c'), &['a']).await;
    assert!(matches!(
        w.approve(w.alice, "req-approve-alice").await.0,
        ApproveOutcome::Approved { .. }
    ));
    let heads = approved_heads(&w.t.db, w.mr.id).await.expect("heads");
    assert_eq!(
        heads
            .iter()
            .map(|h| h.approved_head.clone())
            .collect::<Vec<_>>(),
        vec![Some(oid('a')), Some(oid('c'))],
        "bob's through two ended rows, alice's on c"
    );
    // The plan allows only the change from `a` (bob's approved head).
    w.carry("req-v4", ('c', 'd'), &['a']).await;
    let bob = Some(w.bob.get());
    let alice = Some(w.alice.get());
    let mut audit = w.audit_of("req-v4").await;
    audit.sort();
    assert_eq!(
        audit,
        vec![
            ("merge_request.approved".to_owned(), bob),
            ("merge_request.pushed".to_owned(), None),
            ("merge_request.unapproved".to_owned(), alice),
        ]
    );
    let mut events = w.events_of("req-v4").await;
    events.sort();
    assert_eq!(
        events,
        vec![
            ("merge_request.approved".to_owned(), bob),
            ("merge_request.pushed".to_owned(), None),
            ("merge_request.unapproved".to_owned(), alice),
        ]
    );
    let live = w.live().await;
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].approver, w.bob);
    assert_eq!(live[0].head_sha, oid('d'));
    let after = approved_heads(&w.t.db, w.mr.id).await.expect("heads");
    assert_eq!(
        after[0].approved_head,
        Some(oid('a')),
        "still bob's first head"
    );
    w.t.drop_now().await;
}

// ── Review of ADR-0038 step C: delegated approvals ──────────────────────

impl World {
    /// `who` approves for `on_behalf_of` (or themself).
    async fn approve_for(
        &self,
        who: UserId,
        on_behalf_of: Option<UserId>,
        request: &str,
    ) -> Result<(ApproveOutcome, StoredApproval), ForgeError> {
        approve_mr_decided(
            &self.t.db,
            &as_user(who, request),
            (self.web.project_id, self.mr.iid),
            who,
            None,
            &crate::review::ApproveAs {
                on_behalf_of,
                org: forge_core::approver::PersonOrg::Unknown("not_read".to_owned()),
            },
            |_| Ok(()),
            |_, _, _| (true, None),
        )
        .await
        .map(|(o, a, ())| (o, a))
    }

    /// `delegator` delegates to `delegate` from a day ago for ten days.
    async fn delegate(&self, delegator: UserId, delegate: &str, request: &str) {
        let (s, e): (String, String) = sqlx::query_as(
            "SELECT to_char((now() - interval '1 day') AT TIME ZONE 'UTC', \
               'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), \
             to_char((now() + interval '9 days') AT TIME ZONE 'UTC', \
               'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')",
        )
        .fetch_one(&mut self.t.superuser().await)
        .await
        .expect("period");
        let (outcome, _) = set_delegation_decided(
            &self.t.db,
            &as_user(delegator, request),
            delegator,
            delegate,
            &s,
            &e,
        )
        .await
        .expect("delegation");
        assert_eq!(outcome, DelegationOutcome::Set);
    }

    async fn revoked_reason(&self, approval: forge_core::ApprovalId) -> Option<String> {
        sqlx::query_scalar("SELECT revoked_reason FROM forge.merge_request_approvals WHERE id = $1")
            .bind(approval.get())
            .fetch_one(&mut self.t.superuser().await)
            .await
            .expect("row")
    }
}

async fn carol(w: &World) -> UserId {
    users::create(
        &w.t.db,
        &system("req-user-carol"),
        &NewUser::human("carol", "carol", "carol@example.test"),
    )
    .await
    .expect("carol")
    .user_id
}

#[tokio::test]
async fn one_person_holds_one_live_approval_and_a_removed_delegation_ends_its_approvals() {
    let w = world().await;
    let carol = carol(&w).await;
    // Bob (no role on the project) and Alice (its Owner) delegate to Carol.
    w.delegate(w.bob, "carol", "req-delegate-bob").await;
    let (_, for_bob) = w
        .approve_for(carol, Some(w.bob), "req-for-bob")
        .await
        .expect("for bob");
    // The delegator's role is loaded with the approval (`None`: none).
    let facts = approval_facts(&w.t.db, w.web.project_id, w.mr.id)
        .await
        .expect("facts");
    let d = facts.approvals[0].delegator.as_ref().expect("delegator");
    assert_eq!((d.user, d.role), (w.bob, None));
    // Bob approves himself: his delegate's approval for him ends.
    let _ = w.approve(w.bob, "req-bob-self").await;
    assert_eq!(
        w.revoked_reason(for_bob.id).await.as_deref(),
        Some("unapproved")
    );
    assert_eq!(w.live().await.len(), 1);
    // Carol approving for Bob now is refused: Bob's approval is there.
    let twice = w
        .approve_for(carol, Some(w.bob), "req-for-bob-again")
        .await
        .expect_err("bob holds one");
    assert_eq!(twice.code(), ErrorCode::ApprovalPersonAlreadyApproved);
    // The index is the backstop: a second live row for Bob is refused by
    // the database itself (23505, before any deferred trigger).
    let raw = sqlx::query(
        "INSERT INTO forge.merge_request_approvals (merge_request_id, project_id, approver_id, \
         head_sha, target_branch, credential_origin, on_behalf_of) \
         VALUES ($1, $2, $3, $4, 'refs/heads/main', 'session', $5)",
    )
    .bind(w.mr.id.get())
    .bind(w.web.project_id.get())
    .bind(carol.get())
    .bind(oid('a').as_str())
    .bind(w.bob.get())
    .execute(&mut w.t.superuser().await)
    .await
    .expect_err("unique per person");
    let constraint = raw
        .as_database_error()
        .and_then(|e| e.constraint())
        .map(str::to_owned);
    assert_eq!(
        constraint.as_deref(),
        Some("merge_request_approvals_one_live_per_person")
    );
    // Alice delegates to Carol too; Carol approves for her (role Owner).
    w.delegate(w.alice, "carol", "req-delegate-alice").await;
    let (_, for_alice) = w
        .approve_for(carol, Some(w.alice), "req-for-alice")
        .await
        .expect("for alice");
    let facts = approval_facts(&w.t.db, w.web.project_id, w.mr.id)
        .await
        .expect("facts");
    let a = facts
        .approvals
        .iter()
        .find(|a| a.approval.id == for_alice.id)
        .and_then(|a| a.delegator.as_ref())
        .expect("alice");
    assert_eq!(a.role, Some(forge_core::Role::Owner));
    // Alice removes her delegation: Carol's approval for her ends with it.
    let removed = remove_delegation_decided(&w.t.db, &as_user(w.alice, "req-undelegate"), w.alice)
        .await
        .expect("remove");
    assert_eq!(removed, DelegationOutcome::Removed);
    assert_eq!(
        w.revoked_reason(for_alice.id).await.as_deref(),
        Some("delegation_ended")
    );
    let ended = w.events("merge_request.unapproved").await;
    assert_eq!(
        ended.last().expect("event")["reason"],
        "delegation_ended",
        "{ended:?}"
    );
    // Bob's own approval is untouched.
    let live = w.live().await;
    assert_eq!(live.len(), 1);
    assert_eq!((live[0].approver, live[0].on_behalf_of), (w.bob, None));
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_carry_ends_an_approval_whose_delegation_no_longer_holds() {
    let w = world().await;
    let carol = carol(&w).await;
    w.keep_approvals().await;
    w.delegate(w.bob, "carol", "req-delegate").await;
    let (_, for_bob) = w
        .approve_for(carol, Some(w.bob), "req-for-bob")
        .await
        .expect("for bob");
    // The delegation's period passes (as if time went by).
    let mut su = w.t.superuser().await;
    sqlx::raw_sql(&format!(
        "BEGIN; \
         INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, resource_id, \
           request_id, details) VALUES ('system', 'approval_delegation.set', 'succeeded', \
           'user', {bob}, 'req-test-expire', '{{}}'); \
         UPDATE forge.approval_delegations SET starts_at = now() - interval '3 days', \
           ends_at = now() - interval '1 day' WHERE delegator_id = {bob}; \
         COMMIT;",
        bob = w.bob.get()
    ))
    .execute(&mut su)
    .await
    .expect("expire");
    w.carry("req-v2", ('a', 'b'), &['a']).await;
    assert!(w.live().await.is_empty(), "not carried");
    assert_eq!(
        w.revoked_reason(for_bob.id).await.as_deref(),
        Some("new_commits")
    );
    w.t.drop_now().await;
}

/// Re-review of ADR-0038 step C (HIGH): a delegate who approved more merge
/// requests than one change may end is still revocable — the delegation
/// row goes at once and the approvals end in batches of their own.
#[tokio::test]
async fn a_delegation_with_more_approvals_than_one_change_holds_is_still_removed() {
    let w = world().await;
    let carol = carol(&w).await;
    w.delegate(w.bob, "carol", "req-delegate").await;
    let many = usize::try_from(super::approvals::DELEGATED_BATCH).expect("batch") + 11;
    for n in 0..many {
        let (mr, ()) = open_mr_decided(
            &w.t.db,
            &as_user(w.alice, &format!("req-open-{n}")),
            w.web.project_id,
            w.alice,
            &NewMergeRequest {
                title: format!("Change {n}"),
                description: String::new(),
                source_branch: RefName::parse(&format!("refs/heads/f{n}")).expect("branch"),
                target_branch: RefName::parse("refs/heads/main").expect("branch"),
                draft: false,
            },
            |_| Ok(()),
        )
        .await
        .expect("open");
        record_mr_version(
            &w.t.db,
            &system(&format!("req-v-{n}")),
            mr.id,
            &oid('a'),
            Some(w.alice),
        )
        .await
        .expect("version");
        let _ = approve_mr_decided(
            &w.t.db,
            &as_user(carol, &format!("req-for-bob-{n}")),
            (w.web.project_id, mr.iid),
            carol,
            None,
            &crate::review::ApproveAs {
                on_behalf_of: Some(w.bob),
                org: forge_core::approver::PersonOrg::Unknown("not_read".to_owned()),
            },
            |_| Ok(()),
            |_, _, _| (true, None),
        )
        .await
        .expect("approve for bob");
    }
    let live = || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM forge.merge_request_approvals \
             WHERE on_behalf_of = $1 AND revoked_at IS NULL",
        )
        .bind(w.bob.get())
        .fetch_one(&mut w.t.superuser().await)
        .await
        .expect("count")
    };
    assert_eq!(live().await, i64::try_from(many).expect("many"));
    let removed = remove_delegation_decided(&w.t.db, &as_user(w.bob, "req-undelegate"), w.bob)
        .await
        .expect("removed, however many approvals");
    assert_eq!(removed, DelegationOutcome::Removed);
    assert_eq!(live().await, 0);
    let ended: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM forge.events WHERE event_type = 'merge_request.unapproved' \
         AND payload ->> 'reason' = 'delegation_ended'",
    )
    .fetch_one(&mut w.t.superuser().await)
    .await
    .expect("events");
    assert_eq!(ended, i64::try_from(many).expect("many"));
    w.t.drop_now().await;
}
