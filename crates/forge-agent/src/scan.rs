//! The static scan of a part (plan 16 §8, ADR-0032 §5). Hooks run as `node`
//! on every developer's PC, so the scan is conservative: what it cannot see
//! through is refused, not trusted.
//!
//! **Scripts** — every part file that is not `.md` or `.json` (node runs a
//! file of any extension), the raw source, comments included, read as
//! identifiers so spacing and comments between tokens change nothing
//! (`eval (`, `eval/**/(`):
//! - module `child_process` / `cluster` → needs `exec`;
//! - modules `http` `https` `http2` `net` `tls` `dgram` `dns` `undici`, the
//!   identifiers `fetch` `WebSocket` `XMLHttpRequest` `EventSource` → needs
//!   `network`;
//! - file writes (`writeFile`, `appendFile`, `createWriteStream`, `unlink`,
//!   `rename`, `mkdir`, `copyFile`, `symlink`, `chmod`, `truncate`, `rmSync`,
//!   …) → needs `fs-write`;
//! - refused outright (`dynamic_code`): any `\u` escape; the identifiers
//!   `eval` `Function` `Reflect` `constructor` `binding` `_linkedBinding`
//!   `dlopen` `mainModule` `createRequire` `getBuiltinModule` `globalThis`
//!   `global`; `process` not followed by `.`; `import` used as a call;
//!   a `require` that is not `require(<one plain string literal>)`; the
//!   modules `vm` `worker_threads` `module` `inspector` `v8` `wasi`.
//!   Indirection the scan still cannot see is caught by the other layers
//!   (two people review every part, the server's hook test job, beta first
//!   — plan 16 §8).
//!
//! **Hook commands** (`.claude/settings.json` `hooks`, any other `.json` with
//! `command`, `hooks:` in a Markdown front matter): only the exact form
//! `node <.claude/… .js|.cjs|.mjs of this part>` (optionally after
//! `"$CLAUDE_PROJECT_DIR"/`) runs without `exec`; anything else — shell
//! metacharacters, `node -e`, `node --require`, another program — needs
//! `exec`, and a `node` target outside the part is refused.
//!
//! **`.claude/settings.json`**: only the keys in [`SETTINGS_KEYS`];
//! `statusLine`, `apiKeyHelper`, `env`, … are refused (each runs or changes
//! something the scan cannot judge). `permissions` → needs `permissions`.
//!
//! **Markdown**: a `` !` `` (a slash command's inline bash) → needs `exec`.
//!
//! **`.mcp.json`**: a server with `command` → needs `exec`; with `url` →
//! needs `network`; every endpoint in a server's definition — any
//! `scheme://`, a `//host`, a bare host name or IPv4 address in `url`,
//! `args`, `env` — must name a host on the allow-list (`FG-AGENT-0013`,
//! contract 8: nothing outside the company).

use std::collections::BTreeSet;

use forge_errors::ErrorCode;
use serde_json::Value;

use crate::{check_install_path, Capability, Part, Problem};

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

/// The `settings.json` keys a part may set (security review A1, finding 5):
/// everything else may run a program or change Claude Code beyond what the
/// capabilities describe.
pub const SETTINGS_KEYS: [&str; 7] = [
    "$schema",
    "hooks",
    "permissions",
    "model",
    "includeCoAuthoredBy",
    "cleanupPeriodDays",
    "outputStyle",
];

