//! End-to-end tests of the knowledge tools of `gstn mcp` (plan 20 X1,
//! ADR-0040 Amendment X1): `search_knowledge` (`GET /api/v1/search`) and
//! `get_project_wiki` (`GET /api/v1/projects/{id}/wiki`) against a scripted
//! loopback server. Each is one `GET` as an AI agent: the audience header
//! is what makes the server leave confidential rows out of the read. All
//! data is synthetic.

#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

mod common;
use common::{Mock, Out, Resp, Sandbox, TOKEN};
use serde_json::{json, Value};

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

const PAGE: &str = r#"{"items":[{"kind":"document","project_id":7,"project_path":"acme/web-wiki","title":"設備の点検","iid":null,"path":"pages/WIKI-0001.md","snippet":{"text":"設備の点検の手順","highlights":[{"start":0,"end":2}],"cut_before":false,"cut_after":false},"updated_at":"2026-11-25T00:00:00Z","truncated":false}],"next_cursor":"AgABc2VhbGVk","has_more":true}"#;
const WIKI: &str = r#"{"wiki_project_id":8,"wiki_path":"acme/web-wiki","display_name":"web wiki","default_branch":"main"}"#;

fn knowledge_server() -> Mock {
    Mock::start(|r| {
        if r.target.starts_with("/api/v1/search") {
            Resp::json(200, PAGE)
        } else {
            Resp::json(200, WIKI)
        }
    })
}

#[test]
fn knowledge_tools_are_listed_read_only_with_their_arguments() {
    let srv = knowledge_server();
    let (_, out) = mcp(
        &srv,
        &[json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})],
    );
    let tools = by_id(&out, 1)["result"]["tools"]
        .as_array()
        .expect("tools")
        .clone();
    let want: [(&str, &[&str], &[&str]); 2] = [
        (
            "search_knowledge",
            &["query"],
            &["cursor", "limit", "project_id", "query", "scope"],
        ),
        ("get_project_wiki", &["project_id"], &["project_id"]),
    ];
    for (name, required, properties) in want {
        let t = tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("{name} is not listed"));
        assert_eq!(t["annotations"]["readOnlyHint"], true, "{name}");
        assert_eq!(t["inputSchema"]["additionalProperties"], false, "{name}");
        assert_eq!(t["inputSchema"]["required"], json!(required), "{name}");
        let props: Vec<&str> = t["inputSchema"]["properties"]
            .as_object()
            .expect("properties")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(props, properties, "{name}");
    }
    let search = tools
        .iter()
        .find(|t| t["name"] == "search_knowledge")
        .expect("listed");
    let description = search["description"].as_str().expect("description");
    assert!(description.contains("Confidential"), "{description}");
    assert!(srv.seen().is_empty(), "listing sends nothing");
}

#[test]
fn search_knowledge_calls_the_search_route_encoded() {
    let srv = knowledge_server();
    let (_, out) = mcp(
        &srv,
        &[call(
            1,
            "search_knowledge",
            json!({"query":"設備 pump","scope":"merge_request,document","project_id":7,"limit":5,"cursor":"AgABc2VhbGVk"}),
        )],
    );
    let r = &by_id(&out, 1)["result"];
    assert_eq!(r["isError"], false, "{r}");
    assert_eq!(
        r["structuredContent"]["items"][0]["path"],
        "pages/WIKI-0001.md"
    );
    assert_eq!(r["structuredContent"]["has_more"], true);
    assert_eq!(
        srv.seen()[0].target,
        "/api/v1/search?q=%E8%A8%AD%E5%82%99%20pump&scope=merge_request%2Cdocument&project=7&limit=5&cursor=AgABc2VhbGVk"
    );
}

/// The client half of `agent_search_withholds_confidential` (the server
/// half is in forge-web's search tests): every knowledge call declares the
/// agent audience with the caller's own token, so the server leaves the
/// confidential rows out whatever the token is.
#[test]
fn agent_search_withholds_confidential() {
    let srv = knowledge_server();
    let (_, out) = mcp(
        &srv,
        &[
            call(1, "search_knowledge", json!({"query":"設備"})),
            call(2, "get_project_wiki", json!({"project_id":7})),
        ],
    );
    for id in 1..=2 {
        assert_eq!(by_id(&out, id)["result"]["isError"], false, "{id}");
    }
    let seen = srv.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].target, "/api/v1/search?q=%E8%A8%AD%E5%82%99");
    assert_eq!(seen[1].target, "/api/v1/projects/7/wiki");
    for r in &seen {
        assert_eq!(r.method, "GET");
        assert_eq!(
            r.headers.get("forge-context-audience").map(String::as_str),
            Some("agent"),
            "{}",
            r.target
        );
        assert_eq!(
            r.headers.get("authorization").map(String::as_str),
            Some(format!("Bearer {TOKEN}").as_str())
        );
    }
}

