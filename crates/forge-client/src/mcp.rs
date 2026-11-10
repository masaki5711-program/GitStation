//! `gstn mcp` (M3 X5, plan 01 §3.11, plan 06 §4.5; ADR-0023 Amendments X5
//! and 2026-11-10): an MCP server for an AI agent on stdin / stdout
//! (JSON-RPC 2.0, one message per line; the MCP stdio transport). Every
//! tool is one call of the REST API with the caller's own token, so the
//! server's single `authorize()` decides everything; this process adds no
//! permission of its own.
//!
//! **The tools are `docs/api/agent-tools.toml`** ([`crate::agent_tools`]):
//! `tools/list` and every request are built from that one manifest, which
//! also lists what is kept for people (approve, merge, resolve, deploy
//! approval, tokens, administration, protected refs, webhooks, mirrors,
//! integration bindings) and why. The parity gate
//! (`agent_tools_tests.rs`) keeps it in step with `gstn`'s commands.
//!
//! | Tool | Route | |
//! |---|---|---|
//! | `get_work_item_context` | `GET …/projects/{id}/context/work-item?ref=` | read |
//! | `get_requirement` | `GET …/projects/{id}/context/requirement?id=&ref=` | read |
//! | `search_requirements` | `GET …/projects/{id}/specs/requirements?ref=&q=&limit=` | read |
//! | `get_trace_matrix` | `GET …/projects/{id}/specs/trace?ref=` | read |
//! | `get_merge_request_context` | `GET …/projects/{id}/merge-requests/{iid}/context` | read |
//! | `list_documents` | `GET …/projects/{id}/documents?ref=&doctype=&limit=&after=` | read |
//! | `get_document` | `GET …/projects/{id}/documents/{doc_id}?ref=&lang=` | read |
//! | `get_document_block` | `GET …/documents/{doc_id}/blocks/{block_id}?ref=&lang=` | read |
//! | `get_document_referenced_by` | `GET …/documents/{doc_id}/referenced-by?ref=&block=&limit=&after=` | read |
//! | `list_merge_requests` | `GET …/projects/{id}/merge-requests?state=&limit=&before=` | read |
//! | `get_merge_request` | `GET …/merge-requests/{iid}` | read |
//! | `list_merge_request_versions` | `GET …/merge-requests/{iid}/versions` | read |
//! | `get_merge_request_diff` | `GET …/merge-requests/{iid}/versions/{version}/diff` | read |
//! | `compare_merge_request_versions` | `GET …/merge-requests/{iid}/compare?from=&to=` | read |
//! | `get_merge_request_status` | `GET …/merge-requests/{iid}/status` | read |
//! | `list_merge_request_discussions` | `GET …/merge-requests/{iid}/discussions` | read |
//! | `get_merge_request_conflicts` | `GET …/merge-requests/{iid}/conflicts` | read |
//! | `list_check_runs` | `GET …/projects/{id}/commits/{sha}/checks` | read |
//! | `list_pipelines` | `GET …/projects/{id}/pipelines?ref=&before=&limit=` | read |
//! | `get_pipeline` | `GET …/pipelines/{pipeline_id}` | read |
//! | `create_merge_request` | `POST …/projects/{id}/merge-requests` | write |
//! | `update_merge_request` | `PATCH …/merge-requests/{iid}` (title, description, draft) | write, destructive |
//! | `close_merge_request` | `POST …/merge-requests/{iid}/close` | write, destructive |
//! | `reopen_merge_request` | `POST …/merge-requests/{iid}/reopen` | write |
//! | `comment_on_merge_request` | `POST …/merge-requests/{iid}/discussions` | write |
//! | `reply_to_discussion` | `POST …/discussions/{discussion_id}/notes` | write |
//! | `run_pipeline` | `POST …/projects/{id}/pipelines` | write |
//! | `cancel_pipeline` | `POST …/pipelines/{pipeline_id}/cancel` | write, destructive |
//! | `retry_pipeline` | `POST …/pipelines/{pipeline_id}/retry` | write |
//!
//! Every argument is checked before anything is sent (an unknown or
//! missing one, a wrong type, a number out of range, an overlong or
//! control-character text, a document id, a language or a commit id the
//! server would refuse): `-32602`, and no request. Only integers, document
//! ids and commit ids are formatted into a path; text goes in the query
//! (percent-encoded) or the JSON body. A write tool carries the MCP
//! annotations `readOnlyHint: false`, `destructiveHint` and
//! `idempotentHint`.
//!
//! Every request carries `Forge-Context-Audience: agent`: whatever the
//! token, the server never hands this process what is marked confidential
//! (plan 06 D-5). A tool's failure is a tool result with `isError` and the
//! error's `error_code`, `user_message` and `request_id` (never a crash);
//! a malformed message is a JSON-RPC error (one object per message: no
//! batches; an id is a string or a number). Diagnostics go to stderr, one
//! line each (every failure, and every write with its request id), never
//! the token. stdout carries only protocol messages.
//!
//! **Use a token issued with `is_agent`** for an AI agent: the server
//! recognizes an agent by its token (or a service account / bot) and
//! refuses it what is kept for people (approve, merge, resolve, …); a
//! person's ordinary token is a person's. The header narrows the context
//! answers anyway, but only an `is_agent` token keeps the other routes'
//! answers (and the audit) an agent's.

