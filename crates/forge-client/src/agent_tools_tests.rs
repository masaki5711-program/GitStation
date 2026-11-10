//! The agent tools parity gate (ADR-0023 Amendment 2026-11-10): the MCP
//! server and `gstn` are two entrances to the same API (plan 01 §3.11), so
//! `docs/api/agent-tools.toml` must decide every `gstn` command, name only
//! routes `openapi.json` has, and never make a tool of what is kept for
//! people. Each check is a function over (manifest, commands, openapi,
//! routes) returning its problems: the real files must have none, and the
//! negative tests below show each check failing on a synthetic input.

#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Map, Value};

use crate::agent_tools::{manifest, parse, Decision, Location, Manifest, ToolSpec};
use crate::cli::command_paths;
use crate::registry::{find, COMMANDS};

const OPENAPI: &str = include_str!("../../../docs/api/openapi.json");
const ROUTES: &str = include_str!("../../../docs/authz/routes.generated.json");
const HOW: &str = "add a [[tool]] with this `gstn` to docs/api/agent-tools.toml, or list it under [[excluded]] (kept for people), [[not_api]] (no REST call) or [[deferred]] (not a tool yet) with a one-line reason";
/// Commands that describe or proxy, never one route of their own.
const NEVER_A_TOOL: [&str; 4] = ["mcp", "api", "help", "schema"];

/// A `gstn` leaf command and the routes it calls itself (`CommandSpec.routes`
/// as written; `mcp`'s are derived from the manifest, so they are none here).
struct Command {
    name: String,
    routes: Vec<String>,
}

fn real_commands() -> Vec<Command> {
    command_paths()
        .into_iter()
        .map(|name| {
            let routes = find(&name)
                .map(|c| c.routes.iter().map(|r| (*r).to_owned()).collect())
                .unwrap_or_default();
            Command { name, routes }
        })
        .collect()
}

fn real_manifest() -> &'static Manifest {
    manifest().unwrap_or_else(|why| panic!("docs/api/agent-tools.toml: {why}"))
}

fn json_of(text: &str) -> Value {
    serde_json::from_str(text).expect("json")
}

fn route(t: &ToolSpec) -> String {
    format!("{} {}", t.method, t.path)
}

fn commands_by_name(commands: &[Command]) -> BTreeMap<&str, &Command> {
    commands.iter().map(|c| (c.name.as_str(), c)).collect()
}

// ── the checks ──

/// Every command decided exactly once; no decision names a missing one.
fn decision_problems(m: &Manifest, commands: &[Command]) -> Vec<String> {
    let mut decided: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for t in &m.tools {
        decided.entry(t.gstn.as_str()).or_default().push("tool");
    }
    let lists: [(&str, &[Decision]); 3] = [
        ("excluded", &m.excluded),
        ("not_api", &m.not_api),
        ("deferred", &m.deferred),
    ];
    for (kind, list) in lists {
        for d in list {
            if let Some(g) = &d.gstn {
                decided.entry(g.as_str()).or_default().push(kind);
            }
        }
    }
    let mut problems = Vec::new();
    for c in commands {
        match decided.get(c.name.as_str()) {
            None => problems.push(format!(
                "gstn command `{}` has no agent-tool decision: {HOW}",
                c.name
            )),
            // Several tools may mirror one command (mr diff, mr comment).
            Some(kinds) if kinds.len() > 1 && !kinds.iter().all(|k| *k == "tool") => {
                problems.push(format!(
                    "gstn command `{}` is decided more than once ({kinds:?}): keep one",
                    c.name
                ))
            }
            Some(_) => {}
        }
    }
    let names = commands_by_name(commands);
    for g in decided.keys() {
        if !names.contains_key(g) {
            problems.push(format!(
                "docs/api/agent-tools.toml names `{g}`, which is no gstn command any more: remove or rename it"
            ));
        }
    }
    for d in m.excluded.iter().chain(&m.not_api).chain(&m.deferred) {
        if d.reason.trim().is_empty() {
            problems.push(format!("{d:?} has no reason"));
        }
    }
    problems
}

