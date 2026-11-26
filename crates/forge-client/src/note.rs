//! `gstn note` (plan 20 N1, ADR-0040 §6): your personal notes — the
//! private project `<you>/notes`.
//!
//! - `gstn note show`: `GET /api/v1/user/notes`; `404 FG-REPO-0005` without
//!   notes, `404 FG-KNOW-0005` when the notes project was moved or deleted.
//! - `gstn note create`: `POST /api/v1/user/notes`; `409 FG-KNOW-0006`
//!   when refused, `500 FG-KNOW-0007` when a step after the creation
//!   failed (no pointer; runbook knowledge).
//! - `gstn note new [--title T] [--body TEXT | < file]`: a quick note —
//!   reads the notes, resolves `main`'s head, then ONE direct commit of
//!   `N-YYYYMMDD-HHMMSS.md` (UTC; `forge_doc::note::quick_note`) through
//!   the simple editor's commit (`POST …/repository/commits/single-file`),
//!   which decides `RepoPush` and scans for secrets like a push. An
//!   existing note of the same second is never replaced (checked at the
//!   head first; a later one is the server's `FG-GIT-0027`).

use std::io::Read as _;
use std::time::{SystemTime, UNIX_EPOCH};

use forge_doc::note;
use serde_json::{json, Value};

use crate::app::{emit, usage, EnvFn, Io};
use crate::cli::{Cli, NoteCommand};
use crate::error::{Exit, Result};
use crate::hosting::{call, client, text};
use crate::http::Method;

/// The notes pointer.
const NOTES: &str = "/api/v1/user/notes";
/// The largest note body read from standard input (the server's limit).
const MAX_BODY_BYTES: u64 = 512 * 1024;

fn notes_human(out: &Value) -> String {
    format!(
        "Notes {} (project {}) on {}",
        text(out, "path"),
        text(out, "project_id"),
        text(out, "default_branch"),
    )
}

fn now_seconds() -> Result<i64> {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| usage("clock_before_1970"))?
        .as_secs();
    i64::try_from(secs).map_err(|_| usage("clock_out_of_range"))
}

fn read_body(body: Option<&String>, io: &mut Io<'_>) -> Result<String> {
    if let Some(b) = body {
        return Ok(b.clone()); // clone: the note owns its text
    }
    if io.stdin_tty {
        return Err(usage(
            "stdin_is_a_terminal (give --body TEXT or pipe the text: gstn note new < note.md)",
        ));
    }
    let mut content = String::new();
    io.stdin
        .take(MAX_BODY_BYTES + 1)
        .read_to_string(&mut content)
        .map_err(|_| usage("stdin_not_utf8_text"))?;
    if content.len() as u64 > MAX_BODY_BYTES {
        return Err(usage("content_too_large (at most 512 KiB)"));
    }
    Ok(content)
}

pub fn note(cli: &Cli, command: &NoteCommand, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    match command {
        NoteCommand::Show => {
            let c = client(cli, env, io)?;
            let out = call(&c, Method::Get, NOTES, &[], None)?;
            let human = notes_human(&out);
            emit(io, cli, out, &human)
        }
        NoteCommand::Create => {
            let c = client(cli, env, io)?;
            let out = call(&c, Method::Post, NOTES, &[], Some(&json!({})))?;
            let human = format!("Created {}", notes_human(&out));
            emit(io, cli, out, &human)
        }
        NoteCommand::New { title, body } => {
            let body = read_body(body.as_ref(), io)?;
            let id = note::note_id(now_seconds()?).ok_or_else(|| usage("clock_out_of_range"))?;
            let title = title
                .clone() // clone: the note owns its title
                .unwrap_or_else(|| note::default_title(&body, &id));
            let c = client(cli, env, io)?;
            let notes = call(&c, Method::Get, NOTES, &[], None)?;
            let project = notes
                .get("project_id")
                .and_then(Value::as_i64)
                .ok_or_else(|| usage("notes_answer_without_project_id"))?;
            let branch = text(&notes, "default_branch");
            let owner = text(&notes, "owner");
            let resolved = call(
                &c,
                Method::Get,
                &format!("/api/v1/projects/{project}/repository/resolve"),
                &[("spec".to_owned(), branch.clone())], // clone: the commit names it too
                None,
            )?;
            let start = resolved
                .get("ref")
                .and_then(|r| r.get("commit"))
                .and_then(Value::as_str)
                .ok_or_else(|| usage("resolve_answer_without_commit"))?
                .to_owned();
            let path = note::note_path(&id);
            // Never replace a note: the simple editor's commit replaces a
            // file that exists at `start_sha` (a second note in the same
            // second); a file created after it is refused by the server
            // (FG-GIT-0027).
            let existing = call(
                &c,
                Method::Get,
                &format!("/api/v1/projects/{project}/repository/blob"),
                &[
                    ("ref".to_owned(), start.clone()), // clone: the commit names it too
                    ("path".to_owned(), path.clone()), // clone: as above
                ],
                None,
            );
            match existing {
                Ok(_) => return Err(usage("note_exists (one note per second: try again)")),
                Err(e) if e.exit() == Exit::NotFound => {}
                Err(e) => return Err(e),
            }
            let content = note::quick_note(&id, &title, &owner, &body);
            let out = call(
                &c,
                Method::Post,
                &format!("/api/v1/projects/{project}/repository/commits/single-file"),
                &[],
                Some(&json!({
                    "branch": branch,
                    "start_sha": start,
                    "path": path,
                    "content": content,
                    "message": format!("Add note {id}"),
                })),
            )?;
            let human = format!(
                "Saved note {id} ({path}) as {} on {}",
                text(&out, "commit_sha"),
                text(&out, "branch"),
            );
            emit(io, cli, out, &human)
        }
    }
}