const CHILD_MODULES: [&str; 2] = ["child_process", "cluster"];
const NET_MODULES: [&str; 8] = [
    "http", "https", "http2", "net", "tls", "dgram", "dns", "undici",
];
const DYNAMIC_MODULES: [&str; 6] = ["vm", "worker_threads", "module", "inspector", "v8", "wasi"];
const NET_WORDS: [&str; 4] = ["fetch", "WebSocket", "XMLHttpRequest", "EventSource"];
const DYNAMIC_WORDS: [&str; 12] = [
    "eval",
    "Function",
    "Reflect",
    "constructor",
    "binding",
    "_linkedBinding",
    "dlopen",
    "mainModule",
    "createRequire",
    "getBuiltinModule",
    "globalThis",
    "global",
];
const FS_WRITE_WORDS: [&str; 30] = [
    "writeFile",
    "writeFileSync",
    "appendFile",
    "appendFileSync",
    "createWriteStream",
    "unlink",
    "unlinkSync",
    "rmSync",
    "rmdir",
    "rmdirSync",
    "rename",
    "renameSync",
    "mkdir",
    "mkdirSync",
    "mkdtemp",
    "mkdtempSync",
    "copyFile",
    "copyFileSync",
    "cpSync",
    "symlink",
    "symlinkSync",
    "linkSync",
    "chmod",
    "chmodSync",
    "chown",
    "chownSync",
    "truncate",
    "truncateSync",
    "utimes",
    "utimesSync",
];
/// Extensions that make a dotted word a file name, not a host.
const FILE_EXTS: [&str; 22] = [
    "js", "cjs", "mjs", "ts", "json", "md", "py", "sh", "ps1", "bat", "cmd", "exe", "jar", "txt",
    "yml", "yaml", "toml", "lock", "cfg", "conf", "ini", "env",
];

/// Scan every file of `part` against its declared capabilities.
pub fn scan(part: &Part, policy: &ScanPolicy) -> Vec<Problem> {
    let caps = &part.manifest.capabilities;
    let files: BTreeSet<&str> = part.files.iter().map(|f| f.path.as_str()).collect();
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
            out.push(Problem::about(
                ErrorCode::PartManifestInvalid,
                "file_not_utf8",
                path,
            ));
            continue;
        };
        // Classified ignoring case: Windows and macOS open `Settings.JSON`.
        let key = crate::case_key(path);
        if key == ".claude/settings.json" {
            scan_settings(path, text, &files, &mut out, &need);
        } else if key == ".mcp.json" {
            scan_mcp(path, text, policy, &mut out, &need);
        } else if key.ends_with(".json") {
            // Data beside a hook; if it carries commands, they are hooks.
            if let Ok(v) = serde_json::from_str::<Value>(text) {
                let mut commands = Vec::new();
                collect_key(&v, "command", &mut commands);
                check_commands(path, &commands, &files, &mut out, &need);
            } else {
                out.push(Problem::about(
                    ErrorCode::PartManifestInvalid,
                    "json_invalid",
                    path,
                ));
            }
        } else if key.ends_with(".md") {
            let (hooks, commands) = front_matter_commands(text);
            if hooks && commands.is_empty() {
                // `hooks:` in a form the scan does not read: a program.
                need(&mut out, Capability::Exec, "undeclared_child_process");
            }
            check_commands(path, &commands, &files, &mut out, &need);
            if text.contains("!`") {
                need(&mut out, Capability::Exec, "undeclared_child_process");
            }
        } else {
            let found = scan_script(text);
            if found.child_process {
                need(&mut out, Capability::Exec, "undeclared_child_process");
            }
            if found.network {
                need(&mut out, Capability::Network, "undeclared_network");
            }
            if found.fs_write {
                need(&mut out, Capability::FsWrite, "undeclared_fs_write");
            }
            if let Some(what) = found.dynamic {
                out.push(Problem::new(
                    ErrorCode::PartCapabilityUndeclared,
                    "dynamic_code",
                    Some(format!("{path}: {what}")),
                ));
            }
        }
    }
    out
}

type Need<'a> = dyn Fn(&mut Vec<Problem>, Capability, &'static str) + 'a;

