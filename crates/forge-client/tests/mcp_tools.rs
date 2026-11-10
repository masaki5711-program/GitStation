//! End-to-end tests of the merge request, check and pipeline tools of
//! `gstn mcp` (docs/api/agent-tools.toml, ADR-0023 Amendment 2026-11-10)
//! against a scripted loopback server: the request each sends (method,
//! path, query, JSON body, the agent header and the caller's token), the
//! annotations of the write tools, argument checks before anything is
//! sent, and the error shape. All data is synthetic.

#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

mod common;
use common::{Mock, Out, Resp, Sandbox, TOKEN};
use serde_json::{json, Value};

const MR: &str = r#"{"project_id":7,"iid":3,"title":"Add reload","description":"","state":"open","draft":false,"source_branch":"feature/reload","target_branch":"main","author_id":2,"head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","created_at":"2026-11-10T00:00:00Z","updated_at":"2026-11-10T00:00:00Z","closed_at":null,"closed_by":null}"#;
const PIPELINE: &str = r#"{"id":12,"status":"running","source":"manual","ref":"refs/heads/main","sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","jobs":[]}"#;
const DISCUSSION: &str = r#"{"id":5,"kind":"diff","notes":[{"id":9,"author_id":2,"body":"Why?"}]}"#;
const OID: &str = "cccccccccccccccccccccccccccccccccccccccc";

fn mcp(srv: &Mock, messages: &[Value]) -> (Out, Vec<Value>) {
    let sb = Sandbox::new();
    let input: String = messages.iter().map(|m| format!("{m}\n")).collect();
    let o = sb.run_env(&srv.url, &["mcp"], &input, &[("GSTN_TOKEN", TOKEN)]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let out = o
        .stdout
        .lines()
        .map(|l| serde_json::from_str(l).expect("one JSON message per line"))
        .collect();
    (o, out)
}

fn call(id: i64, name: &str, arguments: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}})
}

fn by_id(out: &[Value], id: i64) -> &Value {
    out.iter()
        .find(|m| m["id"] == json!(id))
        .unwrap_or_else(|| panic!("no answer {id}: {out:?}"))
}

fn ok_server() -> Mock {
    Mock::start(|r| {
        let path = r.target.split('?').next().unwrap_or_default();
        if path.contains("/pipelines") {
            Resp::json(
                if r.method == "POST" && path.ends_with("/pipelines") {
                    201
                } else {
                    200
                },
                PIPELINE,
            )
        } else if path.contains("/discussions") {
            Resp::json(201, DISCUSSION)
        } else if path.ends_with("/checks") {
            Resp::json(200, r#"{"check_runs":[]}"#)
        } else if path.ends_with("/merge-requests") && r.method == "GET" {
            Resp::json(
                200,
                &format!(r#"{{"merge_requests":[{MR}],"next_before":null}}"#),
            )
        } else if path.ends_with("/merge-requests") {
            Resp::json(201, MR)
        } else {
            Resp::json(200, MR)
        }
    })
}

fn body_of(r: &common::Req) -> Value {
    serde_json::from_str(&r.body).expect("a JSON body")
}

#[test]
fn write_tools_are_listed_with_their_annotations() {
    let srv = ok_server();
    let (_, out) = mcp(
        &srv,
        &[json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})],
    );
    let tools = by_id(&out, 1)["result"]["tools"]
        .as_array()
        .expect("tools")
        .clone();
    let find = |name: &str| {
        tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("{name} is not listed"))
            .clone()
    };
    for (name, destructive, idempotent) in [
        ("create_merge_request", false, false),
        ("update_merge_request", true, true),
        ("close_merge_request", true, true),
        ("reopen_merge_request", false, true),
        ("comment_on_merge_request", false, false),
        ("reply_to_discussion", false, false),
        ("run_pipeline", false, false),
        ("cancel_pipeline", true, true),
        ("retry_pipeline", false, false),
    ] {
        let a = &find(name)["annotations"];
        assert_eq!(a["readOnlyHint"], false, "{name}");
        assert_eq!(a["destructiveHint"], destructive, "{name}");
        assert_eq!(a["idempotentHint"], idempotent, "{name}");
    }
    for name in [
        "list_merge_requests",
        "get_merge_request",
        "list_merge_request_versions",
        "get_merge_request_diff",
        "compare_merge_request_versions",
        "get_merge_request_status",
        "list_merge_request_discussions",
        "get_merge_request_conflicts",
        "list_check_runs",
        "list_pipelines",
        "get_pipeline",
    ] {
        let t = find(name);
        assert_eq!(t["annotations"]["readOnlyHint"], true, "{name}");
        assert!(t["annotations"].get("destructiveHint").is_none(), "{name}");
    }
    // Kept for people: never listed.
    for name in tools.iter().map(|t| t["name"].as_str().expect("name")) {
        for word in [
            "approve",
            "merge_merge",
            "resolve",
            "deploy",
            "token",
            "webhook",
            "mirror",
            "binding",
        ] {
            assert!(!name.contains(word), "{name}");
        }
    }
    let state = &find("list_merge_requests")["inputSchema"]["properties"]["state"];
    assert_eq!(state["enum"], json!(["open", "closed", "merged", "all"]));
    assert_eq!(
        find("list_pipelines")["inputSchema"]["properties"]["limit"]["maximum"],
        100
    );
    assert!(srv.seen().is_empty(), "listing sends nothing");
}

