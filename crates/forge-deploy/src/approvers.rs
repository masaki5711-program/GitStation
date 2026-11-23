//! The approvers of deploy environments (ADR-0038 §3.4–§3.6, step C).
//!
//! - [`Deploy::approvers`] (`DeployRead`) / [`Deploy::put_approvers`]
//!   (`DeployApproversManage`: Owner and administrators, people only).
//! - [`Deploy::approver_gate`]: the fact `authorize()` reads for
//!   `DeployApprove` — whether the caller (or the delegator they approve
//!   for) is named as a user, matched through a group or an organisation
//!   selector, or not listed. A selector matches on the person's
//!   organisation now against the unit it names for the deployment's
//!   requester (fail closed: an unknown organisation matches nothing,
//!   `selector_unresolved`).

use forge_auth::{Action, ApproverGate, Authorized, ProjectCtx};
use forge_core::approver::{matches, ApproverSelector, PersonOrg};
use forge_core::deploy::DeployTier;
use forge_core::{ProjectId, Role, UserId};
use forge_db::deploy::{ApproversInput, EnvironmentApprovers, StoredDeployment};
use forge_errors::{ErrorCode, ForgeError};

use crate::{caller, change_context, param, reconfirm, require, ChangeRequest, Deploy};

/// An environment's approvers as an Owner sends them (before validation):
/// usernames, group full paths, selector objects (ADR-0038 §3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApproversSpec {
    pub users: Vec<String>,
    pub groups: Vec<String>,
    pub selectors: Vec<serde_json::Value>,
    pub required: i32,
    pub allow_delegation: bool,
}

/// The caller of a deployment approval against the environment's
/// approvers: the facts `authorize()` decides `DeployApprove` on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApproverFact {
    pub gate: ApproverGate,
    /// The delegator the caller approves for (`None`: for themself, or the
    /// name did not resolve - then `delegation_refused` says so).
    pub on_behalf_of: Option<UserId>,
    /// The delegator's native role on the project now.
    pub delegator_role: Option<Role>,
    /// Why the named delegation does not hold (`unknown_delegator`,
    /// `no_delegation`): a fact, so `authorize()` refuses first (review of
    /// ADR-0038 step C: no delegation or username oracle before it).
    pub delegation_refused: Option<&'static str>,
}

impl ApproverFact {
    /// The caller approves for themself, matching the approvers as `gate`.
    pub fn own(gate: ApproverGate) -> Self {
        Self {
            gate,
            on_behalf_of: None,
            delegator_role: None,
            delegation_refused: None,
        }
    }

    /// A named delegator that did not resolve.
    pub fn unknown_delegator() -> Self {
        Self {
            delegation_refused: Some("unknown_delegator"),
            ..Self::own(ApproverGate::NotListed("unknown_delegator"))
        }
    }
}

/// What the approval's decision was taken on (recorded with it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateFacts {
    pub fact: ApproverFact,
    /// The organisation of the person the approval counts for, now.
    pub org: PersonOrg,
    /// The environment's approvers version.
    pub version: String,
}

impl Deploy {
    /// `GET …/deploy/environments/{tier}/approvers` is part of the
    /// environments read (`DeployRead`): every environment's approvers.
    pub async fn approvers(
        &self,
        proof: &Authorized<ProjectCtx>,
    ) -> Result<Vec<EnvironmentApprovers>, ForgeError> {
        require(proof.action(), Action::DeployRead)?;
        forge_db::deploy::project_approvers(&self.db, *proof.id()).await
    }

