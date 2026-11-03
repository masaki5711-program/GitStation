//! Merging a merge request (M3 R4, ADR-0024 §4; plan 11 §11 decisions 9,
//! 10 and 12).
//!
//! - [`evaluate`]: `evaluate_merge()` and the [`MergeClearance`] proof.
//! - `request.rs`: [`crate::Review::request_merge`], the HTTP entry
//!   (`POST …/merge-requests/{iid}/merge`): the `sha` the requester saw,
//!   the method the project allows, `evaluate_merge()` once, then the
//!   `queued` attempt and its job (202). No git runs in the request.
//! - `execute.rs`: [`Merger::execute`], the job `merge_request.merge`
//!   (`forge serve` runs it, so the ref writer and the browse cache live
//!   in one process): the repository's merge lock; tx1a (the requester's
//!   decision rebuilt from the database — `authorize()` with the target's
//!   protection — and `evaluate_merge()`, the source tip and the recorded
//!   head against the requested head); the trial merge and the commit
//!   outside any transaction; tx1b (decide again, `queued → prepared`);
//!   the fence (the lock still stands, `prepared → executing`, the
//!   reconciliation's window); `update-ref` compare-and-swap; tx2.
//! - `source.rs`: the optional source branch deletion after `applied`,
//!   decided as a branch delete (`RepoPush` on `Resource::Ref`, the push
//!   path's protected-ref rules).
//! - `reconcile.rs`: [`Merger::reconcile`], what `forge serve` runs every
//!   minute: stale `queued` / `prepared` attempts end `superseded`;
//!   `executing` ones are settled by asking git.
//!
//! **No server hooks on the merge path** (plan 11 decision 12): the merge
//! re-verifies under the lock the decision the push path makes for a
//! protected ref (`authorize()` on the target's `RefProtection`) and the
//! recorded head / target tip against the live tips; push-path
//! pre-receive / post-receive hooks stay with X8. `update-ref` and
//! `commit-tree` run with `core.hooksPath` pinned to the empty directory.
//!
//! Every ref writer calls `Browser::invalidate` for the repository (the
//! merge's update and the source deletion).

use std::sync::Arc;
use std::time::Duration;

use forge_auth::{AccountKind, Credential, Principal};
use forge_core::{MergeMethod, UserId};
use forge_db::access::Account;
use forge_db::review::merges::{CredentialFacts, CredentialState};
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::Browser;
use forge_git::{Git, StorageRoot};

mod attribution;
pub mod conditions;
pub mod evaluate;
mod execute;
mod reconcile;
mod request;
mod source;

pub use conditions::{conditions, method_allowed, CheckRuns, Conditions, NoCheckRuns};
pub use evaluate::{
    blocked, evaluate_merge, stored_mergeable, BlockReason, CheckConclusion, CheckRun, CheckStatus,
    MergeClearance, MergeFacts, Mergeable, RequiredCheck,
};
pub use execute::{MergeReport, RECONCILE_BATCH};
pub use forge_db::review::merges::{CredentialRef, StoredMerge};
pub use reconcile::ReconcileReport;
pub use request::MergeAsk;
pub(crate) use request::{head_changed as request_head_changed, is_ref_not_found};

/// The methods a project allows and its defaults (ADR-0024 §4.1): since M3
/// R6 the project's stored settings ([`MergeSettings::from_values`] of
/// `forge_db::review::merge_settings`; without a row
/// [`MergeSettings::default`]: merge commit and squash, default merge
/// commit, the source branch kept unless asked).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeSettings {
    allowed: Vec<MergeMethod>,
    default_method: MergeMethod,
    delete_source_default: bool,
}

impl Default for MergeSettings {
    fn default() -> Self {
        Self {
            allowed: vec![MergeMethod::MergeCommit, MergeMethod::Squash],
            default_method: MergeMethod::MergeCommit,
            delete_source_default: false,
        }
    }
}

impl MergeSettings {
    /// Settings allowing exactly `allowed` (the first is the default).
    /// `None` for an empty list.
    pub fn allowing(allowed: &[MergeMethod]) -> Option<Self> {
        let default_method = *allowed.first()?;
        Some(Self {
            allowed: allowed.to_vec(),
            default_method,
            delete_source_default: false,
        })
    }

