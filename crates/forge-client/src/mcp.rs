//! `gstn mcp` (M3 X5, plan 03 §6, ADR-0023 Amendment X5): a read-only MCP
//! server for an AI agent on stdin / stdout (JSON-RPC 2.0, one message per
//! line; the MCP stdio transport). Every tool is one `GET` of the REST API
//! with the caller's own token, so the server's single `authorize()`
//! decides everything; this process adds no permission of its own.
//!
//! | Tool | Route |
//! |---|---|
//! | `get_work_item_context` | `GET /api/v1/projects/{id}/context/work-item?ref=` |
//! | `get_requirement` | `GET /api/v1/projects/{id}/context/requirement?id=&ref=` |
//! | `search_requirements` | `GET /api/v1/projects/{id}/specs/requirements?q=&ref=&limit=` |
//! | `get_trace_matrix` | `GET /api/v1/projects/{id}/specs/trace?ref=` |
//! | `get_merge_request_context` | `GET /api/v1/projects/{id}/merge-requests/{iid}/context` |
//! | `list_documents` | `GET /api/v1/projects/{id}/documents?ref=&doctype=&limit=&after=` |
//! | `get_document` | `GET /api/v1/projects/{id}/documents/{doc_id}?ref=&lang=` |
//! | `get_document_block` | `GET /api/v1/projects/{id}/documents/{doc_id}/blocks/{block_id}?ref=&lang=` |
//! | `get_document_referenced_by` | `GET /api/v1/projects/{id}/documents/{doc_id}/referenced-by?ref=&block=&limit=&after=` |
//!
//! A document or block id is checked as the server checks it before it is
//! formatted into the path ([`crate::doc_view::parse_doc_id`]), a language
//! as `en` / `pt-BR`, a limit as 1..=500: an invalid one is `-32602` and
//! sends nothing. A confidential document is the server's not-found to an
//! agent; the tool error carries its code, message and request id only.
//!
//! Every request carries `Forge-Context-Audience: agent`: whatever the
//! token, the server never hands this process what is marked confidential
//! (plan 06 D-5). A tool's failure is a tool result with `isError` and the
//! error's `error_code`, `user_message` and `request_id` (never a crash);
//! a malformed message is a JSON-RPC error (one object per message: no
//! batches; an id is a string or a number). Diagnostics go to stderr, one
//! line each, never the token. stdout carries only protocol messages.
//!
//! **Use a token issued with `is_agent`** for an AI agent: the server
//! recognizes an agent by its token (or a service account / bot); a
//! person's ordinary token is a person's. The header narrows every answer
//! here anyway, but only an `is_agent` token keeps the other routes'
//! answers (and the audit) an agent's.

use std::io::{BufRead, BufReader, Write};

use forge_errors::ErrorCode;
use serde_json::{json, Map, Value};

use crate::app::{EnvFn, Io};
use crate::cli::{Cli, ContextCommand};
use crate::error::{ClientError, Result};
use crate::hosting::client;
use crate::http::{ApiClient, Method};

/// The protocol revisions this server speaks, newest first.
pub const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
/// A longer message is refused (`-32600`) and skipped.
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
/// The longest text argument.
pub const MAX_ARG_BYTES: usize = 256;

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// The header that makes every answer an AI agent's (plan 06 D-5).
pub const AUDIENCE: (&str, &str) = ("Forge-Context-Audience", "agent");

struct Tool {
    name: &'static str,
    description: &'static str,
    /// (name, JSON type, required, description)
    args: &'static [(&'static str, &'static str, bool, &'static str)],
}

const PROJECT: (&str, &str, bool, &str) = (
    "project_id",
    "integer",
    true,
    "The GitStation project id (gstn project list).",
);
const REVISION: (&str, &str, bool, &str) = (
    "ref",
    "string",
    false,
    "A commit id, a branch or a tag (default: the default branch).",
);

const DOCUMENT: (&str, &str, bool, &str) = (
    "id",
    "string",
    true,
    "The document id (RA-2026-014): ASCII letters, digits, '.', '_', '-', at most 128 bytes.",
);
const LANG: (&str, &str, bool, &str) = (
    "lang",
    "string",
    false,
    "A translation's language (en, pt-BR); default: the original.",
);
const LIMIT: (&str, &str, bool, &str) = (
    "limit",
    "integer",
    false,
    "At most this many, 1..=500 (default 100).",
);
const AFTER: (&str, &str, bool, &str) = (
    "after",
    "string",
    false,
    "The previous answer's next_after: the next page.",
);
/// The largest `limit` the server accepts.
const MAX_LIMIT: i64 = 500;