#[test]
fn merge_request_read_tools_call_their_routes() {
    let srv = ok_server();
    let (_, out) = mcp(
        &srv,
        &[
            call(
                1,
                "list_merge_requests",
                json!({"project_id":7,"state":"all","limit":5,"before":40}),
            ),
            call(2, "get_merge_request", json!({"project_id":7,"iid":3})),
            call(
                3,
                "list_merge_request_versions",
                json!({"project_id":7,"iid":3}),
            ),
            call(
                4,
                "get_merge_request_diff",
                json!({"project_id":7,"iid":3,"version":2}),
            ),
            call(
                5,
                "compare_merge_request_versions",
                json!({"project_id":7,"iid":3,"from":1,"to":2}),
            ),
            call(
                6,
                "get_merge_request_status",
                json!({"project_id":7,"iid":3}),
            ),
            call(
                7,
                "list_merge_request_discussions",
                json!({"project_id":7,"iid":3}),
            ),
            call(
                8,
                "get_merge_request_conflicts",
                json!({"project_id":7,"iid":3}),
            ),
        ],
    );
    for id in 1..=8 {
        assert_eq!(by_id(&out, id)["result"]["isError"], false, "{id}");
    }
    assert_eq!(
        by_id(&out, 1)["result"]["structuredContent"]["merge_requests"][0]["iid"],
        3
    );
    let seen = srv.seen();
    let targets: Vec<&str> = seen.iter().map(|r| r.target.as_str()).collect();
    assert_eq!(
        targets,
        [
            "/api/v1/projects/7/merge-requests?state=all&limit=5&before=40",
            "/api/v1/projects/7/merge-requests/3",
            "/api/v1/projects/7/merge-requests/3/versions",
            "/api/v1/projects/7/merge-requests/3/versions/2/diff",
            "/api/v1/projects/7/merge-requests/3/compare?from=1&to=2",
            "/api/v1/projects/7/merge-requests/3/status",
            "/api/v1/projects/7/merge-requests/3/discussions",
            "/api/v1/projects/7/merge-requests/3/conflicts",
        ]
    );
    for r in &seen {
        assert_eq!(r.method, "GET");
        assert_eq!(
            r.headers.get("forge-context-audience").map(String::as_str),
            Some("agent")
        );
        assert_eq!(
            r.headers.get("authorization").map(String::as_str),
            Some(format!("Bearer {TOKEN}").as_str())
        );
    }
}

#[test]
fn check_and_pipeline_read_tools_call_their_routes() {
    let srv = ok_server();
    let upper = OID.to_ascii_uppercase();
    let (_, out) = mcp(
        &srv,
        &[
            call(1, "list_check_runs", json!({"project_id":7,"sha":upper})),
            call(
                2,
                "list_pipelines",
                json!({"project_id":7,"ref":"feature/a b","limit":10}),
            ),
            call(3, "get_pipeline", json!({"project_id":7,"pipeline_id":12})),
        ],
    );
    for id in 1..=3 {
        assert_eq!(by_id(&out, id)["result"]["isError"], false, "{id}");
    }
    let targets: Vec<String> = srv.seen().into_iter().map(|r| r.target).collect();
    assert_eq!(
        targets,
        [
            format!("/api/v1/projects/7/commits/{OID}/checks"),
            "/api/v1/projects/7/pipelines?ref=feature%2Fa%20b&limit=10".to_owned(),
            "/api/v1/projects/7/pipelines/12".to_owned(),
        ]
    );
}

