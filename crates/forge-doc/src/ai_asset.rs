//! AI assets (plan 17 §2, P17-3, ADR-0035): the lint of a repository that
//! holds the central AI's skills, playbooks, agent definitions, models, the
//! bench settings and the evaluation cases, and the scan of a case for data
//! that must never be in a repository (plan 17 §4). Pure: paths and bytes
//! in, [`Problem`]s out — no file, clock or network.
//!
//! Not a document type (ADR-0035 §1): the assets are TOML, JSON and
//! Markdown with a YAML front matter whose shapes are PFS's (`pe-*.md`,
//! `me_ai_playbook`), and their checks run across files (a playbook names
//! skills, an agent's allowed tools, profiles). The same functions run in
//! `gstn ai-asset lint` and, for [`case_raw_data`], in the server's
//! `forge/risk` rule `ai_case_raw_data` — only for a project whose
//! `ai_assets.enabled` is on (plan 17 §0.4).
//!
//! The layout ([`kind_of`]): `skills/<name>.md`, `playbooks/<scope>.toml`
//! (`[[playbook]]`, one table per `me_ai_playbook` row), `agents/<name>.toml`
//! (optional until PFS-8), `tools/tools.toml` (optional), `models/models.toml`,
//! `bench/bench.toml`, `evals/cases/<anomaly_class>/<id>.json`,
//! `evals/baselines/<models>.json` (the shape is ADR-0034's, checked by the
//! `forge/ai-eval` worker), `thresholds/proposals/*.md` (never released).

use std::collections::{BTreeMap, BTreeSet};

use crate::yaml::{read_tree, Kind, Node};

/// A larger asset file is refused (`too_large`).
pub const MAX_FILE_BYTES: usize = 256 * 1024;
/// At most this many problems; then `too_many_problems`.
pub const MAX_PROBLEMS: usize = 200;
/// A subject is cut to this many characters.
const MAX_SUBJECT: usize = 96;

/// One file of the asset repository.
#[derive(Debug, Clone, Copy)]
pub struct AssetFile<'a> {
    /// From the repository's root, `/`-separated.
    pub path: &'a str,
    pub bytes: &'a [u8],
}

/// One rule broken: where (line 0: the whole file, or a TOML file, whose
/// subject names the table and key) and a stable rule name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Problem {
    pub path: String,
    pub line: usize,
    pub rule: &'static str,
    pub subject: Option<String>,
}

impl Problem {
    fn new(path: &str, line: usize, rule: &'static str, subject: Option<&str>) -> Self {
        Self {
            path: path.to_owned(),
            line,
            rule,
            subject: subject.map(|s| s.chars().take(MAX_SUBJECT).collect()),
        }
    }
}

/// What an asset path holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetKind {
    Skill,
    Playbook,
    Agent,
    Tools,
    Models,
    Bench,
    Case,
    Baseline,
    ThresholdProposal,
    /// Under `thresholds/` but not a proposal (`threshold_not_proposal`).
    Threshold,
    /// `CODEOWNERS` (the protected paths and the skills' reviewers).
    Owners,
}

/// A file under `evals/` that is no case: documentation (`.md`, any case)
/// and `.gitkeep`. Everything else there is a case (any extension: a
/// misnamed or misplaced case is still scanned; the lint says
/// `case_path_invalid`).
fn not_a_case(name: &str) -> bool {
    name == ".gitkeep"
        || name
            .rsplit_once('.')
            .is_some_and(|(_, e)| e.eq_ignore_ascii_case("md"))
}

/// The asset at `path`, `None` for any other file (README, `.forge-ci.yml`).
pub fn kind_of(path: &str) -> Option<AssetKind> {
    let parts: Vec<&str> = path.split('/').collect();
    let ext = |want: &str| path.rsplit_once('.').is_some_and(|(_, e)| e == want);
    match parts.as_slice() {
        ["skills", _] if ext("md") => Some(AssetKind::Skill),
        ["playbooks", _] if ext("toml") => Some(AssetKind::Playbook),
        ["agents", _] if ext("toml") => Some(AssetKind::Agent),
        ["tools", "tools.toml"] => Some(AssetKind::Tools),
        ["models", "models.toml"] => Some(AssetKind::Models),
        ["bench", "bench.toml"] => Some(AssetKind::Bench),
        ["evals", "baselines", ..] => Some(AssetKind::Baseline),
        ["evals", .., last] if !not_a_case(last) => Some(AssetKind::Case),
        ["thresholds", "proposals", _] if ext("md") => Some(AssetKind::ThresholdProposal),
        ["thresholds", ..] => Some(AssetKind::Threshold),
        ["CODEOWNERS"] | [".gitstation", "CODEOWNERS"] => Some(AssetKind::Owners),
        _ => None,
    }
}

/// Whether a change to `path` is a change of an AI asset (the `forge/risk`
/// rule `ai_asset`, `warn`): anything under the asset directories, and
/// `CODEOWNERS`.
pub fn is_asset_path(path: &str) -> bool {
    let first = path.split('/').next().unwrap_or("");
    matches!(
        first,
        "skills" | "playbooks" | "agents" | "tools" | "models" | "bench" | "evals" | "thresholds"
    ) || kind_of(path) == Some(AssetKind::Owners)
}

/// Whether `path` is an evaluation case (read for raw data by `forge/risk`).
pub fn is_case_path(path: &str) -> bool {
    kind_of(path) == Some(AssetKind::Case)
}

// ── raw data in a case (plan 17 §4) ────────────────────────────────────

/// Something in a case that a synthetic replica never holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawData {
    /// 1-based (1 for a finding about the whole case, or one whose place
    /// in the text cannot be told — an escaped key).
    pub line: usize,
    /// `absolute_timestamp`, `ip_address`, `plant_code`, `free_text`,
    /// `person`, `provenance` (missing, or not `replica` / `synthetic`),
    /// `confidential`, `duplicate_key`, `not_json`.
    pub kind: &'static str,
}