    /// The settings a project stored (M3 R6). An empty method list (the
    /// table refuses it) allows nothing and defaults to a merge commit,
    /// which is then refused (`FG-REVIEW-0010`), never a silent allow.
    pub fn from_values(values: &forge_core::MergeSettingsValues) -> Self {
        Self {
            allowed: values.allowed_methods.clone(), // clone: the settings own their list
            default_method: values.default_method,
            delete_source_default: values.delete_source_default,
        }
    }

    pub fn default_method(&self) -> MergeMethod {
        self.default_method
    }

    pub fn delete_source_default(&self) -> bool {
        self.delete_source_default
    }

    pub fn allowed(&self) -> &[MergeMethod] {
        &self.allowed
    }

    /// `FG-REVIEW-0010` MergeMethodNotAllowed unless `method` is allowed.
    pub fn check(&self, method: MergeMethod) -> Result<(), ForgeError> {
        if self.allowed.contains(&method) {
            return Ok(());
        }
        let allowed: Vec<&str> = self.allowed.iter().map(|m| m.as_str()).collect();
        Err(ForgeError::new(ErrorCode::MergeMethodNotAllowed)
            .with_context("method", method.as_str())
            .with_context("allowed", allowed.join(",")))
    }
}

/// The bounds of a merge (ADR-0024 §4.2; decided in R4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeTimeouts {
    /// The wait for the repository's merge lock.
    pub lock_wait: Duration,
    /// A ref read, `merge-base`, the commit list of the record.
    pub read: Duration,
    /// `commit-tree`.
    pub commit: Duration,
    /// `update-ref` (the compare-and-swap of the target).
    pub update_ref: Duration,
    /// The reconciliation's grace: an `executing` attempt is settled only
    /// `grace + update_ref` after `executing_at`; a worker never starts
    /// `update-ref` later than `grace - update_ref` after it.
    pub grace: Duration,
    /// A `queued` / `prepared` attempt not updated for this long is
    /// superseded by the reconciliation.
    pub idle: Duration,
}

impl Default for MergeTimeouts {
    fn default() -> Self {
        Self {
            lock_wait: forge_db::review::merges::MERGE_LOCK_WAIT,
            read: Duration::from_secs(10),
            commit: Duration::from_secs(30),
            update_ref: Duration::from_secs(30),
            grace: Duration::from_secs(600),
            idle: Duration::from_secs(600),
        }
    }
}

impl MergeTimeouts {
    /// The settings check of ADR-0024 §4.2 step 6 (c): the grace must
    /// exceed twice the `update-ref` timeout (`FG-SYS-0002
    /// config_key=review.merge_grace reason=below_update_ref`).
    pub fn check(&self) -> Result<(), ForgeError> {
        if self.grace > self.update_ref.saturating_mul(2) {
            Ok(())
        } else {
            Err(ForgeError::new(ErrorCode::ConfigInvalid)
                .with_context("config_key", "review.merge_grace")
                .with_context("reason", "below_update_ref"))
        }
    }

    /// When an `executing` attempt is the reconciliation's.
    pub fn executing_after(&self) -> Duration {
        self.grace.saturating_add(self.update_ref)
    }

    /// The latest a worker may start `update-ref` after `executing_at`.
    pub fn update_ref_window(&self) -> Duration {
        self.grace.saturating_sub(self.update_ref)
    }
}

/// A test hook run right before `update-ref` (the concurrency tests push
/// to the target there). `None` in production.
pub type BeforeUpdateRef = Arc<dyn Fn() + Send + Sync>;

/// Where merges run: the database, git, the storage, the browse cache to
/// invalidate, the settings and the bounds.
#[derive(Clone)]
pub struct Merger {
    db: forge_db::Db,
    git: Git,
    storage: StorageRoot,
    browser: Option<Browser>,
    timeouts: MergeTimeouts,
    before_update_ref: Option<BeforeUpdateRef>,
}

impl std::fmt::Debug for Merger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Merger")
            .field("timeouts", &self.timeouts)
            .finish_non_exhaustive()
    }
}

impl Merger {
    pub fn new(db: forge_db::Db, git: Git, storage: StorageRoot) -> Self {
        Self {
            db,
            git,
            storage,
            browser: None,
            timeouts: MergeTimeouts::default(),
            before_update_ref: None,
        }
    }

    /// The browse cache every ref write invalidates (`forge serve`).
    #[must_use]
    pub fn with_browser(mut self, browser: Browser) -> Self {
        self.browser = Some(browser);
        self
    }

