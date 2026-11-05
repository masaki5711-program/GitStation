//! Acting for a person (M3 S2, ADR-0025): forge-web's authentication asks
//! [`ActAs::resolve`] for the parts of an act-as principal, its loaders ask
//! [`ActAs::via`] for the service account's facts on a project, and the
//! confirmation and claim routes go through [`ActAs::issue`] and
//! [`ActAs::claim`] on their proofs. The SQL is forge-db's
//! (`forge_db::act_as`); the decisions are `authorize()`'s.
//!
//! Order of [`ActAs::resolve`] (ADR-0025 §2, §1, §5.3), each before the
//! next runs: the token (`forge_auth::act_as_admission`: a service
//! account's, `act_as`, not an agent's), its integration
//! (`act_as_no_integration`, `integration_disabled`), the keys
//! (`act_as_not_configured`), the person (`FG-INT-0006`), the assertion
//! (`FG-AUTH-0020`). Nothing is written.

use std::sync::Arc;
use std::time::Duration;

use forge_auth::{
    act_as_admission, ActAsConfirmCtx, ActAsCredential, AssertionFacts, Authorized, Credential,
    IntegrationFeedCtx, LoadedConfirmIntegration, LoadedIntegrationFeed, Principal, Scope, ViaView,
};
use forge_core::act_as::{ActAsKeys, ActAsTarget, AssertionToken, Intent};
use forge_core::{IntegrationId, MergeRequestId, ProjectId, RefName, RequestId, UserId};
use forge_db::act_as as db;
use forge_db::change::{Actor, ChangeContext};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

/// The act-as service (`forge serve` builds one; forge-web's `AppState`).
pub struct ActAs {
    db: Db,
    /// `auth.act_as_hmac_keys_file` (`None`: not configured — every act-as
    /// request is `FG-AUTH-0003 reason=act_as_not_configured`).
    keys: Option<Arc<ActAsKeys>>,
}

impl std::fmt::Debug for ActAs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActAs")
            .field("keys", &self.keys.is_some())
            .finish_non_exhaustive()
    }
}

/// What forge-web builds the act-as principal from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActAsParts {
    /// The one active person `Forge-Act-As` named.
    pub person: UserId,
    pub credential: ActAsCredential,
}

/// What a claim answers, once (ADR-0025 §5): the assertion's secret
/// exists only in `assertion` (call [`AssertionToken::reveal`] to answer).
#[derive(Debug)]
pub struct ClaimAnswer {
    pub assertion: AssertionToken,
    pub user: UserId,
    /// Only for a token with `user_employee_number`.
    pub employee_number: Option<String>,
    pub intent: Intent,
    pub project: ProjectId,
    pub iid: i64,
    pub head_sha: String,
    pub target_ref: String,
    pub expires_at: String,
}

fn refused(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Forbidden).with_context("reason", reason)
}

/// The change context of a request (`actor`, its id, the token).
fn context(actor: Actor, request_id: &RequestId, credential: Option<i64>) -> ChangeContext {
    let mut ctx = ChangeContext::new(actor, request_id.clone());
    ctx.credential_id = credential;
    ctx
}

impl ActAs {
    pub fn new(db: Db, keys: Option<ActAsKeys>) -> Self {
        Self {
            db,
            keys: keys.map(Arc::new),
        }
    }

    /// Whether the key file is configured.
    pub fn keys_configured(&self) -> bool {
        self.keys.is_some()
    }

    fn keys(&self) -> Result<&ActAsKeys, ForgeError> {
        self.keys
            .as_deref()
            .ok_or_else(|| refused("act_as_not_configured"))
    }

    /// The parts of an act-as principal: `token` is the principal its
    /// `Authorization` authenticated as (`token_id` its token's id),
    /// `raw` the `Forge-Act-As` value as sent (only its keyed HMAC is
    /// kept), `assertion` the parsed `Forge-Assertion`. See the module
    /// documentation for the order.
    pub async fn resolve(
        &self,
        token: &Principal,
        token_id: Option<i64>,
        raw: &str,
        target: &ActAsTarget,
        assertion: Option<&AssertionToken>,
    ) -> Result<ActAsParts, ForgeError> {
        act_as_admission(token)?;
        let (Some(via), Some(via_token), Credential::Token(scopes)) =
            (token.user(), token_id, token.credential())
        else {
            return Err(refused("act_as_requires_service_account"));
        };
        let Some(of) = db::integration_of(&self.db, via).await? else {
            return Err(refused("act_as_no_integration"));
        };
        if !of.enabled {
            return Err(refused("integration_disabled")
                .with_context("integration_id", of.integration.get()));
        }
        let keys = self.keys()?;
        let person = db::resolve_target(&self.db, target)
            .await
            .map_err(|e| e.with_context("integration_id", of.integration.get()))?;
        let (claimed_key_id, claimed_mac) = keys.claimed(raw, None)?;
        let assertion = match assertion {
            None => None,
            Some(a) => {
                let v = db::verify_assertion(&self.db, keys, a, of.integration, person).await?;
                Some(AssertionFacts {
                    id: v.id,
                    public_id: v.public_id,
                    intent: v.intent,
                    project: v.project,
                    merge_request: v.merge_request,
                    head_sha: v.head_sha,
                    target_ref: v.target_ref,
                    target_sha: v.target_sha,
                })
            }
        };
        Ok(ActAsParts {
            person,
            credential: ActAsCredential {
                via,
                via_kind: token.account_kind(),
                via_token,
                via_scopes: scopes.clone(), // clone: the credential owns them
                via_agent: token.is_agent(),
                integration: of.integration,
                integration_enabled: of.enabled,
                assertion,
                claimed_mac,
                claimed_key_id,
            },
        })
    }

