//! The static scan of a part (plan 16 §8, ADR-0032 §5). Hooks run as `node`
//! on every developer's PC, so the scan is conservative: what it cannot see
//! through is refused, not trusted.
//!
//! Scripts (`.js` / `.cjs` / `.mjs`), the raw source (comments included):
//! - a module name `child_process` / `cluster` anywhere → needs `exec`;
//! - `http` `https` `http2` `net` `tls` `dgram` `dns` `undici`, `fetch(`,
//!   `WebSocket`, `XMLHttpRequest` → needs `network`;
//! - code the scan cannot follow is refused outright (`dynamic_code`): a
//!   `require` whose argument is not one plain string literal (computed,
//!   concatenated, escaped, or `require` passed around), `import(`, `eval(`,
//!   `Function(`, `.constructor`, `process.binding` / `_linkedBinding` /
//!   `dlopen`, `process[`, `globalThis[` / `global[`, `Reflect.`,
//!   `mainModule`, `createRequire`, and the modules `vm` `worker_threads`
//!   `module` `inspector` `v8` `wasi`. Indirection the scan still cannot
//!   see is caught by the other layers (two people review every part, the
//!   server's hook test job, beta first — plan 16 §8).
//!
//! `.claude/settings.json`: `permissions` → needs `permissions`; a hook
//! `command` whose program is not `node` → needs `exec`.
//!
//! `.mcp.json`: a server with `command` → needs `exec`; with `url` → needs
//! `network`; every `http(s)://` / `ws(s)://` URL in a server's definition
//! (url, args, env, headers) must name a host on the allow-list
//! (`FG-AGENT-0013`, contract 8: nothing outside the company).

use forge_errors::ErrorCode;
use serde_json::Value;

use crate::{Capability, Part, Problem};

/// What the scan may allow; from the server's configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanPolicy {
    /// Hosts an MCP server may name: an exact host (`mcp.internal`) or a
    /// suffix starting with a dot (`.corp.internal`). Empty: none.
    pub mcp_allowed_hosts: Vec<String>,
}

impl ScanPolicy {
    fn host_allowed(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.mcp_allowed_hosts.iter().any(|a| {
            let a = a.to_ascii_lowercase();
            match a.strip_prefix('.') {
                Some(suffix) => host
                    .strip_suffix(suffix)
                    .is_some_and(|head| head.ends_with('.') && head.len() > 1),
                None => host == a,
            }
        })
    }
}

const CHILD_MODULES: [&str; 2] = ["child_process", "cluster"];
const NET_MODULES: [&str; 8] = [
    "http", "https", "http2", "net", "tls", "dgram", "dns", "undici",
];
const DYNAMIC_MODULES: [&str; 6] = ["vm", "worker_threads", "module", "inspector", "v8", "wasi"];

/// Scan every file of `part` against its declared capabilities.
pub fn scan(part: &Part, policy: &ScanPolicy) -> Vec<Problem> {
    let caps = &part.manifest.capabilities;
    let mut out = Vec::new();
    for f in &part.files {
        let path = f.path.as_str();
        let need = |out: &mut Vec<Problem>, cap: Capability, reason: &'static str| {
            if !caps.contains(&cap) {
                out.push(Problem::about(
                    ErrorCode::PartCapabilityUndeclared,
                    reason,
                    path,
                ));
            }
        };
        let Ok(text) = std::str::from_utf8(&f.content) else {
            if is_script(path) || path.ends_with(".json") {
                out.push(Problem::about(
                    ErrorCode::PartManifestInvalid,
                    "file_not_utf8",
                    path,
                ));
            }
            continue;
        };
        if is_script(path) {
            let found = scan_script(text);
            if found.child_process {
                need(&mut out, Capability::Exec, "undeclared_child_process");
            }
            if found.network {
                need(&mut out, Capability::Network, "undeclared_network");
            }
            if let Some(what) = found.dynamic {
                out.push(Problem::new(
                    ErrorCode::PartCapabilityUndeclared,
                    "dynamic_code",
                    Some(format!("{path}: {what}")),
                ));
            }
        } else if path == ".claude/settings.json" {
            match serde_json::from_str::<Value>(text) {
                Ok(v) => {
                    if v.get("permissions").is_some() {
                        need(&mut out, Capability::Permissions, "undeclared_permissions");
                    }
                    if let Some(hooks) = v.get("hooks") {
                        let mut commands = Vec::new();
                        collect_key(hooks, "command", &mut commands);
                        if commands.iter().any(|c| !is_node_command(c)) {
                            need(&mut out, Capability::Exec, "undeclared_child_process");
                        }
                    }
                }
                Err(_) => out.push(Problem::about(
                    ErrorCode::PartManifestInvalid,
                    "settings_json_invalid",
                    path,
                )),
            }
        } else if path == ".mcp.json" {
            scan_mcp(path, text, policy, &mut out, &need);
        }
    }
    out
}

