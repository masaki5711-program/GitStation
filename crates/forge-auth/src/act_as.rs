//! The act-as arm of `authorize()` (M3 S2, ADR-0025 §2–§5): a service
//! account's token acting for a person.
//!
//! Order of checks (every one before the person's normal arm):
//! 1. the credential (twice: forge-web's authentication checked it first,
//!    [`act_as_admission`]): a service account's token
//!    (`act_as_requires_service_account`), the `act_as` scope
//!    (`FG-AUTH-0002 required_scope=act_as`), not an agent's token
//!    (`agent_token`), an enabled integration (`integration_disabled`);
//! 2. the action's [`ActAsRule`]: `Denied` → `FG-AUTH-0003
//!    reason=act_as_not_allowed`, before the resource is looked at;
//! 3. the assertion against the rule: `FG-AUTH-0020` `missing` /
//!    `unexpected`;
//! 4. the resource: nothing at the path is the usual `FG-REPO-0005`; a
//!    project not bound to the request's integration is `FG-REPO-0005
//!    reason=act_as_unbound` whoever the person is;
//! 5. a read: both the person and the service account see the project, and
//!    the effective role is the smaller of the two (`FG-REPO-0005
//!    reason=act_as_not_visible`, then the normal arm on the capped view);
//!    a change: the assertion bound to the action and the resource
//!    (`wrong_action`, `wrong_resource`, `sha_mismatch`, `head_changed`,
//!    `target_retargeted`, `target_changed`), then the person's normal arm
//!    on their own view.
//!
//! M3 X3 (ADR-0025 Amendment X3): a binding intent (`binding_set`,
//! `binding_remove`) acts on a project that may not be bound yet, so its
//! scope is not `via_bound` but "the route's integration is the request's
//! own" (`FG-REPO-0005 reason=act_as_other_integration`) and the assertion
//! bound to the project, the kanflow project and (for `binding_set`) the
//! setting (`wrong_resource`, `setting_mismatch`); the person's own role
//! then decides (Maintainer). A `ListAsPerson` listing (`RepositorySearch`)
//! takes no assertion and is decided as the person on the plain instance
//! and on each listed project.
//!
//! The person's normal arm runs for a token-shaped principal with the
//! service account's scopes and **no administrator rights**, so the
//! action's own scope (`merge_approve`) is still required and every rule
//! of the arm (people only, self approval `FG-REVIEW-0013`, the role)
//! applies unchanged.

use forge_errors::{ErrorCode, ForgeError};

use crate::action::{ActAsRule, Action};
use crate::authorize::authorize;
use crate::model::{
    AccountKind, ActAsCredential, ActAsSubject, ActAsView, AssertionFacts, AssertionSubject,
    BindingSubject, Credential, Principal, ProjectView, Resource, Role, Scope, Visibility,
};
use forge_core::act_as::{assertion_invalid, Intent};

/// The credential conditions of an act-as request (ADR-0025 §2), on the
/// principal its token authenticated as — what forge-web's authentication
/// checks before it resolves the person (the act-as arm checks the same
/// again). A session, a person's or a bot's token: `FG-AUTH-0003
/// reason=act_as_requires_service_account`; no `act_as` scope:
/// `FG-AUTH-0002 required_scope=act_as`; an agent's token: `FG-AUTH-0003
/// reason=agent_token`.
pub fn act_as_admission(token: &Principal) -> Result<(), ForgeError> {
    let scopes = match token.credential() {
        Credential::Token(scopes) => scopes.as_slice(),
        Credential::Session | Credential::ActAs(_) => {
            return Err(refused("act_as_requires_service_account"))
        }
    };
    credential_checks(token.account_kind(), scopes, token.is_agent())
}

/// Whether a route of `action` may be reached acting for a person at all
/// (ADR-0025 §4): forge-web's authentication refuses an act-as request
/// for a `Denied` action before it looks anything up — the same refusal
/// the act-as arm gives (`FG-AUTH-0003 reason=act_as_not_allowed`).
pub fn act_as_route(action: Action) -> Result<(), ForgeError> {
    match action.act_as_rule() {
        ActAsRule::Denied => Err(refused("act_as_not_allowed")
            .with_context("action", action.name())
            .with_context("act_as_rule", ActAsRule::Denied.as_str())),
        ActAsRule::ReadWithoutAssertion
        | ActAsRule::RequiresAssertion(_)
        | ActAsRule::ListAsPerson => Ok(()),
    }
}

fn refused(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Forbidden).with_context("reason", reason)
}

fn not_found(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::NotFound).with_context("reason", reason)
}

