//! `forge admin runner register | list | disable` (M4 C4, plan 14,
//! runbook `ci-runner`): the operator's way to register a CI runner until
//! the administration screen exists (the API, `POST /api/v1/admin/runners`,
//! takes an administrator's web session only, so `gstn` cannot).
//!
//! The host operator is the trust root (outside `authorize()`, like
//! `forge admin token issue`); in production running these is an approved
//! runbook step. `register` names the administrator the runner is recorded
//! as registered by (`--admin`, an active human administrator: the
//! table's `created_by`), `disable` the administrator who disables it
//! (`--admin`, the audit row's actor), and the audit row also carries
//! `forge-cli:<os user>`. The runner's token is printed once on stdout and
//! never logged: the log event carries the runner id and name.

use forge_core::config::DatabaseConfig;
use forge_core::RequestId;
use forge_db::change::{Actor, ChangeContext};
use forge_db::ci_runners::{self, NewRunner, RunnerUpdate, StoredRunner};
use forge_db::{users, Db};
use forge_errors::{ErrorCode, ForgeError};

use crate::admin_token::{context, with_db};

/// What `forge admin runner register` was asked for.
#[derive(Debug, Clone)]
pub struct RegisterArgs {
    pub name: String,
    /// Comma-separated tags.
    pub tags: Option<String>,
    pub description: Option<String>,
    /// The administrator recorded as the registrant.
    pub admin: String,
}

fn invalid(param: &'static str, reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::RequestInvalid)
        .with_context("param", param)
        .with_context("reason", reason)
}

/// `linux,podman` → sorted, deduplicated, each a valid tag.
fn parse_tags(raw: Option<&str>) -> Result<Vec<String>, ForgeError> {
    let mut tags: Vec<String> = raw
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect();
    if tags.iter().any(|t| !ci_runners::valid_name(t)) {
        return Err(invalid("tags", "invalid_tag"));
    }
    tags.sort();
    tags.dedup();
    if tags.len() > ci_runners::MAX_TAGS {
        return Err(invalid("tags", "too_many"));
    }
    Ok(tags)
}

pub fn register(
    cfg: &DatabaseConfig,
    request_id: &RequestId,
    args: &RegisterArgs,
) -> Result<String, ForgeError> {
    with_db(cfg, async |db| register_with(db, request_id, args).await)
}

pub fn list(cfg: &DatabaseConfig) -> Result<String, ForgeError> {
    with_db(cfg, async |db| list_with(db).await)
}

pub fn disable(
    cfg: &DatabaseConfig,
    request_id: &RequestId,
    runner_id: i64,
    admin: &str,
) -> Result<String, ForgeError> {
    with_db(cfg, async |db| {
        disable_with(db, request_id, runner_id, admin).await
    })
}

/// `name` as an active human administrator (the user recorded as the
/// actor); `FG-AUTH-0003 reason=<reason>` otherwise.
async fn administrator(
    db: &Db,
    name: &str,
    reason: &'static str,
) -> Result<forge_core::UserId, ForgeError> {
    let admin = users::id_by_username(db, name).await?;
    if !users::is_active_human_admin(db, admin).await? {
        return Err(ForgeError::new(ErrorCode::Forbidden)
            .with_context("reason", reason)
            .with_context("user_id", admin.get()));
    }
    Ok(admin)
}

pub(crate) async fn register_with(
    db: &Db,
    request_id: &RequestId,
    args: &RegisterArgs,
) -> Result<String, ForgeError> {
    if !ci_runners::valid_name(&args.name) {
        return Err(invalid("name", "invalid_name"));
    }
    let description = args.description.clone().unwrap_or_default(); // clone: NewRunner owns it
    if description.len() > ci_runners::MAX_DESCRIPTION || description.chars().any(char::is_control)
    {
        return Err(invalid("description", "invalid"));
    }
    let tags = parse_tags(args.tags.as_deref())?;
    let admin = administrator(db, &args.admin, "registrant_not_an_administrator").await?;
    let mut ctx: ChangeContext = context(request_id);
    ctx.actor = Actor::User(admin);
    let new = NewRunner {
        name: args.name.clone(), // clone: NewRunner owns its name
        description,
        tags,
    };
    let registered = ci_runners::register(db, &ctx, &new).await?;
    tracing::info!(
        target: "forge_cli",
        event = "forge.admin.runner.registered",
        request_id = %request_id,
        runner_id = registered.runner.id,
        name = %registered.runner.name,
        user_id = admin.get(),
        "runner registered"
    );
    Ok(format!(
        "runner_id  {}\nname       {}\ntags       {}\ntoken      {}\n\
         Put the token in the runner host's token file now (runbook ci-runner): it is not shown again.\n",
        registered.runner.id,
        registered.runner.name,
        registered.runner.tags.join(","),
        registered.token.reveal(),
    ))
}

pub(crate) async fn list_with(db: &Db) -> Result<String, ForgeError> {
    Ok(render(&ci_runners::list(db).await?))
}

pub(crate) async fn disable_with(
    db: &Db,
    request_id: &RequestId,
    runner_id: i64,
    admin: &str,
) -> Result<String, ForgeError> {
    let admin = administrator(db, admin, "operator_not_an_administrator").await?;
    if ci_runners::get(db, runner_id).await?.is_none() {
        return Err(ForgeError::new(ErrorCode::NotFound)
            .with_context("resource", "runner")
            .with_context("runner_id", runner_id));
    }
    // The audit row names the operating administrator (`--admin`) and the
    // operator's OS account.
    let mut ctx = context(request_id);
    ctx.actor = Actor::User(admin);
    let change = RunnerUpdate {
        enabled: Some(false),
        ..RunnerUpdate::default()
    };
    let updated = ci_runners::update(db, &ctx, runner_id, &change)
        .await?
        .ok_or_else(|| {
            ForgeError::new(ErrorCode::NotFound)
                .with_context("resource", "runner")
                .with_context("runner_id", runner_id)
        })?;
    tracing::info!(
        target: "forge_cli",
        event = "forge.admin.runner.disabled",
        request_id = %request_id,
        runner_id,
        user_id = admin.get(),
        "runner disabled"
    );
    Ok(format!(
        "runner {runner_id}: {}\n",
        if updated.enabled {
            "enabled"
        } else {
            "disabled (its token and its jobs' tokens are refused from now on)"
        }
    ))
}

/// One line per runner; never a token or its hash.
fn render(runners: &[StoredRunner]) -> String {
    let mut out = String::from("id\tname\tenabled\ttags\tlast_contact_at\tcreated_by\n");
    for r in runners {
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\n",
            r.id,
            r.name,
            r.enabled,
            r.tags.join(","),
            r.last_contact_at.as_deref().unwrap_or("-"),
            r.created_by,
        ));
    }
    out
}

#[cfg(test)]
#[path = "admin_runner_tests.rs"]
mod tests;
