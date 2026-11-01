//! The hosting commands: projects, groups, members, protected refs and
//! namespace resolution. Each is one REST API v1 call (a list: one page, or
//! every page with `--all`, bounded by `PageLimits`).
//!
//! Rules kept here, each with a test in `tests/hosting.rs`:
//! - ids are validated by clap (`cli::parse_id`) and only ever formatted into
//!   the path as integers; free text goes in a JSON body or a query value
//!   (percent-encoded by the URL builder), never into a path;
//! - a destructive command without `--yes` fails before any token read or
//!   request (gstn never prompts: it is driven by scripts and agents);
//! - a failure is the server's `user_message` + `request_id` (see `error.rs`).

use crate::app::{config_for, emit, require_token, usage, EnvFn, Io};
use crate::cli::{
    Cli, GroupCommand, ListArgs, MemberCommand, MemberTarget, NamespaceCommand, ProjectCommand,
    ProtectedRefCommand, UserCommand,
};
use crate::error::{sanitize, ClientError, Result};
use crate::http::{ApiClient, Method, PageLimits, PageShape, LIST_PAGES, MEMBER_PAGES};
use forge_errors::ErrorCode;
use serde_json::{json, Map, Value};

/// `--all` without `--limit` asks for the largest page, to need fewer calls.
const ALL_PAGE_SIZE: u32 = 100;

fn client(cli: &Cli, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<ApiClient> {
    let cfg = config_for(cli, env)?;
    let (token, _source) = require_token(&cfg, env, io)?;
    ApiClient::new(&cfg, Some(token))
}

/// A destructive command needs `--yes`; checked before anything else happens.
fn confirm(yes: bool, what: &str) -> Result<()> {
    if yes {
        Ok(())
    } else {
        Err(usage(format!(
            "confirmation_required ({what}; gstn never prompts: pass --yes to proceed)"
        )))
    }
}

fn call(
    c: &ApiClient,
    method: Method,
    path: &str,
    query: &[(String, String)],
    body: Option<&Value>,
) -> Result<Value> {
    Ok(c.request(method, path, query, body)?.body)
}

/// One page, or (`--all`) every page merged.
fn fetch_list(
    c: &ApiClient,
    path: &str,
    limit: Option<u32>,
    cursor: Option<(&'static str, String)>,
    all: bool,
    shape: &PageShape,
) -> Result<Value> {
    let mut query: Vec<(String, String)> = Vec::new();
    match (limit, all) {
        (Some(n), _) => query.push(("limit".to_string(), n.to_string())),
        (None, true) => query.push(("limit".to_string(), ALL_PAGE_SIZE.to_string())),
        (None, false) => {}
    }
    if let Some((k, v)) = cursor {
        query.push((k.to_string(), v));
    }
    if all {
        Ok(
            c.get_all_keyset(path, &query, PageLimits::default(), shape)?
                .body,
        )
    } else {
        call(c, Method::Get, path, &query, None)
    }
}

// ── rendering ──────────────────────────────────────────────────────────

/// A field as one line of text: strings and numbers as they are, `null` as
/// `-`. Server text is sanitized (it reaches a terminal).
fn text(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => sanitize(s),
        Some(Value::Null) | None => "-".to_string(),
        Some(other) => sanitize(&other.to_string()),
    }
}

fn row(v: &Value, keys: &[&str]) -> String {
    keys.iter()
        .map(|k| text(v, k))
        .collect::<Vec<_>>()
        .join("  ")
}

fn detail(v: &Value, keys: &[&str]) -> String {
    keys.iter()
        .map(|k| format!("{k}: {}", text(v, k)))
        .collect::<Vec<_>>()
        .join("\n")
}

fn list_human(body: &Value, items_key: &str, next_key: &str, flag: &str, keys: &[&str]) -> String {
    let mut lines: Vec<String> = body
        .get(items_key)
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|i| row(i, keys)).collect())
        .unwrap_or_default();
    if lines.is_empty() {
        lines.push("(none)".to_string());
    }
    match body.get(next_key) {
        Some(Value::Null) | None => {}
        Some(next) => {
            let n = next
                .as_str()
                .map_or_else(|| next.to_string(), str::to_string);
            lines.push(format!("more: {flag} {}", sanitize(&n)));
        }
    }
    lines.join("\n")
}

