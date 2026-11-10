//! The agent tools manifest (`docs/api/agent-tools.toml`, ADR-0023
//! Amendment 2026-11-10): the one list of what an AI agent may call through
//! `gstn mcp`, each tool one REST API v1 route. This module reads the
//! embedded file, checks it once, renders the MCP `tools/list` entry of a
//! tool and turns a call's arguments into a request — validating every
//! argument first, so an invalid call sends nothing.
//!
//! The data file, not a Rust table, is the source: a tool is added in one
//! place, and the server's planned `/api/v1/ai-tools` (plan 02 §6) can
//! embed the same file without depending on this crate. The parity gate
//! (`agent_tools_tests.rs`) keeps it in step with `gstn`'s commands,
//! `openapi.json` and the routes' authz actions.

use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::http::Method;

/// The manifest, embedded at build time.
pub const MANIFEST_TEXT: &str = include_str!("../../../docs/api/agent-tools.toml");

/// The longest string argument when the manifest names no `max_bytes`.
pub const DEFAULT_MAX_BYTES: usize = 256;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u32,
    #[serde(rename = "tool")]
    pub tools: Vec<ToolSpec>,
    /// Kept for people: never a tool.
    #[serde(default)]
    pub excluded: Vec<Decision>,
    /// Commands that make no REST call of their own.
    #[serde(default)]
    pub not_api: Vec<Decision>,
    /// An agent may; not a tool yet.
    #[serde(default)]
    pub deferred: Vec<Decision>,
}

/// Why a command (or an authz action) is no tool.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub gstn: Option<String>,
    pub action: Option<String>,
    pub reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// The `gstn` leaf command this tool mirrors.
    pub gstn: String,
    pub read_only: bool,
    pub destructive: Option<bool>,
    pub idempotent: Option<bool>,
    pub method: String,
    /// Verbatim as in `openapi.json`; `{wire}` of each path parameter.
    pub path: String,
    #[serde(default)]
    pub params: Vec<Param>,
    #[serde(default)]
    pub at_least_one: Vec<String>,
    #[serde(default)]
    pub together: Vec<Vec<String>>,
    /// `[a, b]`: `a` only with `b`.
    #[serde(default)]
    pub requires: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Location {
    Path,
    Query,
    Body,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Integer,
    String,
    Boolean,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    /// One line: no control characters.
    #[default]
    Text,
    /// Lines: tab, line feed and carriage return allowed.
    Markdown,
    /// A document or block id, as the server takes it in a path.
    DocId,
    /// `en` or `pt-BR`.
    Lang,
    /// A full commit id (40 or 64 hex digits), lower-cased.
    Oid,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Param {
    pub name: String,
    #[serde(rename = "in")]
    pub location: Location,
    /// The name on the wire (`a.b`: nested in the JSON body).
    pub wire: Option<String>,
    #[serde(rename = "type")]
    pub kind: Kind,
    #[serde(default)]
    pub required: bool,
    pub minimum: Option<i64>,
    pub maximum: Option<i64>,
    pub max_bytes: Option<usize>,
    #[serde(default)]
    pub format: Format,
    #[serde(default)]
    pub allow_empty: bool,
    #[serde(rename = "enum")]
    pub choices: Option<Vec<String>>,
    pub description: String,
}

impl Param {
    pub fn wire(&self) -> &str {
        self.wire.as_deref().unwrap_or(&self.name)
    }

    fn minimum(&self) -> i64 {
        self.minimum.unwrap_or(1)
    }

    fn max_bytes(&self) -> usize {
        self.max_bytes.unwrap_or(DEFAULT_MAX_BYTES)
    }
}

impl ToolSpec {
    pub fn http_method(&self) -> Option<Method> {
        Method::parse(&self.method)
    }

    fn param(&self, name: &str) -> Option<&Param> {
        self.params.iter().find(|p| p.name == name)
    }

    /// The MCP `tools/list` entry.
    pub fn list_entry(&self) -> Value {
        let mut properties = Map::new();
        let mut required = Vec::new();
        for p in &self.params {
            let mut s = json!({"type": kind_name(p.kind), "description": p.description});
            match p.kind {
                Kind::Integer => {
                    s["minimum"] = json!(p.minimum());
                    if let Some(max) = p.maximum {
                        s["maximum"] = json!(max);
                    }
                }
                Kind::String => {
                    s["maxLength"] = json!(p.max_bytes());
                    if let Some(choices) = &p.choices {
                        s["enum"] = json!(choices);
                    }
                }
                Kind::Boolean => {}
            }
            properties.insert(p.name.clone(), s); // clone: the schema owns its keys
            if p.required {
                required.push(json!(p.name));
            }
        }
        let mut annotations = json!({"readOnlyHint": self.read_only, "openWorldHint": false});
        if !self.read_only {
            annotations["destructiveHint"] = json!(self.destructive.unwrap_or(true));
            annotations["idempotentHint"] = json!(self.idempotent.unwrap_or(false));
        }
        json!({
            "name": self.name,
            "description": self.description,
            "inputSchema": {
                "type": "object",
                "properties": properties,
                "required": required,
                "additionalProperties": false,
            },
            "annotations": annotations,
        })
    }

    /// The request of a call, every argument checked first.
    pub fn request(&self, given: &Map<String, Value>) -> Result<Request, Invalid> {
        let method = self
            .http_method()
            .ok_or_else(|| Invalid(format!("tool `{}` is misdefined", self.name)))?;
        for key in given.keys() {
            if self.param(key).is_none() {
                return Err(Invalid(format!("unknown argument `{key}`")));
            }
        }
        let present = |name: &str| given.get(name).is_some_and(|v| !v.is_null());
        for p in &self.params {
            if p.required && !present(&p.name) {
                return Err(Invalid(format!("missing argument `{}`", p.name)));
            }
        }
        if !self.at_least_one.is_empty() && !self.at_least_one.iter().any(|n| present(n)) {
            return Err(Invalid(format!(
                "give at least one of {}",
                quoted(&self.at_least_one)
            )));
        }
        for group in &self.together {
            let n = group.iter().filter(|g| present(g)).count();
            if n != 0 && n != group.len() {
                return Err(Invalid(format!("give {} together", quoted(group))));
            }
        }
        for pair in &self.requires {
            if let [a, b] = pair.as_slice() {
                if present(a) && !present(b) {
                    return Err(Invalid(format!("`{a}` needs `{b}`")));
                }
            }
        }
        let mut path = self.path.clone(); // clone: filled in below
        let mut query = Vec::new();
        let mut body = Map::new();
        for p in &self.params {
            let Some(value) = given.get(&p.name).filter(|v| !v.is_null()) else {
                continue;
            };
            let value = check(p, value)?;
            match p.location {
                Location::Path => {
                    path = path.replace(&format!("{{{}}}", p.wire()), &wire_text(&value));
                }
                Location::Query => {
                    query.push((p.wire().to_owned(), wire_text(&value)));
                }
                Location::Body => insert(&mut body, p.wire(), value),
            }
        }
        if path.contains('{') {
            return Err(Invalid(format!("tool `{}` is misdefined", self.name)));
        }
        let has_body = self.params.iter().any(|p| p.location == Location::Body);
        Ok(Request {
            method,
            path,
            query,
            body: has_body.then_some(Value::Object(body)),
        })
    }
}

/// A checked value as text on the wire: a string as it is, else its JSON.
fn wire_text(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Integer => "integer",
        Kind::String => "string",
        Kind::Boolean => "boolean",
    }
}

