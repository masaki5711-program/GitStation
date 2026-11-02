//! The caller's own account, read inside a write transaction (M1 phase 8b
//! MR7a): the facts `forge_auth::Authorized::reconfirm` decides on with the
//! re-read roles (`roles::lock_roles`), so a write never acts on an
//! authorization taken before its transaction (forge-hosting).

use forge_core::UserId;
use forge_errors::ForgeError;
use sqlx::postgres::PgConnection;

use crate::query_error;

/// The caller's account as the write transaction holds it (the user row
/// locked `FOR SHARE`, so a disable or an administrator change waits for
/// the write to commit). A missing row is not active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Account {
    pub active: bool,
    pub is_admin: bool,
}

/// Lock and read `user`'s row (`FOR SHARE`). Called after the target row
/// and the membership rows (the lock order of `projects::members`).
pub(crate) async fn lock_account(
    conn: &mut PgConnection,
    user: UserId,
) -> Result<Account, ForgeError> {
    let row: Option<(bool, bool)> = sqlx::query_as(
        "SELECT state = 'active', is_admin FROM forge.users WHERE id = $1 FOR SHARE",
    )
    .bind(user.get())
    .fetch_optional(conn)
    .await
    .map_err(|e| query_error("access.account", e))?;
    Ok(
        row.map_or(Account::default(), |(active, is_admin)| Account {
            active,
            is_admin,
        }),
    )
}
