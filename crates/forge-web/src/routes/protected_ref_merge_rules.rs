//! A protected ref rule's merge rule (M3 R6, ADR-0024 §12):
//! `PUT|DELETE /api/v1/projects/{id}/protected-refs/{rule_id}/merge-rule`.
//! Managing it is `ProtectedRefManage` on the rule (its level as both
//! `from` and `to`: people only, Maintainer, never above one's own role);
//! forge-hosting decides again inside the transaction. The answer of a put
//! is the rule with its merge rule (`201` created, `200` replaced or
//! unchanged); a remove is `204`.

use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use forge_auth::{LoadedProtection, RefProtectionCtx};
use forge_core::{parse_positive_id, ProjectId, ProtectedRefId, UserId};
use forge_errors::{ErrorCode, ForgeError};
use forge_hosting::{protection_level, MergeRuleAsk, MergeRuleOutcome};
use serde::Deserialize;

use crate::app::AppState;
use crate::body::param_invalid;
use crate::registry::{BodyLoader, Dispatched, LoadFuture, Loader, Lookup};
use crate::routes::projects::PREFIX;
use crate::routes::protected_refs::{
    found, project_of, respond, written_body, REQUIRED_CHECKS_UNREPORTABLE,
};
use crate::routes::write::{absent, bounded, hosting, json, read_if_manager, write_request};

/// `PUT` / `DELETE` one rule's merge rule.
pub const MERGE_RULE: &str = "/api/v1/projects/{id}/protected-refs/{rule_id}/merge-rule";
/// The body cap (20 check names of at most 108 bytes, JSON-escaped).
pub const MAX_BODY: usize = 8 * 1024;

/// `PUT …/merge-rule` (absent fields: off / 0).
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutMergeRule {
    /// The minimum counted approvals of a merge into the branch, 0..=20.
    #[serde(default)]
    pub required_approvals: u32,
    /// Checks that must have succeeded on the merge request's head, at
    /// most 20, each `ci/…`, `kanflow/…` or `forge/…` (`ext/…` is refused,
    /// FG-REVIEW-0016).
    #[serde(default)]
    pub required_checks: Vec<String>,
    /// Every discussion of the merge request must be resolved.
    #[serde(default)]
    pub require_resolved_discussions: bool,
    /// Pushes may not create or move the branch; merges still may.
    #[serde(default)]
    pub direct_push_forbidden: bool,
    /// Not available until work items exist (M3 I1): `true` is refused
    /// (FG-WEB-0006 `rule=unavailable_until_i1`).
    #[serde(default)]
    pub require_work_item_link: bool,
    /// The merge rule's `updated_at` the caller read, echoed verbatim (an
    /// opaque string), or `null` when the branch has none (create only).
    /// Required (U2 follow-up): another merge rule under the lock — changed,
    /// removed, or created by someone else — is FG-REVIEW-0019 and nothing
    /// is written.
    #[serde(deserialize_with = "Option::deserialize")]
    #[schema(required = true)]
    pub expected_updated_at: Option<String>,
}

/// The ids of `/api/v1/projects/{id}/protected-refs/{rule_id}/merge-rule`.
fn ids(raw_path: &str) -> Result<(ProjectId, ProtectedRefId), ForgeError> {
    let (id, rest) = raw_path
        .strip_prefix(PREFIX)
        .and_then(|rest| rest.split_once("/protected-refs/"))
        .ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    let id = parse_positive_id(id).ok_or_else(|| param_invalid("id", "not_positive_decimal"))?;
    let rule = rest
        .strip_suffix("/merge-rule")
        .and_then(parse_positive_id)
        .ok_or_else(|| param_invalid("rule_id", "not_positive_decimal"))?;
    Ok((ProjectId::new(id), ProtectedRefId::new(rule)))
}