/// Keys whose value is a person's free text (the feedback; plan 17 §4:
/// never, not even summarised — it is an injection path). Compared after
/// JSON decoding, lower-cased, `-` read as `_`.
const FREE_TEXT_KEYS: &[&str] = &[
    "feedback",
    "feedback_text",
    "comment",
    "comments",
    "free_text",
    "freetext",
    "remarks",
    "remark",
    "note_text",
    "operator_comment",
];
/// Keys naming a person (as [`FREE_TEXT_KEYS`]).
const PERSON_KEYS: &[&str] = &[
    "staff_id",
    "staff_no",
    "employee_id",
    "employee_no",
    "employee_number",
    "operator",
    "operator_id",
    "operator_name",
    "person",
    "person_name",
    "user_name",
    "username",
    "worker",
    "worker_name",
];
/// A number at least this large under a date-like key is an epoch time
/// (seconds since 2001-09-09, or milliseconds).
const EPOCH_MIN: f64 = 1_000_000_000.0;

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn digits(bytes: &[u8], at: usize, n: usize) -> bool {
    (at..at + n).all(|i| bytes.get(i).is_some_and(u8::is_ascii_digit))
}

/// `YYYY-MM-DD`, `YYYY/MM/DD` or `YYYY.MM.DD` (with or without a time after
/// it) starting at `at`.
fn date_at(b: &[u8], at: usize) -> bool {
    let sep = b.get(at + 4).copied();
    digits(b, at, 4)
        && matches!(sep, Some(b'-' | b'/' | b'.'))
        && digits(b, at + 5, 2)
        && b.get(at + 7).copied() == sep
        && digits(b, at + 8, 2)
        && !b.get(at + 10).is_some_and(u8::is_ascii_digit)
}

/// The length of a dotted IPv4 address at `at` (0: none).
fn ipv4_at(b: &[u8], at: usize) -> usize {
    let mut i = at;
    for part in 0..4 {
        let start = i;
        while i < b.len() && i - start < 4 && b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        let len = i - start;
        if !(1..=3).contains(&len) {
            return 0;
        }
        if part < 3 {
            if b.get(i) != Some(&b'.') {
                return 0;
            }
            i += 1;
        }
    }
    if b.get(i).is_some_and(|c| c.is_ascii_digit() || *c == b'.') {
        return 0;
    }
    i - at
}

/// The length of an IPv6 address at `at` (0: none): hex groups and colons,
/// with `::` or at least five colons (a time `08:15:00` is neither).
fn ipv6_at(b: &[u8], at: usize) -> usize {
    let mut i = at;
    while b.get(i).is_some_and(|c| c.is_ascii_hexdigit() || *c == b':') {
        i += 1;
    }
    let Some(token) = b.get(at..i) else {
        return 0;
    };
    let colons = token.iter().filter(|c| **c == b':').count();
    let compressed = token.windows(2).any(|w| w == b"::");
    let hex = token.iter().any(u8::is_ascii_hexdigit);
    let ends_ok = !b.get(i).is_some_and(|c| is_word(*c));
    if token.len() >= 3 && hex && ends_ok && ((compressed && colons >= 2) || colons >= 5) {
        token.len()
    } else {
        0
    }
}

/// A real plant code `P<3 digits>` at `at` (`P9xx` is the synthetic range).
fn plant_at(b: &[u8], at: usize) -> bool {
    b.get(at) == Some(&b'P')
        && digits(b, at + 1, 3)
        && b.get(at + 1) != Some(&b'9')
        && !b.get(at + 4).is_some_and(|c| is_word(*c))
}

/// The kinds of raw data in one text (a raw line or a decoded string).
fn scan_text(text: &str) -> Vec<&'static str> {
    let b = text.as_bytes();
    let mut found = Vec::new();
    let mut hit = |kind: &'static str| {
        if !found.contains(&kind) {
            found.push(kind);
        }
    };
    let mut i = 0;
    while i < b.len() {
        let boundary = i == 0
            || b.get(i - 1)
                .is_some_and(|c| !is_word(*c) && !matches!(*c, b'.' | b':'));
        if boundary && date_at(b, i) {
            hit("absolute_timestamp");
            i += 10;
            continue;
        }
        if boundary {
            let n = ipv4_at(b, i);
            if n > 0 {
                hit("ip_address");
                i += n;
                continue;
            }
            let n = ipv6_at(b, i);
            if n > 0 {
                hit("ip_address");
                i += n;
                continue;
            }
        }
        if boundary && plant_at(b, i) {
            hit("plant_code");
            i += 4;
            continue;
        }
        i += 1;
    }
    found
}

/// A key as compared: lower-cased, `-` and spaces read as `_`.
fn key_norm(key: &str) -> String {
    key.chars()
        .map(|c| match c {
            '-' | ' ' => '_',
            other => other.to_ascii_lowercase(),
        })
        .collect()
}

fn key_kinds(key: &str) -> Vec<&'static str> {
    let k = key_norm(key);
    let mut out = Vec::new();
    if FREE_TEXT_KEYS.contains(&k.as_str()) {
        out.push("free_text");
    }
    if PERSON_KEYS.contains(&k.as_str()) {
        out.push("person");
    }
    out
}

/// A key whose value is a point in time.
fn date_like_key(key: &str) -> bool {
    let k = key_norm(key);
    k.ends_with("_at")
        || ["time", "date", "timestamp", "epoch", "when"]
            .iter()
            .any(|w| k.contains(w))
}

/// The JSON key at `at` of a raw line (`"name"` followed by `:`), if any.
fn key_at(line: &str, at: usize) -> Option<&str> {
    let rest = line.get(at + 1..)?;
    let end = rest.find('"')?;
    let name = rest.get(..end)?;
    let after = rest.get(end + 1..)?.trim_start();
    after.starts_with(':').then_some(name)
}