/// A tool mirrors a command that calls that very route itself — never a
/// command without routes of its own (`mcp`, `api`, `help`, `schema`) or
/// one listed as `not_api`.
fn tool_command_problems(m: &Manifest, commands: &[Command]) -> Vec<String> {
    let names = commands_by_name(commands);
    let not_api: BTreeSet<&str> = m.not_api.iter().filter_map(|d| d.gstn.as_deref()).collect();
    let mut problems = Vec::new();
    for t in &m.tools {
        let g = t.gstn.as_str();
        if NEVER_A_TOOL.contains(&g) || not_api.contains(g) {
            problems.push(format!(
                "tool {} mirrors `gstn {g}`, which calls no route of its own: name the command that calls {}",
                t.name,
                route(t)
            ));
            continue;
        }
        match names.get(g) {
            None => problems.push(format!("tool {}: no gstn command `{g}`", t.name)),
            Some(c) if !c.routes.contains(&route(t)) => problems.push(format!(
                "tool {} mirrors `gstn {g}`, which does not call {}",
                t.name,
                route(t)
            )),
            Some(_) => {}
        }
    }
    problems
}

fn resolve<'a>(doc: &'a Value, schema: &'a Value) -> &'a Value {
    match schema.get("$ref").and_then(Value::as_str) {
        Some(r) => {
            let name = r.rsplit('/').next().unwrap_or_default();
            &doc["components"]["schemas"][name]
        }
        None => schema,
    }
}

/// The property `key` of an object schema, through `$ref` and `oneOf`.
fn property<'a>(doc: &'a Value, schema: &'a Value, key: &str) -> Option<&'a Value> {
    let s = resolve(doc, schema);
    if let Some(p) = s.get("properties").and_then(|p| p.get(key)) {
        return Some(p);
    }
    s.get("oneOf")
        .or_else(|| s.get("anyOf"))
        .and_then(Value::as_array)
        .and_then(|alts| alts.iter().find_map(|a| property(doc, a, key)))
}

/// Every tool's route, and every argument's name on the wire, is in
/// `openapi.json`.
fn openapi_problems(m: &Manifest, doc: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    for t in &m.tools {
        let op = &doc["paths"][&t.path][t.method.to_ascii_lowercase()];
        if op.is_null() {
            problems.push(format!(
                "tool {}: {} is not in docs/api/openapi.json",
                t.name,
                route(t)
            ));
            continue;
        }
        let declared: Vec<(String, String)> = op["parameters"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|p| {
                (
                    p["in"].as_str().unwrap_or_default().to_owned(),
                    p["name"].as_str().unwrap_or_default().to_owned(),
                )
            })
            .collect();
        let body = &op["requestBody"]["content"]["application/json"]["schema"];
        for p in &t.params {
            let ok = match p.location {
                Location::Path => declared.contains(&("path".to_owned(), p.wire().to_owned())),
                Location::Query => declared.contains(&("query".to_owned(), p.wire().to_owned())),
                Location::Body => {
                    let mut at = Some(body);
                    for key in p.wire().split('.') {
                        at = at.and_then(|s| property(doc, s, key));
                    }
                    !body.is_null() && at.is_some()
                }
            };
            if !ok {
                problems.push(format!(
                    "tool {}: argument {} ({:?} `{}`) is not a parameter of {} in openapi.json",
                    t.name,
                    p.name,
                    p.location,
                    p.wire(),
                    route(t)
                ));
            }
        }
    }
    problems
}