fn quoted(names: &[String]) -> String {
    names
        .iter()
        .map(|n| format!("`{n}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `a.b` puts the value at `body.a.b`.
fn insert(body: &mut Map<String, Value>, wire: &str, value: Value) {
    match wire.split_once('.') {
        None => {
            body.insert(wire.to_owned(), value);
        }
        Some((outer, inner)) => {
            let slot = body
                .entry(outer.to_owned())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(m) = slot {
                insert(m, inner, value);
            }
        }
    }
}

/// One argument, as the server will take it.
fn check(p: &Param, value: &Value) -> Result<Value, Invalid> {
    let name = &p.name;
    match p.kind {
        Kind::Integer => {
            let n = value
                .as_i64()
                .filter(|n| *n >= p.minimum())
                .ok_or_else(|| {
                    Invalid(if p.minimum() == 1 {
                        format!("`{name}` is a positive whole number")
                    } else {
                        format!("`{name}` is a whole number from {}", p.minimum())
                    })
                })?;
            match p.maximum {
                Some(max) if n > max => Err(Invalid(format!("`{name}` is at most {max}"))),
                _ => Ok(json!(n)),
            }
        }
        Kind::Boolean => value
            .as_bool()
            .map(Value::Bool)
            .ok_or_else(|| Invalid(format!("`{name}` is true or false"))),
        Kind::String => {
            let Value::String(s) = value else {
                return Err(Invalid(format!("`{name}` is a text")));
            };
            let s = check_text(p, s)?;
            if let Some(choices) = &p.choices {
                if !choices.contains(&s) {
                    return Err(Invalid(format!(
                        "`{name}` is one of {}",
                        choices.join(", ")
                    )));
                }
            }
            Ok(Value::String(s))
        }
    }
}

fn check_text(p: &Param, s: &str) -> Result<String, Invalid> {
    let name = &p.name;
    let max = p.max_bytes();
    let lines = p.format == Format::Markdown;
    let bad_char = |c: char| c.is_control() && !(lines && matches!(c, '\n' | '\t' | '\r'));
    let blank_ok = p.allow_empty && s.is_empty();
    if (!blank_ok && s.trim().is_empty()) || s.len() > max || s.chars().any(bad_char) {
        return Err(Invalid(if lines {
            format!("`{name}` is a text of at most {max} bytes without control characters other than tab and line breaks")
        } else {
            format!(
                "`{name}` is a non-empty text of at most {max} bytes without control characters"
            )
        }));
    }
    match p.format {
        Format::Text | Format::Markdown => Ok(s.to_owned()),
        Format::DocId => {
            crate::doc_view::parse_doc_id(s).map_err(|why| Invalid(format!("`{name}`: {why}")))
        }
        Format::Oid => crate::cli::parse_oid(s).map_err(|why| Invalid(format!("`{name}`: {why}"))),
        Format::Lang => {
            let (lang, region) = s.split_once('-').unwrap_or((s, ""));
            let ok = lang.len() == 2
                && lang.bytes().all(|b| b.is_ascii_lowercase())
                && (region.is_empty()
                    || region.len() == 2 && region.bytes().all(|b| b.is_ascii_uppercase()));
            if ok {
                Ok(s.to_owned())
            } else {
                Err(Invalid(format!("`{name}` is a language like en or pt-BR")))
            }
        }
    }
}

/// An invalid call (`-32602`): nothing is sent.
#[derive(Debug)]
pub struct Invalid(pub String);

/// What one call sends.
#[derive(Debug)]
pub struct Request {
    pub method: Method,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub body: Option<Value>,
}

/// The embedded manifest, read and checked once (`Err`: why it is invalid —
/// a build defect the parity gate catches first).
pub fn manifest() -> Result<&'static Manifest, &'static str> {
    static CELL: OnceLock<Result<Manifest, String>> = OnceLock::new();
    CELL.get_or_init(|| parse(MANIFEST_TEXT))
        .as_ref()
        .map_err(String::as_str)
}

/// Read a manifest and check what a request needs of it.
pub fn parse(text: &str) -> Result<Manifest, String> {
    let m: Manifest = toml::from_str(text).map_err(|e| format!("not a manifest: {e}"))?;
    if m.version != 1 {
        return Err(format!("version {} (this gstn reads 1)", m.version));
    }
    let mut names = std::collections::BTreeSet::new();
    for t in &m.tools {
        if !names.insert(t.name.as_str()) {
            return Err(format!("tool {} is listed twice", t.name));
        }
        if t.http_method().is_none() {
            return Err(format!("tool {}: method {}", t.name, t.method));
        }
        if t.read_only != (t.method == "GET") {
            return Err(format!(
                "tool {}: read_only must be (method == GET)",
                t.name
            ));
        }
        if t.read_only && (t.destructive.is_some() || t.idempotent.is_some()) {
            return Err(format!(
                "tool {}: destructive / idempotent are for writes",
                t.name
            ));
        }
        if !t.read_only && (t.destructive.is_none() || t.idempotent.is_none()) {
            return Err(format!(
                "tool {}: a write states destructive and idempotent",
                t.name
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for p in &t.params {
            if !seen.insert(p.name.as_str()) {
                return Err(format!("tool {}: argument {} twice", t.name, p.name));
            }
            if p.location == Location::Path {
                if !p.required {
                    return Err(format!(
                        "tool {}: path argument {} is required",
                        t.name, p.name
                    ));
                }
                // Only what cannot leave its path segment goes into a path.
                let safe = match p.kind {
                    Kind::Integer => true,
                    Kind::String => matches!(p.format, Format::DocId | Format::Oid),
                    Kind::Boolean => false,
                };
                if !safe {
                    return Err(format!(
                        "tool {}: path argument {} is an integer, a doc_id or an oid",
                        t.name, p.name
                    ));
                }
                if !t.path.contains(&format!("{{{}}}", p.wire())) {
                    return Err(format!(
                        "tool {}: {{{}}} is not in the path",
                        t.name,
                        p.wire()
                    ));
                }
            }
        }
        let holes = t.path.matches('{').count();
        let path_params = t
            .params
            .iter()
            .filter(|p| p.location == Location::Path)
            .count();
        if holes != path_params {
            return Err(format!(
                "tool {}: the path has {holes} parameters, the tool {path_params}",
                t.name
            ));
        }
        let groups = t
            .together
            .iter()
            .chain(t.requires.iter())
            .chain(std::iter::once(&t.at_least_one));
        for group in groups {
            for n in group {
                if t.param(n).is_none() {
                    return Err(format!("tool {}: constraint names unknown {n}", t.name));
                }
            }
        }
        if t.requires.iter().any(|r| r.len() != 2) {
            return Err(format!("tool {}: requires is [argument, needed]", t.name));
        }
    }
    Ok(m)
}