fn line_of_key(text: &str, key: &str) -> usize {
    let quoted = format!("\"{key}\"");
    text.lines()
        .position(|l| l.contains(&quoted))
        .map_or(1, |i| i + 1)
}

/// A JSON value read with every duplicate key refused (a reader that keeps
/// the last one must not be able to hide the first).
mod strict {
    use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

    pub struct Strict(pub serde_json::Value);

    impl<'de> Deserialize<'de> for Strict {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            d.deserialize_any(V).map(Strict)
        }
    }

    struct V;

    impl<'de> Visitor<'de> for V {
        type Value = serde_json::Value;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("JSON")
        }
        fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
            Ok(v.into())
        }
        fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
            Ok(v.into())
        }
        fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
            Ok(v.into())
        }
        fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E> {
            Ok(serde_json::Number::from_f64(v).map_or(serde_json::Value::Null, Into::into))
        }
        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
            Ok(v.into())
        }
        fn visit_string<E>(self, v: String) -> Result<Self::Value, E> {
            Ok(v.into())
        }
        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(serde_json::Value::Null)
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut items = Vec::new();
            while let Some(Strict(v)) = seq.next_element::<Strict>()? {
                items.push(v);
            }
            Ok(serde_json::Value::Array(items))
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut out = serde_json::Map::new();
            while let Some(key) = map.next_key::<String>()? {
                let Strict(value) = map.next_value::<Strict>()?;
                if out.contains_key(&key) {
                    return Err(serde::de::Error::custom("duplicate_key"));
                }
                out.insert(key, value);
            }
            Ok(serde_json::Value::Object(out))
        }
    }
}

/// One parsed document: `Err(kind)` when it cannot be read
/// (`duplicate_key` / `not_json`).
fn parse_strict(text: &str) -> Result<serde_json::Value, &'static str> {
    match serde_json::from_str::<strict::Strict>(text) {
        Ok(strict::Strict(v)) => Ok(v),
        Err(e) if e.to_string().contains("duplicate_key") => Err("duplicate_key"),
        Err(_) => Err("not_json"),
    }
}

/// Every key and decoded string of `value` (depth-bounded: deeper is
/// `not_json`, it cannot be shown free of raw data).
fn walk(
    value: &serde_json::Value,
    key: Option<&str>,
    text: &str,
    base_line: usize,
    depth: usize,
    out: &mut Vec<RawData>,
) {
    let line = |key: Option<&str>| key.map_or(base_line, |k| line_of_key(text, k).max(base_line));
    if depth > 64 {
        out.push(RawData {
            line: base_line,
            kind: "not_json",
        });
        return;
    }
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                for kind in key_kinds(k).into_iter().chain(scan_text(k)) {
                    out.push(RawData {
                        line: line(Some(k)),
                        kind,
                    });
                }
                walk(v, Some(k), text, base_line, depth + 1, out);
            }
        }
        serde_json::Value::Array(items) => {
            for v in items {
                walk(v, key, text, base_line, depth + 1, out);
            }
        }
        serde_json::Value::String(s) => {
            for kind in scan_text(s) {
                out.push(RawData {
                    line: line(key),
                    kind,
                });
            }
            let epoch_text = s.len() >= 10 && s.bytes().all(|c| c.is_ascii_digit());
            if epoch_text && key.is_some_and(date_like_key) {
                out.push(RawData {
                    line: line(key),
                    kind: "absolute_timestamp",
                });
            }
        }
        serde_json::Value::Number(n) => {
            let big = n.as_f64().is_some_and(|f| f.abs() >= EPOCH_MIN);
            if big && key.is_some_and(date_like_key) {
                out.push(RawData {
                    line: line(key),
                    kind: "absolute_timestamp",
                });
            }
        }
        serde_json::Value::Bool(_) | serde_json::Value::Null => {}
    }
}

/// The top-level checks of one case object.
fn case_object(value: &serde_json::Value, text: &str, base_line: usize, out: &mut Vec<RawData>) {
    let serde_json::Value::Object(map) = value else {
        out.push(RawData {
            line: base_line,
            kind: "not_json",
        });
        return;
    };
    let at = |k: &str| line_of_key(text, k).max(base_line);
    let provenance = map.get("provenance").and_then(serde_json::Value::as_str);
    if !matches!(provenance, Some("replica" | "synthetic")) {
        out.push(RawData {
            line: at("provenance"),
            kind: "provenance",
        });
    }
    let classification = map.get("classification").and_then(serde_json::Value::as_str);
    if classification.is_some_and(|c| c.eq_ignore_ascii_case("confidential")) {
        out.push(RawData {
            line: at("classification"),
            kind: "confidential",
        });
    }
    walk(value, None, text, base_line, 0, out);
}

fn is_jsonl(path: &str) -> bool {
    path.rsplit_once('.')
        .is_some_and(|(_, e)| e.eq_ignore_ascii_case("jsonl"))
}

/// The raw data in a case at `path` (plan 17 §4). The text is scanned line
/// by line, and the **decoded** JSON is walked (so `\u` escapes and `\/`
/// hide nothing): absolute dates (`-`, `/` or `.`) and times, epoch numbers
/// under date-like keys, IPv4 and IPv6 addresses, real plant codes
/// (anything but `P9xx`), a person's name or free text by key (any case),
/// a `provenance` other than `replica` / `synthetic`, `confidential`, a key
/// written twice. A `.jsonl` case is one object per line. A case that
/// cannot be read as JSON objects cannot be shown free of raw data
/// (`not_json` / `duplicate_key`): fail closed. Line order; each kind once
/// per line; never the matched value.
pub fn case_raw_data(path: &str, text: &str) -> Vec<RawData> {
    let mut out = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        for kind in scan_text(line) {
            out.push(RawData { line: number, kind });
        }
        let b = line.as_bytes();
        for (i, c) in b.iter().enumerate() {
            if *c == b'"' {
                if let Some(key) = key_at(line, i) {
                    for kind in key_kinds(key) {
                        out.push(RawData { line: number, kind });
                    }
                }
            }
        }
    }
    if is_jsonl(path) {
        let mut any = false;
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            any = true;
            match parse_strict(line) {
                Ok(v) => case_object(&v, line, index + 1, &mut out),
                Err(kind) => out.push(RawData {
                    line: index + 1,
                    kind,
                }),
            }
        }
        if !any {
            out.push(RawData {
                line: 1,
                kind: "not_json",
            });
        }
    } else {
        match parse_strict(text) {
            Ok(v) => case_object(&v, text, 1, &mut out),
            Err(kind) => out.push(RawData { line: 1, kind }),
        }
    }
    out.sort_by_key(|r| (r.line, r.kind));
    out.dedup();
    out
}

