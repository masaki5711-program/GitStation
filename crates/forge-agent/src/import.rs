//! Seeding the marketplace (plan 16 A6, ADR-0032 Amendment A6): turn parts
//! written in another shape into `part.toml` part directories, pure.
//!
//! Two sources:
//!
//! - **W1 `part.json` parts** ([`import_marketplace_part`]): the
//!   `marketplace/<category>/<id>/` tree the agent models share
//!   (`part.json` with `{src, target, merge}` files, `requires` as
//!   `<category>/<id>`, hook `events`, per-adapter `overlays`).
//! - **A Claude Code tree** ([`import_claude_item`]): a skill directory, an
//!   agent, a command or a rule file of an existing `.claude/`.
//!
//! The slug is `<kind>-<id>` (`hook-kanflow-claim`, `command-kanflow`):
//! stable whatever else is imported later, and one id may be used by two
//! kinds (`commands/kanflow`, `mcp_servers/kanflow`). Hook `events` become
//! the part's own `.claude/settings.json` (`{"hooks": …}`) so the static
//! scan checks every command; an adapter merges the fragments (A7). An MCP
//! `merge: "$.mcpServers"` file is the part's `.mcp.json` as it is. Overlays
//! move out of the part into the adapter part of their base model
//! ([`adapter_part`]): a signed part is the same bytes for every model.
//!
//! [`seed`] then declares the capabilities the static scan says the part
//! needs, only those the caller allows (`exec`, `network`, `fs-write`;
//! never `permissions`, never a way past `dynamic_code`, a host outside
//! the list or any other refusal), renders
//! `part.toml` deterministically and runs the same `parse_part` and `scan`
//! as `gstn agent verify --part` and the publish step. The inferred
//! capabilities and the findings behind them are reported so the
//! reviewers of the parts MR see them. Overlay lines are a fixed list
//! ([`OVERLAY_LINES`]) that only take powers away.

use std::collections::{BTreeMap, BTreeSet};

use forge_errors::ErrorCode;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::{parse_part, scan, Capability, Kind, Part, Problem, ScanPolicy, TreeEntry, MANIFEST};

/// The version a part gets when its source has none (a Claude Code tree).
pub const FIRST_VERSION: &str = "1.0.0";
/// Where an adapter part keeps its definition (plan 09 §4
/// `target-adapter.yaml`, as JSON).
pub const ADAPTER_FILE: &str = ".claude/gitstation/adapter.json";
/// The two base models (plan 09 §3.2).
pub const BASE_MODELS: [&str; 2] = ["claude-code-tmux", "claude-code-v2"];

const SETTINGS: &str = ".claude/settings.json";
const MCP: &str = ".mcp.json";
const MCP_MERGE: &str = "$.mcpServers";
const FRONTMATTER_PREPEND: &str = "frontmatter-prepend";
/// The only overlay lines accepted (security review A6): each one takes
/// something away from the model; nothing that adds a hook, a tool, a
/// command or a permission can be written this way.
pub const OVERLAY_LINES: [&str; 1] = ["disable-model-invocation: true"];

/// A part before its capabilities are known: what [`seed`] checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Imported {
    pub slug: String,
    pub kind: Kind,
    pub version: String,
    /// Slugs (bare: the source has no version ranges).
    pub requires: Vec<String>,
    /// Install path → bytes.
    pub files: BTreeMap<String, Vec<u8>>,
    /// Where it came from (`hooks/kanflow-claim`, `.claude/agents/x.md`).
    pub source: String,
}

/// One base-model change to a part's file, applied by the adapter (A7).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Overlay {
    pub adapter: String,
    pub slug: String,
    /// The install path the change applies to.
    pub path: String,
    pub op: String,
    pub line: String,
}

/// A W1 part converted, with the overlays it carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceImport {
    pub part: Imported,
    pub overlays: Vec<Overlay>,
}

