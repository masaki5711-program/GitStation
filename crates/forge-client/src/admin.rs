//! `gstn admin service-account …` (M3 S0, ADR-0019 Addendum S0): the
//! token-usable half of the service account administration — list, view,
//! update, its tokens, revoke one (an `admin`-scoped token). Creating an
//! account and issuing its token are an administrator's web session only
//! (the server refuses every token, `FG-AUTH-0002 session_required`), so
//! gstn has no command for them (`coverage.rs`: browser only).
//!
//! Each command is one REST API v1 call; ids are validated by clap and only
//! formatted into the path as integers; free text goes in the JSON body.

use crate::app::{emit, EnvFn, Io};
use crate::cli::{AdminCommand, Cli, ServiceAccountCommand};
use crate::error::Result;
use crate::hosting::{call, client, confirm, list_human, text};
use crate::http::Method;
use serde_json::{json, Map, Value};

const ACCOUNT_KEYS: [&str; 5] = [
    "id",
    "username",
    "state",
    "active_tokens",
    "last_token_expiry",
];
const TOKEN_KEYS: [&str; 6] = [
    "id",
    "name",
    "scopes",
    "expires_at",
    "revoked_at",
    "issued_by_user_id",
];

/// The `namespaces` of an answer as `ci/, kanflow/` (`none`).
fn namespaces(body: &Value) -> String {
    let names: Vec<String> = body
        .get("namespaces")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(|n| format!("{}/", crate::error::sanitize(n)))
                .collect()
        })
        .unwrap_or_default();
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}

pub fn admin(cli: &Cli, cmd: &AdminCommand, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    match cmd {
        AdminCommand::ServiceAccount { command } => service_account(cli, command, env, io),
    }
}

/// One account as lines of text.
fn account_human(v: &Value) -> String {
    let owner = v.get("owner").map_or_else(
        || "-".to_string(),
        |o| format!("{} ({})", text(o, "username"), text(o, "id")),
    );
    [
        format!("id: {}", text(v, "id")),
        format!("username: {}", text(v, "username")),
        format!("display_name: {}", text(v, "display_name")),
        format!("state: {}", text(v, "state")),
        format!("owner: {owner}"),
        format!("purpose: {}", text(v, "purpose")),
        format!("active_tokens: {}", text(v, "active_tokens")),
        format!("last_token_expiry: {}", text(v, "last_token_expiry")),
    ]
    .join("\n")
}

fn service_account(
    cli: &Cli,
    cmd: &ServiceAccountCommand,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    match cmd {
        ServiceAccountCommand::List => {
            let c = client(cli, env, io)?;
            let body = call(&c, Method::Get, "/api/v1/admin/service-accounts", &[], None)?;
            let human = list_human(&body, "items", "-", "", &ACCOUNT_KEYS);
            emit(io, cli, body, &human)
        }
        ServiceAccountCommand::View { id } => {
            let c = client(cli, env, io)?;
            let body = call(
                &c,
                Method::Get,
                &format!("/api/v1/admin/service-accounts/{id}"),
                &[],
                None,
            )?;
            let human = account_human(&body);
            emit(io, cli, body, &human)
        }
        ServiceAccountCommand::Update {
            id,
            display_name,
            purpose,
            owner,
        } => {
            let mut m = Map::new();
            if let Some(v) = display_name {
                m.insert("display_name".to_string(), json!(v));
            }
            if let Some(v) = purpose {
                m.insert("purpose".to_string(), json!(v));
            }
            if let Some(v) = owner {
                m.insert("owner_id".to_string(), json!(v));
            }
            if m.is_empty() {
                return Err(crate::app::usage(
                    "nothing_to_change (pass --display-name, --purpose or --owner)",
                ));
            }
            let c = client(cli, env, io)?;
            let body = call(
                &c,
                Method::Patch,
                &format!("/api/v1/admin/service-accounts/{id}"),
                &[],
                Some(&Value::Object(m)),
            )?;
            let human = format!("Service account {id}: {}", text(&body, "outcome"));
            emit(io, cli, body, &human)
        }
        ServiceAccountCommand::Tokens { id } => {
            let c = client(cli, env, io)?;
            let body = call(
                &c,
                Method::Get,
                &format!("/api/v1/admin/service-accounts/{id}/tokens"),
                &[],
                None,
            )?;
            let human = list_human(&body, "items", "-", "", &TOKEN_KEYS);
            emit(io, cli, body, &human)
        }
        ServiceAccountCommand::CheckNamespaces { id } => {
            let c = client(cli, env, io)?;
            let body = call(
                &c,
                Method::Get,
                &format!("/api/v1/admin/service-accounts/{id}/check-namespaces"),
                &[],
                None,
            )?;
            let human = format!("Service account {id} reports: {}", namespaces(&body));
            emit(io, cli, body, &human)
        }
        ServiceAccountCommand::GrantChecks { id, namespace }
        | ServiceAccountCommand::RevokeChecks { id, namespace } => {
            let grant = matches!(cmd, ServiceAccountCommand::GrantChecks { .. });
            let c = client(cli, env, io)?;
            let body = call(
                &c,
                if grant { Method::Put } else { Method::Delete },
                &format!("/api/v1/admin/service-accounts/{id}/check-namespaces/{namespace}"),
                &[],
                None,
            )?;
            let verb = match (grant, body.get("changed") == Some(&Value::Bool(true))) {
                (true, true) => "granted",
                (false, true) => "revoked",
                (_, false) => "unchanged",
            };
            let human = format!(
                "Service account {id}: {namespace}/ {verb}; reports: {}",
                namespaces(&body)
            );
            emit(io, cli, body, &human)
        }
        ServiceAccountCommand::RevokeToken { id, token_id, yes } => {
            confirm(*yes, "revoking a service account's token")?;
            let c = client(cli, env, io)?;
            call(
                &c,
                Method::Delete,
                &format!("/api/v1/admin/service-accounts/{id}/tokens/{token_id}"),
                &[],
                None,
            )?;
            emit(
                io,
                cli,
                json!({"service_account_id": id, "id": token_id, "revoked": true}),
                &format!("Revoked token {token_id} of service account {id}"),
            )
        }
    }
}
