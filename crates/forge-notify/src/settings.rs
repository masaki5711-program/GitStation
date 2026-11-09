//! A person's own notification settings (M3 N1): what the routes
//! `GET|PUT /api/v1/user/notification-preferences` read and write. The
//! decision is the route's: both commands take the proof of
//! `NotificationPreferencesRead` / `…Write` on the caller's own settings
//! (`forge_auth::authorize` allows nobody else, administrators included),
//! and act for the proof's owner — never for a user taken from the request.

use forge_auth::{Action, Authorized, NotificationPreferencesCtx};
use forge_core::UserId;
use forge_db::notify::{self, Preferences};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};

/// The settings commands.
#[derive(Clone)]
pub struct Settings {
    db: Db,
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

fn owner(proof: &Authorized<NotificationPreferencesCtx>) -> UserId {
    *proof.id()
}

impl Settings {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// The database handle (the in-app notifications use the same one).
    pub fn db(&self) -> Db {
        self.db.clone() // clone: a pool handle
    }

    /// `GET …/notification-preferences` (`NotificationPreferencesRead`).
    pub async fn read(
        &self,
        proof: &Authorized<NotificationPreferencesCtx>,
    ) -> Result<Preferences, ForgeError> {
        require(proof.action(), Action::NotificationPreferencesRead)?;
        notify::preferences(&self.db, owner(proof)).await
    }

    /// The owner's stored settings, for a `PUT` that leaves fields out
    /// (`NotificationPreferencesWrite`: the in-app switches of M3 U4 are
    /// kept when a client of N1 sends only the mail fields).
    pub async fn current(
        &self,
        proof: &Authorized<NotificationPreferencesCtx>,
    ) -> Result<Preferences, ForgeError> {
        require(proof.action(), Action::NotificationPreferencesWrite)?;
        notify::preferences(&self.db, owner(proof)).await
    }

    /// `PUT …/notification-preferences` (`NotificationPreferencesWrite`):
    /// replace the owner's settings; returns what is stored.
    pub async fn replace(
        &self,
        proof: &Authorized<NotificationPreferencesCtx>,
        prefs: &Preferences,
    ) -> Result<Preferences, ForgeError> {
        require(proof.action(), Action::NotificationPreferencesWrite)?;
        notify::set_preferences(&self.db, owner(proof), prefs).await
    }
}