const PROJECT_KEYS: [&str; 7] = [
    "id",
    "path",
    "display_name",
    "kind",
    "visibility",
    "role",
    "namespace_id",
];
const GROUP_KEYS: [&str; 6] = ["id", "path", "display_name", "role", "namespace_id", "name"];
const MEMBER_KEYS: [&str; 5] = ["user_id", "username", "display_name", "role", "active"];

fn put_opt(m: &mut Map<String, Value>, key: &str, v: Option<&String>) {
    if let Some(s) = v {
        m.insert(key.to_string(), Value::String(s.clone()));
    }
}

/// `edit` sends only what was given; nothing given is a usage error.
fn edit_body(display_name: Option<&String>, description: Option<&String>) -> Result<Value> {
    let mut m = Map::new();
    put_opt(&mut m, "display_name", display_name);
    put_opt(&mut m, "description", description);
    if m.is_empty() {
        return Err(usage(
            "nothing_to_change (pass --display-name or --description)",
        ));
    }
    Ok(Value::Object(m))
}

// ── project ────────────────────────────────────────────────────────────

pub fn project(cli: &Cli, cmd: &ProjectCommand, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    match cmd {
        ProjectCommand::List(a) => {
            let body = list(cli, env, io, "/api/v1/projects", a)?;
            let human = list_human(&body, "items", "next_cursor", "--cursor", &PROJECT_KEYS);
            emit(io, cli, body, &human)
        }
        ProjectCommand::View(a) => {
            let c = client(cli, env, io)?;
            let body = call(
                &c,
                Method::Get,
                &format!("/api/v1/projects/{}", a.id),
                &[],
                None,
            )?;
            let human = detail(
                &body,
                &[
                    "id",
                    "path",
                    "display_name",
                    "description",
                    "kind",
                    "visibility",
                    "role",
                    "namespace_id",
                    "created_at",
                ],
            );
            emit(io, cli, body, &human)
        }
        ProjectCommand::Create {
            namespace_id,
            path,
            display_name,
            description,
            kind,
            visibility,
        } => {
            let mut m = Map::new();
            m.insert("namespace_id".to_string(), json!(namespace_id));
            m.insert("path".to_string(), json!(path));
            m.insert("display_name".to_string(), json!(display_name));
            put_opt(&mut m, "description", description.as_ref());
            if let Some(k) = kind {
                m.insert("kind".to_string(), json!(k.as_str()));
            }
            if let Some(v) = visibility {
                m.insert("visibility".to_string(), json!(v.as_str()));
            }
            let c = client(cli, env, io)?;
            let body = call(
                &c,
                Method::Post,
                "/api/v1/projects",
                &[],
                Some(&Value::Object(m)),
            )?;
            let human = format!(
                "Created project {} ({})",
                text(&body, "id"),
                text(&body, "path")
            );
            emit(io, cli, body, &human)
        }
        ProjectCommand::Edit {
            id,
            display_name,
            description,
        } => {
            let req = edit_body(display_name.as_ref(), description.as_ref())?;
            let c = client(cli, env, io)?;
            let body = call(
                &c,
                Method::Patch,
                &format!("/api/v1/projects/{id}"),
                &[],
                Some(&req),
            )?;
            let human = format!(
                "Updated project {} ({})",
                text(&body, "id"),
                text(&body, "path")
            );
            emit(io, cli, body, &human)
        }
        ProjectCommand::Visibility {
            id,
            visibility,
            yes,
        } => {
            confirm(*yes, "changing a project's visibility")?;
            let req = json!({"visibility": visibility.as_str()});
            let c = client(cli, env, io)?;
            let body = call(
                &c,
                Method::Put,
                &format!("/api/v1/projects/{id}/visibility"),
                &[],
                Some(&req),
            )?;
            let human = format!(
                "Project {} ({}) is now {}",
                text(&body, "id"),
                text(&body, "path"),
                text(&body, "visibility")
            );
            emit(io, cli, body, &human)
        }
        ProjectCommand::Move {
            id,
            namespace_id,
            path,
            reclaim,
            yes,
        } => {
            confirm(*yes, "moving or renaming a project")?;
            if namespace_id.is_none() && path.is_none() {
                return Err(usage(
                    "nothing_to_change (pass --namespace-id and/or --path)",
                ));
            }
            let mut m = Map::new();
            if let Some(n) = namespace_id {
                m.insert("namespace_id".to_string(), json!(n));
            }
            put_opt(&mut m, "path", path.as_ref());
            if *reclaim {
                m.insert("reclaim".to_string(), json!(true));
            }
            let c = client(cli, env, io)?;
            let body = call(
                &c,
                Method::Post,
                &format!("/api/v1/projects/{id}/transfer"),
                &[],
                Some(&Value::Object(m)),
            )?;
            let human = move_human("project", &body);
            emit(io, cli, body, &human)
        }
        ProjectCommand::Delete { id, yes } => {
            confirm(*yes, "deleting a project")?;
            let c = client(cli, env, io)?;
            let body = call(
                &c,
                Method::Delete,
                &format!("/api/v1/projects/{id}"),
                &[],
                None,
            )?;
            let human = format!(
                "Deleted project {id}; restorable until {}",
                text(&body, "purge_after")
            );
            emit(io, cli, body, &human)
        }
        ProjectCommand::Restore(a) => {
            let c = client(cli, env, io)?;
            let body = call(
                &c,
                Method::Post,
                &format!("/api/v1/projects/{}/restore", a.id),
                &[],
                None,
            )?;
            let human = format!(
                "Restored project {} ({})",
                text(&body, "id"),
                text(&body, "path")
            );
            emit(io, cli, body, &human)
        }
    }
}