/// No tool is what is kept for people:
/// - a route whose action the server keeps for people (`people_only` in
///   `routes.generated.json`, from `Action::is_people_only`), GET included;
/// - any route of an excluded command (unless a command not excluded calls
///   it too: `mr merge` polls `GET …/merge` as `mr merge-status` does);
/// - the action of an excluded command's write, or an excluded action.
fn exclusion_problems(m: &Manifest, commands: &[Command], routes: &Value) -> Vec<String> {
    let mut action_of: BTreeMap<String, (String, bool)> = BTreeMap::new();
    for r in routes.as_array().map(Vec::as_slice).unwrap_or_default() {
        let (Some(method), Some(path), Some(action)) = (
            r["method"].as_str(),
            r["path"].as_str(),
            r["action"].as_str(),
        ) else {
            continue; // planned / decided-in entries have no route
        };
        let people_only = r["people_only"].as_bool().unwrap_or(false);
        action_of.insert(format!("{method} {path}"), (action.to_owned(), people_only));
    }
    let names = commands_by_name(commands);
    let excluded: BTreeSet<&str> = m
        .excluded
        .iter()
        .filter_map(|d| d.gstn.as_deref())
        .collect();
    let called_elsewhere: BTreeSet<&String> = commands
        .iter()
        .filter(|c| !excluded.contains(c.name.as_str()))
        .flat_map(|c| &c.routes)
        .collect();
    let mut banned_routes: BTreeMap<String, String> = BTreeMap::new();
    let mut banned_actions: BTreeMap<String, String> = BTreeMap::new();
    for d in &m.excluded {
        if let Some(a) = &d.action {
            banned_actions.insert(a.clone(), format!("the excluded action {a}"));
            // clone: owned keys
        }
        let Some(g) = d.gstn.as_deref() else { continue };
        let Some(c) = names.get(g) else { continue }; // decision_problems reports it
        for r in &c.routes {
            if !called_elsewhere.contains(r) {
                banned_routes.insert(r.clone(), format!("excluded `gstn {g}`"));
                // clone: as above
            }
            if !r.starts_with("GET ") {
                if let Some((a, _)) = action_of.get(r) {
                    banned_actions.insert(a.clone(), format!("excluded `gstn {g}` ({r})"));
                    // clone: as above
                }
            }
        }
    }
    let mut problems = Vec::new();
    for t in &m.tools {
        let r = route(t);
        if excluded.contains(t.gstn.as_str()) {
            problems.push(format!(
                "tool {} mirrors excluded `gstn {}`",
                t.name, t.gstn
            ));
        }
        if let Some(why) = banned_routes.get(&r) {
            problems.push(format!("tool {} is {r}, a route of {why}", t.name));
        }
        match action_of.get(&r) {
            None => problems.push(format!(
                "tool {}: {r} is not a server route (docs/authz/routes.generated.json)",
                t.name
            )),
            Some((action, true)) => problems.push(format!(
                "tool {} ({r}) is the people_only action {action}: kept for people",
                t.name
            )),
            Some((action, false)) => {
                if let Some(why) = banned_actions.get(action) {
                    problems.push(format!(
                        "tool {} ({r}) is the authz action {action}, kept for people by {why}",
                        t.name
                    ));
                }
            }
        }
    }
    problems
}

