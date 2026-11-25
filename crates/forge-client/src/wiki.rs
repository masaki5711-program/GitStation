//! `gstn wiki` (plan 20 W2, ADR-0040 §5): a project's wiki — an ordinary
//! project in the same namespace, linked to its owner project.
//!
//! - `gstn wiki create <id> [--path P] [--display-name N]`: one REST call
//!   (`POST /api/v1/projects/{id}/wiki`) creates the wiki, its first commit
//!   and its protection and links it; a refusal is `FG-KNOW-0003`, a
//!   failure after the creation `FG-KNOW-0004` (no link; runbook
//!   knowledge).
//! - `gstn wiki unlink <id> --yes`: `DELETE …/wiki`; the wiki project stays.

use serde_json::{json, Map, Value};

use crate::app::{emit, EnvFn, Io};
use crate::cli::{Cli, WikiCommand};
use crate::error::Result;
use crate::hosting::{call, client, confirm, text};
use crate::http::Method;

pub fn wiki(cli: &Cli, command: &WikiCommand, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    match command {
        WikiCommand::Create {
            id,
            path,
            display_name,
        } => {
            let mut body = Map::new();
            if let Some(p) = path {
                body.insert("path".to_owned(), json!(p));
            }
            if let Some(n) = display_name {
                body.insert("display_name".to_owned(), json!(n));
            }
            let c = client(cli, env, io)?;
            let out = call(
                &c,
                Method::Post,
                &format!("/api/v1/projects/{id}/wiki"),
                &[],
                Some(&Value::Object(body)),
            )?;
            let human = format!(
                "Created wiki {} (project {}) for project {}; first page {}",
                text(&out, "wiki_path"),
                text(&out, "wiki_project_id"),
                text(&out, "project_id"),
                text(&out, "first_page"),
            );
            emit(io, cli, out, &human)
        }
        WikiCommand::Unlink { id, yes } => {
            confirm(*yes, "unlinking a wiki")?;
            let c = client(cli, env, io)?;
            let out = call(
                &c,
                Method::Delete,
                &format!("/api/v1/projects/{id}/wiki"),
                &[],
                None,
            )?;
            let human = format!(
                "Unlinked wiki project {} from project {}; the wiki project stays",
                text(&out, "wiki_project_id"),
                text(&out, "project_id"),
            );
            emit(io, cli, out, &human)
        }
    }
}