fn credential_checks(kind: AccountKind, scopes: &[Scope], agent: bool) -> Result<(), ForgeError> {
    match kind {
        AccountKind::ServiceAccount => {}
        AccountKind::Human | AccountKind::Bot => {
            return Err(refused("act_as_requires_service_account"))
        }
    }
    if !scopes.contains(&Scope::ActAs) {
        let held: Vec<&str> = scopes.iter().map(|s| s.as_str()).collect();
        return Err(ForgeError::new(ErrorCode::InsufficientScope)
            .with_context("required_scope", Scope::ActAs.as_str())
            .with_context("token_scopes", held.join(",")));
    }
    if agent {
        return Err(refused("agent_token"));
    }
    Ok(())
}

/// Decide `action` on `resource` for an act-as principal (`authorize()`
/// calls it first for every `Credential::ActAs`).
pub(crate) fn decide(
    principal: &Principal,
    credential: &ActAsCredential,
    action: Action,
    resource: &Resource,
) -> Result<(), ForgeError> {
    let rule = action.act_as_rule();
    decide_rule(principal, credential, action, rule, resource).map_err(|e| {
        e.with_context("action", action.name())
            .with_context("act_as_rule", rule.as_str())
    })
}

fn decide_rule(
    principal: &Principal,
    credential: &ActAsCredential,
    action: Action,
    rule: ActAsRule,
    resource: &Resource,
) -> Result<(), ForgeError> {
    let Some(person) = principal.user() else {
        return Err(ForgeError::new(ErrorCode::Unauthenticated));
    };
    if !principal.active() {
        return Err(ForgeError::new(ErrorCode::AccountDisabled));
    }
    credential_checks(
        credential.via_kind,
        &credential.via_scopes,
        credential.via_agent,
    )?;
    if !credential.integration_enabled {
        return Err(refused("integration_disabled"));
    }
    // The resolution admits people only (FG-INT-0006 not_human); kept as
    // the backstop.
    if principal.account_kind() != AccountKind::Human {
        return Err(refused("act_as_target_not_human"));
    }
    let intent = match rule {
        ActAsRule::Denied => return Err(refused("act_as_not_allowed")),
        ActAsRule::ReadWithoutAssertion | ActAsRule::ListAsPerson => None,
        ActAsRule::RequiresAssertion(intent) => Some(intent),
    };
    let assertion = match (intent, credential.assertion.as_ref()) {
        (Some(_), None) => return Err(assertion_invalid("missing")),
        (None, Some(_)) => return Err(assertion_invalid("unexpected")),
        (Some(_), Some(a)) => Some(a),
        (None, None) => None,
    };
    // The person, as the normal arm sees them: the service account's
    // scopes, no administrator rights, never an agent.
    let as_person = Principal::authenticated(
        person,
        false,
        true,
        Credential::Token(credential.via_scopes.clone()), // clone: the trial principal owns its scopes
        AccountKind::Human,
    );
    // M3 X3: a listing of what the person may act on, decided as the
    // person on the plain resource (the route's instance, each project).
    if rule == ActAsRule::ListAsPerson {
        return match resource {
            Resource::Instance | Resource::Project(_) => authorize(&as_person, action, resource),
            Resource::Unresolved(reason) => Err(not_found(reason.as_str())),
            Resource::Group { .. }
            | Resource::Namespace { .. }
            | Resource::Ref { .. }
            | Resource::Transfer { .. }
            | Resource::GroupTransfer { .. }
            | Resource::UserRename { .. }
            | Resource::Tombstone(_)
            | Resource::Membership { .. }
            | Resource::RefProtectionChange { .. }
            | Resource::VisibilityChange { .. }
            | Resource::Token { .. }
            | Resource::Session { .. }
            | Resource::TokenRequest { .. }
            | Resource::Admission { .. }
            | Resource::Resolved { .. }
            | Resource::MergeRequest { .. }
            | Resource::Discussion { .. }
            | Resource::CodeownersEnforcement { .. }
            | Resource::MergeSettingsChange { .. }
            | Resource::ServiceAccount(_)
            | Resource::ServiceAccountTokenRequest { .. }
            | Resource::CheckRun { .. }
            | Resource::IntegrationFeed(_)
            | Resource::ProjectEvents { .. }
            | Resource::ActAs(_)
            | Resource::ActAsConfirm(_)
            | Resource::NotificationPreferences { .. } => Err(refused("act_as_facts_missing")),
        };
    }
    let view: &ActAsView = match resource {
        Resource::ActAs(view) => view,
        Resource::Unresolved(reason) => return Err(not_found(reason.as_str())),
        // A route whose loader does not load the act-as facts: refused,
        // never decided as if the service account were the person.
        Resource::Instance
        | Resource::Group { .. }
        | Resource::Namespace { .. }
        | Resource::Project(_)
        | Resource::Ref { .. }
        | Resource::Transfer { .. }
        | Resource::GroupTransfer { .. }
        | Resource::UserRename { .. }
        | Resource::Tombstone(_)
        | Resource::Membership { .. }
        | Resource::RefProtectionChange { .. }
        | Resource::VisibilityChange { .. }
        | Resource::Token { .. }
        | Resource::Session { .. }
        | Resource::TokenRequest { .. }
        | Resource::Admission { .. }
        | Resource::Resolved { .. }
        | Resource::MergeRequest { .. }
        | Resource::Discussion { .. }
        | Resource::CodeownersEnforcement { .. }
        | Resource::MergeSettingsChange { .. }
        | Resource::ServiceAccount(_)
        | Resource::ServiceAccountTokenRequest { .. }
        | Resource::CheckRun { .. }
        | Resource::IntegrationFeed(_)
        | Resource::ProjectEvents { .. }
        | Resource::ActAsConfirm(_)
        | Resource::NotificationPreferences { .. } => return Err(refused("act_as_facts_missing")),
    };
    // M3 X3: a binding intent's scope is the request's own integration and
    // the assertion's binding, not `via_bound` (the project may not be
    // bound yet).
    if let (Some(intent), Some(assertion)) = (intent, assertion) {
        if !intent.is_merge_request() {
            let Some(binding) = view.binding.as_ref() else {
                return Err(refused("act_as_facts_missing"));
            };
            if binding.integration != credential.integration {
                return Err(not_found("act_as_other_integration"));
            }
            bound_to_binding(intent, assertion, binding)?;
            return authorize(&as_person, action, &view.inner);
        }
    }
    if !view.via.bound {
        return Err(not_found("act_as_unbound"));
    }
    match (intent, assertion) {
        (Some(intent), Some(assertion)) => {
            bound_to(intent, assertion, view.subject.as_ref())?;
            authorize(&as_person, action, &view.inner)
        }
        (None, None) => {
            let capped = capped(&view.inner, view.via.role)?;
            authorize(&as_person, action, &capped).map_err(|e| {
                e.with_context("effective_role", effective_role(&view.inner, view.via.role))
            })
        }
        // Excluded above.
        (Some(_), None) | (None, Some(_)) => Err(ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "act_as_rule_assertion_mismatch")),
    }
}

