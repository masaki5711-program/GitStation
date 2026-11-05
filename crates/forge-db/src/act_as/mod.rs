//! Acting for a person (M3 S2, ADR-0025): the person a `Forge-Act-As`
//! value names, the integration of a service account, and the assertions
//! of `forge.act_as_assertions` through their states — `pending` (issued
//! by the person's session: the binding and the state's hash, **no
//! secret**) → `claimed` (the integration's claim: the secret created
//! here, its keyed MAC stored, the secret returned once and never kept) →
//! `consumed` (in the action's own transaction, [`consume_on`]) or
//! `rejected` (verified but not used, [`reject`]; an expired pending row,
//! [`sweep_expired`]).
//!
//! [`verify_assertion`] is the only function that makes a
//! [`VerifiedAssertion`]: the row by its public id (a dummy MAC when there
//! is none, so a miss costs what a hit does), the key and the MAC in
//! constant time, the state, the expiry by the database's clock, the
//! integration and the person. Every refusal is `FG-AUTH-0020` with its
//! `reason`; nothing here logs (the boundary does, once).

use std::time::Duration;

use forge_core::act_as::{
    assertion_invalid, ActAsKeys, ActAsTarget, AssertionBinding, AssertionToken, Intent,
    TargetRefusal,
};
use forge_core::ids::{IntegrationId, MergeRequestId, ProjectId, UserId};
use forge_core::{RefName, Role};
use forge_errors::{ErrorCode, ForgeError};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgConnection;

use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::roles::{self, RoleAnchor};
use crate::{query_error, Db};

/// The audit rows' resource type (tables.toml `audit_resource`).
pub const RESOURCE: &str = "act_as_assertion";
/// How long an assertion lives (ADR-0025 §5.1); the CHECK caps it too.
pub const LIFETIME: Duration = Duration::from_secs(300);
/// A state is 128 bits or more of kanflow's randomness: at least 22
/// characters of `[A-Za-z0-9_-]` (base64url), at most 128.
pub const MIN_STATE_CHARS: usize = 22;
pub const MAX_STATE_CHARS: usize = 128;
/// The cleanup's batch of expired pending rows.
pub const SWEEP_BATCH: i64 = 500;

/// The integration a service account belongs to (one per account).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntegrationOf {
    pub integration: IntegrationId,
    pub enabled: bool,
}

/// The integration whose service account `via` is (`None`: none).
pub async fn integration_of(db: &Db, via: UserId) -> Result<Option<IntegrationOf>, ForgeError> {
    let row: Option<(i64, bool)> =
        sqlx::query_as("SELECT id, enabled FROM forge.integrations WHERE service_account_id = $1")
            .bind(via.get())
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error("act_as.integration_of", e))?;
    Ok(row.map(|(id, enabled)| IntegrationOf {
        integration: IntegrationId::new(id),
        enabled,
    }))
}

/// The person `target` names: exactly one active human, or `FG-INT-0006`
/// with `reason` (`unknown`, `ambiguous`, `not_human`, `disabled`). An
/// employee number is matched exactly; two rows (the unique index broken
/// or dropped) are `ambiguous` and nobody is acted for.
pub async fn resolve_target(db: &Db, target: &ActAsTarget) -> Result<UserId, ForgeError> {
    let rows: Vec<(i64, String, String)> = match target {
        ActAsTarget::User(id) => {
            sqlx::query_as("SELECT id, kind, state FROM forge.users WHERE id = $1")
                .bind(id.get())
                .fetch_all(db.write_pool())
                .await
        }
        ActAsTarget::Employee(number) => {
            sqlx::query_as(
                "SELECT id, kind, state FROM forge.users WHERE employee_number = $1 \
                 ORDER BY id LIMIT 2",
            )
            .bind(number.as_str())
            .fetch_all(db.write_pool())
            .await
        }
    }
    .map_err(|e| query_error("act_as.resolve_target", e))?;
    let refused = |r: TargetRefusal| r.error().with_context("form", target.form());
    match rows.as_slice() {
        [] => Err(refused(TargetRefusal::Unknown)),
        [(id, kind, state)] => {
            if kind != "human" {
                return Err(refused(TargetRefusal::NotHuman));
            }
            if state != "active" {
                return Err(refused(TargetRefusal::Disabled));
            }
            Ok(UserId::new(*id))
        }
        [_, _, ..] => Err(refused(TargetRefusal::Ambiguous)),
    }
}