fn scan_mcp(
    path: &str,
    text: &str,
    policy: &ScanPolicy,
    out: &mut Vec<Problem>,
    need: &dyn Fn(&mut Vec<Problem>, Capability, &'static str),
) {
    let servers = match serde_json::from_str::<Value>(text) {
        Ok(v) => match v.get("mcpServers").and_then(Value::as_object) {
            Some(s) => s.clone(), // clone: the parsed value is local
            None => {
                out.push(Problem::about(
                    ErrorCode::PartManifestInvalid,
                    "mcp_json_invalid",
                    path,
                ));
                return;
            }
        },
        Err(_) => {
            out.push(Problem::about(
                ErrorCode::PartManifestInvalid,
                "mcp_json_invalid",
                path,
            ));
            return;
        }
    };
    for server in servers.values() {
        if server.get("command").is_some() {
            need(out, Capability::Exec, "undeclared_child_process");
        }
        if server.get("url").is_some() {
            need(out, Capability::Network, "undeclared_network");
        }
        let mut strings = Vec::new();
        collect_strings(server, &mut strings);
        for s in strings {
            for host in url_hosts(&s) {
                if !policy.host_allowed(&host) {
                    out.push(Problem::new(
                        ErrorCode::McpEndpointNotAllowed,
                        "mcp_host_not_allowed",
                        Some(host.chars().take(253).collect()),
                    ));
                }
            }
        }
    }
}

fn is_script(path: &str) -> bool {
    [".js", ".cjs", ".mjs"].iter().any(|e| path.ends_with(e))
}

/// `node …` (the hook runtime, plan 09 §3.1). Anything else runs another
/// program.
fn is_node_command(c: &str) -> bool {
    let first = c.split_whitespace().next().unwrap_or("");
    let first = first.trim_matches('"');
    first == "node" || first == "node.exe"
}

fn collect_key(v: &Value, key: &str, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                if k == key {
                    if let Some(s) = x.as_str() {
                        out.push(s.to_owned());
                    } else {
                        // A non-string command: treated as "not node".
                        out.push(String::new());
                    }
                } else {
                    collect_key(x, key, out);
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| collect_key(x, key, out)),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn collect_strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Object(m) => {
            for (k, x) in m {
                out.push(k.clone());
                collect_strings(x, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| collect_strings(x, out)),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// The hosts of every `http(s)://` / `ws(s)://` URL inside `s`. A URL that
/// does not parse yields its raw authority, which is then not on the list.
fn url_hosts(s: &str) -> Vec<String> {
    let lower = s.to_ascii_lowercase();
    let mut hosts = Vec::new();
    for scheme in ["http://", "https://", "ws://", "wss://"] {
        let mut from = 0;
        while let Some(i) = lower.get(from..).and_then(|rest| rest.find(scheme)) {
            let start = from + i;
            // No scheme is a substring of another (`https://` does not
            // contain `http://`), so each URL is found once.
            let tail = s.get(start..).unwrap_or("");
            let end = tail
                .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ',')
                .unwrap_or(tail.len());
            let candidate = tail.get(..end).unwrap_or("");
            match url::Url::parse(candidate) {
                Ok(u) => hosts.push(u.host_str().unwrap_or("").to_owned()),
                Err(_) => hosts.push(candidate.to_owned()),
            }
            from = start + scheme.len();
        }
    }
    hosts
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ScriptFindings {
    pub child_process: bool,
    pub network: bool,
    /// The first construct the scan cannot follow.
    pub dynamic: Option<&'static str>,
}

/// Scan one script (module docs). Comments are scanned too: a lexer that
/// strips them can be desynchronised by a regex literal (`/'/`) into
/// hiding real code as a "comment", so the raw source is what is searched.
pub(crate) fn scan_script(src: &str) -> ScriptFindings {
    let mut f = ScriptFindings::default();
    let mut names = string_literals(src);
    names.extend(module_names(src));
    for lit in &names {
        let name = lit.strip_prefix("node:").unwrap_or(lit);
        if CHILD_MODULES.contains(&name) {
            f.child_process = true;
        }
        if NET_MODULES.contains(&name) {
            f.network = true;
        }
        if DYNAMIC_MODULES.contains(&name) {
            f.dynamic.get_or_insert("dynamic_module");
        }
    }
    // Also in an identifier, a property or an escaped literal.
    if src.contains("child_process") {
        f.child_process = true;
    }
    for token in ["fetch(", "WebSocket", "XMLHttpRequest"] {
        if contains_word(src, token) {
            f.network = true;
        }
    }
    let dynamic_tokens: [(&str, &'static str); 13] = [
        ("eval(", "eval"),
        ("Function(", "function_constructor"),
        (".constructor", "constructor"),
        ("process.binding", "process_binding"),
        ("process._linkedBinding", "process_binding"),
        ("process.dlopen", "process_dlopen"),
        ("process[", "process_index"),
        ("mainModule", "main_module"),
        ("createRequire", "create_require"),
        ("import(", "dynamic_import"),
        ("globalThis[", "global_index"),
        ("global[", "global_index"),
        ("Reflect.", "reflect"),
    ];
    for (token, what) in dynamic_tokens {
        if contains_word(src, token) {
            f.dynamic.get_or_insert(what);
        }
    }
    if let Some(what) = unplain_require(src) {
        f.dynamic.get_or_insert(what);
    }
    f
}

/// The module names of `require('<name>')`, `from '<name>'` and
/// `import '<name>'` (plain literals only; anything else is
/// [`unplain_require`]'s or `import(`'s finding).
fn module_names(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (keyword, paren) in [("require", true), ("from", false), ("import", false)] {
        let mut from = 0;
        while let Some(i) = src.get(from..).and_then(|r| r.find(keyword)) {
            let at = from + i;
            from = at + keyword.len();
            let before = src.get(..at).and_then(|b| b.chars().next_back());
            if before.is_some_and(is_ident) {
                continue;
            }
            let mut rest = src.get(from..).unwrap_or("").trim_start();
            if paren {
                match rest.strip_prefix('(') {
                    Some(r) => rest = r.trim_start(),
                    None => continue,
                }
            }
            let Some(q) = rest
                .chars()
                .next()
                .filter(|c| matches!(c, '\'' | '"' | '`'))
            else {
                continue;
            };
            let body = rest.get(1..).unwrap_or("");
            if let Some(close) = body.find(q) {
                out.push(body.get(..close).unwrap_or("").to_owned());
            }
        }
    }
    out
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// `token` in `code` not preceded by an identifier character (so `myeval(`
/// is not `eval(`, but `globalThis.eval(` is).
fn contains_word(code: &str, token: &str) -> bool {
    let mut from = 0;
    while let Some(i) = code.get(from..).and_then(|r| r.find(token)) {
        let at = from + i;
        let before = code.get(..at).and_then(|b| b.chars().next_back());
        // A token that starts with `.` is a property: anywhere.
        if token.starts_with('.') || before.is_none_or(|c| !is_ident(c)) {
            return true;
        }
        from = at + token.len();
    }
    false
}

/// The first `require` that is not `require(<one plain string literal>)`.
fn unplain_require(code: &str) -> Option<&'static str> {
    let mut from = 0;
    while let Some(i) = code.get(from..).and_then(|r| r.find("require")) {
        let at = from + i;
        from = at + "require".len();
        let before = code.get(..at).and_then(|b| b.chars().next_back());
        let after = code.get(from..).and_then(|a| a.chars().next());
        if before.is_some_and(is_ident) || after.is_some_and(is_ident) {
            continue; // part of another identifier (`requireAuth`)
        }
        let rest = code.get(from..).unwrap_or("").trim_start();
        let Some(args) = rest.strip_prefix('(') else {
            return Some("require_not_called");
        };
        let args = args.trim_start();
        let Some(q) = args
            .chars()
            .next()
            .filter(|c| matches!(c, '\'' | '"' | '`'))
        else {
            return Some("computed_require");
        };
        let body = args.get(1..).unwrap_or("");
        let Some(close) = body.find(q) else {
            return Some("computed_require");
        };
        let name = body.get(..close).unwrap_or("");
        if name.contains('\\') || name.contains("${") {
            return Some("computed_require");
        }
        let after_lit = body.get(close + 1..).unwrap_or("").trim_start();
        if !after_lit.starts_with(')') {
            return Some("computed_require");
        }
    }
    None
}

/// The raw text of every string literal (quotes `'`, `"`, `` ` ``), comments
/// skipped. Used only to *add* findings: if a regex literal desynchronises
/// it, nothing is hidden, because every check that refuses runs on the raw
/// source.
fn string_literals(src: &str) -> Vec<String> {
    let mut literals = Vec::new();
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '/' if chars.peek() == Some(&'/') => {
                for d in chars.by_ref() {
                    if d == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = '\0';
                for d in chars.by_ref() {
                    if prev == '*' && d == '/' {
                        break;
                    }
                    prev = d;
                }
            }
            '\'' | '"' | '`' => {
                let mut lit = String::new();
                let mut escaped = false;
                for d in chars.by_ref() {
                    if escaped {
                        lit.push(d);
                        escaped = false;
                    } else if d == '\\' {
                        lit.push(d);
                        escaped = true;
                    } else if d == c {
                        break;
                    } else {
                        lit.push(d);
                    }
                }
                literals.push(lit);
            }
            _ => {}
        }
    }
    literals
}