// ── reading ─────────────────────────────────────────────────────────────

fn text_of<'a>(file: &AssetFile<'a>, out: &mut Vec<Problem>) -> Option<&'a str> {
    if file.bytes.len() > MAX_FILE_BYTES {
        out.push(Problem::new(file.path, 0, "too_large", None));
        return None;
    }
    match std::str::from_utf8(file.bytes) {
        Ok(text) => Some(text),
        Err(_) => {
            out.push(Problem::new(file.path, 0, "not_utf8", None));
            None
        }
    }
}

fn toml_of(file: &AssetFile<'_>, text: &str, out: &mut Vec<Problem>) -> Option<toml::Table> {
    match text.parse::<toml::Table>() {
        Ok(table) => Some(table),
        Err(e) => {
            let line = e.span().map_or(0, |s| {
                text.get(..s.start).map_or(0, |t| t.lines().count().max(1))
            });
            out.push(Problem::new(file.path, line, "toml_syntax", None));
            None
        }
    }
}

fn stem(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.').map_or(name, |(s, _)| s)
}

/// A name: `[a-z0-9][a-z0-9_-]*` (PFS's `4m-correlator`), at most 64
/// characters.
fn name_ok(s: &str) -> bool {
    let mut chars = s.chars();
    s.len() <= 64
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// An indicator code (`A1`, `B12`): letters, digits and `_`.
fn indicator_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 20 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn date_ok(s: &str) -> bool {
    s.len() == 10 && date_at(s.as_bytes(), 0)
}

// ── profiles (conformance profiles, monotonic: forwarder < core < forms / full) ──

const PROFILES: &[&str] = &["forwarder", "core", "forms", "full"];

fn rank(p: &str) -> u8 {
    match p {
        "core" => 1,
        "forms" | "full" => 2,
        _ => 0,
    }
}

/// Whether an edge of profile `have` provides what `need` asks for
/// (`forms` and `full` are each above `core`, neither above the other).
fn satisfies(have: &str, need: &str) -> bool {
    have == need || rank(have) > rank(need)
}

// ── skills ──────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct Skill {
    profile: Option<String>,
    tools: Vec<String>,
    process_kind: Option<String>,
}

const SKILL_KEYS: &[&str] = &[
    "name",
    "description",
    "axis",
    "process_kind",
    "anomaly_class",
    "indicators",
    "required_tools",
    "required_profile",
    "reviewer",
    "version",
    "valid_from",
];
const SKILL_REQUIRED: &[&str] = &[
    "name",
    "description",
    "axis",
    "anomaly_class",
    "required_tools",
    "required_profile",
    "reviewer",
    "version",
    "valid_from",
];

fn yaml_text(node: &Node) -> Option<&str> {
    match &node.kind {
        Kind::Text(t) => Some(t.as_str()),
        Kind::Null | Kind::List(_) | Kind::Map(_) => None,
    }
}

/// A list of texts, or one text as a list of one (`None`: another shape).
fn yaml_list(node: &Node) -> Option<Vec<String>> {
    match &node.kind {
        Kind::Text(t) => Some(vec![t.clone()]), // clone: the list owns its items
        Kind::List(items) => items
            .iter()
            .map(|n| yaml_text(n).map(str::to_owned))
            .collect(),
        Kind::Null | Kind::Map(_) => None,
    }
}

