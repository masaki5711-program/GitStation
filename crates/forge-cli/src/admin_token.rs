//! `forge admin token issue | revoke | list` (step D3, ADR-0019). The first
//! mutating `forge admin` commands: the host operator is the trust root
//! (outside `authorize()`), `forge_db::tokens::issue` enforces the rules
//! itself, and in production running them is an approved runbook step.
//!
//! The token is printed once on stdout and never logged: the log event
//! carries the token id and public id only.

use std::time::Duration;

use forge_core::config::DatabaseConfig;
use forge_core::{RequestId, Scope};
use forge_db::change::{Actor, ChangeContext};
use forge_db::tokens::{self, NewToken, RevokeOutcome, TokenInfo};
use forge_db::{users, Db};
use forge_errors::{ErrorCode, ForgeError};

use crate::runtime::block_on;

/// What `forge admin token issue` was asked for.
#[derive(Debug, Clone)]
pub struct IssueArgs {
    pub username: String,
    /// Comma-separated scope names.
    pub scopes: String,
    /// `<days>d`; `None` = the default (90 days).
    pub expires_in: Option<String>,
    pub name: String,
    pub agent: bool,
}

fn invalid(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::TokenRequestInvalid).with_context("reason", reason)
}

/// `read_api,write_repository` → scopes; an unknown name is
/// `FG-AUTH-0008 reason=scope_unknown` (the name is not echoed).
fn parse_scopes(list: &str) -> Result<Vec<Scope>, ForgeError> {
    list.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| Scope::parse(s).ok_or_else(|| invalid("scope_unknown")))
        .collect()
}

/// `90d` → 90 days. Anything else is `reason=expires_malformed`
/// (the range itself is checked by `tokens::issue`).
fn parse_days(text: &str) -> Result<Duration, ForgeError> {
    text.strip_suffix('d')
        .and_then(|n| n.parse::<u64>().ok())
        .and_then(|days| days.checked_mul(24 * 3600))
        .map(Duration::from_secs)
        .ok_or_else(|| invalid("expires_malformed"))
}

/// The audit context of an operator command: the system actor, labelled
/// with the OS account that ran it.
fn context(request_id: &RequestId) -> ChangeContext {
    let os_user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| {
            // The audit row still names the CLI; say that the account is lost.
            tracing::warn!(
                target: "forge_cli",
                request_id = %request_id,
                "neither USERNAME nor USER is set; the audit row names forge-cli:unknown"
            );
            "unknown".to_owned()
        });
    let mut ctx = ChangeContext::new(Actor::System, request_id.clone());
    ctx.actor_label = Some(format!("forge-cli:{os_user}"));
    ctx
}

/// Connect, run, close.
fn with_db<T>(
    cfg: &DatabaseConfig,
    op: impl AsyncFnOnce(&Db) -> Result<T, ForgeError>,
) -> Result<T, ForgeError> {
    block_on(async {
        let db = Db::connect(cfg).await?;
        let result = op(&db).await;
        db.close().await;
        result
    })
}

pub fn issue(
    cfg: &DatabaseConfig,
    request_id: &RequestId,
    args: &IssueArgs,
) -> Result<String, ForgeError> {
    with_db(cfg, async |db| issue_with(db, request_id, args).await)
}

pub fn revoke(
    cfg: &DatabaseConfig,
    request_id: &RequestId,
    token_id: i64,
) -> Result<String, ForgeError> {
    with_db(cfg, async |db| revoke_with(db, request_id, token_id).await)
}

pub fn list(cfg: &DatabaseConfig, username: &str) -> Result<String, ForgeError> {
    with_db(cfg, async |db| list_with(db, username).await)
}

pub(crate) async fn issue_with(
    db: &Db,
    request_id: &RequestId,
    args: &IssueArgs,
) -> Result<String, ForgeError> {
    let scopes = parse_scopes(&args.scopes)?;
    let expires_in = args.expires_in.as_deref().map(parse_days).transpose()?;
    let user = users::id_by_username(db, &args.username).await?;
    let new = NewToken {
        user,
        name: args.name.clone(), // clone: NewToken owns its name
        scopes,
        expires_in,
        is_agent: args.agent,
    };
    let issued = tokens::issue(db, &context(request_id), &new).await?;
    tracing::info!(
        target: "forge_cli",
        event = "forge.admin.token.issued",
        request_id = %request_id,
        token_id = issued.token_id,
        public_id = issued.token.public_id().as_str(),
        user_id = user.get(),
        is_agent = args.agent,
        "token issued"
    );
    Ok(format!(
        "token       {}\ntoken_id    {}\npublic_id   {}\nexpires_at  {}\n\
         Store the token now: it is not shown again.\n",
        issued.token.reveal(),
        issued.token_id,
        issued.token.public_id().as_str(),
        issued.expires_at
    ))
}

pub(crate) async fn revoke_with(
    db: &Db,
    request_id: &RequestId,
    token_id: i64,
) -> Result<String, ForgeError> {
    let outcome = tokens::revoke(db, &context(request_id), token_id).await?;
    let state = match outcome {
        RevokeOutcome::Revoked => "revoked",
        RevokeOutcome::AlreadyRevoked => "already revoked (nothing changed)",
    };
    tracing::info!(
        target: "forge_cli",
        event = "forge.admin.token.revoked",
        request_id = %request_id,
        token_id,
        outcome = state,
        "token revoke"
    );
    Ok(format!("token {token_id}: {state}\n"))
}

pub(crate) async fn list_with(db: &Db, username: &str) -> Result<String, ForgeError> {
    let user = users::id_by_username(db, username).await?;
    let listed = tokens::list(db, user).await?;
    Ok(render(&listed))
}

/// One line per token; never the secret or its hash.
fn render(listed: &[TokenInfo]) -> String {
    let mut out =
        String::from("id\tpublic_id\tname\tscopes\texpires_at\trevoked_at\tlast_used_at\n");
    for t in listed {
        let scopes: Vec<&str> = t.scopes.iter().map(|s| s.as_str()).collect();
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            t.token_id,
            t.public_id,
            t.name,
            scopes.join(","),
            t.expires_at,
            t.revoked_at.as_deref().unwrap_or("-"),
            t.last_used_at.as_deref().unwrap_or("-"),
        ));
    }
    out
}

#[cfg(test)]
mod tests;