/// The result of a move or rename: where it went and what it left behind.
fn move_human(label: &str, body: &Value) -> String {
    format!(
        "Moved {label} {}: {} -> {}\ntombstones: {}  projects: {}  reclaimed: {}",
        text(body, "id"),
        text(body, "old_path"),
        text(body, "path"),
        text(body, "tombstones"),
        text(body, "projects"),
        text(body, "reclaimed_tombstones"),
    )
}

fn list(cli: &Cli, env: EnvFn<'_>, io: &mut Io<'_>, path: &str, a: &ListArgs) -> Result<Value> {
    let c = client(cli, env, io)?;
    fetch_list(
        &c,
        path,
        a.limit,
        a.cursor.clone().map(|v| ("cursor", v)),
        a.all,
        &LIST_PAGES,
    )
}

// ── group ──────────────────────────────────────────────────────────────

pub fn group(cli: &Cli, cmd: &GroupCommand, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    match cmd {
        GroupCommand::List(a) => {
            let body = list(cli, env, io, "/api/v1/groups", a)?;
            let human = list_human(&body, "items", "next_cursor", "--cursor", &GROUP_KEYS);
            emit(io, cli, body, &human)
        }
        GroupCommand::View(a) => {
            let c = client(cli, env, io)?;
            let body = call(
                &c,
                Method::Get,
                &format!("/api/v1/groups/{}", a.id),
                &[],
                None,
            )?;
            let human = detail(
                &body,
                &[
                    "id",
                    "path",
                    "display_name",
                    "description",
                    "parent_id",
                    "role",
                    "namespace_id",
                    "created_at",
                ],
            );
            emit(io, cli, body, &human)
        }
        GroupCommand::Create {
            path,
            display_name,
            description,
            parent_id,
        } => {
            let mut m = Map::new();
            m.insert("path".to_string(), json!(path));
            m.insert("display_name".to_string(), json!(display_name));
            put_opt(&mut m, "description", description.as_ref());
            let route = match parent_id {
                Some(p) => format!("/api/v1/groups/{p}/subgroups"),
                None => "/api/v1/groups".to_string(),
            };
            let c = client(cli, env, io)?;
            let body = call(&c, Method::Post, &route, &[], Some(&Value::Object(m)))?;
            let human = format!(
                "Created group {} ({})",
                text(&body, "id"),
                text(&body, "path")
            );
            emit(io, cli, body, &human)
        }
        GroupCommand::Edit {
            id,
            display_name,
            description,
        } => {
            let req = edit_body(display_name.as_ref(), description.as_ref())?;
            let c = client(cli, env, io)?;
            let body = call(
                &c,
                Method::Patch,
                &format!("/api/v1/groups/{id}"),
                &[],
                Some(&req),
            )?;
            let human = format!(
                "Updated group {} ({})",
                text(&body, "id"),
                text(&body, "path")
            );
            emit(io, cli, body, &human)
        }
        GroupCommand::Move {
            id,
            namespace_id,
            top_level,
            path,
            reclaim,
            yes,
        } => {
            confirm(*yes, "moving or renaming a group")?;
            if namespace_id.is_none() && !top_level && path.is_none() {
                return Err(usage(
                    "nothing_to_change (pass --namespace-id, --top-level and/or --path)",
                ));
            }
            let mut m = Map::new();
            if let Some(n) = namespace_id {
                m.insert("namespace_id".to_string(), json!(n));
            }
            if *top_level {
                m.insert("top_level".to_string(), json!(true));
            }
            put_opt(&mut m, "path", path.as_ref());
            if *reclaim {
                m.insert("reclaim".to_string(), json!(true));
            }
            let c = client(cli, env, io)?;
            let body = call(
                &c,
                Method::Post,
                &format!("/api/v1/groups/{id}/transfer"),
                &[],
                Some(&Value::Object(m)),
            )?;
            let human = move_human("group", &body);
            emit(io, cli, body, &human)
        }
    }
}