fn skill(file: &AssetFile<'_>, text: &str, out: &mut Vec<Problem>) -> Option<(String, Skill)> {
    let p = file.path;
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        out.push(Problem::new(p, 1, "front_matter_missing", None));
        return None;
    }
    let body: Vec<&str> = lines
        .by_ref()
        .take_while(|l| l.trim_end() != "---")
        .collect();
    if body.len() + 1 >= text.lines().count() {
        out.push(Problem::new(p, 1, "front_matter_unclosed", None));
        return None;
    }
    let root = match read_tree(&body.join("\n")) {
        Ok(Some(root)) => root,
        Ok(None) => {
            out.push(Problem::new(p, 1, "front_matter_missing", None));
            return None;
        }
        Err(e) => {
            out.push(Problem::new(p, e.line + 1, e.reason, None));
            return None;
        }
    };
    let Kind::Map(entries) = &root.kind else {
        out.push(Problem::new(p, 2, "front_matter_not_a_mapping", None));
        return None;
    };
    let mut fields: BTreeMap<&str, &Node> = BTreeMap::new();
    for (key, value) in entries {
        let line = key.line + 1;
        let Some(name) = yaml_text(key) else {
            out.push(Problem::new(p, line, "key_not_text", None));
            continue;
        };
        if !SKILL_KEYS.contains(&name) {
            out.push(Problem::new(p, line, "key_unknown", Some(name)));
            continue;
        }
        fields.insert(name, value);
    }
    for key in SKILL_REQUIRED {
        if !fields.contains_key(key) {
            out.push(Problem::new(p, 1, "field_missing", Some(*key)));
        }
    }
    let at = |key: &str| fields.get(key).map_or(1, |n| n.line + 1);
    let text_field = |key: &str, out: &mut Vec<Problem>| -> Option<String> {
        let node = fields.get(key)?;
        match yaml_text(node) {
            Some(t) if !t.trim().is_empty() => Some(t.to_owned()),
            Some(_) | None => {
                out.push(Problem::new(p, node.line + 1, "field_invalid", Some(key)));
                None
            }
        }
    };
    let list_field = |key: &str, ok: fn(&str) -> bool, out: &mut Vec<Problem>| -> Vec<String> {
        let Some(node) = fields.get(key) else {
            return Vec::new();
        };
        match yaml_list(node) {
            Some(items) if items.iter().all(|i| ok(i)) => items,
            Some(_) | None => {
                out.push(Problem::new(p, node.line + 1, "field_invalid", Some(key)));
                Vec::new()
            }
        }
    };
    let name = text_field("name", out);
    if let Some(n) = &name {
        if !name_ok(n) {
            out.push(Problem::new(p, at("name"), "skill_name_invalid", Some(n)));
        } else if n != stem(p) {
            out.push(Problem::new(p, at("name"), "skill_name_mismatch", Some(n)));
        }
    }
    let _ = text_field("description", out);
    let _ = text_field("reviewer", out);
    let _ = text_field("version", out);
    let axis = text_field("axis", out);
    let process_kind = text_field("process_kind", out);
    match axis.as_deref() {
        Some("process" | "工程") if process_kind.is_none() => {
            out.push(Problem::new(p, at("axis"), "process_kind_missing", None));
        }
        Some("process" | "工程" | "cross" | "横断") | None => {}
        Some(other) => out.push(Problem::new(p, at("axis"), "axis_invalid", Some(other))),
    }
    if let Some(d) = text_field("valid_from", out) {
        if !date_ok(&d) {
            out.push(Problem::new(
                p,
                at("valid_from"),
                "date_invalid",
                Some("valid_from"),
            ));
        }
    }
    let classes = list_field("anomaly_class", name_ok, out);
    if fields.contains_key("anomaly_class") && classes.is_empty() {
        out.push(Problem::new(
            p,
            at("anomaly_class"),
            "field_invalid",
            Some("anomaly_class"),
        ));
    }
    let _indicators = list_field("indicators", indicator_ok, out);
    let tools = list_field("required_tools", name_ok, out);
    let profile = text_field("required_profile", out);
    if let Some(pr) = &profile {
        if !PROFILES.contains(&pr.as_str()) {
            out.push(Problem::new(
                p,
                at("required_profile"),
                "profile_invalid",
                Some(pr),
            ));
        }
    }
    let name = name.filter(|n| name_ok(n))?;
    Some((
        name,
        Skill {
            profile: profile.filter(|pr| PROFILES.contains(&pr.as_str())),
            tools,
            process_kind: process_kind
                .filter(|_| matches!(axis.as_deref(), Some("process" | "工程"))),
        },
    ))
}

// ── TOML assets ─────────────────────────────────────────────────────────

fn toml_str<'a>(t: &'a toml::Table, key: &str) -> Option<&'a str> {
    t.get(key).and_then(toml::Value::as_str)
}

fn toml_names(v: Option<&toml::Value>) -> Option<Vec<String>> {
    v?.as_array()?
        .iter()
        .map(|i| i.as_str().filter(|s| name_ok(s)).map(str::to_owned))
        .collect()
}

#[derive(Debug, Default)]
struct Agent {
    tools: Vec<String>,
    profile: Option<String>,
}

fn agent(
    file: &AssetFile<'_>,
    table: &toml::Table,
    out: &mut Vec<Problem>,
) -> Option<(String, Agent)> {
    let p = file.path;
    let Some(name) = toml_str(table, "name") else {
        out.push(Problem::new(p, 0, "field_missing", Some("name")));
        return None;
    };
    if !name_ok(name) || name != stem(p) {
        out.push(Problem::new(p, 0, "agent_name_mismatch", Some(name)));
        return None;
    }
    let tools = match toml_names(table.get("allowed_tools")) {
        Some(tools) => tools,
        None => {
            out.push(Problem::new(p, 0, "field_invalid", Some("allowed_tools")));
            Vec::new()
        }
    };
    let profile = match toml_str(table, "required_profile") {
        Some(pr) if PROFILES.contains(&pr) => Some(pr.to_owned()),
        Some(pr) => {
            out.push(Problem::new(p, 0, "profile_invalid", Some(pr)));
            None
        }
        None => None,
    };
    Some((name.to_owned(), Agent { tools, profile }))
}

fn tools(
    file: &AssetFile<'_>,
    table: &toml::Table,
    out: &mut Vec<Problem>,
) -> BTreeMap<String, Option<String>> {
    let p = file.path;
    let mut known = BTreeMap::new();
    for key in table.keys().filter(|k| k.as_str() != "tool") {
        out.push(Problem::new(p, 0, "key_unknown", Some(key)));
    }
    let Some(list) = table.get("tool").and_then(toml::Value::as_array) else {
        out.push(Problem::new(p, 0, "field_missing", Some("tool")));
        return known;
    };
    for (i, item) in list.iter().enumerate() {
        let at = format!("tool[{i}]");
        let Some(t) = item.as_table() else {
            out.push(Problem::new(p, 0, "field_invalid", Some(&at)));
            continue;
        };
        let Some(name) = toml_str(t, "name").filter(|n| name_ok(n)) else {
            out.push(Problem::new(
                p,
                0,
                "field_invalid",
                Some(&format!("{at}.name")),
            ));
            continue;
        };
        let profile = match toml_str(t, "required_profile") {
            Some(pr) if PROFILES.contains(&pr) => Some(pr.to_owned()),
            Some(pr) => {
                out.push(Problem::new(p, 0, "profile_invalid", Some(pr)));
                None
            }
            None => None,
        };
        if known.insert(name.to_owned(), profile).is_some() {
            out.push(Problem::new(p, 0, "tool_duplicate", Some(name)));
        }
    }
    known
}