/// The integration's service account on a project (ADR-0025 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViaFacts {
    /// Its native role (a binding makes it a Reporter).
    pub role: Option<Role>,
    /// The project is bound to the integration.
    pub bound: bool,
}

const BOUND: &str = "SELECT EXISTS (SELECT 1 FROM forge.integration_bindings \
     WHERE integration_id = $1 AND project_id = $2)";

/// [`ViaFacts`] on the read pool (a route's loader).
pub async fn via_facts(
    db: &Db,
    integration: IntegrationId,
    via: UserId,
    project: ProjectId,
) -> Result<ViaFacts, ForgeError> {
    let bound: bool = sqlx::query_scalar(BOUND)
        .bind(integration.get())
        .bind(project.get())
        .fetch_one(db.pool().executor())
        .await
        .map_err(|e| crate::read_query_error("act_as.via_bound", e))?;
    let role = match roles::load_project_roles(db, project, via).await {
        Ok(r) => r.native,
        // The project went away: nothing to see.
        Err(e) if e.code() == ErrorCode::NotFound => None,
        Err(e) => return Err(e),
    };
    Ok(ViaFacts { role, bound })
}

/// [`ViaFacts`] inside a write transaction, the binding and the service
/// account's memberships locked `FOR SHARE` (an unbind or a role change
/// waits for the action).
pub(crate) async fn via_on(
    conn: &mut PgConnection,
    integration: IntegrationId,
    via: UserId,
    project: ProjectId,
) -> Result<ViaFacts, ForgeError> {
    let bound: Option<i64> = sqlx::query_scalar(
        "SELECT project_id FROM forge.integration_bindings \
         WHERE integration_id = $1 AND project_id = $2 FOR SHARE",
    )
    .bind(integration.get())
    .bind(project.get())
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| query_error("act_as.via_bound_lock", e))?;
    let role = roles::lock_roles(conn, RoleAnchor::Project(project), via)
        .await?
        .native;
    Ok(ViaFacts {
        role,
        bound: bound.is_some(),
    })
}

/// A state's shape: `[A-Za-z0-9_-]`, [`MIN_STATE_CHARS`]..=[`MAX_STATE_CHARS`].
pub fn state_ok(state: &str) -> bool {
    (MIN_STATE_CHARS..=MAX_STATE_CHARS).contains(&state.len())
        && state
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

fn state_hash(state: &str) -> [u8; 32] {
    Sha256::digest(state.as_bytes()).into()
}

/// What a confirmation binds an assertion to (ADR-0025 §5.1).
#[derive(Debug, Clone)]
pub struct NewAssertion<'a> {
    pub integration: IntegrationId,
    pub user: UserId,
    pub intent: Intent,
    pub project: ProjectId,
    pub merge_request: MergeRequestId,
    pub head_sha: &'a str,
    pub target_ref: &'a RefName,
    pub target_sha: &'a str,
    /// Kanflow's state (never stored: its SHA-256 only).
    pub state: &'a str,
    /// [`LIFETIME`] in production (at most; tests ask for less).
    pub lifetime: Duration,
}

fn audit(
    action: &'static str,
    id: i64,
    project: ProjectId,
    details: serde_json::Value,
) -> AuditEntry {
    AuditEntry {
        action,
        outcome: Outcome::Succeeded,
        resource_type: RESOURCE,
        resource_id: Some(id),
        project_id: Some(project),
        error_code: None,
        details,
    }
}