/// The write tools' happy paths: each sends its method, path and JSON body
/// once, with the caller's token as the agent; stderr has one line per
/// write with its request id.
#[test]
fn merge_request_write_tools_send_their_bodies() {
    let srv = ok_server();
    let (o, out) = mcp(
        &srv,
        &[
            call(
                1,
                "create_merge_request",
                json!({"project_id":7,"source_branch":"feature/reload","target_branch":"main","title":"Add reload","description":"PBI-43\n\n- reload","draft":true}),
            ),
            call(
                2,
                "update_merge_request",
                json!({"project_id":7,"iid":3,"draft":false}),
            ),
            call(
                3,
                "update_merge_request",
                json!({"project_id":7,"iid":3,"title":"Reload","description":""}),
            ),
            call(4, "close_merge_request", json!({"project_id":7,"iid":3})),
            call(5, "reopen_merge_request", json!({"project_id":7,"iid":3})),
            call(
                6,
                "comment_on_merge_request",
                json!({"project_id":7,"iid":3,"body":"General note"}),
            ),
            call(
                7,
                "comment_on_merge_request",
                json!({"project_id":7,"iid":3,"body":"Why?","version":2,"path":"src/a.rs","line":10,"side":"old"}),
            ),
            call(
                8,
                "reply_to_discussion",
                json!({"project_id":7,"iid":3,"discussion_id":5,"body":"Because."}),
            ),
        ],
    );
    for id in 1..=8 {
        assert_eq!(
            by_id(&out, id)["result"]["isError"],
            false,
            "{id}: {:?}",
            by_id(&out, id)
        );
    }
    assert_eq!(by_id(&out, 1)["result"]["structuredContent"]["iid"], 3);
    let seen = srv.seen();
    let calls: Vec<(&str, &str)> = seen
        .iter()
        .map(|r| (r.method.as_str(), r.target.as_str()))
        .collect();
    assert_eq!(
        calls,
        [
            ("POST", "/api/v1/projects/7/merge-requests"),
            ("PATCH", "/api/v1/projects/7/merge-requests/3"),
            ("PATCH", "/api/v1/projects/7/merge-requests/3"),
            ("POST", "/api/v1/projects/7/merge-requests/3/close"),
            ("POST", "/api/v1/projects/7/merge-requests/3/reopen"),
            ("POST", "/api/v1/projects/7/merge-requests/3/discussions"),
            ("POST", "/api/v1/projects/7/merge-requests/3/discussions"),
            (
                "POST",
                "/api/v1/projects/7/merge-requests/3/discussions/5/notes"
            ),
        ]
    );
    assert_eq!(
        body_of(&seen[0]),
        json!({"source_branch":"feature/reload","target_branch":"main","title":"Add reload","description":"PBI-43\n\n- reload","draft":true})
    );
    assert_eq!(body_of(&seen[1]), json!({"draft":false}));
    assert_eq!(
        body_of(&seen[2]),
        json!({"title":"Reload","description":""})
    );
    assert!(
        seen[3].body.is_empty() && seen[4].body.is_empty(),
        "close and reopen send no body"
    );
    assert_eq!(body_of(&seen[5]), json!({"body":"General note"}));
    assert_eq!(
        body_of(&seen[6]),
        json!({"body":"Why?","position":{"version":2,"path":"src/a.rs","line":10,"side":"old"}})
    );
    assert_eq!(body_of(&seen[7]), json!({"body":"Because."}));
    for r in &seen {
        assert_eq!(
            r.headers.get("forge-context-audience").map(String::as_str),
            Some("agent")
        );
        assert_eq!(
            r.headers.get("authorization").map(String::as_str),
            Some(format!("Bearer {TOKEN}").as_str())
        );
    }
    assert_eq!(o.stderr.matches("gstn mcp: ").count(), 8, "{}", o.stderr);
    assert!(
        o.stderr
            .contains("gstn mcp: create_merge_request done (request id"),
        "{}",
        o.stderr
    );
    assert!(!o.stderr.contains(TOKEN) && !o.stdout.contains(TOKEN));
}