/// `name:tag@sha256:<64 hex>`; the problem's rule otherwise.
pub fn model_ref_problem(raw: &str) -> Option<&'static str> {
    let (named, digest) = match raw.split_once('@') {
        Some((n, d)) => (n, Some(d)),
        None => (raw, None),
    };
    let tag = named.rsplit_once(':').map(|(_, t)| t);
    if named.is_empty() || named.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Some("model_ref_invalid");
    }
    if matches!(tag, None | Some("") | Some("latest")) {
        return Some("model_latest");
    }
    let Some(digest) = digest else {
        return Some("model_digest_missing");
    };
    let hex = digest.strip_prefix("sha256:").unwrap_or("");
    if hex.len() != 64
        || !hex
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    {
        return Some("model_digest_missing");
    }
    None
}

fn models(file: &AssetFile<'_>, table: &toml::Table, out: &mut Vec<Problem>) {
    let p = file.path;
    if table.is_empty() {
        out.push(Problem::new(p, 0, "models_empty", None));
    }
    for (band, value) in table {
        let Some(raw) = value.as_str().filter(|_| name_ok(band)) else {
            out.push(Problem::new(p, 0, "field_invalid", Some(band)));
            continue;
        };
        if let Some(rule) = model_ref_problem(raw) {
            out.push(Problem::new(p, 0, rule, Some(band)));
        }
    }
}

/// Every text under a key ending in `image` must be pinned by digest.
fn bench(file: &AssetFile<'_>, table: &toml::Table, out: &mut Vec<Problem>) {
    fn walk(prefix: &str, value: &toml::Value, path: &str, out: &mut Vec<Problem>, depth: usize) {
        if depth > 8 {
            return;
        }
        match value {
            toml::Value::Table(t) => {
                for (k, v) in t {
                    walk(&format!("{prefix}{k}."), v, path, out, depth + 1);
                }
            }
            toml::Value::Array(items) => {
                for v in items {
                    walk(prefix, v, path, out, depth + 1);
                }
            }
            toml::Value::String(s) if prefix.trim_end_matches('.').ends_with("image") => {
                if model_ref_problem(s).is_some() {
                    out.push(Problem::new(
                        path,
                        0,
                        "image_not_pinned",
                        Some(prefix.trim_end_matches('.')),
                    ));
                }
            }
            toml::Value::String(_)
            | toml::Value::Integer(_)
            | toml::Value::Float(_)
            | toml::Value::Boolean(_)
            | toml::Value::Datetime(_) => {}
        }
    }
    for (k, v) in table {
        walk(&format!("{k}."), v, file.path, out, 0);
    }
}

// ── playbooks ───────────────────────────────────────────────────────────

const PLAYBOOK_KEYS: &[&str] = &[
    "indicator_code",
    "plant_code",
    "scope_type",
    "scope_key",
    "severity_filter",
    "valid_from",
    "valid_to",
    "priority",
    "agent_name",
    "skill_refs",
    "tool_sequence",
    "max_tool_calls",
    "max_seconds",
    "required_profile",
    "is_active",
    "note",
];
const SCOPE_TYPES: &[&str] = &[
    "global",
    "plant",
    "process_kind",
    "workctr",
    "work_station",
    "machine",
    "item",
];
const SEVERITIES: &[&str] = &["critical", "warning", "info", "*"];

#[derive(Debug)]
struct Playbook {
    at: String,
    path: String,
    scope_type: String,
    scope_key: String,
    agent: String,
    skills: Vec<String>,
    tools: Vec<String>,
    profile: Option<String>,
}

fn playbooks(file: &AssetFile<'_>, table: &toml::Table, out: &mut Vec<Problem>) -> Vec<Playbook> {
    let p = file.path;
    for key in table.keys().filter(|k| k.as_str() != "playbook") {
        out.push(Problem::new(p, 0, "key_unknown", Some(key)));
    }
    let Some(rows) = table.get("playbook").and_then(toml::Value::as_array) else {
        out.push(Problem::new(p, 0, "field_missing", Some("playbook")));
        return Vec::new();
    };
    let mut found = Vec::new();
    let mut keys = BTreeSet::new();
    for (i, row) in rows.iter().enumerate() {
        let at = format!("playbook[{i}]");
        let Some(t) = row.as_table() else {
            out.push(Problem::new(p, 0, "field_invalid", Some(&at)));
            continue;
        };
        for key in t.keys().filter(|k| !PLAYBOOK_KEYS.contains(&k.as_str())) {
            out.push(Problem::new(
                p,
                0,
                "key_unknown",
                Some(&format!("{at}.{key}")),
            ));
        }
        let need = |key: &str, out: &mut Vec<Problem>| -> Option<String> {
            match toml_str(t, key) {
                Some(s) if !s.trim().is_empty() => Some(s.to_owned()),
                Some(_) => {
                    out.push(Problem::new(
                        p,
                        0,
                        "field_invalid",
                        Some(&format!("{at}.{key}")),
                    ));
                    None
                }
                None => {
                    out.push(Problem::new(
                        p,
                        0,
                        "field_missing",
                        Some(&format!("{at}.{key}")),
                    ));
                    None
                }
            }
        };
        let indicator = need("indicator_code", out);
        let scope_type = need("scope_type", out);
        let scope_key = need("scope_key", out);
        let severity = need("severity_filter", out);
        let valid_from = need("valid_from", out);
        let agent = need("agent_name", out);
        if let Some(s) = scope_type.as_deref().filter(|s| !SCOPE_TYPES.contains(s)) {
            out.push(Problem::new(p, 0, "scope_type_invalid", Some(s)));
        }
        if let Some(s) = severity.as_deref().filter(|s| !SEVERITIES.contains(s)) {
            out.push(Problem::new(p, 0, "severity_invalid", Some(s)));
        }
        if let Some(d) = valid_from.as_deref().filter(|d| !date_ok(d)) {
            out.push(Problem::new(p, 0, "date_invalid", Some(d)));
        }
        if !t.get("priority").is_some_and(toml::Value::is_integer) {
            out.push(Problem::new(
                p,
                0,
                "field_missing",
                Some(&format!("{at}.priority")),
            ));
        }
        for key in ["max_tool_calls", "max_seconds"] {
            if let Some(v) = t.get(key) {
                if !v.as_integer().is_some_and(|n| n > 0) {
                    out.push(Problem::new(
                        p,
                        0,
                        "field_invalid",
                        Some(&format!("{at}.{key}")),
                    ));
                }
            }
        }
        let skills = toml_names(t.get("skill_refs")).filter(|s| !s.is_empty());
        if skills.is_none() {
            out.push(Problem::new(
                p,
                0,
                "field_invalid",
                Some(&format!("{at}.skill_refs")),
            ));
        }
        let tools = toml_names(t.get("tool_sequence"));
        if tools.is_none() {
            out.push(Problem::new(
                p,
                0,
                "field_invalid",
                Some(&format!("{at}.tool_sequence")),
            ));
        }
        let profile = match toml_str(t, "required_profile") {
            Some(pr) if PROFILES.contains(&pr) => Some(pr.to_owned()),
            Some(pr) => {
                out.push(Problem::new(p, 0, "profile_invalid", Some(pr)));
                None
            }
            None => None,
        };
        let unique = (
            indicator.clone(), // clone: the key set owns its parts
            toml_str(t, "plant_code").map(str::to_owned),
            scope_type.clone(), // clone: as above
            scope_key.clone(),  // clone: as above
            severity,
            valid_from,
        );
        if !keys.insert(unique) {
            out.push(Problem::new(p, 0, "playbook_duplicate", Some(&at)));
        }
        if let (Some(scope_type), Some(scope_key), Some(agent)) = (scope_type, scope_key, agent) {
            found.push(Playbook {
                at,
                path: p.to_owned(),
                scope_type,
                scope_key,
                agent,
                skills: skills.unwrap_or_default(),
                tools: tools.unwrap_or_default(),
                profile,
            });
        }
    }
    found
}

