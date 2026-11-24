//! ADR-0038 §3.4–§3.6 (step C): deploy environment approvers, the
//! approvals count, delegation under the approval's lock.

use forge_core::approver::ApproverSelector;

use super::*;
use crate::review::{set_delegation_decided, DelegationOutcome};

fn input(users: Vec<UserId>, required: i32, allow_delegation: bool) -> ApproversInput {
    ApproversInput {
        users,
        groups: Vec::new(),
        selectors: Vec::new(),
        required,
        allow_delegation,
    }
}

async fn put(
    w: &World,
    tier: DeployTier,
    i: &ApproversInput,
) -> Result<EnvironmentApprovers, ForgeError> {
    put_approvers_decided(
        &w.t.db,
        &ctx(Actor::User(w.alice), "req-approvers"),
        w.project(),
        tier,
        i,
        |_| Ok(()),
    )
    .await
}

impl World {
    /// `who` becomes a Reporter of the project (a delegator holds the role
    /// a named approver's gate asks for).
    async fn reporter(&self, who: UserId) {
        projects::add_member(
            &self.t.db,
            &ctx(Actor::System, "req-reporter"),
            self.project(),
            who,
            forge_core::Role::Reporter,
        )
        .await
        .expect("reporter");
    }

    /// Bob delegates to `cy` around now.
    async fn bob_delegates_to_cy(&self, request: &str) {
        let (s, e) = self.period().await;
        let _ = set_delegation_decided(
            &self.t.db,
            &ctx(Actor::User(self.bob), request),
            self.bob,
            "cy",
            &s,
            &e,
        )
        .await
        .expect("delegation");
    }
}

async fn prod_request(w: &World, c: char) -> StoredDeployment {
    w.request(w.alice, DeployTier::Prod, &sha(c))
        .await
        .expect("request")
}

#[tokio::test]
async fn two_named_approvers_are_needed_when_the_environment_requires_two() {
    let w = world().await;
    let carol = w.person("cy").await;
    w.env(DeployTier::Prod, Vec::new()).await;
    w.target(DeployTier::Prod, "web-1").await.expect("target");
    let set = put(&w, DeployTier::Prod, &input(vec![w.bob, carol], 2, false))
        .await
        .expect("approvers");
    assert_eq!((set.required, set.users.len()), (2, 2));
    let d = prod_request(&w, 'a').await;
    let first = w.approve(w.bob, d.id).await.expect("bob");
    assert_eq!(first.status, DeploymentStatus::PendingApproval);
    assert_eq!(first.approvals, 1);
    assert_eq!(first.approved_by, None);
    let again = w.approve(w.bob, d.id).await.expect_err("bob twice");
    assert_eq!(again.code(), ErrorCode::DeploymentStateConflict);
    let second = w.approve(carol, d.id).await.expect("carol");
    assert_eq!(second.status, DeploymentStatus::Queued);
    assert_eq!(second.approved_by, Some(carol.get()));
    let approvals = deployment_approvals(&w.t.db, w.project(), d.id)
        .await
        .expect("approvals");
    assert_eq!(approvals.len(), 2);
    w.t.drop_now().await;
}