fn assert_none(problems: Vec<String>) {
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

// ── the gate on the real files ──

#[test]
fn the_embedded_manifest_is_valid() {
    let m = real_manifest();
    assert!(!m.tools.is_empty());
    for d in m.not_api.iter().chain(&m.deferred) {
        assert!(
            d.gstn.is_some() && d.action.is_none(),
            "{d:?}: names a gstn command"
        );
    }
    for d in &m.excluded {
        assert!(
            d.gstn.is_some() != d.action.is_some(),
            "{d:?}: names a gstn command or an authz action"
        );
    }
}

/// The gate's core: a `gstn` command without a decision fails, saying
/// what to add (C3 or any later MR that adds a command meets it here).
#[test]
fn every_gstn_command_has_an_agent_tool_decision() {
    assert_none(decision_problems(real_manifest(), &real_commands()));
}

#[test]
fn every_tool_mirrors_a_command_that_calls_its_route() {
    assert_none(tool_command_problems(real_manifest(), &real_commands()));
}

#[test]
fn every_tool_route_and_argument_is_in_openapi() {
    assert_none(openapi_problems(real_manifest(), &json_of(OPENAPI)));
}

#[test]
fn no_excluded_operation_is_a_tool() {
    let routes = json_of(ROUTES);
    // The server's export is there (gen-authz): people-only actions are data.
    let people_only: BTreeSet<&str> = routes
        .as_array()
        .expect("array")
        .iter()
        .filter(|r| r["people_only"] == true)
        .filter_map(|r| r["action"].as_str())
        .collect();
    for a in [
        "MergeRequestApprove",
        "MergeRequestMerge",
        "DiscussionResolve",
        "WebhookManage",
    ] {
        assert!(
            people_only.contains(a),
            "routes.generated.json has no people_only {a}"
        );
    }
    assert_none(exclusion_problems(
        real_manifest(),
        &real_commands(),
        &routes,
    ));
}

/// `gstn help --json` says which routes `gstn mcp` calls: derived from the
/// manifest, never kept by hand.
#[test]
fn the_mcp_command_lists_every_tool_route() {
    let spec = find("mcp").expect("mcp");
    assert!(
        spec.routes.is_empty(),
        "registry.rs `mcp` routes are derived, not written"
    );
    let listed: BTreeSet<String> = spec.all_routes().into_iter().collect();
    let used: BTreeSet<String> = real_manifest().tools.iter().map(route).collect();
    assert_eq!(listed, used);
    let help = crate::registry::command_json(spec);
    assert_eq!(help["routes"].as_array().map(Vec::len), Some(listed.len()));
    assert!(
        spec.scopes.contains(&"api"),
        "write tools need the api scope"
    );
    assert!(COMMANDS.iter().any(|c| c.name == "mcp"));
}

#[test]
fn every_tool_builds_its_route_and_refuses_unknown_arguments() {
    for t in &real_manifest().tools {
        let mut given = Map::new();
        for p in t.params.iter().filter(|p| p.required) {
            let v = match (p.kind, p.format) {
                (crate::agent_tools::Kind::Integer, _) => json!(7),
                (_, crate::agent_tools::Format::Oid) => json!("a".repeat(40)),
                _ => json!("PBI-1"),
            };
            given.insert(p.name.clone(), v); // clone: the map owns its keys
        }
        // A tool whose constraints need one more argument gets the first.
        if let Some(first) = t.at_least_one.first() {
            given.insert(first.clone(), json!("x")); // clone: as above
        }
        let req = t
            .request(&given)
            .unwrap_or_else(|e| panic!("{}: {e:?}", t.name));
        assert!(
            req.path.starts_with("/api/v1/projects/7/"),
            "{}: {}",
            t.name,
            req.path
        );
        assert!(!req.path.contains('{'), "{}: {}", t.name, req.path);
        given.insert("token".to_owned(), json!("x"));
        assert!(t.request(&given).is_err(), "{}", t.name);
    }
}

// ── the gate failing (synthetic inputs) ──

const PROJECT: &str = r#"{ name = "project_id", in = "path", wire = "id", type = "integer", required = true, description = "p" }"#;

fn tool(name: &str, gstn: &str, method: &str, path: &str) -> String {
    let (read_only, extra) = if method == "GET" {
        ("true", "")
    } else {
        ("false", "destructive = false\nidempotent = false\n")
    };
    format!(
        "[[tool]]\nname = \"{name}\"\ngstn = \"{gstn}\"\nread_only = {read_only}\n{extra}method = \"{method}\"\npath = \"{path}\"\ndescription = \"d\"\nparams = [{PROJECT}]\n"
    )
}

fn synthetic(body: &str) -> Manifest {
    parse(&format!("version = 1\n{body}")).expect("synthetic manifest")
}

fn command(name: &str, routes: &[&str]) -> Command {
    Command {
        name: name.to_owned(),
        routes: routes.iter().map(|r| (*r).to_owned()).collect(),
    }
}

fn server_routes() -> Value {
    json!([
        {"method":"GET","path":"/api/v1/projects/{id}/pipelines","action":"PipelineRead","people_only":false},
        {"method":"GET","path":"/api/v1/projects/{id}/webhooks","action":"WebhookManage","people_only":true},
        {"method":"POST","path":"/api/v1/projects/{id}/webhooks","action":"WebhookManage","people_only":true},
        {"method":"GET","path":"/api/v1/projects/{id}/push-mirror","action":"PushMirrorView","people_only":false},
        {"method":"POST","path":"/api/v1/projects/{id}/push-mirror/sync","action":"PushMirrorSync","people_only":false},
        {"method":"POST","path":"/api/v1/projects/{id}/push-mirror/kick","action":"PushMirrorSync","people_only":false},
        {"action":"AuditRead","planned":true}
    ])
}

fn has(problems: &[String], needle: &str) -> bool {
    problems.iter().any(|p| p.contains(needle))
}

#[test]
fn an_undecided_command_fails_with_what_to_add() {
    let m = synthetic(&tool(
        "list_pipelines",
        "pipeline list",
        "GET",
        "/api/v1/projects/{id}/pipelines",
    ));
    let commands = [
        command("pipeline list", &["GET /api/v1/projects/{id}/pipelines"]),
        command(
            "pipeline logs",
            &["GET /api/v1/projects/{id}/pipelines/{pipeline_id}/logs"],
        ),
    ];
    let problems = decision_problems(&m, &commands);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].starts_with("gstn command `pipeline logs` has no agent-tool decision"));
    assert!(problems[0].ends_with(HOW), "{}", problems[0]);
}