/// The assertion against the action and the merge request as loaded
/// (ADR-0025 §5.3, the `authorize()` rows).
fn bound_to(
    intent: Intent,
    assertion: &AssertionFacts,
    subject: Option<&ActAsSubject>,
) -> Result<(), ForgeError> {
    let with_id =
        |e: ForgeError| e.with_context("assertion_public_id", assertion.public_id.clone());
    if assertion.intent != intent {
        return Err(with_id(assertion_invalid("wrong_action")));
    }
    let Some(s) = subject else {
        return Err(refused("act_as_facts_missing"));
    };
    let AssertionSubject::MergeRequest {
        merge_request,
        head_sha,
        target_ref,
        target_sha,
    } = &assertion.subject
    else {
        return Err(with_id(assertion_invalid("wrong_resource")));
    };
    if assertion.project != s.project || *merge_request != s.merge_request {
        return Err(with_id(assertion_invalid("wrong_resource")));
    }
    if intent == Intent::Approve && s.requested_sha.as_deref() != Some(head_sha) {
        return Err(with_id(assertion_invalid("sha_mismatch")));
    }
    if s.head_sha.as_deref() != Some(head_sha) {
        return Err(with_id(assertion_invalid("head_changed")));
    }
    if s.target_ref != *target_ref {
        return Err(with_id(assertion_invalid("target_retargeted")));
    }
    if s.target_sha.as_deref() != Some(target_sha) {
        return Err(with_id(assertion_invalid("target_changed")));
    }
    Ok(())
}

/// A binding assertion against the action and the binding the request
/// writes (M3 X3, ADR-0025 Amendment X3): the intent, the project, the
/// kanflow project (`wrong_resource`) and, for `binding_set`, the setting
/// the person confirmed (`setting_mismatch`).
fn bound_to_binding(
    intent: Intent,
    assertion: &AssertionFacts,
    subject: &BindingSubject,
) -> Result<(), ForgeError> {
    let with_id =
        |e: ForgeError| e.with_context("assertion_public_id", assertion.public_id.clone());
    if assertion.intent != intent {
        return Err(with_id(assertion_invalid("wrong_action")));
    }
    let AssertionSubject::Binding {
        external_project_id,
        resolves_work_items,
    } = &assertion.subject
    else {
        return Err(with_id(assertion_invalid("wrong_resource")));
    };
    if assertion.project != subject.project || *external_project_id != subject.external_project_id {
        return Err(with_id(assertion_invalid("wrong_resource")));
    }
    if *resolves_work_items != subject.resolves_work_items {
        return Err(with_id(assertion_invalid("setting_mismatch")));
    }
    Ok(())
}

