//! Project webhooks (M3 I2; ADR-0016 §8 and its Amendment I2; plan 02
//! §5.5 / §11.1; docs/api/webhooks.md).
//!
//! - [`Webhooks`]: the commands and reads of the routes, on the route's
//!   proof. **Every write is decided twice** (as forge-hosting, forge-review
//!   and forge-checks): the route proves the action on the project the
//!   loader read for the caller; forge-db's `*_decided` command locks the
//!   project, the caller's roles and account and calls `decide` — here
//!   `Authorized::reconfirm` on those facts. The URL passes forge-core's
//!   target policy after the decision (`FG-INT-0010`: the allowlist,
//!   always-refused ranges, user information, the scheme); the secret is
//!   generated here, sealed for the row's `uid` with
//!   `webhook.secret_keys_file`, shown once and never stored in the clear.
//!   Only this crate calls the `*_decided` webhook commands
//!   (`tests/decided_callers.rs`).
//! - [`WebhookDispatch`] (`webhook-dispatch`, independent): for each
//!   published event of a project, each enabled webhook whose owner may
//!   still see the event's class (`forge_auth::prove_webhook_delivery`)
//!   and whose allowlist admits the type gets one delivery row and its
//!   first `webhook.deliver` job.
//! - [`DeliverHandler`] (`webhook.deliver`): one attempt — decided again
//!   for the owner, the URL checked again against the policy of the moment
//!   and the addresses its name resolves to now, the connection pinned to
//!   the checked address (no redirect followed, no proxy, 3 s connect,
//!   10 s in all, at most 4 KiB of the answer read), signed with the
//!   current (and, during the 24-hour overlap, the previous) secret, and
//!   recorded: success, the next attempt after the backoff
//!   (`FG-INT-0020`), the last one (`FG-INT-0004`), the automatic disable
//!   (`FG-INT-0011`).

use std::sync::Arc;

use forge_auth::{
    authorize, AccountFacts, Action, Authorized, Credential, LoadedProject, Principal, ProjectCtx,
    ProjectView, Resource, WebhookCtx, WebhookDeliveryCtx,
};
use forge_core::webhook::{
    webhook_event_types, TargetPolicy, WebhookSecret, WebhookSecretKeys, WebhookUrl,
};
use forge_core::{RequestId, UserId, WebhookDeliveryId};
use forge_db::access::Account;
use forge_db::change::{Actor, ChangeContext};
use forge_db::projects::ProjectAccessFacts;
use forge_db::webhooks::{self as db, NewWebhook, WebhookPatch};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

mod body;
mod client;
mod deliver;
mod dispatch;

pub use body::delivery_body;
pub use client::{Sent, Transport};
pub use deliver::{DeliverHandler, DELIVER_JOB};
pub use dispatch::{WebhookDispatch, DISPATCH};
pub use forge_db::webhooks::{
    Delivery, DeliveryState, UpdateOutcome, Webhook, MAX_DELIVERIES_LISTED,
};

/// The request a write belongs to (the audit row's request id and token).
#[derive(Debug, Clone)]
pub struct WebhookRequest {
    pub request_id: RequestId,
    /// The personal access token used (`None` for a session).
    pub credential_id: Option<i64>,
}

/// `POST …/webhooks`: the URL and the event types (`None`: `["*"]`).
#[derive(Debug, Clone)]
pub struct WebhookInput {
    pub url: String,
    pub event_types: Option<Vec<String>>,
}

/// `PATCH …/webhooks/{id}` (`None`: unchanged).
#[derive(Debug, Clone, Default)]
pub struct WebhookChanges {
    pub url: Option<String>,
    pub event_types: Option<Vec<String>>,
    pub enabled: Option<bool>,
}

/// A created webhook and its secret (shown this once).
#[derive(Debug)]
pub struct Created {
    pub webhook: Webhook,
    pub secret: WebhookSecret,
}

/// A rotated secret (shown this once).
#[derive(Debug)]
pub struct RotatedSecret {
    pub secret: WebhookSecret,
    pub secret_version: i32,
    /// RFC 3339, UTC: the previous secret still signs until then.
    pub previous_valid_until: String,
}

/// The webhook commands and reads (`forge serve` builds one).
#[derive(Clone)]
pub struct Webhooks {
    db: Db,
    policy: TargetPolicy,
    keys: Option<Arc<WebhookSecretKeys>>,
}

impl std::fmt::Debug for Webhooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Webhooks").finish_non_exhaustive()
    }
}