#[test]
fn a_command_decided_twice_or_gone_fails() {
    let m = synthetic(&format!(
        "{}[[deferred]]\ngstn = \"pipeline list\"\nreason = \"r\"\n[[not_api]]\ngstn = \"pipeline gone\"\nreason = \"r\"\n",
        tool("list_pipelines", "pipeline list", "GET", "/api/v1/projects/{id}/pipelines")
    ));
    let commands = [command(
        "pipeline list",
        &["GET /api/v1/projects/{id}/pipelines"],
    )];
    let problems = decision_problems(&m, &commands);
    assert!(
        has(&problems, "`pipeline list` is decided more than once"),
        "{problems:?}"
    );
    assert!(
        has(
            &problems,
            "`pipeline gone`, which is no gstn command any more"
        ),
        "{problems:?}"
    );
}

#[test]
fn a_tool_route_missing_from_openapi_fails() {
    let m = synthetic(&tool(
        "logs",
        "pipeline logs",
        "GET",
        "/api/v1/projects/{id}/pipelines/logs",
    ));
    let doc = json!({"paths": {"/api/v1/projects/{id}/pipelines": {"get": {"parameters": [{"in":"path","name":"id"}]}}}});
    let problems = openapi_problems(&m, &doc);
    assert!(
        has(&problems, "is not in docs/api/openapi.json"),
        "{problems:?}"
    );
    // The route exists but an argument does not.
    let m = synthetic(&tool(
        "list",
        "pipeline list",
        "GET",
        "/api/v1/projects/{id}/pipelines",
    ));
    let doc = json!({"paths": {"/api/v1/projects/{id}/pipelines": {"get": {"parameters": []}}}});
    let problems = openapi_problems(&m, &doc);
    assert!(has(&problems, "argument project_id"), "{problems:?}");
}

#[test]
fn a_tool_on_mcp_or_a_command_without_that_route_fails() {
    let webhooks = "/api/v1/projects/{id}/webhooks";
    let m = synthetic(&format!(
        "{}{}",
        tool("list_webhooks", "mcp", "GET", webhooks),
        tool("list_hooks", "pipeline list", "GET", webhooks)
    ));
    let commands = [
        // mcp's routes are derived from the manifest: none of its own.
        command("mcp", &[]),
        command("pipeline list", &["GET /api/v1/projects/{id}/pipelines"]),
    ];
    let problems = tool_command_problems(&m, &commands);
    assert!(
        has(
            &problems,
            "tool list_webhooks mirrors `gstn mcp`, which calls no route of its own"
        ),
        "{problems:?}"
    );
    assert!(
        has(
            &problems,
            "tool list_hooks mirrors `gstn pipeline list`, which does not call"
        ),
        "{problems:?}"
    );
    // Even were mcp's routes written by hand, it is never a tool's command.
    let commands = [command("mcp", &["GET /api/v1/projects/{id}/webhooks"])];
    assert!(has(
        &tool_command_problems(&m, &commands),
        "calls no route of its own"
    ));
}