const TOOLS: &[Tool] = &[
    Tool {
        name: "get_work_item_context",
        description: "A kanflow work item (PBI-<n>) in one project: the merge requests linked to it and the @req tests of its acceptance criteria (PBI-<n>/AC-<m>) on the default branch, each with its source (project, commit, path, line).",
        args: &[
            PROJECT,
            ("ref", "string", true, "The work item, canonical PBI-<n>."),
        ],
    },
    Tool {
        name: "get_requirement",
        description: "One requirement of a project's specification: its text and document, the @req tests naming it, and its state in the newest version tags, each with its source.",
        args: &[
            PROJECT,
            ("id", "string", true, "The requirement id (ZEVA-CFG-012)."),
            REVISION,
        ],
    },
    Tool {
        name: "search_requirements",
        description: "Search a project's own specification: requirements whose id or text contains the query (case-insensitive), with path and line.",
        args: &[
            PROJECT,
            ("query", "string", false, "Text the id or the body contains; omit for all."),
            REVISION,
            ("limit", "integer", false, "At most this many requirements, 1..=500 (default 100)."),
        ],
    },
    Tool {
        name: "get_trace_matrix",
        description: "Each requirement of a project (its own specification and every pinned reference you may read) with the @req tests naming it: tested or untested.",
        args: &[PROJECT, REVISION],
    },
    Tool {
        name: "get_merge_request_context",
        description: "A merge request's review context: its changed files, the requirements it adds or changes, its work items and the check runs on its head, each with its source.",
        args: &[
            PROJECT,
            ("iid", "integer", true, "The merge request's number in the project."),
        ],
    },
    Tool {
        name: "list_documents",
        description: "A project's documents (id, title, type, path, classification, translations), by id, paged with next_after. Confidential documents are left out (`withheld` counts them when unfiltered).",
        args: &[
            PROJECT,
            REVISION,
            ("doctype", "string", false, "Only documents of this type (safety.risk-assessment)."),
            LIMIT,
            AFTER,
        ],
    },
    Tool {
        name: "get_document",
        description: "One document: its front matter, its sections of text and blocks, and every [[...]] reference resolved (a more confidential target is its id only). Cite its id and path.",
        args: &[PROJECT, DOCUMENT, REVISION, LANG],
    },
    Tool {
        name: "get_document_block",
        description: "One block of a document (a hazard, a requirement, a step) alone, with its section and the references inside it.",
        args: &[
            PROJECT,
            DOCUMENT,
            ("block", "string", true, "The block id (H-01), as for the document id."),
            REVISION,
            LANG,
        ],
    },
    Tool {
        name: "get_document_referenced_by",
        description: "The documents (and translations) that refer to a document, or to one block of it, with the lines; paged with next_after. A more confidential referrer is its id only.",
        args: &[
            PROJECT,
            DOCUMENT,
            ("block", "string", false, "Only the references to this block (H-01)."),
            REVISION,
            LIMIT,
            AFTER,
        ],
    },
];

fn tools_list() -> Value {
    let tools: Vec<Value> = TOOLS
        .iter()
        .map(|t| {
            let mut properties = Map::new();
            let mut required = Vec::new();
            for (name, kind, needed, description) in t.args {
                let mut p = json!({"type": kind, "description": description});
                if *kind == "integer" {
                    p["minimum"] = json!(1);
                } else {
                    p["maxLength"] = json!(MAX_ARG_BYTES);
                }
                properties.insert((*name).to_owned(), p);
                if *needed {
                    required.push(json!(name));
                }
            }
            json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": {
                    "type": "object",
                    "properties": properties,
                    "required": required,
                    "additionalProperties": false,
                },
                "annotations": {"readOnlyHint": true, "openWorldHint": false},
            })
        })
        .collect();
    json!({ "tools": tools })
}

/// A tool call's arguments, checked against the tool's list.
struct Args<'a> {
    tool: &'static Tool,
    given: &'a Map<String, Value>,
}

/// An invalid call (`-32602`).
#[derive(Debug)]
struct Invalid(String);