use std::io::{BufRead, BufReader, Write};

use forge_errors::ErrorCode;
use serde_json::{json, Map, Value};

use crate::agent_tools::{manifest, Invalid, Manifest};
use crate::app::{EnvFn, Io};
use crate::cli::Cli;
use crate::error::{ClientError, Result};
use crate::hosting::client;
use crate::http::ApiClient;

/// The protocol revisions this server speaks, newest first.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
/// A longer message is refused (`-32600`) and skipped.
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// The header that makes every answer an AI agent's (plan 06 D-5).
pub const AUDIENCE: (&str, &str) = ("Forge-Context-Audience", "agent");

fn error_text(e: &ClientError) -> String {
    let mut out = format!("{}: {}", e.code_id(), e.user_message());
    if let Some(id) = e.request_id() {
        out.push_str(&format!(" (request id {id})"));
    }
    out
}

fn tools_list(tools: &Manifest) -> Value {
    let list: Vec<Value> = tools.tools.iter().map(|t| t.list_entry()).collect();
    json!({ "tools": list })
}

/// One answered `tools/call`: the result, or an invalid call.
fn call_tool(
    api: &ApiClient,
    tools: &Manifest,
    params: &Value,
    stderr: &mut dyn Write,
) -> std::result::Result<Value, Invalid> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| Invalid("`name` is required".to_owned()))?;
    let tool = tools
        .tools
        .iter()
        .find(|t| t.name == name)
        .ok_or_else(|| Invalid(format!("unknown tool `{name}`")))?;
    let empty = Map::new();
    let given = match params.get("arguments") {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(m)) => m,
        Some(_) => return Err(Invalid("`arguments` is an object".to_owned())),
    };
    let req = tool.request(given)?;
    Ok(
        match api.request(req.method, &req.path, &req.query, req.body.as_ref()) {
            Ok(res) => {
                if !tool.read_only {
                    // A change on the server: one line for the operator.
                    let _logged = writeln!(
                        stderr,
                        "gstn mcp: {name} done (request id {})",
                        res.request_id.as_deref().unwrap_or("-")
                    );
                }
                let text = serde_json::to_string(&res.body).unwrap_or_default();
                // structuredContent is an object (MCP); every route answers
                // one, anything else is wrapped.
                let structured = if res.body.is_object() {
                    res.body
                } else {
                    json!({ "result": res.body })
                };
                json!({
                    "content": [{"type": "text", "text": text}],
                    "structuredContent": structured,
                    "isError": false,
                })
            }
            Err(e) => {
                // One line for the operator; the agent gets the same facts.
                let _logged = writeln!(stderr, "gstn mcp: {name} failed: {}", error_text(&e));
                json!({
                    "content": [{"type": "text", "text": error_text(&e)}],
                    // The agent gets the code, the message and the request id
                    // only — never `detail` (client-side text such as a path).
                    "structuredContent": {
                        "error_code": e.code_id(),
                        "user_message": e.user_message(),
                        "request_id": e.request_id(),
                    },
                    "isError": true,
                })
            }
        },
    )
}