#[test]
fn a_people_only_route_is_never_a_tool_get_or_write() {
    let m = synthetic(&format!(
        "{}{}",
        tool(
            "list_webhooks",
            "deferred webhook list",
            "GET",
            "/api/v1/projects/{id}/webhooks"
        ),
        tool(
            "create_webhook",
            "deferred webhook create",
            "POST",
            "/api/v1/projects/{id}/webhooks"
        )
    ));
    let commands = [
        command(
            "deferred webhook list",
            &["GET /api/v1/projects/{id}/webhooks"],
        ),
        command(
            "deferred webhook create",
            &["POST /api/v1/projects/{id}/webhooks"],
        ),
    ];
    let problems = exclusion_problems(&m, &commands, &server_routes());
    assert!(has(&problems, "tool list_webhooks (GET /api/v1/projects/{id}/webhooks) is the people_only action WebhookManage"), "{problems:?}");
    assert!(has(&problems, "tool create_webhook (POST /api/v1/projects/{id}/webhooks) is the people_only action WebhookManage"), "{problems:?}");
}

#[test]
fn an_excluded_command_route_is_never_a_tool_get_or_write() {
    let excluded = "[[excluded]]\ngstn = \"mirror show\"\nreason = \"r\"\n[[excluded]]\ngstn = \"mirror sync\"\nreason = \"r\"\n";
    let m = synthetic(&format!(
        "{}{}{}{excluded}",
        // GET of an excluded command, under another command's name.
        tool(
            "show_mirror",
            "browse mirror",
            "GET",
            "/api/v1/projects/{id}/push-mirror"
        ),
        // The excluded write itself.
        tool(
            "sync_mirror",
            "browse sync",
            "POST",
            "/api/v1/projects/{id}/push-mirror/sync"
        ),
        // Another route with the excluded write's action.
        tool(
            "kick_mirror",
            "browse kick",
            "POST",
            "/api/v1/projects/{id}/push-mirror/kick"
        )
    ));
    let commands = [
        command("mirror show", &["GET /api/v1/projects/{id}/push-mirror"]),
        command(
            "mirror sync",
            &["POST /api/v1/projects/{id}/push-mirror/sync"],
        ),
        command("browse mirror", &["GET /api/v1/projects/{id}/push-mirror"]),
        command(
            "browse sync",
            &["POST /api/v1/projects/{id}/push-mirror/sync"],
        ),
        command(
            "browse kick",
            &["POST /api/v1/projects/{id}/push-mirror/kick"],
        ),
    ];
    // `browse mirror` / `browse sync` call the excluded routes too, so the
    // route rule spares them; the action rule still catches the write.
    let problems = exclusion_problems(&m, &commands, &server_routes());
    assert!(has(&problems, "tool sync_mirror (POST /api/v1/projects/{id}/push-mirror/sync) is the authz action PushMirrorSync"), "{problems:?}");
    assert!(has(&problems, "tool kick_mirror (POST /api/v1/projects/{id}/push-mirror/kick) is the authz action PushMirrorSync"), "{problems:?}");
    // Without another caller, the excluded GET route itself is refused.
    let commands = [
        command("mirror show", &["GET /api/v1/projects/{id}/push-mirror"]),
        command(
            "mirror sync",
            &["POST /api/v1/projects/{id}/push-mirror/sync"],
        ),
        command("browse sync", &[]),
        command("browse kick", &[]),
        command("browse mirror", &[]),
    ];
    let problems = exclusion_problems(&m, &commands, &server_routes());
    assert!(has(&problems, "tool show_mirror is GET /api/v1/projects/{id}/push-mirror, a route of excluded `gstn mirror show`"), "{problems:?}");
    // A tool mirroring an excluded command, and an excluded action.
    let m = synthetic(&format!(
        "{}[[excluded]]\ngstn = \"pipeline list\"\nreason = \"r\"\n[[excluded]]\naction = \"PipelineRead\"\nreason = \"r\"\n",
        tool("list_pipelines", "pipeline list", "GET", "/api/v1/projects/{id}/pipelines")
    ));
    let commands = [command(
        "pipeline list",
        &["GET /api/v1/projects/{id}/pipelines"],
    )];
    let problems = exclusion_problems(&m, &commands, &server_routes());
    assert!(
        has(&problems, "mirrors excluded `gstn pipeline list`"),
        "{problems:?}"
    );
    assert!(
        has(
            &problems,
            "kept for people by the excluded action PipelineRead"
        ),
        "{problems:?}"
    );
    // A route the server does not have.
    let m = synthetic(&tool(
        "ghost",
        "pipeline list",
        "GET",
        "/api/v1/projects/{id}/ghost",
    ));
    let problems = exclusion_problems(&m, &commands, &server_routes());
    assert!(has(&problems, "is not a server route"), "{problems:?}");
}