// ── user ───────────────────────────────────────────────────────────────

pub fn user(cli: &Cli, cmd: &UserCommand, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    match cmd {
        UserCommand::Rename {
            id,
            username,
            reclaim,
            yes,
        } => {
            confirm(*yes, "renaming a user")?;
            let mut m = Map::new();
            m.insert("username".to_string(), json!(username));
            if *reclaim {
                m.insert("reclaim".to_string(), json!(true));
            }
            let c = client(cli, env, io)?;
            let body = call(
                &c,
                Method::Post,
                &format!("/api/v1/users/{id}/rename"),
                &[],
                Some(&Value::Object(m)),
            )?;
            let human = move_human("user", &body);
            emit(io, cli, body, &human)
        }
    }
}

// ── member ─────────────────────────────────────────────────────────────

/// `(base route, label, id)` of the member target.
fn target(t: &MemberTarget) -> Result<(String, &'static str, i64)> {
    match (t.project, t.group) {
        (Some(id), None) => Ok((format!("/api/v1/projects/{id}/members"), "project", id)),
        (None, Some(id)) => Ok((format!("/api/v1/groups/{id}/members"), "group", id)),
        // clap's group (required, not multiple) rules this out; reported as
        // usage rather than a panic if the definition ever drifts.
        _ => Err(usage("member_target (exactly one of --project, --group)")),
    }
}

pub fn member(cli: &Cli, cmd: &MemberCommand, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    match cmd {
        MemberCommand::List {
            target: t,
            limit,
            after,
            all,
        } => {
            let (route, _, _) = target(t)?;
            let c = client(cli, env, io)?;
            let body = fetch_list(
                &c,
                &route,
                *limit,
                after.map(|n| ("after", n.to_string())),
                *all,
                &MEMBER_PAGES,
            )?;
            let human = list_human(&body, "members", "next_after", "--after", &MEMBER_KEYS);
            emit(io, cli, body, &human)
        }
        MemberCommand::Add {
            target: t,
            user_id,
            role,
        } => {
            let (route, label, id) = target(t)?;
            let req = json!({"user_id": user_id, "role": role.as_str()});
            let c = client(cli, env, io)?;
            let body = call(&c, Method::Post, &route, &[], Some(&req))?;
            let human = format!("Added user {user_id} to {label} {id} as {}", role.as_str());
            emit(io, cli, body, &human)
        }
        MemberCommand::SetRole {
            target: t,
            user_id,
            role,
        } => {
            let (route, label, id) = target(t)?;
            let req = json!({"role": role.as_str()});
            let c = client(cli, env, io)?;
            let body = call(
                &c,
                Method::Patch,
                &format!("{route}/{user_id}"),
                &[],
                Some(&req),
            )?;
            let human = format!("User {user_id} is now {} in {label} {id}", role.as_str());
            emit(io, cli, body, &human)
        }
        MemberCommand::Remove {
            target: t,
            user_id,
            yes,
        } => {
            confirm(*yes, "removing a member")?;
            let (route, label, id) = target(t)?;
            let c = client(cli, env, io)?;
            call(&c, Method::Delete, &format!("{route}/{user_id}"), &[], None)?;
            let out =
                json!({"removed": true, "target": label, "target_id": id, "user_id": user_id});
            emit(
                io,
                cli,
                out,
                &format!("Removed user {user_id} from {label} {id}"),
            )
        }
    }
}