/// Issue a `pending` assertion for the person of `ctx` (their session):
/// the binding and the state's hash, no secret, no public id, no MAC; one
/// `act_as_assertion.issued` audit row. The merge request is locked
/// `FOR SHARE` and must still have `head_sha` and `target_ref`
/// (`FG-REVIEW-0008 reason=stale_view` otherwise). A malformed state is
/// `FG-WEB-0006 param=state`.
pub async fn issue(
    db: &Db,
    ctx: &ChangeContext,
    new: &NewAssertion<'_>,
) -> Result<i64, ForgeError> {
    if !state_ok(new.state) {
        return Err(ForgeError::new(ErrorCode::RequestInvalid)
            .with_context("reason", "param_invalid")
            .with_context("param", "state")
            .with_context("rule", "format"));
    }
    let lifetime = new.lifetime.min(LIFETIME).as_secs_f64().max(0.001);
    let hash = state_hash(new.state);
    let request = ctx.request_id.as_str().to_owned();
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let (mr, project) = (new.merge_request, new.project);
        let current: Option<(Option<String>, String)> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "SELECT head_sha, target_branch FROM forge.merge_requests \
                     WHERE id = $1 AND project_id = $2 FOR SHARE",
                )
                .bind(mr.get())
                .bind(project.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("act_as.issue.merge_request", e))
            })
            .await?;
        let Some((head, target)) = current else {
            return Err(ForgeError::new(ErrorCode::NotFound)
                .with_context("resource", "merge_request")
                .with_context("reason", "absent"));
        };
        if head.as_deref() != Some(new.head_sha) || target != new.target_ref.as_str() {
            return Err(ForgeError::new(ErrorCode::HeadChanged)
                .with_context("reason", "stale_view")
                .with_context("merge_request_id", mr.get()));
        }
        let (hash_ref, request_ref) = (&hash, &request);
        let id: i64 = tx
            .step(|c| async move {
                sqlx::query_scalar(
                    "INSERT INTO forge.act_as_assertions (integration_id, user_id, intent, \
                     project_id, merge_request_id, head_sha, target_ref, target_sha, state_hash, \
                     expires_at, issued_request_id) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, \
                     now() + make_interval(secs => $10), $11) RETURNING id",
                )
                .bind(new.integration.get())
                .bind(new.user.get())
                .bind(new.intent.as_str())
                .bind(project.get())
                .bind(mr.get())
                .bind(new.head_sha)
                .bind(new.target_ref.as_str())
                .bind(new.target_sha)
                .bind(hash_ref.as_slice())
                .bind(lifetime)
                .bind(request_ref.as_str())
                .fetch_one(c)
                .await
                .map_err(|e| query_error("act_as.issue", e))
            })
            .await?;
        let details = serde_json::json!({
            "intent": new.intent.as_str(),
            "integration_id": new.integration.get(),
            "merge_request_id": mr.get(),
            "head_sha": new.head_sha,
        });
        let record = ChangeRecord {
            audit: audit("act_as_assertion.issued", id, project, details),
            affected: Vec::new(),
            events: Vec::new(),
        };
        Ok(Settle::Commit(record, id))
    }
    .await;
    tx.settle("act_as_assertion.issued", outcome).await
}

/// What a claim returns, once: the assertion (its secret exists only in
/// this value) and what it is bound to.
#[derive(Debug)]
pub struct Claimed {
    pub token: AssertionToken,
    pub user: UserId,
    /// The person's employee number (the caller shows it only with the
    /// `user_employee_number` scope).
    pub employee_number: Option<String>,
    pub intent: Intent,
    pub project: ProjectId,
    pub merge_request: MergeRequestId,
    pub iid: i64,
    pub head_sha: String,
    pub target_ref: String,
    /// RFC 3339, UTC.
    pub expires_at: String,
}

type ClaimRow = (
    i64,
    String,
    bool,
    i64,
    String,
    i64,
    i64,
    String,
    String,
    String,
    i64,
    String,
    Option<String>,
    Option<i64>,
);

fn intent_of(raw: &str, id: i64) -> Result<Intent, ForgeError> {
    Intent::parse(raw).ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "unknown_intent")
            .with_context("assertion_id", id)
    })
}