#[test]
fn a_misdefined_manifest_is_refused() {
    let ok = tool("t", "mr view", "GET", "/api/v1/projects/{id}");
    let base = |t: &str| format!("version = 1\n{t}");
    assert!(parse(&base(&ok)).is_ok());
    let refused = |t: String, why: &str| {
        assert!(parse(&base(&t)).is_err(), "accepted: {why}");
    };
    // A text in a path could leave its segment.
    refused(
        ok.replace(r#"type = "integer""#, r#"type = "string""#),
        "text in a path",
    );
    // A write that says it reads.
    refused(
        ok.replace(r#"method = "GET""#, r#"method = "POST""#),
        "write as read",
    );
    // A path parameter the tool does not fill.
    refused(
        ok.replace("/api/v1/projects/{id}\"", "/api/v1/projects/{id}/x/{iid}\""),
        "unfilled hole",
    );
    // An unknown key is a typo, not ignored.
    refused(ok.replace("read_only", "readonly"), "unknown key");
    refused(format!("{ok}{ok}"), "duplicate tool name");
    refused(
        ok.replace(r#"in = "path""#, r#"in = "header""#),
        "unknown in",
    );
    refused(
        ok.replace(r#"type = "integer""#, r#"type = "number""#),
        "unknown type",
    );
    refused(
        ok.replace("description = \"d\"\n", ""),
        "missing required field",
    );
    refused(
        format!("{ok}at_least_one = [\"nope\"]\n"),
        "at_least_one names an unknown argument",
    );
    let body = r#"{ name = "title", in = "body", type = "string", description = "t" }"#;
    let write = tool("w", "mr edit", "PATCH", "/api/v1/projects/{id}");
    refused(
        write.replace(&format!("[{PROJECT}]"), &format!("[{PROJECT}, {body}]")),
        "body without wire",
    );
    let dotted = body.replace(r#"in = "body","#, r#"in = "body", wire = "a.","#);
    refused(
        write.replace(&format!("[{PROJECT}]"), &format!("[{PROJECT}, {dotted}]")),
        "empty wire segment",
    );
    let named = body.replace(r#"in = "body","#, r#"in = "body", wire = "title","#);
    assert!(parse(&base(
        &write.replace(&format!("[{PROJECT}]"), &format!("[{PROJECT}, {named}]"))
    ))
    .is_ok());
    assert!(
        parse("version = 1\n[[tool]\nname = ").is_err(),
        "TOML syntax error"
    );
    assert!(parse("version = 2\ntool = []").is_err(), "another version");
}