impl<'a> Args<'a> {
    fn of(
        tool: &'static Tool,
        given: &'a Map<String, Value>,
    ) -> std::result::Result<Self, Invalid> {
        for key in given.keys() {
            if !tool.args.iter().any(|(name, ..)| name == key) {
                return Err(Invalid(format!("unknown argument `{key}`")));
            }
        }
        for (name, _, needed, _) in tool.args {
            if *needed && !given.contains_key(*name) {
                return Err(Invalid(format!("missing argument `{name}`")));
            }
        }
        Ok(Self { tool, given })
    }

    fn id(&self, name: &str) -> std::result::Result<Option<i64>, Invalid> {
        match self.given.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_i64()
                .filter(|n| *n >= 1)
                .map(Some)
                .ok_or_else(|| Invalid(format!("`{name}` is a positive whole number"))),
        }
    }

    fn need_id(&self, name: &str) -> std::result::Result<i64, Invalid> {
        self.id(name)?
            .ok_or_else(|| Invalid(format!("missing argument `{name}`")))
    }

    fn text(&self, name: &str) -> std::result::Result<Option<String>, Invalid> {
        match self.given.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s))
                if !s.trim().is_empty()
                    && s.len() <= MAX_ARG_BYTES
                    && !s.chars().any(char::is_control) =>
            {
                Ok(Some(s.clone())) // clone: the request owns its values
            }
            Some(_) => Err(Invalid(format!(
                "`{name}` is a non-empty text of at most {MAX_ARG_BYTES} bytes without control characters"
            ))),
        }
    }

    fn need_text(&self, name: &str) -> std::result::Result<String, Invalid> {
        self.text(name)?
            .ok_or_else(|| Invalid(format!("missing argument `{name}`")))
    }

    /// A document or block id, as the server accepts it in the path.
    fn doc_ref(&self, name: &str) -> std::result::Result<Option<String>, Invalid> {
        match self.text(name)? {
            None => Ok(None),
            Some(raw) => crate::doc_view::parse_doc_id(&raw)
                .map(Some)
                .map_err(|why| Invalid(format!("`{name}`: {why}"))),
        }
    }

    fn need_doc_ref(&self, name: &str) -> std::result::Result<String, Invalid> {
        self.doc_ref(name)?
            .ok_or_else(|| Invalid(format!("missing argument `{name}`")))
    }

    /// `en` or `pt-BR`, as the server accepts it.
    fn lang(&self) -> std::result::Result<Option<String>, Invalid> {
        let Some(raw) = self.text("lang")? else {
            return Ok(None);
        };
        let (lang, region) = raw.split_once('-').unwrap_or((&raw, ""));
        let ok = lang.len() == 2
            && lang.bytes().all(|b| b.is_ascii_lowercase())
            && (region.is_empty()
                || region.len() == 2 && region.bytes().all(|b| b.is_ascii_uppercase()));
        if ok {
            Ok(Some(raw))
        } else {
            Err(Invalid("`lang` is a language like en or pt-BR".to_owned()))
        }
    }

    /// `limit`, 1..=[`MAX_LIMIT`].
    fn limit(&self) -> std::result::Result<Option<String>, Invalid> {
        match self.id("limit")? {
            Some(n) if n > MAX_LIMIT => Err(Invalid(format!("`limit` is at most {MAX_LIMIT}"))),
            n => Ok(n.map(|n| n.to_string())),
        }
    }
}