#[test]
fn pipeline_write_tools_send_their_requests() {
    let srv = ok_server();
    let (_, out) = mcp(
        &srv,
        &[
            call(1, "run_pipeline", json!({"project_id":7,"ref":"main"})),
            call(
                2,
                "cancel_pipeline",
                json!({"project_id":7,"pipeline_id":12}),
            ),
            call(
                3,
                "retry_pipeline",
                json!({"project_id":7,"pipeline_id":12}),
            ),
        ],
    );
    for id in 1..=3 {
        assert_eq!(by_id(&out, id)["result"]["isError"], false, "{id}");
        assert_eq!(
            by_id(&out, id)["result"]["structuredContent"]["id"],
            12,
            "{id}"
        );
    }
    let seen = srv.seen();
    let calls: Vec<(&str, &str)> = seen
        .iter()
        .map(|r| (r.method.as_str(), r.target.as_str()))
        .collect();
    assert_eq!(
        calls,
        [
            ("POST", "/api/v1/projects/7/pipelines"),
            ("POST", "/api/v1/projects/7/pipelines/12/cancel"),
            ("POST", "/api/v1/projects/7/pipelines/12/retry"),
        ]
    );
    assert_eq!(body_of(&seen[0]), json!({"ref":"main"}));
}

/// A refusal of a write (an agent token where the server keeps the route
/// for people, a Developer's right missing) is a tool result with the code,
/// the message and the request id only.
#[test]
fn a_refused_write_is_a_tool_error_with_request_id() {
    let srv = Mock::start(|_| {
        Resp::json(
            403,
            r#"{"error_code":"FG-AUTH-0002","user_message":"You do not have permission for this action.","request_id":"req-w-403","detail":"reason=agent_token"}"#,
        )
    });
    let (o, out) = mcp(
        &srv,
        &[
            call(
                1,
                "create_merge_request",
                json!({"project_id":7,"source_branch":"a","target_branch":"main","title":"t"}),
            ),
            call(
                2,
                "cancel_pipeline",
                json!({"project_id":7,"pipeline_id":12}),
            ),
            call(
                3,
                "reply_to_discussion",
                json!({"project_id":7,"iid":3,"discussion_id":5,"body":"x"}),
            ),
        ],
    );
    for id in 1..=3 {
        let r = &by_id(&out, id)["result"];
        assert_eq!(r["isError"], true, "{id}");
        let mut keys: Vec<&str> = r["structuredContent"]
            .as_object()
            .expect("structuredContent")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["error_code", "request_id", "user_message"], "{id}");
        assert_eq!(r["structuredContent"]["error_code"], "FG-AUTH-0002");
        assert_eq!(r["structuredContent"]["request_id"], "req-w-403");
    }
    assert!(!o.stdout.contains("agent_token"), "{}", o.stdout);
    assert!(
        o.stderr
            .contains("create_merge_request failed: FG-AUTH-0002"),
        "{}",
        o.stderr
    );
}

#[test]
fn invalid_arguments_of_the_new_tools_send_nothing() {
    let srv = ok_server();
    let (_, out) = mcp(
        &srv,
        &[
            // update: nothing to change
            call(1, "update_merge_request", json!({"project_id":7,"iid":3})),
            // inline comment: version without path and line
            call(
                2,
                "comment_on_merge_request",
                json!({"project_id":7,"iid":3,"body":"x","version":2}),
            ),
            // side without a position
            call(
                3,
                "comment_on_merge_request",
                json!({"project_id":7,"iid":3,"body":"x","side":"new"}),
            ),
            call(
                4,
                "comment_on_merge_request",
                json!({"project_id":7,"iid":3,"body":"x","version":2,"path":"a","line":1,"side":"left"}),
            ),
            // empty and control-character bodies
            call(
                5,
                "reply_to_discussion",
                json!({"project_id":7,"iid":3,"discussion_id":5,"body":"  "}),
            ),
            call(
                6,
                "comment_on_merge_request",
                json!({"project_id":7,"iid":3,"body":"a\u{1b}[2J"}),
            ),
            // a commit id that is no commit id (would go into the path)
            call(
                7,
                "list_check_runs",
                json!({"project_id":7,"sha":"../../admin"}),
            ),
            call(
                8,
                "list_merge_requests",
                json!({"project_id":7,"state":"draft"}),
            ),
            call(9, "list_pipelines", json!({"project_id":7,"limit":101})),
            call(
                10,
                "create_merge_request",
                json!({"project_id":7,"source_branch":"a","target_branch":"main"}),
            ),
            call(
                11,
                "create_merge_request",
                json!({"project_id":7,"source_branch":"a","target_branch":"main","title":"t","draft":"yes"}),
            ),
            call(
                12,
                "get_pipeline",
                json!({"project_id":7,"pipeline_id":"12"}),
            ),
            call(
                13,
                "compare_merge_request_versions",
                json!({"project_id":7,"iid":3,"from":1}),
            ),
            call(14, "run_pipeline", json!({"project_id":7,"ref":"main\nx"})),
            call(
                15,
                "create_merge_request",
                json!({"project_id":7,"source_branch":"a","target_branch":"main","title":"t","description":"x".repeat(32769)}),
            ),
            call(16, "get_merge_request", json!({"project_id":7,"iid":0})),
            // kept for people: no such tool
            call(17, "approve_merge_request", json!({"project_id":7,"iid":3})),
            call(18, "merge_merge_request", json!({"project_id":7,"iid":3})),
        ],
    );
    for id in 1..=18 {
        assert_eq!(
            by_id(&out, id)["error"]["code"],
            -32602,
            "{id}: {:?}",
            by_id(&out, id)
        );
    }
    assert!(srv.seen().is_empty(), "no invalid call reached the server");
}