/// A part ready to be written (`entries`: `part.toml` and the files) and
/// what the checks said.
#[derive(Debug, Clone)]
pub struct Seeded {
    pub slug: String,
    pub source: String,
    pub entries: Vec<TreeEntry>,
    /// `Some` when the lint passed (the scan may still have problems).
    pub part: Option<Part>,
    /// Capabilities declared because the static scan needs them.
    pub inferred: BTreeSet<Capability>,
    /// Why each inferred capability was declared: (capability, reason,
    /// file) of the finding, for the reviewers of the parts MR.
    pub evidence: Vec<(Capability, &'static str, String)>,
    pub problems: Vec<Problem>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct W1Part {
    id: String,
    category: String,
    #[serde(default)]
    kind: Option<String>,
    version: String,
    #[serde(default)]
    description: Option<String>,
    files: Vec<W1File>,
    #[serde(default)]
    requires: Vec<String>,
    #[serde(default)]
    events: Vec<W1Event>,
    #[serde(default)]
    overlays: BTreeMap<String, Vec<W1Overlay>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct W1File {
    src: String,
    target: String,
    #[serde(default)]
    merge: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct W1Event {
    event: String,
    #[serde(default)]
    matcher: Option<String>,
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct W1Overlay {
    file: String,
    op: String,
    line: String,
}

fn invalid(reason: &'static str, subject: &str) -> Problem {
    Problem::about(
        ErrorCode::PartManifestInvalid,
        reason,
        &subject.chars().take(260).collect::<String>(),
    )
}

/// The part kind of a W1 category directory.
pub fn kind_of_category(category: &str) -> Option<Kind> {
    Some(match category {
        "skills" => Kind::Skill,
        "sub_agents" => Kind::Agent,
        "rules" => Kind::Rule,
        "commands" => Kind::Command,
        "hooks" => Kind::Hook,
        "mcp_servers" => Kind::Mcp,
        _ => return None,
    })
}

/// The slug of a part: `<kind>-<id>` (module docs).
pub fn slug_for(kind: Kind, id: &str) -> Option<String> {
    let ok = !id.is_empty()
        && id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && !id.starts_with('-')
        && !id.ends_with('-');
    ok.then(|| format!("{}-{id}", kind.as_str()))
}

/// A W1 source path (`src`): relative, `/`-separated, no `.` / `..` /
/// empty segment, no `\` / `:` / control character. The caller reads only
/// paths that pass.
fn valid_source_path(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= 255
        && !p.chars().any(|c| c.is_control() || c == '\\' || c == ':')
        // Security review A6 (MEDIUM): the install-path segment rules too —
        // Windows drops a trailing dot or space (`".. "` would become `..`).
        && p.split('/').all(|s| {
            !s.is_empty()
                && s != "."
                && s != ".."
                && !s.ends_with('.')
                && !s.ends_with(' ')
                && !s.eq_ignore_ascii_case(".git")
        })
}

fn parse_w1(part_json: &[u8]) -> Result<W1Part, Problem> {
    serde_json::from_slice::<W1Part>(part_json).map_err(|e| {
        // serde_json names the key and the position, never other content.
        invalid(
            "source_manifest_invalid",
            &e.to_string().chars().take(200).collect::<String>(),
        )
    })
}

/// The `src` paths a W1 `part.json` names, checked: what the caller reads
/// next to it before [`import_marketplace_part`].
pub fn marketplace_sources(part_json: &[u8]) -> Result<Vec<String>, Vec<Problem>> {
    let w1 = parse_w1(part_json).map_err(|p| vec![p])?;
    let mut out = Vec::new();
    let mut problems = Vec::new();
    for f in &w1.files {
        if valid_source_path(&f.src) {
            out.push(f.src.clone());
        } else {
            problems.push(invalid("source_path_invalid", &f.src));
        }
    }
    if problems.is_empty() {
        Ok(out)
    } else {
        Err(problems)
    }
}

/// A W1 part (`marketplace/<category>/<id>/part.json`) as a part: the
/// directory names must match the manifest, `sources` holds the bytes of
/// every `src` ([`marketplace_sources`]).
pub fn import_marketplace_part(
    category: &str,
    id: &str,
    part_json: &[u8],
    sources: &BTreeMap<String, Vec<u8>>,
) -> Result<MarketplaceImport, Vec<Problem>> {
    let origin = format!("{category}/{id}");
    let w1 = parse_w1(part_json).map_err(|p| vec![p])?;
    let mut problems = Vec::new();
    if w1.category != category || w1.id != id {
        problems.push(invalid("source_directory_mismatch", &origin));
    }
    let Some(kind) = kind_of_category(category) else {
        return Err(vec![invalid("source_category_unknown", category)]);
    };
    let Some(slug) = slug_for(kind, id) else {
        return Err(vec![invalid("source_id_invalid", id)]);
    };
    if let Some(k) = &w1.kind {
        if kind != Kind::Hook || !matches!(k.as_str(), "hook" | "library" | "script") {
            problems.push(invalid("source_kind_invalid", k));
        }
    }
    // The description has no place in part.toml v1 (the digest covers the
    // manifest's fields only); it stays in the source.
    let _ = &w1.description;

    let mut files = BTreeMap::new();
    let mut targets = BTreeMap::new();
    for f in &w1.files {
        match f.merge.as_deref() {
            None => {}
            Some(MCP_MERGE) if f.target == MCP && kind == Kind::Mcp => {}
            Some(_) => {
                problems.push(invalid("merge_unsupported", &f.target));
                continue;
            }
        }
        let Some(bytes) = sources.get(&f.src) else {
            problems.push(invalid("source_file_missing", &f.src));
            continue;
        };
        if f.merge.is_some() && !mcp_fragment(bytes) {
            problems.push(invalid("mcp_fragment_invalid", &f.src));
            continue;
        }
        if files.insert(f.target.clone(), bytes.clone()).is_some() {
            problems.push(invalid("duplicate_file", &f.target));
        }
        targets.insert(f.src.as_str(), f.target.as_str());
    }

    if !w1.events.is_empty() {
        if kind != Kind::Hook {
            problems.push(invalid("events_outside_hook", &origin));
        } else if files.contains_key(SETTINGS) {
            problems.push(invalid("settings_conflict", SETTINGS));
        } else {
            files.insert(SETTINGS.to_owned(), settings_fragment(&w1.events));
        }
    }

    let mut requires = Vec::new();
    for r in &w1.requires {
        match r
            .split_once('/')
            .and_then(|(c, i)| slug_for(kind_of_category(c)?, i))
        {
            Some(s) => requires.push(s),
            None => problems.push(invalid("source_require_invalid", r)),
        }
    }

    let mut overlays = Vec::new();
    for (adapter, list) in &w1.overlays {
        if !BASE_MODELS.contains(&adapter.as_str()) {
            problems.push(invalid("overlay_adapter_unknown", adapter));
            continue;
        }
        for o in list {
            let Some(path) = targets.get(o.file.as_str()) else {
                problems.push(invalid("overlay_file_unknown", &o.file));
                continue;
            };
            if o.op != FRONTMATTER_PREPEND {
                problems.push(invalid("overlay_op_unsupported", &o.op));
                continue;
            }
            // Security review A6 (CRITICAL): the line is prepended to a
            // signed file by the adapter (A7) and never scanned here, so
            // only lines that can take a power away are accepted.
            if !OVERLAY_LINES.contains(&o.line.as_str()) {
                problems.push(invalid("overlay_line_invalid", &o.file));
                continue;
            }
            overlays.push(Overlay {
                adapter: adapter.clone(),
                slug: slug.clone(),
                path: (*path).to_owned(),
                op: o.op.clone(),
                line: o.line.clone(),
            });
        }
    }

    if !problems.is_empty() {
        return Err(problems);
    }
    Ok(MarketplaceImport {
        part: Imported {
            slug,
            kind,
            version: w1.version,
            requires,
            files,
            source: origin,
        },
        overlays,
    })
}

/// `{"mcpServers": {…}}` with at least one server.
fn mcp_fragment(bytes: &[u8]) -> bool {
    serde_json::from_slice::<Value>(bytes)
        .ok()
        .and_then(|v| v.get("mcpServers").and_then(Value::as_object).cloned())
        .is_some_and(|m| !m.is_empty())
}

/// Claude Code's `settings.json` shape of a hook part's events, grouped by
/// event (sorted) and matcher (in source order), deterministic bytes.
fn settings_fragment(events: &[W1Event]) -> Vec<u8> {
    let mut by_event: BTreeMap<&str, Vec<(Option<&str>, Value)>> = BTreeMap::new();
    for e in events {
        let mut hook = Map::new();
        hook.insert("type".into(), json!("command"));
        hook.insert("command".into(), json!(e.command));
        if let Some(t) = e.timeout {
            hook.insert("timeout".into(), json!(t));
        }
        let groups = by_event.entry(e.event.as_str()).or_default();
        let matcher = e.matcher.as_deref();
        match groups.iter_mut().find(|(m, _)| *m == matcher) {
            Some((_, Value::Array(hooks))) => hooks.push(Value::Object(hook)),
            _ => groups.push((matcher, Value::Array(vec![Value::Object(hook)]))),
        }
    }
    let mut hooks = Map::new();
    for (event, groups) in by_event {
        let list: Vec<Value> = groups
            .into_iter()
            .map(|(matcher, hooks)| {
                let mut g = Map::new();
                if let Some(m) = matcher {
                    g.insert("matcher".into(), json!(m));
                }
                g.insert("hooks".into(), hooks);
                Value::Object(g)
            })
            .collect();
        hooks.insert(event.to_owned(), Value::Array(list));
    }
    let mut root = Map::new();
    root.insert("hooks".into(), Value::Object(hooks));
    pretty(&Value::Object(root))
}

fn pretty(v: &Value) -> Vec<u8> {
    let mut out = serde_json::to_vec_pretty(v).unwrap_or_default();
    out.push(b'\n');
    out
}

/// One item of a Claude Code tree as a part: `files` are install paths
/// (`.claude/skills/<id>/…`, `.claude/agents/<id>.md`, …) with their bytes.
pub fn import_claude_item(
    kind: Kind,
    id: &str,
    source: &str,
    files: BTreeMap<String, Vec<u8>>,
) -> Result<Imported, Vec<Problem>> {
    let slug = slug_for(kind, id).ok_or_else(|| vec![invalid("source_id_invalid", id)])?;
    Ok(Imported {
        slug,
        kind,
        version: FIRST_VERSION.to_owned(),
        requires: Vec::new(),
        files,
        source: source.to_owned(),
    })
}

/// The adapter part of a base model (plan 16 A6 "two adapters"): how its
/// builder writes parts (copy; `settings.json` hooks and `.mcp.json`
/// servers JSON-merged) and the overlays of that model, sorted.
pub fn adapter_part(adapter: &str, overlays: &[Overlay]) -> Result<Imported, Vec<Problem>> {
    if !BASE_MODELS.contains(&adapter) {
        return Err(vec![invalid("overlay_adapter_unknown", adapter)]);
    }
    let mut mine: Vec<&Overlay> = overlays.iter().filter(|o| o.adapter == adapter).collect();
    mine.sort();
    let def = json!({
        "schema_version": 1,
        "adapter_id": adapter,
        "outputs": [
            {"path": SETTINGS, "strategy": "json_merge", "at": "$.hooks"},
            {"path": MCP, "strategy": "json_merge", "at": MCP_MERGE},
            {"path": "*", "strategy": "copy"},
        ],
        "overlays": mine.iter().map(|o| json!({
            "part": o.slug, "path": o.path, "op": o.op, "line": o.line,
        })).collect::<Vec<_>>(),
    });
    let mut files = BTreeMap::new();
    files.insert(ADAPTER_FILE.to_owned(), pretty(&def));
    import_claude_item(Kind::Adapter, adapter, &format!("adapter {adapter}"), files)
}

/// TOML basic string (JSON's escapes are a subset TOML reads).
fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_owned())
}

fn list(items: impl IntoIterator<Item = String>) -> String {
    let items: Vec<String> = items.into_iter().map(|s| quote(&s)).collect();
    format!("[{}]", items.join(", "))
}

/// `part.toml` for `p` with `caps`, deterministic.
pub fn render_manifest(p: &Imported, caps: &BTreeSet<Capability>) -> String {
    let mut out = format!(
        "slug = {}\nkind = {}\nversion = {}\n",
        quote(&p.slug),
        quote(p.kind.as_str()),
        quote(&p.version)
    );
    let mut requires = p.requires.clone();
    requires.sort();
    requires.dedup();
    if !requires.is_empty() {
        out.push_str(&format!("requires = {}\n", list(requires)));
    }
    if !caps.is_empty() {
        out.push_str(&format!(
            "capabilities = {}\n",
            list(caps.iter().map(|c| c.as_str().to_owned()))
        ));
    }
    out.push_str(&format!("files = {}\n", list(p.files.keys().cloned())));
    out
}

fn entries(p: &Imported, caps: &BTreeSet<Capability>) -> Vec<TreeEntry> {
    let mut out = vec![TreeEntry::file(MANIFEST, render_manifest(p, caps))];
    out.extend(
        p.files
            .iter()
            .map(|(path, bytes)| TreeEntry::file(path, bytes.clone())),
    );
    out
}

/// The capability an undeclared-use finding asks for; `None` for every
/// other finding (those stay problems).
fn inferable(p: &Problem) -> Option<Capability> {
    if p.code != ErrorCode::PartCapabilityUndeclared {
        return None;
    }
    Some(match p.reason {
        "undeclared_child_process" => Capability::Exec,
        "undeclared_network" => Capability::Network,
        "undeclared_fs_write" => Capability::FsWrite,
        "undeclared_permissions" => Capability::Permissions,
        _ => return None,
    })
}

/// Declare what the scan needs — only capabilities in `allow`, never
/// `permissions` — render `part.toml`, lint and scan (module docs).
///
/// Security review A6 (HIGH): `exec` is also what the scan asks for a
/// command it cannot read (a hook command that is not `node`, a `` !` ``
/// line), so inference is opt-in per capability and every inferred one
/// carries the findings that caused it ([`Seeded::evidence`]).
pub fn seed(p: &Imported, policy: &ScanPolicy, allow: &BTreeSet<Capability>) -> Seeded {
    let none = BTreeSet::new();
    let failed = |entries, inferred, evidence, problems| Seeded {
        slug: p.slug.clone(),
        source: p.source.clone(),
        entries,
        part: None,
        inferred,
        evidence,
        problems,
    };
    let first = entries(p, &none);
    let part = match parse_part(&first) {
        Ok(part) => part,
        Err(problems) => return failed(first, none, Vec::new(), problems),
    };
    let evidence: Vec<(Capability, &'static str, String)> = scan(&part, policy)
        .iter()
        .filter_map(|f| {
            let cap = inferable(f)?;
            (allow.contains(&cap) && cap != Capability::Permissions)
                .then(|| (cap, f.reason, f.subject.clone().unwrap_or_default()))
        })
        .collect();
    let inferred: BTreeSet<Capability> = evidence.iter().map(|(c, _, _)| *c).collect();
    let (entries, part) = if inferred.is_empty() {
        (first, part)
    } else {
        let again = entries(p, &inferred);
        match parse_part(&again) {
            Ok(part) => (again, part),
            Err(problems) => return failed(again, inferred, evidence, problems),
        }
    };
    let problems = scan(&part, policy);
    Seeded {
        slug: p.slug.clone(),
        source: p.source.clone(),
        entries,
        part: Some(part),
        inferred,
        evidence,
        problems,
    }
}

/// Problems across one import: a slug used twice, a `requires` naming a
/// part that is not imported. Indexed by the position in `parts`.
pub fn check_set(parts: &[Imported]) -> Vec<(usize, Problem)> {
    let mut out = Vec::new();
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, p) in parts.iter().enumerate() {
        if seen.insert(p.slug.as_str(), i).is_some() {
            out.push((i, invalid("slug_collision", &p.slug)));
        }
    }
    for (i, p) in parts.iter().enumerate() {
        for r in &p.requires {
            if !seen.contains_key(r.as_str()) {
                out.push((i, invalid("require_not_in_import", r)));
            }
        }
    }
    out
}

#[cfg(test)]
#[path = "import_tests.rs"]
mod tests;
