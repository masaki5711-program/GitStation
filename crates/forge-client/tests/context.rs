//! End-to-end tests of `gstn context` and `gstn mcp` (M3 X5, plan 03 §6)
//! against a scripted loopback server. All data is synthetic.

#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

mod common;
use common::{Mock, Out, Resp, Sandbox, TOKEN};
use serde_json::{json, Value};

fn gstn(sb: &Sandbox, srv: &Mock, args: &[&str], stdin: &str) -> Out {
    sb.run_env(&srv.url, args, stdin, &[("GSTN_TOKEN", TOKEN)])
}

const C: &str = "0123456789abcdef0123456789abcdef01234567";

fn source(path: &str, line: u32) -> String {
    format!(
        r#"{{"project_id":7,"project":"acme/app","version":"main","commit":"{C}","path":"{path}","line":{line}}}"#
    )
}

fn work_item_body() -> String {
    format!(
        r#"{{"work_item":{{"ref":"PBI-43","integration_id":1,"kanflow_project_id":"kf-1","known":true,"first_seen_at":"2026-11-01T00:00:00Z"}},"merge_requests":[{{"iid":3,"title":"Log out","state":"open","draft":false,"source_branch":"feature","target_branch":"main","link_source":"auto","linked_at":"2026-11-01T00:00:00Z","source":{}}}],"merge_requests_truncated":false,"ref":{{"type":"branch","name":"main","full_name":"refs/heads/main","commit":"{C}"}},"tests":[{{"target":"PBI-43/AC-1","source":{}}}],"test_count":1,"incomplete":null}}"#,
        source("src/a.rs", 0),
        source("tests/login.rs", 4)
    )
}

#[test]
fn context_commands_call_their_routes_and_show_sources() {
    let srv = Mock::start(|r| {
        if r.target.contains("/context/work-item") {
            return Resp::json(200, &work_item_body());
        }
        if r.target.contains("/context/requirement") {
            return Resp::json(
                200,
                &format!(
                    r#"{{"ref":{{"type":"tag","name":"v1.5","full_name":"refs/tags/v1.5","commit":"{C}"}},"requirement":{{"id":"APP-CORE-1","body":"Reload within 10 s.","priority":"must","deprecated":false,"source":{}}},"document":{{"path":"spec/core.md","spec":"app/core","title":"Core","status":"approved","confidential":false}},"tests":[],"test_count":0,"versions":[{{"name":"v1.5","commit":"{C}","status":"same","source":null}}],"versions_truncated":false,"incomplete":null}}"#,
                    source("spec/core.md", 6)
                ),
            );
        }
        Resp::json(
            200,
            &format!(
                r#"{{"merge_request":{{"iid":12,"title":"Fix","description":"","description_truncated":false,"state":"open","draft":false,"source_branch":"fix","target_branch":"main","merge_base_sha":null,"source":{}}},"files":[],"files_truncated":false,"requirements":[],"requirements_truncated":false,"work_items":[],"checks":[],"withheld":{{"error_code":"FG-AUTH-0021","documents":1,"requirements":2,"work_items":0}},"incomplete":null}}"#,
                source("", 0)
            ),
        )
    });
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &["context", "work-item", "--project", "7", "--ref", "PBI-43"],
        "",
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(
        srv.seen().pop().expect("request").target,
        "/api/v1/projects/7/context/work-item?ref=PBI-43"
    );
    for want in ["PBI-43", "!3", "PBI-43/AC-1", "tests/login.rs:4", C] {
        assert!(o.stdout.contains(want), "{want}\n{}", o.stdout);
    }
    let r = gstn(
        &sb,
        &srv,
        &[
            "context",
            "requirement",
            "--project",
            "7",
            "--id",
            "APP-CORE-1",
            "--ref",
            "v1.5",
            "--json",
        ],
        "",
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(
        srv.seen().pop().expect("request").target,
        "/api/v1/projects/7/context/requirement?id=APP-CORE-1&ref=v1.5"
    );
    assert_eq!(r.json()["requirement"]["source"]["line"], 6);
    let m = gstn(
        &sb,
        &srv,
        &["context", "merge-request", "--project", "7", "--iid", "12"],
        "",
    );
    assert_eq!(m.code, 0, "{}", m.stderr);
    let seen = srv.seen().pop().expect("request");
    assert_eq!(seen.target, "/api/v1/projects/7/merge-requests/12/context");
    // A person's command does not ask to be answered as an agent.
    assert!(!seen.headers.contains_key("forge-context-audience"));
    assert!(
        m.stdout.contains("withheld for an AI agent"),
        "{}",
        m.stdout
    );
    // Ids are checked before anything is sent.
    let bad = gstn(
        &sb,
        &srv,
        &["context", "merge-request", "--project", "7", "--iid", "0"],
        "",
    );
    assert_ne!(bad.code, 0);
}

fn lines(out: &str) -> Vec<Value> {
    out.lines()
        .map(|l| serde_json::from_str(l).expect("one JSON message per line"))
        .collect()
}

fn script(messages: &[Value]) -> String {
    let mut s: String = messages.iter().map(|m| format!("{m}\n")).collect();
    s.push_str("not json\n");
    s
}

#[test]
fn mcp_lists_read_only_tools_and_calls_the_api_as_an_agent() {
    let srv = Mock::start(|r| {
        if r.target.contains("/context/requirement") {
            return Resp::json(
                403,
                r#"{"error_code":"FG-AUTH-0021","user_message":"This item is marked confidential.","request_id":"req-mcp-1"}"#,
            );
        }
        if r.target.contains("/specs/requirements") {
            return Resp::json(
                200,
                r#"{"requirements":[{"id":"APP-CORE-1"}],"withheld":null}"#,
            );
        }
        Resp::json(200, &work_item_body())
    });
    let sb = Sandbox::new();
    let input = script(&[
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get_work_item_context","arguments":{"project_id":7,"ref":"PBI-43"}}}),
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"search_requirements","arguments":{"project_id":7,"query":"reload a","limit":5}}}),
        json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"get_requirement","arguments":{"project_id":7,"id":"APP-SEC-1"}}}),
        json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"get_trace_matrix","arguments":{"project_id":7,"token":"x"}}}),
        json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"delete_project","arguments":{}}}),
        json!({"jsonrpc":"2.0","id":8,"method":"resources/list"}),
        json!({"jsonrpc":"2.0","id":9,"method":"ping"}),
    ]);
    let o = gstn(&sb, &srv, &["mcp"], &input);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let out = lines(&o.stdout);
    assert_eq!(out.len(), 10, "{}", o.stdout);
    let by_id = |id: i64| {
        out.iter()
            .find(|m| m["id"] == json!(id))
            .unwrap_or_else(|| panic!("no answer {id}: {}", o.stdout))
    };
    assert_eq!(by_id(1)["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(by_id(1)["result"]["serverInfo"]["name"], "gstn");
    let names: Vec<&str> = by_id(2)["result"]["tools"]
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
            "get_merge_request_context"
        ]
    );
    for t in by_id(2)["result"]["tools"].as_array().expect("tools") {
        assert_eq!(t["annotations"]["readOnlyHint"], true);
        assert_eq!(t["inputSchema"]["additionalProperties"], false);
    }
    let call = &by_id(3)["result"];
    assert_eq!(call["isError"], false);
    assert_eq!(call["structuredContent"]["work_item"]["ref"], "PBI-43");
    assert!(call["content"][0]["text"]
        .as_str()
        .expect("text")
        .contains("PBI-43/AC-1"));
    assert_eq!(by_id(4)["result"]["isError"], false);
    // A refusal is a tool result with the code and the request id.
    let refused = &by_id(5)["result"];
    assert_eq!(refused["isError"], true);
    assert_eq!(refused["structuredContent"]["error_code"], "FG-AUTH-0021");
    assert!(refused["content"][0]["text"]
        .as_str()
        .expect("text")
        .contains("req-mcp-1"));
    assert!(
        o.stderr.contains("get_requirement failed: FG-AUTH-0021"),
        "{}",
        o.stderr
    );
    // Unknown arguments, tools and methods; garbage.
    assert_eq!(by_id(6)["error"]["code"], -32602);
    assert_eq!(by_id(7)["error"]["code"], -32602);
    assert_eq!(by_id(8)["error"]["code"], -32601);
    assert_eq!(by_id(9)["result"], json!({}));
    assert!(out
        .iter()
        .any(|m| m["id"].is_null() && m["error"]["code"] == -32700));
    // Every request: GET, the caller's token, answered as an AI agent.
    let seen = srv.seen();
    assert_eq!(seen.len(), 3, "the invalid calls sent nothing");
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
    assert_eq!(
        seen[0].target,
        "/api/v1/projects/7/context/work-item?ref=PBI-43"
    );
    assert_eq!(
        seen[1].target,
        "/api/v1/projects/7/specs/requirements?q=reload%20a&limit=5"
    );
    assert!(!o.stdout.contains(TOKEN));
    assert!(!o.stderr.contains(TOKEN));
}