/// The person's native role and visibility (`ProjectView` of the person).
fn project_view(resource: &Resource) -> Option<&ProjectView> {
    match resource {
        Resource::Project(p) => Some(p),
        Resource::MergeRequest { project, .. } => Some(project),
        Resource::Instance
        | Resource::Group { .. }
        | Resource::Namespace { .. }
        | Resource::Ref { .. }
        | Resource::Transfer { .. }
        | Resource::GroupTransfer { .. }
        | Resource::UserRename { .. }
        | Resource::Tombstone(_)
        | Resource::Membership { .. }
        | Resource::RefProtectionChange { .. }
        | Resource::VisibilityChange { .. }
        | Resource::Token { .. }
        | Resource::Session { .. }
        | Resource::TokenRequest { .. }
        | Resource::Admission { .. }
        | Resource::Unresolved(_)
        | Resource::Resolved { .. }
        | Resource::Discussion { .. }
        | Resource::CodeownersEnforcement { .. }
        | Resource::MergeSettingsChange { .. }
        | Resource::ServiceAccount(_)
        | Resource::ServiceAccountTokenRequest { .. }
        | Resource::CheckRun { .. }
        | Resource::IntegrationFeed(_)
        | Resource::ProjectEvents { .. }
        | Resource::ActAs(_)
        | Resource::ActAsConfirm(_)
        | Resource::NotificationPreferences { .. } => None,
    }
}

/// The role a read acting for a person is decided with (ADR-0025 §3): the
/// smaller of the person's native role and the service account's
/// membership role; none unless both see the project.
fn min_role(person: &ProjectView, via: Option<Role>) -> Option<Role> {
    let (Some(p), Some(v)) = (person.role, via) else {
        return None;
    };
    Some(p.min(v))
}

fn effective_role(inner: &Resource, via: Option<Role>) -> &'static str {
    project_view(inner)
        .and_then(|p| min_role(p, via))
        .map_or("none", Role::as_str)
}

/// The person's resource with the capped view: a private project whose
/// role is the smaller of the two (so nothing internal-wide widens it).
fn capped(inner: &Resource, via: Option<Role>) -> Result<Resource, ForgeError> {
    let Some(person) = project_view(inner) else {
        return Err(refused("act_as_facts_missing"));
    };
    let both_see =
        (person.role.is_some() || person.visibility == Visibility::Internal) && via.is_some();
    if !both_see {
        return Err(not_found("act_as_not_visible"));
    }
    let view = ProjectView {
        visibility: Visibility::Private,
        role: min_role(person, via),
        pending_deletion: person.pending_deletion,
    };
    Ok(match inner {
        Resource::MergeRequest {
            author,
            state,
            draft,
            source_branch,
            target_branch,
            edit,
            target_protection,
            ..
        } => Resource::MergeRequest {
            project: view,
            author: *author,
            state: *state,
            draft: *draft,
            source_branch: source_branch.clone(), // clone: the capped resource owns its names
            target_branch: target_branch.clone(),
            edit: edit.clone(),
            target_protection: *target_protection,
        },
        Resource::Project(_)
        | Resource::Instance
        | Resource::Group { .. }
        | Resource::Namespace { .. }
        | Resource::Ref { .. }
        | Resource::Transfer { .. }
        | Resource::GroupTransfer { .. }
        | Resource::UserRename { .. }
        | Resource::Tombstone(_)
        | Resource::Membership { .. }
        | Resource::RefProtectionChange { .. }
        | Resource::VisibilityChange { .. }
        | Resource::Token { .. }
        | Resource::Session { .. }
        | Resource::TokenRequest { .. }
        | Resource::Admission { .. }
        | Resource::Unresolved(_)
        | Resource::Resolved { .. }
        | Resource::Discussion { .. }
        | Resource::CodeownersEnforcement { .. }
        | Resource::MergeSettingsChange { .. }
        | Resource::ServiceAccount(_)
        | Resource::ServiceAccountTokenRequest { .. }
        | Resource::CheckRun { .. }
        | Resource::IntegrationFeed(_)
        | Resource::ProjectEvents { .. }
        | Resource::ActAs(_)
        | Resource::ActAsConfirm(_)
        | Resource::NotificationPreferences { .. } => Resource::Project(view),
    })
}
