//! The agent tools parity gate (ADR-0023 Amendment 2026-11-10): the MCP
//! server and `gstn` are two entrances to the same API (plan 01 §3.11), so
//! `docs/api/agent-tools.toml` must decide every `gstn` command, name only
//! routes `openapi.json` has, and never make a tool of what is kept for
//! people. Deterministic: the manifest, the clap tree, the registry,
//! `openapi.json` and `routes.generated.json` only.

#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Map, Value};

use crate::agent_tools::{manifest, parse, Decision, Location, Manifest, ToolSpec};
use crate::cli::command_paths;
use crate::registry::find;

const OPENAPI: &str = include_str!("../../../docs/api/openapi.json");
const ROUTES: &str = include_str!("../../../docs/authz/routes.generated.json");
const HOW: &str = "add a [[tool]] with this `gstn` to docs/api/agent-tools.toml, or list it under [[excluded]] (kept for people), [[not_api]] (no REST call) or [[deferred]] (not a tool yet) with a one-line reason";

fn tools() -> &'static Manifest {
    manifest().unwrap_or_else(|why| panic!("docs/api/agent-tools.toml: {why}"))
}

fn route(t: &ToolSpec) -> String {
    format!("{} {}", t.method, t.path)
}

/// `METHOD /path` → authz action, from the generated route table.
fn actions() -> BTreeMap<String, String> {
    let v: Value = serde_json::from_str(ROUTES).expect("routes json");
    v.as_array()
        .expect("array")
        .iter()
        .map(|r| {
            (
                format!(
                    "{} {}",
                    r["method"].as_str().unwrap_or_default(),
                    r["path"].as_str().unwrap_or_default()
                ),
                r["action"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

#[test]
fn the_embedded_manifest_is_valid() {
    let m = tools();
    assert!(!m.tools.is_empty());
    for d in m.excluded.iter().chain(&m.not_api).chain(&m.deferred) {
        assert!(!d.reason.trim().is_empty(), "{d:?} has no reason");
    }
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
    let m = tools();
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
    let commands = command_paths();
    let mut problems = Vec::new();
    for c in &commands {
        match decided.get(c.as_str()) {
            None => problems.push(format!(
                "gstn command `{c}` has no agent-tool decision: {HOW}"
            )),
            Some(kinds) => {
                let set: BTreeSet<&&str> = kinds.iter().collect();
                if set.len() > 1 || (kinds.len() > 1 && !kinds.iter().all(|k| *k == "tool")) {
                    problems.push(format!(
                        "gstn command `{c}` is decided more than once ({kinds:?}): keep one"
                    ));
                }
            }
        }
    }
    for g in decided.keys() {
        if !commands.iter().any(|c| c == g) {
            problems.push(format!(
                "docs/api/agent-tools.toml names `{g}`, which is no gstn command any more: remove or rename it"
            ));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
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

#[test]
fn every_tool_route_and_argument_is_in_openapi() {
    let doc: Value = serde_json::from_str(OPENAPI).expect("openapi json");
    let mut problems = Vec::new();
    for t in &tools().tools {
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
                        at = at.and_then(|s| property(&doc, s, key));
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
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn every_tool_route_is_one_its_gstn_command_calls() {
    for t in &tools().tools {
        let spec =
            find(&t.gstn).unwrap_or_else(|| panic!("tool {}: no command {}", t.name, t.gstn));
        assert!(
            spec.routes.contains(&route(t).as_str()),
            "tool {} mirrors `gstn {}`, which does not call {}",
            t.name,
            t.gstn,
            route(t)
        );
    }
}

/// No tool is what the manifest keeps for people: not an excluded command,
/// not a write route of one, and not any route with the authz action of
/// such a route or of an excluded action.
#[test]
fn no_excluded_operation_is_a_tool() {
    let m = tools();
    let actions = actions();
    let mut banned_routes = BTreeSet::new();
    let mut banned_actions: BTreeMap<String, String> = BTreeMap::new();
    for d in &m.excluded {
        if let Some(a) = &d.action {
            banned_actions.insert(a.clone(), format!("excluded action {a}")); // clone: owned keys
        }
        let Some(g) = &d.gstn else { continue };
        let spec = find(g).unwrap_or_else(|| panic!("excluded `{g}` is no gstn command"));
        for r in spec.routes.iter().filter(|r| !r.starts_with("GET ")) {
            banned_routes.insert((*r).to_owned());
            if let Some(a) = actions.get(*r) {
                banned_actions.insert(a.clone(), format!("`gstn {g}` ({r})")); // clone: as above
            }
        }
    }
    assert!(banned_actions.contains_key("MergeRequestApprove"));
    assert!(banned_actions.contains_key("MergeRequestMerge"));
    assert!(banned_actions.contains_key("DiscussionResolve"));
    assert!(banned_actions.contains_key("DeployApprove"));
    let excluded: BTreeSet<&str> = m
        .excluded
        .iter()
        .filter_map(|d| d.gstn.as_deref())
        .collect();
    for t in &m.tools {
        let r = route(t);
        assert!(
            !excluded.contains(t.gstn.as_str()),
            "tool {} mirrors excluded `gstn {}`",
            t.name,
            t.gstn
        );
        assert!(
            !banned_routes.contains(&r),
            "tool {} is the excluded route {r}",
            t.name
        );
        let action = actions.get(&r).unwrap_or_else(|| {
            panic!(
                "tool {}: {r} is not a server route (routes.generated.json)",
                t.name
            )
        });
        if let Some(why) = banned_actions.get(action) {
            panic!(
                "tool {} ({r}) is the authz action {action}, kept for people by {why}",
                t.name
            );
        }
        // A read tool reads: its route's action is no write of anything.
        if t.read_only {
            assert_eq!(t.method, "GET", "{}", t.name);
        }
    }
}

/// `gstn help --json` says which routes `gstn mcp` calls: the same set.
#[test]
fn the_mcp_command_lists_every_tool_route() {
    let spec = find("mcp").expect("mcp");
    let listed: BTreeSet<String> = spec.routes.iter().map(|r| (*r).to_owned()).collect();
    let used: BTreeSet<String> = tools().tools.iter().map(route).collect();
    assert_eq!(
        listed, used,
        "registry.rs `mcp` routes and docs/api/agent-tools.toml differ"
    );
    assert!(
        spec.scopes.contains(&"api"),
        "write tools need the api scope"
    );
}

#[test]
fn every_tool_builds_its_route_and_refuses_unknown_arguments() {
    for t in &tools().tools {
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

#[test]
fn a_misdefined_manifest_is_refused() {
    let base = |tool: &str| format!("version = 1\n[[tool]]\n{tool}\n");
    let ok = r#"name = "t"
gstn = "mr view"
read_only = true
method = "GET"
path = "/api/v1/projects/{id}"
description = "d"
params = [{ name = "project_id", in = "path", wire = "id", type = "integer", required = true, description = "p" }]"#;
    assert!(parse(&base(ok)).is_ok());
    // A text in a path could leave its segment.
    let text_in_path = ok.replace(r#"type = "integer""#, r#"type = "string""#);
    assert!(parse(&base(&text_in_path)).is_err());
    // A write that says it reads.
    let lying = ok.replace(r#"method = "GET""#, r#"method = "POST""#);
    assert!(parse(&base(&lying)).is_err());
    // A path parameter the tool does not fill.
    let hole = ok.replace("/api/v1/projects/{id}", "/api/v1/projects/{id}/x/{iid}");
    assert!(parse(&base(&hole)).is_err());
    // An unknown key is a typo, not ignored.
    let typo = ok.replace("read_only", "readonly");
    assert!(parse(&base(&typo)).is_err());
    assert!(parse("version = 2\ntool = []").is_err());
}