/// Review X5 (rust HIGH-1, e, f): an oversized line, a batch, a non-object,
/// a bad id and a notification; the server keeps serving after each and
/// ends cleanly at the end of input.
#[test]
fn mcp_refuses_malformed_messages_and_keeps_serving() {
    let srv = Mock::start(|_| Resp::json(200, &work_item_body()));
    let sb = Sandbox::new();
    let mut input = String::new();
    input.push_str(&"x".repeat(2 * 1024 * 1024));
    input.push('\n');
    for m in [
        json!([{"jsonrpc":"2.0","id":1,"method":"ping"}]),
        json!("ping"),
        json!({"jsonrpc":"2.0","id":true,"method":"ping"}),
        json!({"jsonrpc":"2.0","id":null,"method":"ping"}),
        json!({"jsonrpc":"2.0","id":{"a":1},"method":"ping"}),
        json!({"jsonrpc":"2.0","method":"tools/call","params":{"name":"get_trace_matrix","arguments":{"project_id":7}}}),
        json!({"jsonrpc":"2.0","id":"s-1","method":"ping"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_work_item_context","arguments":{"project_id":7,"ref":"PBI-43"}}}),
    ] {
        input.push_str(&format!("{m}\n"));
    }
    let o = gstn(&sb, &srv, &["mcp"], &input);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let out = lines(&o.stdout);
    let codes: Vec<Value> = out.iter().map(|m| m["error"]["code"].clone()).collect();
    assert_eq!(
        codes,
        [
            json!(-32600),
            json!(-32600),
            json!(-32600),
            json!(-32600),
            json!(-32600),
            json!(-32600),
            Value::Null,
            Value::Null
        ],
        "{}",
        o.stdout
    );
    for refused in &out[..6] {
        assert_eq!(refused["id"], Value::Null);
    }
    assert_eq!(out[6]["id"], "s-1");
    assert_eq!(out[7]["id"], 2);
    assert_eq!(out[7]["result"]["isError"], false);
    // The notification-shaped call sent nothing and got no answer.
    assert_eq!(srv.seen().len(), 1);
    assert!(
        o.stdout.len() < 64 * 1024,
        "the oversized line was not echoed"
    );
}