fn response(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// The answer to one message (`None` for a notification).
fn answer(api: &ApiClient, tools: &Manifest, line: &str, stderr: &mut dyn Write) -> Option<Value> {
    let Ok(msg) = serde_json::from_str::<Value>(line) else {
        return Some(error(Value::Null, PARSE_ERROR, "not JSON"));
    };
    // One object per message: a batch (an array) and any other value are
    // refused (review X5 e; batches are not supported).
    let Value::Object(fields) = &msg else {
        return Some(error(
            Value::Null,
            INVALID_REQUEST,
            "a message is one JSON object (no batches)",
        ));
    };
    // An id is a string or a number; any other (null included) is refused
    // without echoing it.
    let id = match fields.get("id") {
        None => None,
        Some(v @ (Value::String(_) | Value::Number(_))) => Some(v.clone()), // clone: echoed
        Some(_) => {
            return Some(error(
                Value::Null,
                INVALID_REQUEST,
                "id is a string or a number",
            ))
        }
    };
    if fields.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Some(error(
            id.unwrap_or(Value::Null),
            INVALID_REQUEST,
            "jsonrpc must be 2.0",
        ));
    }
    let Some(method) = fields.get("method").and_then(Value::as_str) else {
        // A response or garbage: nothing this server asked for.
        return id.map(|id| error(id, INVALID_REQUEST, "not a request"));
    };
    // A notification (no id) is never answered.
    let id = id?;
    let params = msg.get("params").cloned().unwrap_or(Value::Null); // clone: read below
    Some(match method {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str);
            let version = asked
                .filter(|v| PROTOCOL_VERSIONS.contains(v))
                .unwrap_or(PROTOCOL_VERSIONS[0]);
            response(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "gstn", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": "GitStation with your token: read work items, requirements, the trace matrix, documents, merge requests (diff, status and blockers, discussions, conflicts), check runs and pipelines; open, update, comment on, close and reopen merge requests; run, cancel and retry pipelines. Approving, merging, resolving discussions and deploying are kept for people: ask one. Every context item carries its source (project, commit, path, line): cite it. What is marked confidential is never returned (`withheld` counts it; asking for it is FG-AUTH-0021). A failure carries error_code and request_id.",
                }),
            )
        }
        "ping" => response(id, json!({})),
        "tools/list" => response(id, tools_list(tools)),
        "tools/call" => match call_tool(api, tools, &params, stderr) {
            Ok(result) => response(id, result),
            Err(Invalid(why)) => error(id, INVALID_PARAMS, &why),
        },
        _ => error(id, METHOD_NOT_FOUND, "unknown method"),
    })
}

/// What [`read_message`] read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Message {
    /// One line (with its `\n`, unless the input ended) is in the buffer.
    Line,
    /// The line was longer than [`MAX_MESSAGE_BYTES`]: skipped, not kept.
    TooLarge,
}

/// Read one line into `buf` (cleared first), never keeping more than
/// [`MAX_MESSAGE_BYTES`] of it: a longer line is consumed through the
/// reader's own buffer and dropped (review X5, rust HIGH-1: a multi-GB
/// line costs no memory). `None` at the end of input.
pub(crate) fn read_message(
    reader: &mut impl BufRead,
    buf: &mut Vec<u8>,
) -> std::io::Result<Option<Message>> {
    buf.clear();
    let mut too_large = false;
    let mut read_any = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(match (read_any, too_large) {
                (false, _) => None,
                (true, true) => Some(Message::TooLarge),
                (true, false) => Some(Message::Line),
            });
        }
        read_any = true;
        let (take, done) = match available.iter().position(|b| *b == b'\n') {
            Some(i) => (i + 1, true),
            None => (available.len(), false),
        };
        if !too_large {
            if buf.len() + take > MAX_MESSAGE_BYTES + 1 {
                too_large = true;
                buf.clear();
                buf.shrink_to_fit();
            } else {
                buf.extend_from_slice(available.get(..take).unwrap_or_default());
            }
        }
        reader.consume(take);
        if done {
            return Ok(Some(if too_large {
                Message::TooLarge
            } else {
                Message::Line
            }));
        }
    }
}