/// `FG-SYS-0001 reason=proof_action_mismatch` unless the proof is for
/// `expected`.
fn require(action: Action, expected: Action) -> Result<(), ForgeError> {
    if action == expected {
        return Ok(());
    }
    Err(ForgeError::new(ErrorCode::Internal)
        .with_context("reason", "proof_action_mismatch")
        .with_context("command_action", expected.name())
        .with_context("proof_action", action.name()))
}

fn caller(principal: &Principal, action: Action) -> Result<UserId, ForgeError> {
    principal.user().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "proof_without_user")
            .with_context("action", action.name())
    })
}

fn change_context(request: &WebhookRequest, principal: &Principal, user: UserId) -> ChangeContext {
    // Managing a webhook is a person's (`people_only`): always a user.
    let mut ctx = ChangeContext::new(Actor::User(user), request.request_id.clone()); // clone: the context owns it
    ctx.credential_id = request.credential_id;
    ctx.effective_scope = match principal.credential() {
        Credential::Session => Vec::new(),
        Credential::Token(scopes) => scopes.iter().map(|s| s.as_str().to_owned()).collect(),
    };
    ctx
}

fn account(a: Account) -> AccountFacts {
    AccountFacts {
        active: a.active,
        is_admin: a.is_admin,
    }
}

fn loaded(facts: &ProjectAccessFacts) -> LoadedProject {
    LoadedProject {
        id: facts.project,
        visibility: facts.access.visibility,
        role: facts.access.role,
        pending_deletion: facts.access.pending_deletion,
    }
}

/// The visibility classes `types` name: `*` and `<family>.*` the
/// integration class, a name its own class.
fn classes_of(types: &[String]) -> Vec<forge_core::VisibilityClass> {
    let mut out = Vec::new();
    for entry in types {
        let class = match forge_core::EventType::parse(entry) {
            Some(t) => t.visibility_class(),
            None => forge_core::VisibilityClass::Integration,
        };
        if !out.contains(&class) {
            out.push(class);
        }
    }
    out
}

/// ADR-0016 §8: the creator may choose only the classes they may receive
/// (`WebhookDeliver`, decided by `authorize()` for the caller on the
/// project as the route loaded it); the delivery decides it again for the
/// owner (`prove_webhook_delivery`).
fn decide_classes(
    principal: &Principal,
    project: ProjectView,
    types: &[String],
) -> Result<(), ForgeError> {
    for class in classes_of(types) {
        authorize(
            principal,
            Action::WebhookDeliver,
            &Resource::ProjectEvents { project, class },
        )?;
    }
    Ok(())
}

/// One event type a webhook may name, with its class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscribableType {
    pub name: &'static str,
    pub class: forge_core::VisibilityClass,
}

/// One class a webhook may name and whether the caller may subscribe to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscribableClass {
    pub class: forge_core::VisibilityClass,
    pub subscribable: bool,
}

/// What `GET …/webhooks/event-types` answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventTypeCatalog {
    pub items: Vec<SubscribableType>,
    pub classes: Vec<SubscribableClass>,
}

/// The classes a webhook may name (never admin), in the order of the docs.
const WEBHOOK_CLASSES: [forge_core::VisibilityClass; 3] = [
    forge_core::VisibilityClass::Integration,
    forge_core::VisibilityClass::Maintainer,
    forge_core::VisibilityClass::Deployment,
];

fn absent(resource: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::NotFound)
        .with_context("resource", resource)
        .with_context("reason", "absent")
}

impl Webhooks {
    /// `keys`: `webhook.secret_keys_file` (`None`: not configured — creating
    /// a webhook or rotating its secret is `FG-SYS-0002 reason=missing`).
    pub fn new(db: Db, policy: TargetPolicy, keys: Option<Arc<WebhookSecretKeys>>) -> Self {
        Self { db, policy, keys }
    }

    fn keys(&self) -> Result<&WebhookSecretKeys, ForgeError> {
        self.keys.as_deref().ok_or_else(|| {
            ForgeError::new(ErrorCode::ConfigInvalid)
                .with_context("config_key", forge_core::webhook::SECRET_KEYS_KEY)
                .with_context("reason", "missing")
        })
    }

    /// The URL as the target policy admits it now (`FG-INT-0010`).
    fn checked_url(&self, raw: &str) -> Result<WebhookUrl, ForgeError> {
        let url = WebhookUrl::parse(raw, self.policy.allow_http())?;
        self.policy.check_url(&url)?;
        Ok(url)
    }

