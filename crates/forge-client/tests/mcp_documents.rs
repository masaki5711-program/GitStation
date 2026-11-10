//! End-to-end tests of the document tools of `gstn mcp` (plan 13 stage 3,
//! MR2): `list_documents`, `get_document`, `get_document_block` and
//! `get_document_referenced_by` against a scripted loopback server. Each is
//! one `GET` of the document API as an AI agent; the server decides what
//! is confidential. All data is synthetic.

#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

mod common;
use common::{Mock, Out, Resp, Sandbox, TOKEN};
use serde_json::{json, Value};

const DOC_TOOLS: [&str; 4] = [
    "list_documents",
    "get_document",
    "get_document_block",
    "get_document_referenced_by",
];

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

const LIST: &str = r#"{"documents":[{"id":"RA-2026-014","title":"Press line risk assessment","doctype":"safety.risk-assessment","path":"docs/RA-2026-014.md","classification":"internal","translations":[]}],"next_after":null,"withheld":null,"incomplete":null}"#;
const DOC: &str = r#"{"id":"RA-2026-014","title":"Press line risk assessment","doctype":"safety.risk-assessment","owner":"safety","classification":"internal","path":"docs/RA-2026-014.md","sections":[],"refs":[],"incomplete":null}"#;
const BLOCK: &str = r#"{"id":"RA-2026-014","title":"Press line risk assessment","doctype":"safety.risk-assessment","classification":"internal","path":"docs/RA-2026-014.md","section":"Hazards","block":{"kind":"hazard","id":"H-01","attrs":[],"display":"Pinch point"},"refs":[]}"#;
const REFERRERS: &str = r#"{"referrers":[{"id":"WI-2026-003","lang":null,"title":"Press line work instruction","classification":"internal","path":"docs/WI-2026-003.md","refs":[{"line":12,"block":"H-01"}]}],"total":1,"next_after":null,"incomplete":null}"#;

fn document_server() -> Mock {
    Mock::start(|r| {
        let path = r.target.split('?').next().unwrap_or_default();
        if path.ends_with("/referenced-by") {
            Resp::json(200, REFERRERS)
        } else if path.contains("/blocks/") {
            Resp::json(200, BLOCK)
        } else if path.ends_with("/documents") {
            Resp::json(200, LIST)
        } else {
            Resp::json(200, DOC)
        }
    })
}

#[test]
fn document_tools_are_listed_with_their_arguments() {
    let srv = document_server();
    let (_, out) = mcp(
        &srv,
        &[json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})],
    );
    let tools = by_id(&out, 1)["result"]["tools"]
        .as_array()
        .expect("tools")
        .clone();
    let want: [(&str, &[&str], &[&str]); 4] = [
        (
            "list_documents",
            &["project_id"],
            &["project_id", "ref", "doctype", "limit", "after"],
        ),
        (
            "get_document",
            &["project_id", "id"],
            &["project_id", "id", "ref", "lang"],
        ),
        (
            "get_document_block",
            &["project_id", "id", "block"],
            &["project_id", "id", "block", "ref", "lang"],
        ),
        (
            "get_document_referenced_by",
            &["project_id", "id"],
            &["project_id", "id", "block", "ref", "limit", "after"],
        ),
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
        let mut sorted = properties.to_vec();
        sorted.sort_unstable();
        assert_eq!(props, sorted, "{name}");
        assert!(
            !t["description"].as_str().expect("description").is_empty(),
            "{name}"
        );
    }
    assert!(srv.seen().is_empty(), "listing sends nothing");
}

#[test]
fn a_confidential_document_is_a_tool_error_with_request_id() {
    // The server answers a confidential document to an agent as not found;
    // anything it adds beyond the contract never reaches the agent.
    let srv = Mock::start(|_| {
        Resp::json(
            404,
            r#"{"error_code":"FG-REPO-0005","user_message":"The project or repository was not found.","request_id":"req-doc-404","internal":"/srv/forge/repos/secret.git"}"#,
        )
    });
    let (o, out) = mcp(
        &srv,
        &[
            call(1, "get_document", json!({"project_id":7,"id":"SEC-1"})),
            call(
                2,
                "get_document_block",
                json!({"project_id":7,"id":"SEC-1","block":"H-01"}),
            ),
            call(
                3,
                "get_document_referenced_by",
                json!({"project_id":7,"id":"SEC-1"}),
            ),
            call(4, "list_documents", json!({"project_id":7})),
        ],
    );
    for id in 1..=4 {
        let r = &by_id(&out, id)["result"];
        assert_eq!(r["isError"], true, "{id}");
        assert_eq!(r["structuredContent"]["error_code"], "FG-REPO-0005");
        assert_eq!(
            r["structuredContent"]["user_message"],
            "The project or repository was not found."
        );
        assert_eq!(r["structuredContent"]["request_id"], "req-doc-404");
        let mut keys: Vec<&str> = r["structuredContent"]
            .as_object()
            .expect("structuredContent")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["error_code", "request_id", "user_message"], "{id}");
        let text = r["content"][0]["text"].as_str().expect("text");
        assert!(text.contains("req-doc-404"), "{text}");
        assert!(text.contains("FG-REPO-0005"), "{text}");
    }
    assert!(!o.stdout.contains("/srv/forge"), "{}", o.stdout);
    assert!(!o.stderr.contains("/srv/forge"), "{}", o.stderr);
    assert!(
        o.stderr.contains("get_document failed: FG-REPO-0005"),
        "{}",
        o.stderr
    );
}