/// The rule in the path, decided as a change of it at its own level.
async fn lookup(
    state: &AppState,
    parts: &Parts,
    viewer: Option<UserId>,
) -> Result<Lookup<RefProtectionCtx>, ForgeError> {
    let (id, rule) = ids(parts.uri.path())?;
    let Some(project) = project_of(state, id, viewer).await? else {
        return Ok(absent());
    };
    let hosting = hosting(state)?;
    let stored = read_if_manager(project.role, || {
        bounded(
            state,
            "protected_ref_load",
            hosting.protected_ref_state(id, rule),
        )
    })
    .await?;
    let level = stored.and_then(protection_level);
    Ok(found(RefProtectionCtx::new(
        project,
        LoadedProtection {
            rule: Some(rule),
            from: level,
            to: level,
        },
    )))
}

/// `PUT` / `DELETE …/merge-rule`.
pub struct MergeRuleOf;

impl Loader<AppState> for MergeRuleOf {
    type Ctx = RefProtectionCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, RefProtectionCtx> {
        Box::pin(lookup(state, parts, viewer))
    }
}

impl BodyLoader<AppState, PutMergeRule> for MergeRuleOf {
    type Ctx = RefProtectionCtx;
    fn load<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a Parts,
        _body: &'a PutMergeRule,
        viewer: Option<UserId>,
    ) -> LoadFuture<'a, RefProtectionCtx> {
        Box::pin(lookup(state, parts, viewer))
    }
}

fn rule_of(proof: &forge_auth::Authorized<RefProtectionCtx>) -> Result<ProtectedRefId, ForgeError> {
    proof.id().1.ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "proof_without_rule")
    })
}

/// `PUT …/merge-rule` → `201` / `200` the rule with its merge rule.
pub async fn put(state: AppState, request: Dispatched<RefProtectionCtx, PutMergeRule>) -> Response {
    let w = request.into_write();
    let built = async {
        let ask = MergeRuleAsk {
            required_approvals: w.input.required_approvals,
            required_checks: w.input.required_checks.clone(), // clone: the ask owns its names
            require_resolved_discussions: w.input.require_resolved_discussions,
            direct_push_forbidden: w.input.direct_push_forbidden,
            require_work_item_link: w.input.require_work_item_link,
        };
        let (outcome, _) = hosting(&state)?
            .put_merge_rule(
                &write_request(&w),
                &w.proof,
                &ask,
                w.input.expected_updated_at.as_deref(),
            )
            .await?;
        let mut body = written_body(&state, w.proof.id().0, rule_of(&w.proof)?).await?;
        // M3 C1 (narrowing R6's warning): a required check that no active
        // service account may report blocks every merge into the branch
        // (runbook checks-reporting); name each one.
        let required: Vec<forge_core::CheckName> = body
            .merge_rule
            .as_ref()
            .map(|m| {
                m.required_checks
                    .iter()
                    .filter_map(|n| forge_core::CheckName::parse(n).ok())
                    .collect()
            })
            .unwrap_or_default();
        let unreportable = bounded(
            &state,
            "check_reporters_load",
            crate::routes::checks::checks(&state)?.unreportable(w.proof.id().0, &required),
        )
        .await?;
        if !unreportable.is_empty() {
            body.warnings.push(REQUIRED_CHECKS_UNREPORTABLE.to_owned());
            body.unreportable_checks = unreportable.iter().map(|n| n.as_str().to_owned()).collect();
        }
        Ok((outcome, json(body)?))
    }
    .await;
    match built {
        Ok((MergeRuleOutcome::Created, body)) => respond(&w.ctx, StatusCode::CREATED, Ok(body)),
        Ok((
            MergeRuleOutcome::Replaced | MergeRuleOutcome::Unchanged | MergeRuleOutcome::Removed,
            body,
        )) => respond(&w.ctx, StatusCode::OK, Ok(body)),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}

/// `DELETE …/merge-rule` → `204`.
pub async fn delete(state: AppState, request: Dispatched<RefProtectionCtx>) -> Response {
    let w = request.into_write();
    let done = async {
        hosting(&state)?
            .remove_merge_rule(&write_request(&w), &w.proof)
            .await
    }
    .await;
    match done {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => w.ctx.fail(e).into_response(),
    }
}