/// Claim the pending assertion of `integration` whose state is `state`
/// (ADR-0025 §5.1): in one transaction, the row `FOR UPDATE`, then the
/// public id and the secret created (OS RNG), the MAC with the active key,
/// the row `claimed` with one `act_as_assertion.claimed` audit row; the
/// secret leaves only in the answer. `FG-AUTH-0020` `claim_unknown` (no
/// row of this integration with the state; a malformed state too),
/// `claim_repeated` (not pending any more: nothing is returned again),
/// `claim_expired` (the row stays pending; the cleanup rejects it).
pub async fn claim(
    db: &Db,
    ctx: &ChangeContext,
    keys: &ActAsKeys,
    integration: IntegrationId,
    state: &str,
) -> Result<Claimed, ForgeError> {
    if !state_ok(state) {
        return Err(assertion_invalid("claim_unknown"));
    }
    let hash = state_hash(state);
    let request = ctx.request_id.as_str().to_owned();
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let hash_ref = &hash;
        let row: Option<ClaimRow> = tx
            .step(|c| async move {
                sqlx::query_as(concat!(
                    "SELECT a.id, a.state, a.expires_at <= now(), a.user_id, a.intent, ",
                    "a.project_id, a.merge_request_id, a.head_sha, a.target_ref, a.target_sha, ",
                    "(extract(epoch FROM a.expires_at) * 1000000)::bigint, ",
                    "to_char(a.expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), ",
                    "u.employee_number, m.iid ",
                    "FROM forge.act_as_assertions a JOIN forge.users u ON u.id = a.user_id ",
                    "LEFT JOIN forge.merge_requests m ON m.id = a.merge_request_id ",
                    "WHERE a.state_hash = $1 AND a.integration_id = $2 ",
                    "ORDER BY a.id DESC LIMIT 1 FOR UPDATE OF a"
                ))
                .bind(hash_ref.as_slice())
                .bind(integration.get())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("act_as.claim.lock", e))
            })
            .await?;
        let Some((
            id,
            row_state,
            expired,
            user,
            intent,
            project,
            mr,
            head_sha,
            target_ref,
            target_sha,
            micros,
            expires_at,
            employee_number,
            iid,
        )) = row
        else {
            return Err(assertion_invalid("claim_unknown"));
        };
        if row_state != "pending" {
            return Err(assertion_invalid("claim_repeated").with_context("assertion_id", id));
        }
        if expired {
            return Err(assertion_invalid("claim_expired").with_context("assertion_id", id));
        }
        let Some(iid) = iid else {
            // The merge request was purged since the confirmation.
            return Err(assertion_invalid("claim_unknown").with_context("assertion_id", id));
        };
        let intent = intent_of(&intent, id)?;
        let token = AssertionToken::generate()?;
        let binding = AssertionBinding {
            public_id: token.public_id(),
            integration_id: integration.get(),
            user_id: user,
            intent,
            project_id: project,
            merge_request_id: mr,
            head_sha: &head_sha,
            target_ref: &target_ref,
            target_sha: &target_sha,
            expires_at_micros: micros,
        };
        let (kid, mac) = keys.seal(token.secret(), &binding)?;
        let (public_id, request_ref) = (token.public_id().to_owned(), &request);
        let public_ref = &public_id;
        tx.step(|c| async move {
            sqlx::query(
                "UPDATE forge.act_as_assertions SET state = 'claimed', public_id = $2, mac = $3, \
                 key_id = $4, claimed_at = now(), claimed_request_id = $5 WHERE id = $1",
            )
            .bind(id)
            .bind(public_ref.as_str())
            .bind(mac.as_slice())
            .bind(kid)
            .bind(request_ref.as_str())
            .execute(c)
            .await
            .map_err(|e| query_error("act_as.claim", e))
        })
        .await?;
        let project = ProjectId::new(project);
        let details = serde_json::json!({
            "public_id": public_id,
            "intent": intent.as_str(),
            "integration_id": integration.get(),
            "merge_request_id": mr,
            "head_sha": head_sha,
        });
        let record = ChangeRecord {
            audit: audit("act_as_assertion.claimed", id, project, details),
            affected: Vec::new(),
            events: Vec::new(),
        };
        Ok(Settle::Commit(
            record,
            Claimed {
                token,
                user: UserId::new(user),
                employee_number,
                intent,
                project,
                merge_request: MergeRequestId::new(mr),
                iid,
                head_sha,
                target_ref,
                expires_at,
            },
        ))
    }
    .await;
    tx.settle("act_as_assertion.claimed", outcome).await
}

/// A claimed assertion that [`verify_assertion`] verified: what it is
/// bound to. Only that function makes one (`non_exhaustive`: no struct
/// literal outside this crate).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct VerifiedAssertion {
    pub id: i64,
    pub public_id: String,
    pub intent: Intent,
    pub project: ProjectId,
    pub merge_request: MergeRequestId,
    pub head_sha: String,
    pub target_ref: RefName,
    pub target_sha: String,
}

type VerifyRow = (
    i64,
    String,
    Option<Vec<u8>>,
    Option<i16>,
    i64,
    i64,
    String,
    i64,
    i64,
    String,
    String,
    String,
    i64,
    bool,
);