// ── cases ───────────────────────────────────────────────────────────────

const CASE_KINDS: &[&str] = &["regular", "safety", "reproduction"];

fn case_id_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn case(file: &AssetFile<'_>, text: &str, out: &mut Vec<Problem>) {
    let p = file.path;
    for raw in case_raw_data(p, text) {
        out.push(Problem::new(p, raw.line, "case_raw_data", Some(raw.kind)));
    }
    let parts: Vec<&str> = p.split('/').collect();
    let json = p.rsplit_once('.').is_some_and(|(_, e)| e == "json");
    let dir = match parts.as_slice() {
        ["evals", "cases", dir, _] if json => *dir,
        _ => {
            out.push(Problem::new(p, 0, "case_path_invalid", None));
            return;
        }
    };
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(text) else {
        return; // `case_raw_data not_json` / `duplicate_key` above
    };
    let s = |key: &str| map.get(key).and_then(serde_json::Value::as_str);
    match s("id") {
        Some(id) if case_id_ok(id) && id == stem(p) => {}
        Some(id) => out.push(Problem::new(
            p,
            line_of_key(text, "id"),
            "case_id_mismatch",
            Some(id),
        )),
        None => out.push(Problem::new(p, 1, "field_missing", Some("id"))),
    }
    match s("anomaly_class") {
        Some(c) if dir == "security" || c == dir => {}
        Some(c) => out.push(Problem::new(
            p,
            line_of_key(text, "anomaly_class"),
            "case_class_mismatch",
            Some(c),
        )),
        None => out.push(Problem::new(p, 1, "field_missing", Some("anomaly_class"))),
    }
    match s("kind") {
        Some("safety") => {}
        Some(k) if CASE_KINDS.contains(&k) && dir == "security" => {
            out.push(Problem::new(
                p,
                line_of_key(text, "kind"),
                "case_kind_mismatch",
                Some(k),
            ));
        }
        Some(k) if CASE_KINDS.contains(&k) => {}
        Some(k) => out.push(Problem::new(
            p,
            line_of_key(text, "kind"),
            "case_kind_invalid",
            Some(k),
        )),
        None => out.push(Problem::new(p, 1, "field_missing", Some("kind"))),
    }
    if s("provenance") == Some("replica") {
        let ok = s("source_hash").is_some_and(|h| {
            h.len() == 64
                && h.chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        });
        if !ok {
            out.push(Problem::new(
                p,
                line_of_key(text, "provenance"),
                "source_hash_invalid",
                None,
            ));
        }
    }
    match s("classification") {
        None | Some("internal" | "public") => {}
        Some("confidential") => {} // `case_raw_data confidential` above
        Some(other) => out.push(Problem::new(
            p,
            line_of_key(text, "classification"),
            "classification_invalid",
            Some(other),
        )),
    }
    if !map
        .get("expected")
        .is_some_and(serde_json::Value::is_object)
    {
        out.push(Problem::new(p, 1, "field_missing", Some("expected")));
    }
    if let Some(v) = map.get("expected_fail") {
        if !v.is_boolean() {
            out.push(Problem::new(
                p,
                line_of_key(text, "expected_fail"),
                "field_invalid",
                Some("expected_fail"),
            ));
        }
    }
}

// ── the lint ────────────────────────────────────────────────────────────