    /// `GET …/webhooks` (`WebhookRead`).
    pub async fn list(&self, proof: &Authorized<ProjectCtx>) -> Result<Vec<Webhook>, ForgeError> {
        require(proof.action(), Action::WebhookRead)?;
        db::list(&self.db, *proof.id()).await
    }

    /// `GET …/webhooks/event-types` (`WebhookRead`): every event type a
    /// webhook may name (the admin class never), and per class whether the
    /// caller may subscribe to it — the decision `create` / `update` make
    /// (`WebhookDeliver` through `authorize()`).
    pub fn event_types(
        &self,
        proof: &Authorized<ProjectCtx>,
    ) -> Result<EventTypeCatalog, ForgeError> {
        require(proof.action(), Action::WebhookRead)?;
        let mut items: Vec<SubscribableType> = forge_core::EventType::ALL
            .into_iter()
            .map(|t| SubscribableType {
                name: t.as_str(),
                class: t.visibility_class(),
            })
            .filter(|t| WEBHOOK_CLASSES.contains(&t.class))
            .collect();
        items.sort_by_key(|t| t.name);
        let classes = WEBHOOK_CLASSES
            .into_iter()
            .map(|class| SubscribableClass {
                class,
                subscribable: authorize(
                    proof.principal(),
                    Action::WebhookDeliver,
                    &Resource::ProjectEvents {
                        project: proof.ctx().view(),
                        class,
                    },
                )
                .is_ok(),
            })
            .collect();
        Ok(EventTypeCatalog { items, classes })
    }

    /// `GET …/webhooks/{id}` (`WebhookRead`): another project's or none is
    /// `FG-REPO-0005 resource=webhook`.
    pub async fn get(&self, proof: &Authorized<WebhookCtx>) -> Result<Webhook, ForgeError> {
        require(proof.action(), Action::WebhookRead)?;
        let (project, webhook) = *proof.id();
        db::get(&self.db, project, webhook)
            .await?
            .ok_or_else(|| absent("webhook"))
    }