fn scan_settings(
    path: &str,
    text: &str,
    files: &BTreeSet<&str>,
    out: &mut Vec<Problem>,
    need: &Need<'_>,
) {
    let Ok(Value::Object(v)) = serde_json::from_str::<Value>(text) else {
        out.push(Problem::about(
            ErrorCode::PartManifestInvalid,
            "settings_json_invalid",
            path,
        ));
        return;
    };
    for key in v.keys() {
        if !SETTINGS_KEYS.contains(&key.as_str()) {
            out.push(Problem::new(
                ErrorCode::PartCapabilityUndeclared,
                "settings_key_not_allowed",
                Some(format!(
                    "{path}: {}",
                    key.chars().take(64).collect::<String>()
                )),
            ));
        }
    }
    if v.contains_key("permissions") {
        need(out, Capability::Permissions, "undeclared_permissions");
    }
    if let Some(hooks) = v.get("hooks") {
        let mut commands = Vec::new();
        collect_key(hooks, "command", &mut commands);
        check_commands(path, &commands, files, out, need);
    }
}

/// Hook commands (module docs): the plain `node <part script>` form, or
/// `exec`.
fn check_commands(
    path: &str,
    commands: &[String],
    files: &BTreeSet<&str>,
    out: &mut Vec<Problem>,
    need: &Need<'_>,
) {
    let mut needs_exec = false;
    for c in commands {
        match node_target(c) {
            Some(target) if files.contains(target) => {}
            Some(_) => out.push(Problem::new(
                ErrorCode::PartCapabilityUndeclared,
                "hook_target_not_in_part",
                Some(format!(
                    "{path}: {}",
                    c.chars().take(200).collect::<String>()
                )),
            )),
            None => needs_exec = true,
        }
    }
    if needs_exec {
        need(out, Capability::Exec, "undeclared_child_process");
    }
}

/// The script of `node <path>` when the command is exactly that form: one
/// space, no flag, no shell metacharacter, a canonical install path of a
/// Node script (optionally after `"$CLAUDE_PROJECT_DIR"/`).
pub(crate) fn node_target(command: &str) -> Option<&str> {
    let rest = command.strip_prefix("node ")?;
    let rest = rest
        .strip_prefix("\"$CLAUDE_PROJECT_DIR\"/")
        .or_else(|| rest.strip_prefix("$CLAUDE_PROJECT_DIR/"))
        .unwrap_or(rest);
    let plain = rest
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'/' | b'-'));
    let script = [".js", ".cjs", ".mjs"].iter().any(|e| rest.ends_with(e));
    (plain && script && rest.starts_with(".claude/") && check_install_path(rest).is_ok())
        .then_some(rest)
}

/// A Markdown file's front matter: whether it has `hooks:`, and the values
/// of its `command:` lines (quotes removed).
pub(crate) fn front_matter_commands(text: &str) -> (bool, Vec<String>) {
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return (false, Vec::new());
    }
    let mut hooks = false;
    let mut commands = Vec::new();
    for line in lines {
        let t = line.trim();
        if t == "---" {
            break;
        }
        if t.starts_with("hooks:") {
            hooks = true;
        }
        let t = t.trim_start_matches("- ");
        if let Some(v) = t.strip_prefix("command:") {
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|x| x.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')))
                .unwrap_or(v);
            commands.push(v.to_owned());
        }
    }
    (hooks, commands)
}

/// Whether a Markdown file runs commands (hooks in its front matter, or a
/// slash command's `` !` `` line).
pub(crate) fn markdown_runs_commands(text: &str) -> bool {
    let (hooks, commands) = front_matter_commands(text);
    hooks || !commands.is_empty() || text.contains("!`")
}