/// The lint of the asset files of a repository (module docs). Every other
/// file is ignored. Problems sorted by path, line, rule; at most
/// [`MAX_PROBLEMS`] (then one `too_many_problems`).
pub fn lint(files: &[AssetFile<'_>]) -> Vec<Problem> {
    let mut out = Vec::new();
    let mut skills: BTreeMap<String, Skill> = BTreeMap::new();
    let mut agents: BTreeMap<String, Agent> = BTreeMap::new();
    let mut agents_dir = false;
    let mut known_tools: Option<BTreeMap<String, Option<String>>> = None;
    let mut books: Vec<Playbook> = Vec::new();
    for file in files {
        let Some(kind) = kind_of(file.path) else {
            continue;
        };
        if kind == AssetKind::Threshold {
            out.push(Problem::new(file.path, 0, "threshold_not_proposal", None));
            continue;
        }
        let Some(text) = text_of(file, &mut out) else {
            continue;
        };
        match kind {
            AssetKind::Skill => {
                if let Some((name, s)) = skill(file, text, &mut out) {
                    skills.insert(name, s);
                }
            }
            AssetKind::Case => case(file, text, &mut out),
            AssetKind::Playbook
            | AssetKind::Agent
            | AssetKind::Tools
            | AssetKind::Models
            | AssetKind::Bench => {
                let Some(table) = toml_of(file, text, &mut out) else {
                    continue;
                };
                match kind {
                    AssetKind::Playbook => books.extend(playbooks(file, &table, &mut out)),
                    AssetKind::Agent => {
                        agents_dir = true;
                        if let Some((name, a)) = agent(file, &table, &mut out) {
                            agents.insert(name, a);
                        }
                    }
                    AssetKind::Tools => known_tools = Some(tools(file, &table, &mut out)),
                    AssetKind::Models => models(file, &table, &mut out),
                    AssetKind::Bench => bench(file, &table, &mut out),
                    AssetKind::Skill
                    | AssetKind::Case
                    | AssetKind::Baseline
                    | AssetKind::ThresholdProposal
                    | AssetKind::Threshold
                    | AssetKind::Owners => {}
                }
            }
            // The baseline's shape is the `forge/ai-eval` worker's
            // (ADR-0034); proposals and CODEOWNERS are free text here.
            AssetKind::Baseline
            | AssetKind::ThresholdProposal
            | AssetKind::Threshold
            | AssetKind::Owners => {}
        }
    }
    let tool_profile = |name: &str| tool_profile_of(known_tools.as_ref(), name);
    for (name, s) in &skills {
        for tool in &s.tools {
            if tool_profile(tool) == Some(None) {
                out.push(Problem::new(
                    &format!("skills/{name}.md"),
                    1,
                    "tool_unknown",
                    Some(tool),
                ));
            }
        }
    }
    for b in &books {
        let p = b.path.as_str();
        let edge = b.profile.as_deref().unwrap_or("forwarder");
        let agent = agents.get(&b.agent);
        if agents_dir && agent.is_none() {
            out.push(Problem::new(
                p,
                0,
                "agent_unknown",
                Some(&format!("{}.agent_name: {}", b.at, b.agent)),
            ));
        }
        if let Some(need) = agent.and_then(|a| a.profile.as_deref()) {
            if !satisfies(edge, need) {
                out.push(Problem::new(
                    p,
                    0,
                    "profile_too_low",
                    Some(&format!("{}: agent {}", b.at, b.agent)),
                ));
            }
        }
        for tool in &b.tools {
            match tool_profile(tool) {
                Some(None) => out.push(Problem::new(
                    p,
                    0,
                    "tool_unknown",
                    Some(&format!("{}: {tool}", b.at)),
                )),
                Some(Some(need)) if !satisfies(edge, need) => {
                    out.push(Problem::new(
                        p,
                        0,
                        "profile_too_low",
                        Some(&format!("{}: tool {tool}", b.at)),
                    ));
                }
                Some(Some(_)) | None => {}
            }
            if agent.is_some_and(|a| !a.tools.contains(tool)) {
                out.push(Problem::new(
                    p,
                    0,
                    "tool_not_allowed",
                    Some(&format!("{}: {tool}", b.at)),
                ));
            }
        }
        for name in &b.skills {
            let Some(s) = skills.get(name) else {
                out.push(Problem::new(
                    p,
                    0,
                    "skill_unknown",
                    Some(&format!("{}.skill_refs: {name}", b.at)),
                ));
                continue;
            };
            if let Some(need) = s.profile.as_deref() {
                if !satisfies(edge, need) {
                    out.push(Problem::new(
                        p,
                        0,
                        "profile_too_low",
                        Some(&format!("{}: skill {name}", b.at)),
                    ));
                }
            }
            if let Some(kind) = &s.process_kind {
                if b.scope_type != "process_kind" || &b.scope_key != kind {
                    out.push(Problem::new(
                        p,
                        0,
                        "skill_process_mismatch",
                        Some(&format!("{}: skill {name}", b.at)),
                    ));
                }
            }
            if let Some(a) = agent {
                for tool in s.tools.iter().filter(|t| !a.tools.contains(*t)) {
                    out.push(Problem::new(
                        p,
                        0,
                        "tool_not_allowed",
                        Some(&format!("{}: skill {name} needs {tool}", b.at)),
                    ));
                }
            }
        }
    }
    out.sort();
    out.dedup();
    if out.len() > MAX_PROBLEMS {
        out.truncate(MAX_PROBLEMS);
        out.push(Problem::new("", 0, "too_many_problems", None));
    }
    out
}

/// A tool's profile in the registry: `None` without a registry,
/// `Some(None)` for a tool it does not list (a tool without one is
/// `forwarder`).
fn tool_profile_of<'a>(
    known: Option<&'a BTreeMap<String, Option<String>>>,
    name: &str,
) -> Option<Option<&'a str>> {
    known.map(|t| t.get(name).map(|p| p.as_deref().unwrap_or("forwarder")))
}

/// The problems of a release's file list (plan 17 §1.3, §1.8): threshold
/// proposals are a person's decision in PFS and never ship in a bundle,
/// and the manifest is the server's to write.
pub fn release_problems(paths: &[&str]) -> Vec<Problem> {
    let mut out: Vec<Problem> = paths
        .iter()
        .filter_map(|path| {
            let first = path.split('/').next().unwrap_or("");
            match first {
                "thresholds" => Some(Problem::new(path, 0, "release_includes_thresholds", None)),
                "manifest" => Some(Problem::new(path, 0, "release_includes_manifest", None)),
                _ => None,
            }
        })
        .collect();
    out.sort();
    out
}

#[cfg(test)]
#[path = "ai_asset_tests.rs"]
mod tests;