    /// Other bounds (tests); checked like the configuration.
    pub fn with_timeouts(mut self, timeouts: MergeTimeouts) -> Result<Self, ForgeError> {
        timeouts.check()?;
        self.timeouts = timeouts;
        Ok(self)
    }

    /// Run `hook` right before every `update-ref` (tests only).
    #[must_use]
    pub fn with_before_update_ref(mut self, hook: BeforeUpdateRef) -> Self {
        self.before_update_ref = Some(hook);
        self
    }

    pub fn timeouts(&self) -> MergeTimeouts {
        self.timeouts
    }

    /// Drop the browse caches of `repo` after a ref write.
    fn invalidate(&self, repo: forge_core::RepoId) {
        if let Some(browser) = &self.browser {
            browser.invalidate(repo);
        }
    }
}

/// `FG-AUTH-0003 reason=credential_revoked`: the requester's credential
/// is revoked, expired or gone (ADR-0024 §5).
fn revoked(detail: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Forbidden)
        .with_context("reason", "credential_revoked")
        .with_context("detail", detail)
        .with_context("action", forge_auth::Action::MergeRequestMerge.name())
}

/// The requester's principal rebuilt from the database (ADR-0024 §5): the
/// account's current flags and kind, the credential's current scopes and
/// agent flag — never a copy taken at the request.
pub(crate) fn job_principal(
    user: UserId,
    account: Account,
    facts: &CredentialFacts,
) -> Result<Principal, ForgeError> {
    let kind: AccountKind = facts.kind;
    let (credential, agent) = match &facts.credential {
        CredentialState::Missing => return Err(revoked("credential_missing")),
        CredentialState::Session { usable: false }
        | CredentialState::Token { usable: false, .. } => {
            return Err(revoked("revoked_or_expired"))
        }
        CredentialState::Session { usable: true } => (Credential::Session, false),
        CredentialState::Token {
            scopes,
            is_agent,
            usable: true,
        } => (Credential::Token(scopes.clone()), *is_agent), // clone: the principal owns its scopes
    };
    Ok(
        Principal::authenticated(user, account.is_admin, account.active, credential, kind)
            .as_agent(agent),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_settings_allow_merge_commit_and_squash_only() {
        let s = MergeSettings::default();
        assert!(s.check(MergeMethod::MergeCommit).is_ok());
        assert!(s.check(MergeMethod::Squash).is_ok());
        let err = s.check(MergeMethod::FastForward).expect_err("not allowed");
        assert_eq!(err.code(), ErrorCode::MergeMethodNotAllowed);
        assert_eq!(s.default_method(), MergeMethod::MergeCommit);
        assert!(!s.delete_source_default());
        assert!(MergeSettings::allowing(&[]).is_none());
    }

    #[test]
    fn the_grace_must_exceed_twice_the_update_ref_timeout() {
        let t = MergeTimeouts::default();
        assert!(t.check().is_ok());
        assert_eq!(t.executing_after(), Duration::from_secs(630));
        assert_eq!(t.update_ref_window(), Duration::from_secs(570));
        let short = MergeTimeouts {
            grace: Duration::from_secs(60),
            ..t
        };
        assert_eq!(
            short.check().expect_err("short").code(),
            ErrorCode::ConfigInvalid
        );
    }

    #[test]
    fn a_revoked_or_missing_credential_never_makes_a_principal() {
        let account = Account {
            active: true,
            is_admin: false,
        };
        for state in [
            CredentialState::Missing,
            CredentialState::Session { usable: false },
            CredentialState::Token {
                scopes: vec![forge_core::Scope::MergeExecute],
                is_agent: false,
                usable: false,
            },
        ] {
            let facts = CredentialFacts {
                kind: forge_core::UserKind::Human,
                credential: state,
            };
            let err = job_principal(UserId::new(1), account, &facts).expect_err("refused");
            assert_eq!(err.code(), ErrorCode::Forbidden);
        }
        let agent = CredentialFacts {
            kind: forge_core::UserKind::Human,
            credential: CredentialState::Token {
                scopes: vec![forge_core::Scope::MergeExecute],
                is_agent: true,
                usable: true,
            },
        };
        assert!(job_principal(UserId::new(1), account, &agent)
            .expect("principal")
            .is_agent());
    }
}