/// Verify `presented` for a request of `integration` acting for `person`
/// (ADR-0025 §5.3, the forge-db rows): `unknown`, `key_retired`,
/// `mac_mismatch`, `consumed` (used or rejected before), `expired` (by
/// the database's clock), `wrong_integration`, `wrong_user` — each
/// `FG-AUTH-0020` with `assertion_public_id` (the public id is safe).
pub async fn verify_assertion(
    db: &Db,
    keys: &ActAsKeys,
    presented: &AssertionToken,
    integration: IntegrationId,
    person: UserId,
) -> Result<VerifiedAssertion, ForgeError> {
    let public_id = presented.public_id().to_owned();
    let row: Option<VerifyRow> = sqlx::query_as(concat!(
        "SELECT id, state, mac, key_id, integration_id, user_id, intent, project_id, ",
        "merge_request_id, head_sha, target_ref, target_sha, ",
        "(extract(epoch FROM expires_at) * 1000000)::bigint, expires_at <= now() ",
        "FROM forge.act_as_assertions WHERE public_id = $1"
    ))
    .bind(public_id.as_str())
    .fetch_optional(db.write_pool())
    .await
    .map_err(|e| query_error("act_as.verify", e))?;
    let refused =
        |reason| assertion_invalid(reason).with_context("assertion_public_id", public_id.clone());
    let Some((
        id,
        state,
        mac,
        key_id,
        row_integration,
        user,
        intent,
        project,
        mr,
        head_sha,
        target_ref,
        target_sha,
        micros,
        expired,
    )) = row
    else {
        keys.dummy_verify(presented.secret());
        return Err(refused("unknown"));
    };
    let (Some(mac), Some(kid)) = (mac, key_id) else {
        // A pending row has no public id; reaching one is a broken row.
        keys.dummy_verify(presented.secret());
        return Err(refused("unknown"));
    };
    let intent = intent_of(&intent, id)?;
    let binding = AssertionBinding {
        public_id: &public_id,
        integration_id: row_integration,
        user_id: user,
        intent,
        project_id: project,
        merge_request_id: mr,
        head_sha: &head_sha,
        target_ref: &target_ref,
        target_sha: &target_sha,
        expires_at_micros: micros,
    };
    keys.verify(kid, presented.secret(), &binding, &mac)
        .map_err(|r| refused(r.as_str()))?;
    if state != "claimed" {
        return Err(refused("consumed").with_context("state", state));
    }
    if expired {
        return Err(refused("expired"));
    }
    if row_integration != integration.get() {
        return Err(refused("wrong_integration"));
    }
    if user != person.get() {
        return Err(refused("wrong_user"));
    }
    let target_ref = RefName::parse(&target_ref).map_err(|_| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "stored_target_ref_invalid")
            .with_context("assertion_id", id)
    })?;
    Ok(VerifiedAssertion {
        id,
        public_id,
        intent,
        project: ProjectId::new(project),
        merge_request: MergeRequestId::new(mr),
        head_sha,
        target_ref,
        target_sha,
    })
}

/// The assertion an action consumes in its own transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActAsUse {
    pub assertion: i64,
    pub public_id: String,
    pub integration: IntegrationId,
    /// The service account (its facts are re-read under the lock).
    pub via: UserId,
}

/// Consume `using` inside the action's transaction (ADR-0025 §5.3, the
/// ChangeTx row): exactly one claimed, unexpired row, or `FG-AUTH-0020
/// reason=consumed` (a concurrent second use) and the action rolls back.
/// The `act_as_assertion.consumed` audit row to add to the change.
pub(crate) async fn consume_on(
    tx: &mut ChangeTx,
    using: &ActAsUse,
    request: &str,
) -> Result<AuditEntry, ForgeError> {
    let id = using.assertion;
    let row: Option<(i64, String, i64, String)> = tx
        .step(|c| async move {
            sqlx::query_as(
                "UPDATE forge.act_as_assertions SET state = 'consumed', finished_at = now(), \
                 finished_request_id = $2 WHERE id = $1 AND state = 'claimed' \
                 AND expires_at > now() \
                 RETURNING project_id, intent, merge_request_id, head_sha",
            )
            .bind(id)
            .bind(request)
            .fetch_optional(c)
            .await
            .map_err(|e| query_error("act_as.consume", e))
        })
        .await?;
    let Some((project, intent, mr, head_sha)) = row else {
        return Err(assertion_invalid("consumed")
            .with_context("assertion_public_id", using.public_id.clone()));
    };
    Ok(audit(
        "act_as_assertion.consumed",
        id,
        ProjectId::new(project),
        serde_json::json!({
            "public_id": using.public_id,
            "intent": intent,
            "integration_id": using.integration.get(),
            "merge_request_id": mr,
            "head_sha": head_sha,
        }),
    ))
}