/// The whole tool list, in order: adding or removing a tool is a decision
/// this test makes visible.
#[test]
fn the_tool_list_is_exactly_the_manifest() {
    let srv = ok_server();
    let (_, out) = mcp(
        &srv,
        &[json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})],
    );
    let names: Vec<&str> = by_id(&out, 1)["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|t| t["name"].as_str().expect("name"))
        .collect();
    assert_eq!(
        names,
        [
            "get_work_item_context",
            "get_requirement",
            "search_requirements",
            "get_trace_matrix",
            "get_merge_request_context",
            "list_documents",
            "get_document",
            "get_document_block",
            "get_document_referenced_by",
            "list_merge_requests",
            "get_merge_request",
            "list_merge_request_versions",
            "get_merge_request_diff",
            "compare_merge_request_versions",
            "get_merge_request_status",
            "list_merge_request_discussions",
            "get_merge_request_conflicts",
            "list_check_runs",
            "list_pipelines",
            "get_pipeline",
            "get_job_log",
            "get_pipeline_diagnosis",
            "create_merge_request",
            "update_merge_request",
            "close_merge_request",
            "reopen_merge_request",
            "comment_on_merge_request",
            "reply_to_discussion",
            "run_pipeline",
            "cancel_pipeline",
            "retry_pipeline",
        ]
    );
}

/// For every tool, dropping any one required argument is `-32602` and
/// nothing reaches the server.
#[test]
fn every_missing_required_argument_is_refused_before_sending() {
    let manifest = forge_client::agent_tools::manifest().expect("manifest");
    let mut calls = Vec::new();
    let mut id = 0;
    for t in &manifest.tools {
        let required: Vec<&forge_client::agent_tools::Param> =
            t.params.iter().filter(|p| p.required).collect();
        for dropped in &required {
            let mut args = serde_json::Map::new();
            for p in required.iter().filter(|p| p.name != dropped.name) {
                let v = match (p.kind, p.format) {
                    (forge_client::agent_tools::Kind::Integer, _) => json!(7),
                    (_, forge_client::agent_tools::Format::Oid) => json!(OID),
                    _ => json!("PBI-1"),
                };
                args.insert(p.name.clone(), v);
            }
            if let Some(first) = t.at_least_one.first() {
                args.insert(first.clone(), json!("x"));
            }
            id += 1;
            calls.push((
                id,
                t.name.clone(),
                dropped.name.clone(),
                call(id, &t.name, Value::Object(args)),
            ));
        }
    }
    assert!(calls.len() > 40, "{}", calls.len());
    let srv = ok_server();
    let messages: Vec<Value> = calls.iter().map(|c| c.3.clone()).collect();
    let (_, out) = mcp(&srv, &messages);
    for (id, tool, dropped, _) in &calls {
        let e = &by_id(&out, *id)["error"];
        assert_eq!(e["code"], -32602, "{tool} without {dropped}");
        assert_eq!(
            e["message"],
            format!("missing argument `{dropped}`"),
            "{tool}"
        );
    }
    assert!(
        srv.seen().is_empty(),
        "no call without a required argument was sent"
    );
}

fn assert_error_shape(o: &Out, out: &[Value], ids: &[i64], code: &str) {
    for id in ids {
        let r = &by_id(out, *id)["result"];
        assert_eq!(r["isError"], true, "{id}");
        let mut keys: Vec<&str> = r["structuredContent"]
            .as_object()
            .expect("structuredContent")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["error_code", "request_id", "user_message"], "{id}");
        assert_eq!(r["structuredContent"]["error_code"], code, "{id}");
        assert!(!r["structuredContent"]["user_message"]
            .as_str()
            .expect("message")
            .is_empty());
    }
    assert!(!o.stdout.contains(TOKEN) && !o.stderr.contains(TOKEN));
}