fn scan_mcp(path: &str, text: &str, policy: &ScanPolicy, out: &mut Vec<Problem>, need: &Need<'_>) {
    let parsed = serde_json::from_str::<Value>(text);
    let Some(servers) = parsed
        .as_ref()
        .ok()
        .and_then(|v| v.get("mcpServers"))
        .and_then(Value::as_object)
    else {
        out.push(Problem::about(
            ErrorCode::PartManifestInvalid,
            "mcp_json_invalid",
            path,
        ));
        return;
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
            for host in endpoint_hosts(&s) {
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

fn collect_key(v: &Value, key: &str, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                if k == key {
                    // A non-string command is "not the node form".
                    out.push(x.as_str().unwrap_or_default().to_owned());
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

/// Every host an MCP string may reach: any `scheme://…`, a `//host…`, a
/// bare dotted host name (not a file name) or an IPv4 address, with or
/// without a port. A URL that does not parse yields its raw text, which is
/// then not on the list.
fn endpoint_hosts(s: &str) -> Vec<String> {
    let mut hosts = Vec::new();
    let tokens = s.split(|c: char| {
        c.is_whitespace() || matches!(c, '=' | ',' | '"' | '\'' | ';' | '(' | ')' | '<' | '>')
    });
    for token in tokens.filter(|t| !t.is_empty()) {
        if token.contains("://") {
            match url::Url::parse(token) {
                Ok(u) => hosts.push(u.host_str().unwrap_or(token).to_owned()),
                Err(_) => hosts.push(token.to_owned()),
            }
        } else if let Some(rest) = token.strip_prefix("//") {
            let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
            hosts.push(strip_port(host).to_owned());
        } else {
            let head = token.split('/').next().unwrap_or(token);
            let host = strip_port(head);
            if looks_like_host(host) {
                hosts.push(host.to_owned());
            }
        }
    }
    hosts
}

fn strip_port(h: &str) -> &str {
    match h.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|c| c.is_ascii_digit()) => host,
        _ => h,
    }
}

fn looks_like_host(h: &str) -> bool {
    let labels: Vec<&str> = h.split('.').collect();
    if labels.len() < 2
        || labels.iter().any(|l| l.is_empty())
        || !h
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-')
    {
        return false;
    }
    let ipv4 = labels.len() == 4
        && labels
            .iter()
            .all(|l| l.len() <= 3 && l.bytes().all(|c| c.is_ascii_digit()));
    let last = labels.last().copied().unwrap_or("");
    let named = last.len() >= 2
        && last.bytes().all(|c| c.is_ascii_alphabetic())
        && !FILE_EXTS.contains(&last.to_ascii_lowercase().as_str());
    ipv4 || named
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ScriptFindings {
    pub child_process: bool,
    pub network: bool,
    pub fs_write: bool,
    /// The first construct the scan cannot follow.
    pub dynamic: Option<&'static str>,
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Every identifier-like word of `src` with its byte offset (strings and
/// comments included: the scan is conservative).
fn words(src: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in src.char_indices() {
        match (is_ident(c), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                out.push((s, src.get(s..i).unwrap_or("")));
                start = None;
            }
            (true, Some(_)) | (false, None) => {}
        }
    }
    if let Some(s) = start {
        out.push((s, src.get(s..).unwrap_or("")));
    }
    out
}

fn next_non_ws(src: &str, from: usize) -> Option<char> {
    src.get(from..)
        .and_then(|r| r.chars().find(|c| !c.is_whitespace()))
}

/// Scan one script (module docs).
pub(crate) fn scan_script(src: &str) -> ScriptFindings {
    let mut f = ScriptFindings::default();
    if src.contains("\\u") {
        f.dynamic.get_or_insert("unicode_escape");
    }
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
    for (at, w) in words(src) {
        let after = at + w.len();
        if w == "child_process" {
            f.child_process = true;
        } else if NET_WORDS.contains(&w) {
            f.network = true;
        } else if FS_WRITE_WORDS.contains(&w) {
            f.fs_write = true;
        } else if let Some(d) = DYNAMIC_WORDS.iter().find(|d| **d == w) {
            f.dynamic.get_or_insert(*d);
        } else if w == "process" && next_non_ws(src, after) != Some('.') {
            f.dynamic.get_or_insert("process_access");
        } else if w == "import" {
            let static_form = next_non_ws(src, after)
                .is_some_and(|c| matches!(c, '{' | '*' | '\'' | '"' | '.') || is_ident(c));
            if !static_form {
                f.dynamic.get_or_insert("dynamic_import");
            }
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