/// Reject a claimed assertion that was verified but not used (ADR-0025
/// §5.3: any refusal or failure after the verification), in its own
/// transaction: `true` when this call rejected it, `false` when it was
/// no longer claimed (consumed by the action, or rejected before).
pub async fn reject(
    db: &Db,
    ctx: &ChangeContext,
    assertion: i64,
    reason: &'static str,
) -> Result<bool, ForgeError> {
    let request = ctx.request_id.as_str().to_owned();
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let request_ref = &request;
        let row: Option<(i64, String, i64, i64, String, Option<String>)> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "UPDATE forge.act_as_assertions SET state = 'rejected', reject_reason = $2, \
                     finished_at = now(), finished_request_id = $3 \
                     WHERE id = $1 AND state = 'claimed' \
                     RETURNING project_id, intent, integration_id, merge_request_id, head_sha, \
                     public_id",
                )
                .bind(assertion)
                .bind(reason)
                .bind(request_ref.as_str())
                .fetch_optional(c)
                .await
                .map_err(|e| query_error("act_as.reject", e))
            })
            .await?;
        let Some((project, intent, integration, mr, head_sha, public_id)) = row else {
            return Ok(Settle::Discard(false));
        };
        let details = serde_json::json!({
            "public_id": public_id,
            "intent": intent,
            "integration_id": integration,
            "merge_request_id": mr,
            "head_sha": head_sha,
            "reject_reason": reason,
        });
        let record = ChangeRecord {
            audit: audit(
                "act_as_assertion.rejected",
                assertion,
                ProjectId::new(project),
                details,
            ),
            affected: Vec::new(),
            events: Vec::new(),
        };
        Ok(Settle::Commit(record, true))
    }
    .await;
    tx.settle("act_as_assertion.rejected", outcome).await
}

/// Reject at most `max` pending assertions past their expiry
/// (`reject_reason = expired`; the claim refused them already), one audit
/// row each, as `ctx` (the worker's `system`). The number rejected.
pub async fn sweep_expired(db: &Db, ctx: &ChangeContext, max: i64) -> Result<u64, ForgeError> {
    let request = ctx.request_id.as_str().to_owned();
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let request_ref = &request;
        let rows: Vec<(i64, i64, String, i64, i64, String)> = tx
            .step(|c| async move {
                sqlx::query_as(
                    "UPDATE forge.act_as_assertions SET state = 'rejected', \
                     reject_reason = 'expired', finished_at = now(), finished_request_id = $2 \
                     WHERE id IN (SELECT id FROM forge.act_as_assertions \
                                  WHERE state = 'pending' AND expires_at <= now() \
                                  ORDER BY id LIMIT $1 FOR UPDATE SKIP LOCKED) \
                     RETURNING id, project_id, intent, integration_id, merge_request_id, head_sha",
                )
                .bind(max)
                .bind(request_ref.as_str())
                .fetch_all(c)
                .await
                .map_err(|e| query_error("act_as.sweep_expired", e))
            })
            .await?;
        let mut entries = rows
            .into_iter()
            .map(|(id, project, intent, integration, mr, head_sha)| {
                audit(
                    "act_as_assertion.rejected",
                    id,
                    ProjectId::new(project),
                    serde_json::json!({
                        "intent": intent,
                        "integration_id": integration,
                        "merge_request_id": mr,
                        "head_sha": head_sha,
                        "reject_reason": "expired",
                    }),
                )
            })
            .collect::<Vec<_>>();
        if entries.is_empty() {
            return Ok(Settle::Discard(0));
        }
        let count = entries.len() as u64;
        let first = entries.remove(0);
        let record = ChangeRecord {
            audit: first,
            affected: entries,
            events: Vec::new(),
        };
        Ok(Settle::Commit(record, count))
    }
    .await;
    tx.settle("act_as_assertion.rejected", outcome).await
}

/// Active tokens with the `act_as` scope (`forge doctor act_as.keys`:
/// such a token without a key file is a configuration error).
pub async fn active_act_as_tokens(db: &Db) -> Result<i64, ForgeError> {
    sqlx::query_scalar(
        "SELECT count(*) FROM forge.tokens WHERE 'act_as' = ANY (scopes) \
         AND revoked_at IS NULL AND expires_at > now()",
    )
    .fetch_one(db.pool().executor())
    .await
    .map_err(|e| crate::read_query_error("act_as.active_tokens", e))
}

#[cfg(test)]
mod tests;
