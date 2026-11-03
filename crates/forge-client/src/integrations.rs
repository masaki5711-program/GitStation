//! `gstn admin integration …` and `gstn integration-binding …` (M3 I1a,
//! plan 11 §5 I1): the token-usable half of the integration routes —
//! listing and viewing integrations and their bindings (administrators, an
//! `admin`-scoped token of a person), and a project's binding (read as a
//! Reporter; set and remove as a Maintainer). Creating and changing an
//! integration are an administrator's web session only (the server refuses
//! every token, `FG-AUTH-0002 session_required`), so gstn has no command
//! for them (`coverage.rs`: browser only).
//!
//! Each command is one REST API v1 call; ids are validated by clap and only
//! formatted into the path as integers; the external project id goes in the
//! JSON body.

use crate::app::{emit, EnvFn, Io};
use crate::cli::{Cli, IntegrationBindingCommand, IntegrationCommand};
use crate::error::Result;
use crate::hosting::{call, client, confirm, list_human, text};
use crate::http::Method;
use serde_json::{json, Value};

const INTEGRATION_KEYS: [&str; 4] = ["id", "kind", "enabled", "bindings"];
const BINDING_KEYS: [&str; 4] = [
    "project_id",
    "integration_id",
    "external_project_id",
    "granted_membership",
];

fn binding_path(project: i64, integration: i64) -> String {
    format!("/api/v1/projects/{project}/integration-bindings/{integration}")
}

/// One integration as lines of text.
fn integration_human(v: &Value) -> String {
    let account = v.get("service_account").map_or_else(
        || "-".to_string(),
        |a| format!("{} ({})", text(a, "username"), text(a, "id")),
    );
    let types = v
        .get("event_types")
        .and_then(Value::as_array)
        .map(|t| {
            t.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    [
        format!("id: {}", text(v, "id")),
        format!("kind: {}", text(v, "kind")),
        format!("service_account: {account}"),
        format!("enabled: {}", text(v, "enabled")),
        format!("event_types: {types}"),
        format!("bindings: {}", text(v, "bindings")),
    ]
    .join("\n")
}

pub fn integration(
    cli: &Cli,
    cmd: &IntegrationCommand,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    match cmd {
        IntegrationCommand::List => {
            let c = client(cli, env, io)?;
            let body = call(&c, Method::Get, "/api/v1/admin/integrations", &[], None)?;
            let human = list_human(&body, "items", "-", "", &INTEGRATION_KEYS);
            emit(io, cli, body, &human)
        }
        IntegrationCommand::View { id } => {
            let c = client(cli, env, io)?;
            let body = call(
                &c,
                Method::Get,
                &format!("/api/v1/admin/integrations/{id}"),
                &[],
                None,
            )?;
            let human = integration_human(&body);
            emit(io, cli, body, &human)
        }
        IntegrationCommand::Bindings { id } => {
            let c = client(cli, env, io)?;
            let body = call(
                &c,
                Method::Get,
                &format!("/api/v1/admin/integrations/{id}/bindings"),
                &[],
                None,
            )?;
            let human = list_human(&body, "items", "-", "", &BINDING_KEYS);
            emit(io, cli, body, &human)
        }
    }
}

pub fn binding(
    cli: &Cli,
    cmd: &IntegrationBindingCommand,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    match cmd {
        IntegrationBindingCommand::List { project } => {
            let c = client(cli, env, io)?;
            let body = call(
                &c,
                Method::Get,
                &format!("/api/v1/projects/{project}/integration-bindings"),
                &[],
                None,
            )?;
            let human = list_human(&body, "items", "-", "", &BINDING_KEYS);
            emit(io, cli, body, &human)
        }
        IntegrationBindingCommand::Set {
            project,
            integration,
            external_project,
        } => {
            let c = client(cli, env, io)?;
            let body = call(
                &c,
                Method::Put,
                &binding_path(*project, *integration),
                &[],
                Some(&json!({"external_project_id": external_project})),
            )?;
            let human = format!(
                "Project {project} and integration {integration}: {}",
                text(&body, "outcome")
            );
            emit(io, cli, body, &human)
        }
        IntegrationBindingCommand::Remove {
            project,
            integration,
            yes,
        } => {
            confirm(*yes, "unbinding a project from an integration")?;
            let c = client(cli, env, io)?;
            call(
                &c,
                Method::Delete,
                &binding_path(*project, *integration),
                &[],
                None,
            )?;
            emit(
                io,
                cli,
                json!({"project_id": project, "integration_id": integration, "unbound": true}),
                &format!("Unbound project {project} from integration {integration}"),
            )
        }
    }
}