#[tokio::test]
async fn approvers_are_validated_and_read_back() {
    let w = world().await;
    // No environment yet.
    let none = put(&w, DeployTier::Prod, &input(vec![w.bob], 1, false))
        .await
        .expect_err("no environment");
    assert_eq!(none.code(), ErrorCode::NotFound);
    w.env(DeployTier::Prod, Vec::new()).await;
    // More approvals than the people named.
    let more = put(&w, DeployTier::Prod, &input(vec![w.bob], 2, false))
        .await
        .expect_err("more than named");
    assert_eq!(more.code(), ErrorCode::RequestInvalid);
    let selector = ApproverSelector::from_value(&serde_json::json!({
        "relation": "requester_department", "min_rank": 55, "escalate": "parent"
    }))
    .expect("selector");
    let mut with_selector = input(vec![w.bob], 2, true);
    with_selector.selectors = vec![selector.clone()];
    let stored = put(&w, DeployTier::Prod, &with_selector)
        .await
        .expect("approvers");
    assert_eq!(stored.selectors, vec![selector]);
    assert!(stored.allow_delegation);
    let read = project_approvers(&w.t.db, w.project()).await.expect("read");
    assert_eq!(read, vec![stored]);
    assert!(w
        .audit_actions()
        .await
        .iter()
        .any(|a| a == "deploy_environment.approvers_set"));
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_changed_approver_list_is_a_conflict() {
    let w = world().await;
    w.env(DeployTier::Prod, Vec::new()).await;
    w.target(DeployTier::Prod, "web-1").await.expect("target");
    let d = prod_request(&w, 'a').await;
    let stale = DeployApprovalInput {
        on_behalf_of: None,
        org: forge_core::approver::PersonOrg::Unknown("no_snapshot".to_owned()),
        version: "2000-01-01T00:00:00.000000Z".to_owned(),
        delegator_required: None,
    };
    let e = approve_decided(
        &w.t.db,
        &ctx(Actor::User(w.bob), "req-approve-stale"),
        w.project(),
        d.id,
        &stale,
        |_, _| Ok(()),
    )
    .await
    .expect_err("stale approvers");
    assert_eq!(e.code(), ErrorCode::DeploymentStateConflict);
    w.t.drop_now().await;
}

#[tokio::test]
async fn an_approval_for_someone_without_a_delegation_is_refused() {
    let w = world().await;
    w.reporter(w.bob).await;
    let carol = w.person("cy").await;
    w.env(DeployTier::Prod, Vec::new()).await;
    w.target(DeployTier::Prod, "web-1").await.expect("target");
    put(&w, DeployTier::Prod, &input(vec![w.bob], 1, true))
        .await
        .expect("approvers");
    let d = prod_request(&w, 'a').await;
    // No delegation: refused, nothing written.
    let e = w
        .approve_as(carol, d.id, Some(w.bob))
        .await
        .expect_err("no delegation");
    assert_eq!(e.code(), ErrorCode::ApprovalDelegationInactive);
    // Bob delegates to Carol: she approves for him.
    let (outcome, _) = set_delegation_decided(
        &w.t.db,
        &ctx(Actor::User(w.bob), "req-delegate"),
        w.bob,
        "cy",
        &w.period().await.0,
        &w.period().await.1,
    )
    .await
    .expect("delegation");
    assert_eq!(outcome, DelegationOutcome::Set);
    let approved = w
        .approve_as(carol, d.id, Some(w.bob))
        .await
        .expect("delegated");
    assert_eq!(approved.status, DeploymentStatus::Queued);
    let rows = deployment_approvals(&w.t.db, w.project(), d.id)
        .await
        .expect("rows");
    assert_eq!(rows[0].on_behalf_of, Some(w.bob));
    // An ended delegation is refused like none.
    let d2 = prod_request(&w, 'b').await;
    w.end_delegation(w.bob).await;
    let ended = w
        .approve_as(carol, d2.id, Some(w.bob))
        .await
        .expect_err("ended");
    assert_eq!(ended.code(), ErrorCode::ApprovalDelegationInactive);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_delegate_and_the_delegator_count_once() {
    let w = world().await;
    w.reporter(w.bob).await;
    let carol = w.person("cy").await;
    let dave = w.person("dave").await;
    w.env(DeployTier::Prod, Vec::new()).await;
    w.target(DeployTier::Prod, "web-1").await.expect("target");
    put(&w, DeployTier::Prod, &input(vec![w.bob, dave], 2, true))
        .await
        .expect("approvers");
    let _ = set_delegation_decided(
        &w.t.db,
        &ctx(Actor::User(w.bob), "req-delegate"),
        w.bob,
        "cy",
        &w.period().await.0,
        &w.period().await.1,
    )
    .await
    .expect("delegation");
    let d = prod_request(&w, 'a').await;
    w.approve(w.bob, d.id).await.expect("bob");
    let twice = w
        .approve_as(carol, d.id, Some(w.bob))
        .await
        .expect_err("bob counted already");
    assert_eq!(twice.code(), ErrorCode::DeploymentStateConflict);
    let done = w.approve(dave, d.id).await.expect("dave");
    assert_eq!(done.status, DeploymentStatus::Queued);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_delegation_is_checked_set_listed_and_removed() {
    let w = world().await;
    let carol = w.person("cy").await;
    let (start, end) = w.period().await;
    let set = |delegate: &'static str, s: String, e: String| {
        let db = w.t.db.clone();
        let bob = w.bob;
        async move {
            set_delegation_decided(
                &db,
                &ctx(Actor::User(bob), "req-delegate"),
                bob,
                delegate,
                &s,
                &e,
            )
            .await
        }
    };
    for (delegate, s, e, param) in [
        ("bob", start.clone(), end.clone(), "delegate"),
        ("nobody", start.clone(), end.clone(), "delegate"),
        ("cy", "not-a-time".to_owned(), end.clone(), "starts_at"),
        ("cy", end.clone(), start.clone(), "ends_at"),
        (
            "cy",
            "2026-01-01T00:00:00Z".to_owned(),
            "2026-03-01T00:00:00Z".to_owned(),
            "ends_at",
        ),
    ] {
        let refused = set(delegate, s, e).await.expect_err("refused");
        assert_eq!(refused.code(), ErrorCode::RequestInvalid, "{delegate}");
        assert!(format!("{refused:?}").contains(param), "{refused:?}");
    }
    let (outcome, stored) = set("cy", start.clone(), end.clone()).await.expect("set");
    assert_eq!(outcome, DelegationOutcome::Set);
    assert!(stored.active);
    assert_eq!(stored.delegate, carol);
    let (again, _) = set("cy", start, end).await.expect("same");
    assert_eq!(again, DelegationOutcome::Unchanged);
    let to_carol = crate::review::delegations_to(&w.t.db, carol)
        .await
        .expect("to");
    assert_eq!(to_carol.len(), 1);
    assert_eq!(to_carol[0].delegator, w.bob);
    let removed = crate::review::remove_delegation_decided(
        &w.t.db,
        &ctx(Actor::User(w.bob), "req-undelegate"),
        w.bob,
    )
    .await
    .expect("remove");
    assert_eq!(removed, DelegationOutcome::Removed);
    assert!(crate::review::delegation_of(&w.t.db, w.bob)
        .await
        .expect("read")
        .is_none());
    let actions = w.audit_actions().await;
    assert!(actions.iter().any(|a| a == "approval_delegation.set"));
    assert!(actions.iter().any(|a| a == "approval_delegation.removed"));
    w.t.drop_now().await;
}

#[tokio::test]
async fn approvals_end_with_the_approver_list_and_a_disabled_approver_stops_counting() {
    let w = world().await;
    let carol = w.person("cy").await;
    let dave = w.person("dave").await;
    w.env(DeployTier::Prod, Vec::new()).await;
    w.target(DeployTier::Prod, "web-1").await.expect("target");
    let three = input(vec![w.bob, carol, dave], 2, false);
    put(&w, DeployTier::Prod, &three).await.expect("approvers");
    let d = prod_request(&w, 'a').await;
    let first = w.approve(w.bob, d.id).await.expect("bob");
    assert_eq!(first.approvals, 1);
    // The list is set again: Bob's approval was decided on the old one.
    put(&w, DeployTier::Prod, &three)
        .await
        .expect("approvers again");
    let read = get_deployment(&w.t.db, w.project(), d.id)
        .await
        .expect("read")
        .expect("deployment");
    assert_eq!(read.approvals, 0);
    assert!(deployment_approvals(&w.t.db, w.project(), d.id)
        .await
        .expect("rows")
        .is_empty());
    // Bob approves again, then leaves: his approval no longer counts.
    w.approve(w.bob, d.id).await.expect("bob again");
    let _ = users::disable(&w.t.db, &ctx(Actor::System, "req-disable"), w.bob)
        .await
        .expect("disable");
    let second = w.approve(carol, d.id).await.expect("carol");
    assert_eq!(second.status, DeploymentStatus::PendingApproval);
    assert_eq!(second.approvals, 1);
    let third = w.approve(dave, d.id).await.expect("dave");
    assert_eq!(third.status, DeploymentStatus::Queued);
    w.t.drop_now().await;
}

#[tokio::test]
async fn an_approval_for_someone_is_refused_where_the_environment_does_not_allow_it() {
    let w = world().await;
    let carol = w.person("cy").await;
    w.env(DeployTier::Prod, Vec::new()).await;
    w.target(DeployTier::Prod, "web-1").await.expect("target");
    put(&w, DeployTier::Prod, &input(vec![w.bob], 1, false))
        .await
        .expect("approvers");
    let _ = set_delegation_decided(
        &w.t.db,
        &ctx(Actor::User(w.bob), "req-delegate"),
        w.bob,
        "cy",
        &w.period().await.0,
        &w.period().await.1,
    )
    .await
    .expect("delegation");
    let d = prod_request(&w, 'a').await;
    let e = w
        .approve_as(carol, d.id, Some(w.bob))
        .await
        .expect_err("delegation off");
    assert_eq!(e.code(), ErrorCode::DeployApproverNotListed);
    assert!(deployment_approvals(&w.t.db, w.project(), d.id)
        .await
        .expect("rows")
        .is_empty());
    w.t.drop_now().await;
}

/// Re-review of ADR-0038 step C: an approval given for someone counts only
/// while their delegation is active and they hold the role its gate asked
/// for — at every count, and read again under the approval's lock.
#[tokio::test]
async fn a_delegated_deploy_approval_counts_only_while_the_delegation_and_the_role_hold() {
    let w = world().await;
    let cy = w.person("cy").await;
    let dave = w.person("dave").await;
    let erin = w.person("erin").await;
    w.reporter(w.bob).await;
    w.env(DeployTier::Prod, Vec::new()).await;
    w.target(DeployTier::Prod, "web-1").await.expect("target");
    put(
        &w,
        DeployTier::Prod,
        &input(vec![w.bob, dave, erin], 2, true),
    )
    .await
    .expect("approvers");
    // The delegation ends after Cy approved for Bob: no longer counted.
    w.bob_delegates_to_cy("req-delegate-1").await;
    let d1 = prod_request(&w, 'a').await;
    let first = w.approve_as(cy, d1.id, Some(w.bob)).await.expect("for bob");
    assert_eq!(first.approvals, 1);
    w.end_delegation(w.bob).await;
    let shown = get_deployment(&w.t.db, w.project(), d1.id)
        .await
        .expect("read")
        .expect("deployment");
    assert_eq!(shown.approvals, 0);
    let after = w.approve(dave, d1.id).await.expect("dave");
    assert_eq!(
        (after.status, after.approvals),
        (DeploymentStatus::PendingApproval, 1)
    );
    // Bob loses his role after Cy approved for him: no longer counted.
    w.bob_delegates_to_cy("req-delegate-2").await;
    let d2 = prod_request(&w, 'b').await;
    w.approve_as(cy, d2.id, Some(w.bob)).await.expect("for bob");
    projects::remove_member(
        &w.t.db,
        &ctx(Actor::System, "req-unmember"),
        w.project(),
        w.bob,
        forge_core::Role::Reporter,
    )
    .await
    .expect("remove bob");
    let after = w.approve(dave, d2.id).await.expect("dave");
    assert_eq!(
        (after.status, after.approvals),
        (DeploymentStatus::PendingApproval, 1)
    );
    // Without the role, approving for Bob is refused under the lock.
    let d3 = prod_request(&w, 'c').await;
    let refused = w
        .approve_as(cy, d3.id, Some(w.bob))
        .await
        .expect_err("no role");
    assert_eq!(refused.code(), ErrorCode::Forbidden);
    assert!(deployment_approvals(&w.t.db, w.project(), d3.id)
        .await
        .expect("rows")
        .is_empty());
    w.t.drop_now().await;
}