    async fn stored(
        &self,
        proof_project: forge_core::ProjectId,
        id: forge_core::WebhookId,
    ) -> Result<Webhook, ForgeError> {
        db::get(&self.db, proof_project, id).await?.ok_or_else(|| {
            // Written by this request and gone before the answer: only a
            // concurrent delete can do it.
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "webhook_vanished")
                .with_context("webhook_id", id.get())
        })
    }

    /// `POST …/webhooks` (`WebhookManage`): the webhook (owned by the
    /// caller) and its secret, shown once. Errors: `FG-INT-0010` (the
    /// URL), `FG-WEB-0006 field=event_types`, `FG-INT-0012
    /// reason=webhook_limit`, `FG-SYS-0002` without secret keys, the
    /// decision's.
    pub async fn create(
        &self,
        request: &WebhookRequest,
        proof: &Authorized<ProjectCtx>,
        input: &WebhookInput,
    ) -> Result<Created, ForgeError> {
        require(proof.action(), Action::WebhookManage)?;
        let user = caller(proof.principal(), Action::WebhookManage)?;
        let project = *proof.id();
        let url = self.checked_url(&input.url)?;
        let types = match &input.event_types {
            Some(t) => webhook_event_types(t)?,
            None => vec![forge_core::webhook::ALL_EVENT_TYPES.to_owned()],
        };
        decide_classes(proof.principal(), proof.ctx().view(), &types)?;
        let keys = self.keys()?;
        let secret = WebhookSecret::generate()?;
        let uid = uuid::Uuid::new_v4();
        let new = NewWebhook {
            uid,
            url: url.as_str().to_owned(),
            origin: url.origin(),
            event_types: types,
            secret: keys.seal(&uid, &secret)?,
        };
        let ctx = change_context(request, proof.principal(), user);
        let (id, ()) = db::create_webhook_decided(&self.db, &ctx, project, user, &new, |facts| {
            let fresh = ProjectCtx::new(loaded(&facts.project));
            proof.reconfirm(account(facts.account), fresh).map(drop)
        })
        .await?;
        Ok(Created {
            webhook: self.stored(project, id).await?,
            secret,
        })
    }

    /// `PATCH …/webhooks/{id}` (`WebhookManage`).
    pub async fn update(
        &self,
        request: &WebhookRequest,
        proof: &Authorized<WebhookCtx>,
        changes: &WebhookChanges,
    ) -> Result<(UpdateOutcome, Webhook), ForgeError> {
        require(proof.action(), Action::WebhookManage)?;
        let user = caller(proof.principal(), Action::WebhookManage)?;
        let (project, webhook) = *proof.id();
        let url = changes
            .url
            .as_deref()
            .map(|u| {
                self.checked_url(u)
                    .map(|u| (u.as_str().to_owned(), u.origin()))
            })
            .transpose()?;
        let event_types = changes
            .event_types
            .as_deref()
            .map(webhook_event_types)
            .transpose()?;
        if let Some(types) = &event_types {
            decide_classes(proof.principal(), proof.ctx().project_view(), types)?;
        }
        let patch = WebhookPatch {
            url,
            event_types,
            enabled: changes.enabled,
        };
        let ctx = change_context(request, proof.principal(), user);
        let (outcome, ()) =
            db::update_webhook_decided(&self.db, &ctx, (project, webhook), user, &patch, |facts| {
                let fresh = WebhookCtx::new(loaded(&facts.project), webhook);
                proof.reconfirm(account(facts.account), fresh).map(drop)
            })
            .await?;
        Ok((outcome, self.stored(project, webhook).await?))
    }

    /// `DELETE …/webhooks/{id}` (`WebhookManage`).
    pub async fn delete(
        &self,
        request: &WebhookRequest,
        proof: &Authorized<WebhookCtx>,
    ) -> Result<(), ForgeError> {
        require(proof.action(), Action::WebhookManage)?;
        let user = caller(proof.principal(), Action::WebhookManage)?;
        let (project, webhook) = *proof.id();
        let ctx = change_context(request, proof.principal(), user);
        db::delete_webhook_decided(&self.db, &ctx, (project, webhook), user, |facts| {
            let fresh = WebhookCtx::new(loaded(&facts.project), webhook);
            proof.reconfirm(account(facts.account), fresh).map(drop)
        })
        .await
    }

    /// `POST …/webhooks/{id}/secret` (`WebhookManage`): a new secret, shown
    /// once; the previous one still signs for 24 hours.
    pub async fn rotate_secret(
        &self,
        request: &WebhookRequest,
        proof: &Authorized<WebhookCtx>,
    ) -> Result<RotatedSecret, ForgeError> {
        require(proof.action(), Action::WebhookManage)?;
        let user = caller(proof.principal(), Action::WebhookManage)?;
        let (project, webhook) = *proof.id();
        let keys = self.keys()?;
        let secret = WebhookSecret::generate()?;
        let ctx = change_context(request, proof.principal(), user);
        let (rotated, ()) = db::rotate_webhook_secret_decided(
            &self.db,
            &ctx,
            (project, webhook),
            user,
            |uid| keys.seal(uid, &secret),
            |facts| {
                let fresh = WebhookCtx::new(loaded(&facts.project), webhook);
                proof.reconfirm(account(facts.account), fresh).map(drop)
            },
        )
        .await?;
        Ok(RotatedSecret {
            secret,
            secret_version: rotated.secret_version,
            previous_valid_until: rotated.previous_valid_until,
        })
    }

    /// `GET …/webhooks/{id}/deliveries` (`WebhookDeliveryRead`): newest
    /// first, before `before`, at most `limit` (1..=100).
    pub async fn deliveries(
        &self,
        proof: &Authorized<WebhookCtx>,
        before: Option<WebhookDeliveryId>,
        limit: i64,
    ) -> Result<Vec<Delivery>, ForgeError> {
        require(proof.action(), Action::WebhookDeliveryRead)?;
        let (project, webhook) = *proof.id();
        db::deliveries(&self.db, project, webhook, before, limit)
            .await?
            .ok_or_else(|| absent("webhook"))
    }

    /// `POST …/deliveries/{delivery_id}/redeliver` (`WebhookRedeliver`).
    pub async fn redeliver(
        &self,
        request: &WebhookRequest,
        proof: &Authorized<WebhookDeliveryCtx>,
    ) -> Result<(), ForgeError> {
        require(proof.action(), Action::WebhookRedeliver)?;
        let user = caller(proof.principal(), Action::WebhookRedeliver)?;
        let (project, webhook, delivery) = *proof.id();
        let ctx = change_context(request, proof.principal(), user);
        db::redeliver_webhook_decided(
            &self.db,
            &ctx,
            (project, webhook, delivery),
            user,
            |facts| {
                let fresh = WebhookDeliveryCtx::new(loaded(&facts.project), webhook, delivery);
                proof.reconfirm(account(facts.account), fresh).map(drop)
            },
        )
        .await
    }
}