/// A new read tool and a new write tool against a 404, an HTML 502 (a proxy
/// page) and an unreachable server: the code, the message and the request
/// id only — never the page, a path or the token.
#[test]
fn new_tools_fail_with_the_error_shape_only() {
    let calls = [
        call(1, "get_pipeline", json!({"project_id":7,"pipeline_id":12})),
        call(
            2,
            "create_merge_request",
            json!({"project_id":7,"source_branch":"a","target_branch":"main","title":"t"}),
        ),
    ];
    let srv = Mock::start(|_| {
        Resp::json(
            404,
            r#"{"error_code":"FG-REPO-0005","user_message":"The project or repository was not found.","request_id":"req-404","internal":"/srv/forge/repos/x.git"}"#,
        )
    });
    let (o, out) = mcp(&srv, &calls);
    assert_error_shape(&o, &out, &[1, 2], "FG-REPO-0005");
    assert_eq!(
        by_id(&out, 2)["result"]["structuredContent"]["request_id"],
        "req-404"
    );
    assert!(!o.stdout.contains("/srv/forge"), "{}", o.stdout);

    let srv = Mock::start(|_| Resp {
        status: 502,
        headers: vec![("Content-Type".into(), "text/html".into())],
        body: "<html><body>Bad Gateway at proxy-01.internal</body></html>".into(),
    });
    let (o, out) = mcp(&srv, &calls);
    let code = by_id(&out, 1)["result"]["structuredContent"]["error_code"]
        .as_str()
        .expect("code")
        .to_owned();
    assert!(code.starts_with("FG-"), "{code}");
    assert_error_shape(&o, &out, &[1, 2], &code);
    assert!(
        !o.stdout.contains("proxy-01") && !o.stdout.contains("<html"),
        "{}",
        o.stdout
    );

    // Nothing listens on port 1.
    let sb = Sandbox::new();
    let input: String = calls.iter().map(|m| format!("{m}\n")).collect();
    let o = sb.run_env(
        "http://127.0.0.1:1",
        &["mcp"],
        &input,
        &[("GSTN_TOKEN", TOKEN)],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    let out: Vec<Value> = o
        .stdout
        .lines()
        .map(|l| serde_json::from_str(l).expect("json"))
        .collect();
    assert_error_shape(&o, &out, &[1, 2], "FG-CLI-0003");
}

/// ADR-0029 decision 5 (M4 C5): the CI output tools send the agent
/// audience header with the caller's token, and a refusal of a marked
/// repository's log or diagnosis is a tool error carrying FG-AUTH-0022 —
/// never the log text, whatever else the server's answer held.
#[test]
fn ci_output_tools_declare_the_agent_and_fail_closed() {
    let srv = Mock::start(|_| {
        Resp::json(
            403,
            r#"{"error_code":"FG-AUTH-0022","user_message":"This repository holds a confidential document (or it could not be checked), so an AI agent's account cannot clone, fetch or download an archive of it, or read its CI logs and diagnosis. Use a person's account.","request_id":"req-ci-22","text":"SECRET LOG LINE"}"#,
        )
    });
    let (o, out) = mcp(
        &srv,
        &[
            call(
                1,
                "get_job_log",
                json!({"project_id":7,"pipeline_id":12,"job_id":9,"attempt":2,"from":0}),
            ),
            call(
                2,
                "get_pipeline_diagnosis",
                json!({"project_id":7,"pipeline_id":12}),
            ),
        ],
    );
    for id in 1..=2 {
        let r = &by_id(&out, id)["result"];
        assert_eq!(r["isError"], true, "{id}");
        assert_eq!(r["structuredContent"]["error_code"], "FG-AUTH-0022", "{id}");
        assert_eq!(r["structuredContent"]["request_id"], "req-ci-22", "{id}");
    }
    assert!(!o.stdout.contains("SECRET LOG LINE"), "{}", o.stdout);
    let seen = srv.seen();
    let targets: Vec<&str> = seen.iter().map(|r| r.target.as_str()).collect();
    assert_eq!(
        targets,
        [
            "/api/v1/projects/7/pipelines/12/jobs/9/log?attempt=2&from=0",
            "/api/v1/projects/7/pipelines/12/diagnosis",
        ]
    );
    for r in &seen {
        assert_eq!(r.method, "GET");
        assert_eq!(
            r.headers.get("forge-context-audience").map(String::as_str),
            Some("agent")
        );
    }
}