/// [`read_message`], with the end of the transport (end of input, a closed
/// pipe) as `None` and any other read failure as `FG-CLI-0009`.
pub(crate) fn next_message(
    reader: &mut impl BufRead,
    buf: &mut Vec<u8>,
) -> Result<Option<Message>> {
    match read_message(reader, buf) {
        Ok(m) => Ok(m),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::UnexpectedEof
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(ClientError::local(
            ErrorCode::CliInputFailed,
            format!("reason=stdin_unreadable kind={:?}", e.kind()),
        )),
    }
}

/// Serve until stdin ends.
pub fn serve(cli: &Cli, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    // The embedded manifest is checked by the parity gate; were it ever
    // invalid, nothing is served (never a server without its tools).
    let tools = manifest().map_err(|why| {
        ClientError::local(
            ErrorCode::CliConfigInvalid,
            format!("config_key=agent_tools reason=invalid ({why})"),
        )
    })?;
    let api = client(cli, env, io)?.with_header(AUDIENCE.0, AUDIENCE.1);
    let mut reader = BufReader::new(&mut *io.stdin);
    let mut raw = Vec::new();
    while let Some(read) = next_message(&mut reader, &mut raw)? {
        let reply = match read {
            Message::TooLarge => Some(error(Value::Null, INVALID_REQUEST, "message too large")),
            Message::Line => {
                let line = String::from_utf8_lossy(&raw);
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                answer(&api, tools, line, io.stderr)
            }
        };
        if let Some(reply) = reply {
            let text = serde_json::to_string(&reply).map_err(|_| {
                ClientError::local(ErrorCode::CliResponseInvalid, "reason=serialize")
            })?;
            io.stdout
                .write_all(text.as_bytes())
                .and_then(|()| io.stdout.write_all(b"\n"))
                .and_then(|()| io.stdout.flush())
                .map_err(|e| {
                    ClientError::local(
                        ErrorCode::CliOutputFailed,
                        format!("stream=stdout kind={:?}", e.kind()),
                    )
                })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)] // tests may fail loudly

    use std::io::Read as _;

    use super::*;

    /// Review X5 (rust HIGH-1): a line far past the limit is skipped
    /// without being kept — the buffer never grows past the limit plus one
    /// read — and the next message is read whole.
    #[test]
    fn an_oversized_line_is_skipped_in_bounded_memory() {
        let huge = std::io::repeat(b'x').take(64 * 1024 * 1024);
        let rest = std::io::Cursor::new(b"\n{\"next\":1}\n".to_vec());
        let mut reader = std::io::BufReader::new(huge.chain(rest));
        let mut buf = Vec::new();
        assert_eq!(
            read_message(&mut reader, &mut buf).expect("read"),
            Some(Message::TooLarge)
        );
        assert!(
            buf.capacity() <= MAX_MESSAGE_BYTES + 64 * 1024,
            "kept {} bytes",
            buf.capacity()
        );
        assert_eq!(
            read_message(&mut reader, &mut buf).expect("read"),
            Some(Message::Line)
        );
        assert_eq!(buf, b"{\"next\":1}\n");
        assert_eq!(read_message(&mut reader, &mut buf).expect("read"), None);
    }

    /// Review X5 (e): a broken input that is no end of input is an input
    /// failure, never a usage error.
    #[test]
    fn a_broken_input_is_an_input_failure() {
        struct Broken;
        impl std::io::Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("synthetic"))
            }
        }
        let mut reader = std::io::BufReader::new(Broken);
        let err = next_message(&mut reader, &mut Vec::new()).expect_err("broken");
        assert_eq!(err.code_id(), ErrorCode::CliInputFailed.id());
        struct Gone;
        impl std::io::Read for Gone {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
        }
        let mut gone = std::io::BufReader::new(Gone);
        assert_eq!(
            next_message(&mut gone, &mut Vec::new()).expect("ends"),
            None
        );
    }
}
