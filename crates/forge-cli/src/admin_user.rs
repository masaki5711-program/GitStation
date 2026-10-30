//! `forge admin user disable` and `forge admin bootstrap-admin` (D3 MR2,
//! ADR-0019 §9–§11 and Amendment). Mutating operator commands: the host
//! operator is the trust root (outside `authorize()`), `forge_db::users`
//! enforces the administration guard itself, and in production running them
//! is an approved runbook step (ops/runbooks/user-offboarding.md).
//!
//! `--force` exists only here: HTTP never gets a way past the guard. The log
//! events carry user ids and flags, never names.

use forge_core::config::DatabaseConfig;
use forge_core::RequestId;
use forge_db::users::{self, Bootstrap, DisableOutcome, Guard, NewUser};
use forge_db::Db;
use forge_errors::ForgeError;

use crate::admin_token::{context, with_db};

/// What `forge admin bootstrap-admin` was asked for.
#[derive(Debug, Clone)]
pub struct BootstrapArgs {
    pub username: String,
    /// `--create`: the user does not exist yet.
    pub create: Option<NewAdmin>,
}

/// The new administrator of `bootstrap-admin --create`.
#[derive(Debug, Clone)]
pub struct NewAdmin {
    pub email: String,
    /// Default: the username.
    pub display_name: Option<String>,
}

pub fn disable(
    cfg: &DatabaseConfig,
    request_id: &RequestId,
    username: &str,
    force: bool,
) -> Result<String, ForgeError> {
    with_db(cfg, async |db| {
        disable_with_db(db, request_id, username, force).await
    })
}

pub fn bootstrap_admin(
    cfg: &DatabaseConfig,
    request_id: &RequestId,
    args: &BootstrapArgs,
) -> Result<String, ForgeError> {
    with_db(cfg, async |db| bootstrap_with(db, request_id, args).await)
}

pub(crate) async fn disable_with_db(
    db: &Db,
    request_id: &RequestId,
    username: &str,
    force: bool,
) -> Result<String, ForgeError> {
    let user = users::id_by_username(db, username).await?;
    let guard = if force { Guard::Force } else { Guard::Enforce };
    let outcome = users::disable_with(db, &context(request_id), user, guard).await?;
    let (state, ownerless_groups, ownerless_projects, left_without_admin) = match outcome {
        DisableOutcome::Disabled => ("disabled", 0, 0, false),
        DisableOutcome::DisabledPastGuard {
            ownerless_groups,
            ownerless_projects,
            left_without_admin,
        } => (
            "disabled_past_guard",
            ownerless_groups,
            ownerless_projects,
            left_without_admin,
        ),
        DisableOutcome::AlreadyDisabled => ("already_disabled", 0, 0, false),
    };
    if matches!(outcome, DisableOutcome::DisabledPastGuard { .. }) {
        // Forge is now short of an administrator or a group Owner (doctor:
        // FG-AUTH-0010) until it is recovered: not a routine event.
        tracing::warn!(
            target: "forge_cli",
            event = "forge.admin.user.disabled",
            request_id = %request_id,
            user_id = user.get(),
            forced = force,
            outcome = state,
            ownerless_groups,
            ownerless_projects,
            left_without_admin,
            "user disabled past the administration guard"
        );
    } else {
        tracing::info!(
            target: "forge_cli",
            event = "forge.admin.user.disabled",
            request_id = %request_id,
            user_id = user.get(),
            forced = force,
            outcome = state,
            ownerless_groups,
            ownerless_projects,
            left_without_admin,
            "user disable"
        );
    }
    Ok(match outcome {
        DisableOutcome::Disabled => format!("user {username}: disabled\n"),
        DisableOutcome::AlreadyDisabled => {
            format!("user {username}: already disabled (nothing changed)\n")
        }
        DisableOutcome::DisabledPastGuard {
            ownerless_groups,
            ownerless_projects,
            left_without_admin,
        } => {
            let mut out = format!("user {username}: disabled (forced)\n");
            if ownerless_groups > 0 {
                out.push_str(&format!(
                    "  {ownerless_groups} top-level group(s) now have no direct active Owner\n"
                ));
            }
            if ownerless_projects > 0 {
                out.push_str(&format!(
                    "  {ownerless_projects} project(s) now have no active Owner\n"
                ));
            }
            if left_without_admin {
                out.push_str(
                    "  no active human administrator is left: \
                     recover with `forge admin bootstrap-admin`\n",
                );
            }
            out.push_str(
                "  `forge doctor` reports this as FG-AUTH-0010 until it is recovered \
                 (ops/runbooks/user-offboarding.md)\n",
            );
            out
        }
    })
}

pub(crate) async fn bootstrap_with(
    db: &Db,
    request_id: &RequestId,
    args: &BootstrapArgs,
) -> Result<String, ForgeError> {
    let request = match &args.create {
        None => Bootstrap::Promote(users::id_by_username(db, &args.username).await?),
        Some(new) => Bootstrap::Create(NewUser::human(
            &args.username,
            new.display_name.as_deref().unwrap_or(&args.username),
            &new.email,
        )),
    };
    let done = users::bootstrap_admin(db, &context(request_id), &request).await?;
    let created = done.namespace_id.is_some();
    tracing::info!(
        target: "forge_cli",
        event = "forge.admin.user.admin_bootstrapped",
        request_id = %request_id,
        user_id = done.user_id.get(),
        created,
        "administrator bootstrapped"
    );
    let how = if created { "created" } else { "promoted" };
    Ok(format!("user {}: administrator ({how})\n", args.username))
}

#[cfg(test)]
mod tests;
