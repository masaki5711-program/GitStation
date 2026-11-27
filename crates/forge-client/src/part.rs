//! `gstn part …` and `gstn agent keys` (plan 16 §6 A5, ADR-0032 Amendment
//! A5): the agent marketplace's catalog and public keys (`read_api`).
//!
//! - `gstn part list [--kind K] [--channel C] [--q TEXT]`: the catalog.
//! - `gstn part view <slug> [<version>]`: a part's versions, or one version
//!   with its files.
//! - `gstn part publish|promote|revoke`: these are decided only in a
//!   person's web session (ADR-0032 Amendment A4 §8: a token, an agent's
//!   included, is refused). gstn sends nothing and names the page.
//! - `gstn part import`: seeding parts, local only (`part_import`, A6).
//! - `gstn agent keys`: the registered public keys; `--json` prints the
//!   `agent-keys.json` that `gstn agent verify` reads from the gstn
//!   configuration directory.

use serde_json::{json, Value};

use crate::app::{emit, EnvFn, Io};
use crate::cli::{Cli, PartCommand};
use crate::error::Result;
use crate::hosting::{call, client};
use crate::http::Method;

/// The marketplace page of a part (plan 16 §6, A8).
fn page(slug: &str) -> String {
    format!("/-/marketplace/{slug}")
}

fn version_line(v: &Value) -> String {
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("-").to_owned();
    let capabilities = v
        .get("capabilities")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    format!(
        "{} {} [{}] {} {} {capabilities}",
        s("slug"),
        s("version"),
        s("channel"),
        s("state"),
        s("digest"),
    )
}

/// A slug or version safe in an API path (the server checks again):
/// `[0-9A-Za-z.-]`, 1..=64, not `.` or `..`.
fn path_word(raw: &str) -> Result<&str> {
    let ok = (1..=64).contains(&raw.len())
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        && raw != "."
        && raw != "..";
    if ok {
        Ok(raw)
    } else {
        Err(crate::error::ClientError::local(
            forge_errors::ErrorCode::CliInputFailed,
            "reason=invalid_slug_or_version",
        ))
    }
}

/// `gstn part …` (module docs).
pub fn part(cli: &Cli, cmd: &PartCommand, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    match cmd {
        PartCommand::Import {
            from,
            format,
            out,
            only,
            mcp_allowed_hosts,
            declare_from_scan,
        } => crate::part_import::import(
            cli,
            io,
            &crate::part_import::ImportArgs {
                from,
                format,
                out: out.as_deref(),
                only: only.as_deref(),
                mcp_allowed_hosts: mcp_allowed_hosts.as_deref().unwrap_or(&[]),
                declare_from_scan: declare_from_scan.as_deref().unwrap_or(&[]),
            },
        ),
        PartCommand::List { kind, channel, q } => {
            let c = client(cli, env, io)?;
            let mut query = Vec::new();
            for (key, value) in [("kind", kind), ("channel", channel), ("q", q)] {
                if let Some(v) = value {
                    query.push((key.to_owned(), v.clone())); // clone: the query owns it
                }
            }
            let out = call(&c, Method::Get, "/api/v1/agent-parts", &query, None)?;
            let lines: Vec<String> = out
                .get("parts")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .map(|p| match p.get("latest").filter(|l| !l.is_null()) {
                            Some(latest) => version_line(latest),
                            None => format!(
                                "{} (no live version)",
                                p.get("slug").and_then(Value::as_str).unwrap_or("-")
                            ),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let human = if lines.is_empty() {
                "no agent parts".to_owned()
            } else {
                lines.join("\n")
            };
            emit(io, cli, out, &human)
        }
        PartCommand::View { slug, version } => {
            let slug = path_word(slug)?;
            if let Some(v) = version {
                path_word(v)?;
            }
            let c = client(cli, env, io)?;
            match version {
                None => {
                    let path = format!("/api/v1/agent-parts/{slug}");
                    let out = call(&c, Method::Get, &path, &[], None)?;
                    let human = out
                        .get("versions")
                        .and_then(Value::as_array)
                        .map(|vs| vs.iter().map(version_line).collect::<Vec<_>>().join("\n"))
                        .unwrap_or_default();
                    emit(io, cli, out, &human)
                }
                Some(v) => {
                    let path = format!("/api/v1/agent-parts/{slug}/versions/{v}");
                    let out = call(&c, Method::Get, &path, &[], None)?;
                    let mut human = out.get("version").map(version_line).unwrap_or_default();
                    let files = out.get("files").and_then(Value::as_array);
                    for f in files.into_iter().flatten() {
                        let s = |k: &str| f.get(k).and_then(Value::as_str).unwrap_or("-");
                        human.push_str(&format!("\n  {} {} {}", s("mode"), s("sha256"), s("path")));
                    }
                    emit(io, cli, out, &human)
                }
            }
        }
        PartCommand::Publish { slug }
        | PartCommand::Promote { slug }
        | PartCommand::Revoke { slug } => {
            let what = match cmd {
                PartCommand::Publish { .. } => "publish",
                PartCommand::Promote { .. } => "promote",
                PartCommand::Revoke { .. }
                | PartCommand::List { .. }
                | PartCommand::View { .. }
                | PartCommand::Import { .. } => "revoke",
            };
            let path = page(slug);
            let human = format!(
                "{what} is decided only in a web session of a person (a token is refused): open {path} on the server"
            );
            let out = json!({
                "action": what,
                "web_session_only": true,
                "page": path,
            });
            emit(io, cli, out, &human)
        }
    }
}

/// `gstn agent keys` (module docs).
pub fn keys(cli: &Cli, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    let c = client(cli, env, io)?;
    let out = call(&c, Method::Get, "/api/v1/agent-keys", &[], None)?;
    let human = out
        .get("keys")
        .and_then(Value::as_array)
        .map(|keys| {
            keys.iter()
                .map(|k| {
                    let s = |f: &str| k.get(f).and_then(Value::as_str);
                    let state = if s("revoked_at").is_some() {
                        "revoked"
                    } else if s("retired_at").is_some() {
                        "retired"
                    } else {
                        "current"
                    };
                    format!(
                        "{} {} {state}",
                        s("kid").unwrap_or("-"),
                        s("public_key").unwrap_or("-")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    emit(io, cli, out, &human)
}