    /// `PUT …/deploy/environments/{tier}/approvers`
    /// (`DeployApproversManage`): replace who may approve the
    /// environment's deployments. Unknown users or groups, a selector that
    /// does not validate, too many of each are `FG-WEB-0006`.
    pub async fn put_approvers(
        &self,
        request: &ChangeRequest,
        proof: &Authorized<ProjectCtx>,
        tier: DeployTier,
        spec: &ApproversSpec,
    ) -> Result<EnvironmentApprovers, ForgeError> {
        require(proof.action(), Action::DeployApproversManage)?;
        let user = caller(proof.principal(), Action::DeployApproversManage)?;
        let selectors = spec
            .selectors
            .iter()
            .map(|v| ApproverSelector::from_value(v).map_err(|why| param("selectors", why)))
            .collect::<Result<Vec<_>, _>>()?;
        let found = forge_db::review::owners(&self.db, &spec.users, &spec.groups).await?;
        let mut users = Vec::with_capacity(spec.users.len());
        for name in &spec.users {
            let id = found
                .users
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, id)| *id)
                .ok_or_else(|| param("users", "unknown_user"))?;
            users.push(id);
        }
        let mut groups = Vec::with_capacity(spec.groups.len());
        for path in &spec.groups {
            let id = found
                .groups
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, id)| *id)
                .ok_or_else(|| param("groups", "unknown_group"))?;
            groups.push(id);
        }
        let input = ApproversInput {
            users,
            groups,
            selectors,
            required: spec.required,
            allow_delegation: spec.allow_delegation,
        };
        let project = *proof.id();
        let ctx = change_context(request, proof.principal(), user);
        let stored = forge_db::deploy::put_approvers_decided(
            &self.db,
            &ctx,
            project,
            tier,
            &input,
            |facts| reconfirm(proof, facts),
        )
        .await?;
        tracing::info!(
            target: "forge_deploy",
            event = "forge.deploy.approvers_set",
            request_id = %request.request_id,
            project_id = project.get(),
            environment = tier.as_str(),
            users = stored.users.len(),
            groups = stored.groups.len(),
            selectors = stored.selectors.len(),
            approvals_required = stored.required,
            allow_delegation = stored.allow_delegation,
            user_id = user.get(),
            "deploy environment approvers set"
        );
        Ok(stored)
    }

    /// The names of approver users and groups (display only; an id
    /// without a name is absent from the answer).
    pub async fn approver_names(
        &self,
        users: &[UserId],
        groups: &[forge_core::GroupId],
    ) -> Result<forge_db::review::ApproverNames, ForgeError> {
        forge_db::review::approver_names(&self.db, users, groups).await
    }

    /// The delegator `name` of a deployment approval by `caller`: an
    /// active user, `None` when the name does not resolve (a fact for
    /// `authorize()`: [`ApproverFact::unknown_delegator`]). Naming oneself
    /// is `FG-WEB-0006`.
    pub async fn delegator(
        &self,
        name: &str,
        caller: UserId,
    ) -> Result<Option<UserId>, ForgeError> {
        let found = forge_db::review::owners(&self.db, &[name.to_owned()], &[]).await?;
        let id = found
            .users
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, id)| *id);
        if id == Some(caller) {
            return Err(param("on_behalf_of", "self"));
        }
        Ok(id)
    }

    /// Whether `caller` (approving for `on_behalf_of`, when given) matches
    /// the approvers of `deployment`'s environment (module docs), with the
    /// delegator's role now. A named delegator without an active delegation
    /// to the caller is the fact `delegation_refused = no_delegation`
    /// (`authorize()` refuses with `FG-REVIEW-0033`; checked again under
    /// the approval's lock).
    pub async fn approver_gate(
        &self,
        project: ProjectId,
        deployment: &StoredDeployment,
        caller: UserId,
        on_behalf_of: Option<UserId>,
    ) -> Result<GateFacts, ForgeError> {
        let who = on_behalf_of.unwrap_or(caller);
        let delegator_role = match on_behalf_of {
            Some(d) => {
                forge_db::roles::load_project_roles(&self.db, project, d)
                    .await?
                    .native
            }
            None => None,
        };
        let org = forge_db::org_master::person_orgs(&self.db, &[who])
            .await?
            .remove(&who)
            .unwrap_or_else(|| PersonOrg::Unknown("no_snapshot".to_owned()));
        let Some(env) =
            forge_db::deploy::environment_approvers(&self.db, project, deployment.tier).await?
        else {
            return Err(ForgeError::new(ErrorCode::NotFound)
                .with_context("resource", "deploy_environment")
                .with_context("environment", deployment.tier.as_str()));
        };
        let mut refused = None;
        if let Some(delegator) = on_behalf_of {
            if !env.unrestricted()
                && env.allow_delegation
                && !forge_db::review::active_delegation(&self.db, delegator, caller).await?
            {
                refused = Some("no_delegation");
            }
        }
        let facts = |gate| GateFacts {
            fact: ApproverFact {
                gate,
                on_behalf_of,
                delegator_role,
                delegation_refused: refused,
            },
            org: org.clone(), // clone: each answer owns it
            version: env.version.clone(),
        };
        if on_behalf_of.is_some() && (env.unrestricted() || !env.allow_delegation) {
            return Ok(facts(ApproverGate::NotListed("delegation_off")));
        }
        if env.unrestricted() {
            return Ok(facts(ApproverGate::Unrestricted));
        }
        if env.users.contains(&who) {
            return Ok(facts(ApproverGate::Named));
        }
        if !env.groups.is_empty() {
            let members =
                forge_db::review::group_members(&self.db, &env.groups, usize::MAX).await?;
            if members.iter().any(|(_, u)| *u == who) {
                return Ok(facts(ApproverGate::Matched));
            }
        }
        if env.selectors.is_empty() {
            return Ok(facts(ApproverGate::NotListed("not_listed")));
        }
        let requester = deployment.requested_by.map(UserId::new);
        // The people who can never approve it (the requester and everyone
        // behind the SHA) are no candidates: escalation looks past them
        // (review of ADR-0038 step C).
        let exclude: Vec<UserId> = requester
            .into_iter()
            .chain(deployment.change_authors.iter().copied().map(UserId::new))
            .collect();
        let resolution = forge_db::org_master::resolve_selectors(
            &self.db,
            project,
            requester,
            &env.selectors,
            Role::Maintainer,
            &exclude,
        )
        .await?;
        let mut unresolved = false;
        for s in &env.selectors {
            match (resolution.target(s), org.known()) {
                (Some(Ok(target)), Some(snapshot)) if matches(&s.qualifier, target, snapshot) => {
                    return Ok(facts(ApproverGate::Matched));
                }
                (Some(Ok(_)), Some(_)) => {}
                _ => unresolved = true,
            }
        }
        Ok(facts(ApproverGate::NotListed(if unresolved {
            "selector_unresolved"
        } else {
            "not_listed"
        })))
    }
}