/// The route of a call: path and query.
fn route_of(args: &Args<'_>) -> std::result::Result<(String, Vec<(String, String)>), Invalid> {
    let project = args.need_id("project_id")?;
    let mut query = Vec::new();
    let mut push = |key: &str, value: Option<String>| {
        if let Some(v) = value {
            query.push((key.to_owned(), v));
        }
    };
    let path = match args.tool.name {
        "get_work_item_context" => {
            let (path, q, _) = crate::context::request_of(&ContextCommand::WorkItem {
                project,
                reference: args.need_text("ref")?,
            });
            return Ok((path, q));
        }
        "get_requirement" => {
            let (path, q, _) = crate::context::request_of(&ContextCommand::Requirement {
                project,
                id: args.need_text("id")?,
                rev: args.text("ref")?,
            });
            return Ok((path, q));
        }
        "get_merge_request_context" => {
            let (path, q, _) = crate::context::request_of(&ContextCommand::MergeRequest {
                project,
                iid: args.need_id("iid")?,
            });
            return Ok((path, q));
        }
        "search_requirements" => {
            push("ref", args.text("ref")?);
            push("q", args.text("query")?);
            push("limit", args.limit()?);
            format!("/api/v1/projects/{project}/specs/requirements")
        }
        "get_trace_matrix" => {
            push("ref", args.text("ref")?);
            format!("/api/v1/projects/{project}/specs/trace")
        }
        "list_documents" => {
            push("ref", args.text("ref")?);
            push("doctype", args.text("doctype")?);
            push("limit", args.limit()?);
            push("after", args.text("after")?);
            format!("/api/v1/projects/{project}/documents")
        }
        "get_document" => {
            let id = args.need_doc_ref("id")?;
            push("ref", args.text("ref")?);
            push("lang", args.lang()?);
            format!("/api/v1/projects/{project}/documents/{id}")
        }
        "get_document_block" => {
            let id = args.need_doc_ref("id")?;
            let block = args.need_doc_ref("block")?;
            push("ref", args.text("ref")?);
            push("lang", args.lang()?);
            format!("/api/v1/projects/{project}/documents/{id}/blocks/{block}")
        }
        "get_document_referenced_by" => {
            let id = args.need_doc_ref("id")?;
            push("ref", args.text("ref")?);
            push("block", args.doc_ref("block")?);
            push("limit", args.limit()?);
            push("after", args.text("after")?);
            format!("/api/v1/projects/{project}/documents/{id}/referenced-by")
        }
        other => return Err(Invalid(format!("unknown tool `{other}`"))),
    };
    Ok((path, query))
}

fn error_text(e: &ClientError) -> String {
    let mut out = format!("{}: {}", e.code_id(), e.user_message());
    if let Some(id) = e.request_id() {
        out.push_str(&format!(" (request id {id})"));
    }
    out
}

/// One answered `tools/call`: the result, or an invalid call.
fn call_tool(
    api: &ApiClient,
    params: &Value,
    stderr: &mut dyn Write,
) -> std::result::Result<Value, Invalid> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| Invalid("`name` is required".to_owned()))?;
    let tool = TOOLS
        .iter()
        .find(|t| t.name == name)
        .ok_or_else(|| Invalid(format!("unknown tool `{name}`")))?;
    let empty = Map::new();
    let given = match params.get("arguments") {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(m)) => m,
        Some(_) => return Err(Invalid("`arguments` is an object".to_owned())),
    };
    let args = Args::of(tool, given)?;
    let (path, query) = route_of(&args)?;
    Ok(match api.request(Method::Get, &path, &query, None) {
        Ok(res) => {
            let text = serde_json::to_string(&res.body).unwrap_or_default();
            json!({
                "content": [{"type": "text", "text": text}],
                "structuredContent": res.body,
                "isError": false,
            })
        }
        Err(e) => {
            // One line for the operator; the agent gets the same facts.
            let _logged = writeln!(stderr, "gstn mcp: {name} failed: {}", error_text(&e));
            json!({
                "content": [{"type": "text", "text": error_text(&e)}],
                "structuredContent": serde_json::to_value(e.as_json()).unwrap_or(Value::Null),
                "isError": true,
            })
        }
    })
}

fn response(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// The answer to one message (`None` for a notification).
fn answer(api: &ApiClient, line: &str, stderr: &mut dyn Write) -> Option<Value> {
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
                    "instructions": "GitStation's context, read-only, with your token: work items, requirements, the trace matrix, merge requests and documents. Every item carries its source (project, commit, path, line): cite it. What is marked confidential is never returned (`withheld` counts it; asking for it is FG-AUTH-0021).",
                }),
            )
        }
        "ping" => response(id, json!({})),
        "tools/list" => response(id, tools_list()),
        "tools/call" => match call_tool(api, &params, stderr) {
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
                answer(&api, line, io.stderr)
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

    #[test]
    fn every_tool_has_a_route_and_refuses_unknown_arguments() {
        for t in TOOLS {
            let mut given = Map::new();
            for (name, kind, needed, _) in t.args {
                if *needed {
                    given.insert(
                        (*name).to_owned(),
                        if *kind == "integer" {
                            json!(7)
                        } else {
                            json!("PBI-1")
                        },
                    );
                }
            }
            let args = Args::of(t, &given).expect("valid");
            assert!(route_of(&args)
                .expect("route")
                .0
                .starts_with("/api/v1/projects/7/"));
            given.insert("token".to_owned(), json!("x"));
            assert!(Args::of(t, &given).is_err(), "{}", t.name);
        }
    }
}