#[test]
fn get_project_wiki_answers_the_link() {
    let srv = knowledge_server();
    let (_, out) = mcp(
        &srv,
        &[call(1, "get_project_wiki", json!({"project_id":7}))],
    );
    let r = &by_id(&out, 1)["result"];
    assert_eq!(r["isError"], false);
    assert_eq!(r["structuredContent"]["wiki_project_id"], 8);
}

#[test]
fn a_refused_search_is_a_tool_error_with_request_id() {
    // A too-short query or an unreadable wiki: the server's error, nothing
    // more (an internal field it adds never reaches the agent).
    let srv = Mock::start(|r| {
        if r.target.starts_with("/api/v1/search") {
            Resp::json(
                400,
                r#"{"error_code":"FG-WEB-0006","user_message":"The request is invalid.","request_id":"req-search-400","internal":"tsquery syntax"}"#,
            )
        } else {
            Resp::json(
                404,
                r#"{"error_code":"FG-REPO-0005","user_message":"The project or repository was not found.","request_id":"req-wiki-404","internal":"reason=wiki_unreadable"}"#,
            )
        }
    });
    let (o, out) = mcp(
        &srv,
        &[
            call(1, "search_knowledge", json!({"query":"設"})),
            call(2, "get_project_wiki", json!({"project_id":7})),
        ],
    );
    for (id, code, rid) in [
        (1, "FG-WEB-0006", "req-search-400"),
        (2, "FG-REPO-0005", "req-wiki-404"),
    ] {
        let r = &by_id(&out, id)["result"];
        assert_eq!(r["isError"], true, "{id}");
        assert_eq!(r["structuredContent"]["error_code"], code);
        assert_eq!(r["structuredContent"]["request_id"], rid);
        let mut keys: Vec<&str> = r["structuredContent"]
            .as_object()
            .expect("structuredContent")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["error_code", "request_id", "user_message"], "{id}");
    }
    assert!(!o.stdout.contains("tsquery"), "{}", o.stdout);
    assert!(!o.stdout.contains("wiki_unreadable"), "{}", o.stdout);
    for r in &srv.seen() {
        assert_eq!(
            r.headers.get("forge-context-audience").map(String::as_str),
            Some("agent"),
            "{}",
            r.target
        );
    }
}

#[test]
fn knowledge_arguments_at_their_bounds_are_sent() {
    let srv = knowledge_server();
    let scope = format!("document{}", ",".repeat(56)); // 64 bytes: the server judges it
    let (_, out) = mcp(
        &srv,
        &[
            call(
                1,
                "search_knowledge",
                json!({"query":"設".repeat(266),"limit":50}),
            ),
            call(
                2,
                "search_knowledge",
                json!({"query":"ab","limit":1,"scope":scope}),
            ),
            call(
                3,
                "search_knowledge",
                json!({"query":"ab","cursor":"c".repeat(128)}),
            ),
        ],
    );
    for id in 1..=3 {
        assert_eq!(by_id(&out, id)["result"]["isError"], false, "{id}");
    }
    assert_eq!(srv.seen().len(), 3, "every call at its bound was sent");
    let (_, out) = mcp(
        &srv,
        &[call(
            1,
            "search_knowledge",
            json!({"query":"ab","cursor":"c".repeat(129)}),
        )],
    );
    assert_eq!(by_id(&out, 1)["error"]["code"], -32602);
    assert_eq!(srv.seen().len(), 3, "the overlong cursor was not");
}

#[test]
fn bad_knowledge_arguments_are_invalid_params() {
    let srv = knowledge_server();
    let (_, out) = mcp(
        &srv,
        &[
            call(1, "search_knowledge", json!({})),
            call(2, "search_knowledge", json!({"query":"x".repeat(801)})),
            call(3, "search_knowledge", json!({"query":"ab","limit":51})),
            call(4, "search_knowledge", json!({"query":"ab","limit":0})),
            call(5, "search_knowledge", json!({"query":"ab","project_id":0})),
            call(
                6,
                "search_knowledge",
                json!({"query":"ab","scope":"x".repeat(65)}),
            ),
            call(7, "search_knowledge", json!({"query":"a\nb"})),
            call(8, "search_knowledge", json!({"query":"ab","page":2})),
            call(9, "get_project_wiki", json!({})),
            call(10, "get_project_wiki", json!({"project_id":"7"})),
        ],
    );
    for id in 1..=10 {
        assert_eq!(
            by_id(&out, id)["error"]["code"],
            -32602,
            "{id}: {:?}",
            by_id(&out, id)
        );
    }
    assert!(srv.seen().is_empty(), "no invalid call reached the server");
}