// ── protected-ref ──────────────────────────────────────────────────────

pub fn protected_ref(
    cli: &Cli,
    cmd: &ProtectedRefCommand,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
) -> Result<()> {
    match cmd {
        ProtectedRefCommand::List { project } => {
            let c = client(cli, env, io)?;
            let body = call(
                &c,
                Method::Get,
                &format!("/api/v1/projects/{project}/protected-refs"),
                &[],
                None,
            )?;
            let human = list_human(&body, "rules", "", "", &["id", "pattern", "push_min_role"]);
            emit(io, cli, body, &human)
        }
        ProtectedRefCommand::Add {
            project,
            pattern,
            push_min_role,
        } => {
            let req = json!({"pattern": pattern, "push_min_role": push_min_role.as_str()});
            let c = client(cli, env, io)?;
            let body = call(
                &c,
                Method::Post,
                &format!("/api/v1/projects/{project}/protected-refs"),
                &[],
                Some(&req),
            )?;
            let human = format!(
                "Protected {} (push from {}) as rule {}",
                text(&body, "pattern"),
                text(&body, "push_min_role"),
                text(&body, "id")
            );
            emit(io, cli, body, &human)
        }
        ProtectedRefCommand::Edit {
            project,
            rule_id,
            push_min_role,
        } => {
            let req = json!({"push_min_role": push_min_role.as_str()});
            let c = client(cli, env, io)?;
            let body = call(
                &c,
                Method::Patch,
                &format!("/api/v1/projects/{project}/protected-refs/{rule_id}"),
                &[],
                Some(&req),
            )?;
            let human = format!(
                "Rule {} ({}) now needs {} to push",
                text(&body, "id"),
                text(&body, "pattern"),
                text(&body, "push_min_role")
            );
            emit(io, cli, body, &human)
        }
        ProtectedRefCommand::Remove {
            project,
            rule_id,
            yes,
        } => {
            confirm(*yes, "removing a protected ref rule")?;
            let c = client(cli, env, io)?;
            call(
                &c,
                Method::Delete,
                &format!("/api/v1/projects/{project}/protected-refs/{rule_id}"),
                &[],
                None,
            )?;
            let out = json!({"removed": true, "project_id": project, "rule_id": rule_id});
            emit(
                io,
                cli,
                out,
                &format!("Removed rule {rule_id} from project {project}"),
            )
        }
    }
}

// ── namespace ──────────────────────────────────────────────────────────

pub fn namespace(cli: &Cli, cmd: &NamespaceCommand, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    match cmd {
        NamespaceCommand::Resolve { path } => {
            let c = client(cli, env, io)?;
            // The path is a query value: the URL builder percent-encodes it.
            let q = [("path".to_string(), path.clone())];
            let body = call(&c, Method::Get, "/api/v1/namespaces/resolve", &q, None)?;
            let id_key = match body.get("kind").and_then(Value::as_str) {
                Some("project") => "project_id",
                Some("group") => "group_id",
                Some("user") => "user_id",
                _ => {
                    return Err(ClientError::local(
                        ErrorCode::CliResponseInvalid,
                        "reason=resolution_kind_unknown",
                    ))
                }
            };
            let moved = if body.get("moved").and_then(Value::as_bool) == Some(true) {
                " (moved)"
            } else {
                ""
            };
            let human = format!(
                "{} {} {}{moved}\nnamespace_id: {}",
                text(&body, "kind"),
                text(&body, id_key),
                text(&body, "canonical_path"),
                text(&body, "namespace_id"),
            );
            emit(io, cli, body, &human)
        }
        NamespaceCommand::ReleasePath { path, yes } => {
            confirm(*yes, "releasing a namespace tombstone")?;
            let req = json!({"path": path});
            let c = client(cli, env, io)?;
            let body = call(
                &c,
                Method::Post,
                "/api/v1/namespaces/tombstones/release",
                &[],
                Some(&req),
            )?;
            let human = format!(
                "Released {} ({} tombstone); the path can be used again",
                text(&body, "path"),
                text(&body, "kind")
            );
            emit(io, cli, body, &human)
        }
    }
}