#[test]
fn the_agent_header_is_sent_on_every_document_tool() {
    let srv = document_server();
    let (_, out) = mcp(
        &srv,
        &[
            call(1, "list_documents", json!({"project_id":7})),
            call(
                2,
                "get_document",
                json!({"project_id":7,"id":"RA-2026-014"}),
            ),
            call(
                3,
                "get_document_block",
                json!({"project_id":7,"id":"RA-2026-014","block":"H-01"}),
            ),
            call(
                4,
                "get_document_referenced_by",
                json!({"project_id":7,"id":"RA-2026-014"}),
            ),
        ],
    );
    for id in 1..=4 {
        assert_eq!(by_id(&out, id)["result"]["isError"], false, "{id}");
    }
    let seen = srv.seen();
    assert_eq!(seen.len(), DOC_TOOLS.len());
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
fn an_overlong_argument_is_invalid_params() {
    let srv = document_server();
    let long_id = format!("A{}", "1".repeat(128)); // 129 bytes: past the id limit
    let long_text = "x".repeat(257); // past every text argument
    let (_, out) = mcp(
        &srv,
        &[
            call(1, "get_document", json!({"project_id":7,"id":long_id})),
            call(
                2,
                "get_document_block",
                json!({"project_id":7,"id":"RA-1","block":long_id}),
            ),
            call(
                3,
                "get_document_referenced_by",
                json!({"project_id":7,"id":long_id}),
            ),
            call(
                4,
                "list_documents",
                json!({"project_id":7,"after":long_text}),
            ),
            call(
                5,
                "list_documents",
                json!({"project_id":7,"doctype":long_text}),
            ),
            call(
                6,
                "get_document",
                json!({"project_id":7,"id":"RA-1","ref":long_text}),
            ),
            // Not overlong, still refused before any request.
            call(
                7,
                "get_document",
                json!({"project_id":7,"id":"../etc/passwd"}),
            ),
            call(
                8,
                "get_document_block",
                json!({"project_id":7,"id":"RA-1","block":"H 01"}),
            ),
            call(
                9,
                "get_document",
                json!({"project_id":7,"id":"RA-1","lang":"english"}),
            ),
            call(10, "list_documents", json!({"project_id":7,"limit":501})),
            call(
                11,
                "get_document_referenced_by",
                json!({"project_id":7,"id":"RA-1","limit":0}),
            ),
            call(
                12,
                "get_document_referenced_by",
                json!({"project_id":7,"id":"RA-1","block":"-x"}),
            ),
            call(13, "get_document", json!({"project_id":7})),
        ],
    );
    for id in 1..=13 {
        assert_eq!(
            by_id(&out, id)["error"]["code"],
            -32602,
            "{id}: {:?}",
            by_id(&out, id)
        );
    }
    assert!(srv.seen().is_empty(), "no invalid call reached the server");
}

#[test]
fn list_documents_calls_the_list_route() {
    let srv = document_server();
    let (_, out) = mcp(
        &srv,
        &[call(
            1,
            "list_documents",
            json!({"project_id":7,"ref":"main","doctype":"safety.risk-assessment","limit":20,"after":"RA-2026-001"}),
        )],
    );
    let r = &by_id(&out, 1)["result"];
    assert_eq!(r["isError"], false);
    assert_eq!(r["structuredContent"]["documents"][0]["id"], "RA-2026-014");
    assert_eq!(
        srv.seen()[0].target,
        "/api/v1/projects/7/documents?ref=main&doctype=safety.risk-assessment&limit=20&after=RA-2026-001"
    );
}

#[test]
fn get_document_calls_the_document_route() {
    let srv = document_server();
    let (_, out) = mcp(
        &srv,
        &[call(
            1,
            "get_document",
            json!({"project_id":7,"id":"RA-2026-014","ref":"v1","lang":"en"}),
        )],
    );
    let r = &by_id(&out, 1)["result"];
    assert_eq!(r["isError"], false);
    assert_eq!(r["structuredContent"]["owner"], "safety");
    assert!(r["content"][0]["text"]
        .as_str()
        .expect("text")
        .contains("RA-2026-014"));
    assert_eq!(
        srv.seen()[0].target,
        "/api/v1/projects/7/documents/RA-2026-014?ref=v1&lang=en"
    );
}

#[test]
fn get_document_block_calls_the_block_route() {
    let srv = document_server();
    let (_, out) = mcp(
        &srv,
        &[call(
            1,
            "get_document_block",
            json!({"project_id":7,"id":"RA-2026-014","block":"H-01","lang":"pt-BR"}),
        )],
    );
    let r = &by_id(&out, 1)["result"];
    assert_eq!(r["isError"], false);
    assert_eq!(r["structuredContent"]["block"]["id"], "H-01");
    assert_eq!(
        srv.seen()[0].target,
        "/api/v1/projects/7/documents/RA-2026-014/blocks/H-01?lang=pt-BR"
    );
}

#[test]
fn get_document_referenced_by_calls_the_referrers_route() {
    let srv = document_server();
    let (_, out) = mcp(
        &srv,
        &[call(
            1,
            "get_document_referenced_by",
            json!({"project_id":7,"id":"RA-2026-014","block":"H-01","ref":"main","limit":10,"after":"WI-2026-001"}),
        )],
    );
    let r = &by_id(&out, 1)["result"];
    assert_eq!(r["isError"], false);
    assert_eq!(r["structuredContent"]["total"], 1);
    assert_eq!(
        srv.seen()[0].target,
        "/api/v1/projects/7/documents/RA-2026-014/referenced-by?ref=main&block=H-01&limit=10&after=WI-2026-001"
    );
}