    /// The service account's facts on `project` for an act-as principal
    /// (`None` for any other: the route decides on the plain context).
    pub async fn via(
        &self,
        principal: &Principal,
        project: ProjectId,
    ) -> Result<Option<ViaView>, ForgeError> {
        let Some(c) = principal.act_as_credential() else {
            return Ok(None);
        };
        let facts = db::via_facts(&self.db, c.integration, c.via, project).await?;
        Ok(Some(ViaView {
            role: facts.role,
            bound: facts.bound,
        }))
    }

    /// Reject the verified assertion of a request that did not consume it
    /// (ADR-0025 §5.3: any refusal or failure after the verification), in
    /// its own transaction, as the request's actor. `true` when rejected
    /// now; `false` when it was consumed by the action (or already ended).
    pub async fn reject_unused(
        &self,
        principal: &Principal,
        request_id: &RequestId,
        reason: &'static str,
    ) -> Result<bool, ForgeError> {
        let Some(c) = principal.act_as_credential() else {
            return Ok(false);
        };
        let (Some(a), Some(person)) = (c.assertion.as_ref(), principal.user()) else {
            return Ok(false);
        };
        let actor = Actor::UserViaServiceAccount {
            user: person,
            via: c.via,
            credential_id: c.via_token,
            act_as_claimed: c.claimed_mac.clone(), // clone: the audit row owns it
            act_as_key_id: c.claimed_key_id,
            via_assertion_id: Some(a.public_id.clone()),
        };
        let ctx = context(actor, request_id, Some(c.via_token));
        db::reject(&self.db, &ctx, a.id, reason).await
    }

    /// The confirmation's integration as loaded: enabled, bound to
    /// `project` (`None`: absent — the same 404 as an invisible merge
    /// request).
    pub async fn confirm_integration(
        &self,
        integration: IntegrationId,
        project: ProjectId,
    ) -> Result<Option<LoadedConfirmIntegration>, ForgeError> {
        let Some(found) = forge_db::integrations::get(&self.db, integration).await? else {
            return Ok(None);
        };
        let bindings = forge_db::integrations::project_bindings(&self.db, project).await?;
        Ok(Some(LoadedConfirmIntegration {
            integration: found.id,
            enabled: found.enabled,
            bound: bindings.iter().any(|b| b.integration == integration),
        }))
    }

    /// The claim route's loader: the integration of the path.
    pub async fn claim_target(
        &self,
        integration: IntegrationId,
    ) -> Result<Option<LoadedIntegrationFeed>, ForgeError> {
        Ok(forge_db::integrations::get(&self.db, integration)
            .await?
            .map(|i| LoadedIntegrationFeed {
                integration: i.id,
                service_account: i.service_account,
                enabled: i.enabled,
            }))
    }

    /// `POST /-/confirm/projects/{id}/merge-requests/{iid}/assertions`
    /// (`ActAsAssertionIssue`, on its proof): write the `pending`
    /// assertion for the person (no secret yet). `merge_request` is the
    /// merge request's internal id, `head_sha` the sha the person saw,
    /// `target_sha` the target's tip as read for the screen.
    #[allow(clippy::too_many_arguments)] // the binding, field by field
    pub async fn issue(
        &self,
        proof: &Authorized<ActAsConfirmCtx>,
        request_id: &RequestId,
        session: Option<i64>,
        merge_request: MergeRequestId,
        head_sha: &str,
        target_sha: &str,
        state: &str,
    ) -> Result<i64, ForgeError> {
        let (project, _, integration) = *proof.id();
        let person = proof.principal().user().ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_without_user")
        })?;
        self.keys()?;
        let target: RefName = proof.ctx().merge_request().target_branch.clone(); // clone: the binding owns it
        let ctx = context(Actor::User(person), request_id, session);
        db::issue(
            &self.db,
            &ctx,
            &db::NewAssertion {
                integration,
                user: person,
                intent: proof.ctx().intent(),
                project,
                merge_request,
                head_sha,
                target_ref: &target,
                target_sha,
                state,
                lifetime: db::LIFETIME,
            },
        )
        .await
    }

    /// `POST /api/v1/integrations/{id}/act-as/claim` (`ActAsAssertionClaim`,
    /// on its proof): claim the pending assertion of `state` — the secret
    /// is created now and answered once.
    pub async fn claim(
        &self,
        proof: &Authorized<IntegrationFeedCtx>,
        request_id: &RequestId,
        token_id: Option<i64>,
        state: &str,
    ) -> Result<ClaimAnswer, ForgeError> {
        let integration = *proof.id();
        let via = proof.principal().user().ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_without_user")
        })?;
        let keys = self.keys()?;
        let ctx = context(Actor::ServiceAccount(via), request_id, token_id);
        let claimed = db::claim(&self.db, &ctx, keys, integration, state).await?;
        let employee = match proof.principal().credential() {
            Credential::Token(scopes) => scopes.contains(&Scope::UserEmployeeNumber),
            Credential::Session | Credential::ActAs(_) => false,
        };
        Ok(ClaimAnswer {
            assertion: claimed.token,
            user: claimed.user,
            employee_number: claimed.employee_number.filter(|_| employee),
            intent: claimed.intent,
            project: claimed.project,
            iid: claimed.iid,
            head_sha: claimed.head_sha,
            target_ref: claimed.target_ref,
            expires_at: claimed.expires_at,
        })
    }

    /// Reject the expired pending assertions (the worker, as `system`).
    pub async fn sweep_expired(&self, request_id: &RequestId) -> Result<u64, ForgeError> {
        let ctx = context(Actor::System, request_id, None);
        db::sweep_expired(&self.db, &ctx, db::SWEEP_BATCH).await
    }
}

/// How long the worker waits between two sweeps of expired assertions.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60);
