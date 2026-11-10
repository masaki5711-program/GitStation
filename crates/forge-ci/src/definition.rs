//! The schema of `.forge-ci.yml` (version 1) and its reading. Every key is
//! known; an unknown key is a problem (`key_unknown`), never ignored. The
//! YAML is forge-doc's strict tree: anchors, aliases, tags, a second
//! document and duplicate keys (so duplicate job names) are refused, and
//! nesting is bounded.

use std::collections::HashSet;

use forge_doc::yaml::{self, Kind, Node};

use crate::Problem;

/// The largest file read (`too_large`).
pub const MAX_BYTES: usize = 64 * 1024;
/// At most this many stages (`too_many_stages`).
pub const MAX_STAGES: usize = 16;
/// At most this many jobs (`too_many_jobs`).
pub const MAX_JOBS: usize = 48;
/// At most this many `needs` in one job (`too_many_needs`).
pub const MAX_NEEDS: usize = 16;
/// At most this many script lines in one job (`too_many_script_lines`).
pub const MAX_SCRIPT_LINES: usize = 200;
/// At most this many runner tags in one job (`too_many_tags`).
pub const MAX_TAGS: usize = 8;
/// At most this many secrets in one job (`too_many_secrets`).
pub const MAX_SECRETS: usize = 16;
/// At most this many artifact paths in one job (`too_many_artifacts`).
pub const MAX_ARTIFACT_PATHS: usize = 16;
/// At most this many branch patterns in one job's rules (`too_many_branches`).
pub const MAX_BRANCHES: usize = 16;
/// The longest `timeout` a job may ask for: 4 hours (`timeout_too_long`).
pub const MAX_TIMEOUT_SECS: u32 = 4 * 60 * 60;
/// The longest name (job, stage, tag, secret) and pattern / path.
const MAX_NAME: usize = 63;
const MAX_PATH: usize = 255;

/// A valid pipeline definition (only [`crate::check`] builds one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    pub stages: Vec<String>,
    /// In file order.
    pub jobs: Vec<Job>,
}

/// One job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub name: String,
    /// The line of the job's key.
    pub line: usize,
    pub stage: String,
    /// The position of `stage` in [`Definition::stages`].
    pub stage_index: usize,
    pub needs: Vec<String>,
    /// `<name>@sha256:<64 hex>`.
    pub image: String,
    /// Text for the runner; never interpreted here.
    pub script: Vec<String>,
    pub timeout_secs: u32,
    pub tags: Vec<String>,
    /// Names only; values live on the server (C6).
    pub secrets: Vec<String>,
    pub rules: Rules,
    pub artifacts: Vec<String>,
}

/// When a job runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rules {
    /// Branch patterns for a push: exact names, or a prefix ending in `*`
    /// (`*` alone: every branch). Default `["*"]`.
    pub branches: Vec<String>,
    /// Whether the job runs in a merge request's pipeline. Default `true`.
    pub merge_requests: bool,
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            branches: vec!["*".to_owned()],
            merge_requests: true,
        }
    }
}

impl Rules {
    /// Whether a push to `branch` runs the job.
    pub fn matches_branch(&self, branch: &str) -> bool {
        self.branches.iter().any(|p| pattern_matches(p, branch))
    }
}

/// `pattern` (exact, or a prefix ending in `*`) matches `name`.
pub(crate) fn pattern_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

fn yaml_problem(p: yaml::YamlProblem) -> Problem {
    Problem::new(p.line, p.reason)
}

/// Read `bytes`. `None` when the file cannot become a definition; the
/// problems found are pushed (several per file when they are independent).
pub(crate) fn parse(bytes: &[u8], problems: &mut Vec<Problem>) -> Option<Definition> {
    if bytes.len() > MAX_BYTES {
        problems.push(Problem::new(0, "too_large"));
        return None;
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        problems.push(Problem::new(0, "not_utf8"));
        return None;
    };
    let root = match yaml::read_tree(text) {
        Ok(Some(root)) => root,
        Ok(None) => {
            problems.push(Problem::new(0, "empty_file"));
            return None;
        }
        Err(p) => {
            problems.push(yaml_problem(p));
            return None;
        }
    };
    let fields = match yaml::mapping(&root, "not_a_mapping") {
        Ok(f) => f,
        Err(p) => {
            problems.push(yaml_problem(p));
            return None;
        }
    };
    let (mut version, mut stages, mut jobs) = (None, None, None);
    for f in &fields {
        match f.key {
            "version" => version = Some(f),
            "stages" => stages = Some(f),
            "jobs" => jobs = Some(f),
            other => problems.push(Problem::about(f.line, "key_unknown", other)),
        }
    }
    let before = problems.len();
    match version {
        None => problems.push(Problem::about(root.line, "field_missing", "version")),
        Some(f) => match &f.value.kind {
            Kind::Text(v) if v == "1" => {}
            Kind::Text(_) | Kind::Null | Kind::List(_) | Kind::Map(_) => {
                problems.push(Problem::about(f.line, "version_unsupported", "version"))
            }
        },
    }
    let stages = match stages {
        None => {
            problems.push(Problem::about(root.line, "field_missing", "stages"));
            None
        }
        Some(f) => read_stages(f.value, f.line, problems),
    };
    let jobs = match jobs {
        None => {
            problems.push(Problem::about(root.line, "field_missing", "jobs"));
            None
        }
        Some(f) => read_jobs(f.value, f.line, stages.as_deref(), problems),
    };
    match (stages, jobs) {
        (Some(stages), Some(jobs)) if problems.len() == before => Some(Definition { stages, jobs }),
        _ => None,
    }
}

fn read_stages(node: &Node, at: usize, problems: &mut Vec<Problem>) -> Option<Vec<String>> {
    let items = texts(node, at, "stages", problems)?;
    if items.is_empty() {
        problems.push(Problem::new(at, "stages_empty"));
        return None;
    }
    if items.len() > MAX_STAGES {
        problems.push(Problem::new(at, "too_many_stages"));
        return None;
    }
    let mut ok = true;
    let mut seen = HashSet::new();
    for (line, name) in &items {
        if !is_name(name) {
            problems.push(Problem::about(*line, "stage_name_invalid", name.as_str()));
            ok = false;
        } else if !seen.insert(name.as_str()) {
            problems.push(Problem::about(*line, "stage_duplicate", name.as_str()));
            ok = false;
        }
    }
    ok.then(|| items.into_iter().map(|(_, n)| n).collect())
}

fn read_jobs(
    node: &Node,
    at: usize,
    stages: Option<&[String]>,
    problems: &mut Vec<Problem>,
) -> Option<Vec<Job>> {
    let entries = match yaml::mapping(node, "field_invalid") {
        Ok(e) => e,
        Err(p) if p.reason == "field_invalid" => {
            problems.push(Problem::about(p.line, "field_invalid", "jobs"));
            return None;
        }
        Err(p) => {
            problems.push(yaml_problem(p));
            return None;
        }
    };
    if entries.is_empty() {
        problems.push(Problem::new(at, "jobs_empty"));
        return None;
    }
    if entries.len() > MAX_JOBS {
        problems.push(Problem::new(at, "too_many_jobs"));
        return None;
    }
    let mut jobs = Vec::with_capacity(entries.len());
    let mut ok = true;
    for e in &entries {
        if !is_name(e.key) {
            problems.push(Problem::about(e.line, "job_name_invalid", e.key));
            ok = false;
            continue;
        }
        match read_job(e.key, e.line, e.value, stages, problems) {
            Some(job) => jobs.push(job),
            None => ok = false,
        }
    }
    ok.then_some(jobs)
}

#[allow(clippy::too_many_lines)] // one flat pass over the job's keys
fn read_job(
    name: &str,
    at: usize,
    node: &Node,
    stages: Option<&[String]>,
    problems: &mut Vec<Problem>,
) -> Option<Job> {
    let fields = match yaml::mapping(node, "field_invalid") {
        Ok(f) => f,
        Err(p) => {
            problems.push(Problem::about(p.line, p.reason, name));
            return None;
        }
    };
    let before = problems.len();
    let mut stage = None;
    let mut needs = Vec::new();
    let mut image = None;
    let mut script = None;
    let mut timeout = None;
    let mut tags = Vec::new();
    let mut secrets = Vec::new();
    let mut rules = Rules::default();
    let mut artifacts = Vec::new();
    for f in &fields {
        let (line, value) = (f.line, f.value);
        match f.key {
            "stage" => {
                if let Some(s) = one_text(value, line, "stage", problems) {
                    stage = Some((line, s));
                }
            }
            "needs" => {
                if let Some(items) = texts(value, line, "needs", problems) {
                    if items.len() > MAX_NEEDS {
                        problems.push(Problem::about(line, "too_many_needs", name));
                    }
                    needs = items;
                }
            }
            "image" => {
                if let Some(s) = one_text(value, line, "image", problems) {
                    if is_pinned_image(&s) {
                        image = Some(s);
                    } else {
                        problems.push(Problem::about(line, "image_not_pinned", name));
                    }
                }
            }
            "script" => script = read_script(value, line, name, problems),
            "timeout" => {
                if let Some(s) = one_text(value, line, "timeout", problems) {
                    match parse_duration(&s) {
                        None => problems.push(Problem::about(line, "timeout_invalid", s)),
                        Some(secs) if secs > MAX_TIMEOUT_SECS => {
                            problems.push(Problem::about(line, "timeout_too_long", s));
                        }
                        Some(secs) => timeout = Some(secs),
                    }
                }
            }
            "tags" => {
                tags = bounded_names(
                    value,
                    line,
                    "tags",
                    MAX_TAGS,
                    "too_many_tags",
                    problems,
                    |t| (!is_name(t)).then_some("tag_invalid"),
                )
            }
            "secrets" => {
                secrets = bounded_names(
                    value,
                    line,
                    "secrets",
                    MAX_SECRETS,
                    "too_many_secrets",
                    problems,
                    |s| (!is_secret_name(s)).then_some("secret_name_invalid"),
                )
            }
            "rules" => {
                if let Some(r) = read_rules(value, line, problems) {
                    rules = r;
                }
            }
            "artifacts" => artifacts = read_artifacts(value, line, problems),
            other => problems.push(Problem::about(line, "key_unknown", other)),
        }
    }
    let stage_index = match &stage {
        None => {
            problems.push(Problem::about(at, "field_missing", "stage"));
            None
        }
        Some((line, s)) => match stages {
            // The stages themselves are broken: reported there.
            None => None,
            Some(list) => {
                let i = list.iter().position(|x| x == s);
                if i.is_none() {
                    problems.push(Problem::about(*line, "stage_unknown", s.as_str()));
                }
                i
            }
        },
    };
    if image.is_none() && !fields.iter().any(|f| f.key == "image") {
        problems.push(Problem::about(at, "field_missing", "image"));
    }
    if !fields.iter().any(|f| f.key == "script") {
        problems.push(Problem::about(at, "script_empty", name));
    }
    if !fields.iter().any(|f| f.key == "timeout") {
        problems.push(Problem::about(at, "timeout_missing", name));
    }
    if problems.len() != before {
        return None;
    }
    Some(Job {
        name: name.to_owned(),
        line: at,
        stage: stage?.1,
        stage_index: stage_index?,
        needs: needs.into_iter().map(|(_, n)| n).collect(),
        image: image?,
        script: script?,
        timeout_secs: timeout?,
        tags,
        secrets,
        rules,
        artifacts,
    })
}

fn read_script(
    node: &Node,
    at: usize,
    job: &str,
    problems: &mut Vec<Problem>,
) -> Option<Vec<String>> {
    let items = texts(node, at, "script", problems)?;
    if items.is_empty() {
        problems.push(Problem::about(at, "script_empty", job));
        return None;
    }
    if items.len() > MAX_SCRIPT_LINES {
        problems.push(Problem::about(at, "too_many_script_lines", job));
        return None;
    }
    if let Some((line, _)) = items.iter().find(|(_, s)| s.trim().is_empty()) {
        problems.push(Problem::about(*line, "script_empty", job));
        return None;
    }
    Some(items.into_iter().map(|(_, s)| s).collect())
}

fn read_rules(node: &Node, at: usize, problems: &mut Vec<Problem>) -> Option<Rules> {
    let fields = match yaml::mapping(node, "field_invalid") {
        Ok(f) => f,
        Err(p) => {
            problems.push(Problem::about(p.line.max(at), p.reason, "rules"));
            return None;
        }
    };
    let mut rules = Rules::default();
    let before = problems.len();
    for f in &fields {
        match f.key {
            "branches" => {
                rules.branches = bounded_names(
                    f.value,
                    f.line,
                    "branches",
                    MAX_BRANCHES,
                    "too_many_branches",
                    problems,
                    |b| (!is_branch_pattern(b)).then_some("branch_pattern_invalid"),
                );
            }
            "merge_requests" => match &f.value.kind {
                Kind::Text(v) if v == "true" => rules.merge_requests = true,
                Kind::Text(v) if v == "false" => rules.merge_requests = false,
                Kind::Text(_) | Kind::Null | Kind::List(_) | Kind::Map(_) => {
                    problems.push(Problem::about(f.line, "field_invalid", "merge_requests"))
                }
            },
            other => problems.push(Problem::about(f.line, "key_unknown", other)),
        }
    }
    (problems.len() == before).then_some(rules)
}

fn read_artifacts(node: &Node, at: usize, problems: &mut Vec<Problem>) -> Vec<String> {
    let fields = match yaml::mapping(node, "field_invalid") {
        Ok(f) => f,
        Err(p) => {
            problems.push(Problem::about(p.line.max(at), p.reason, "artifacts"));
            return Vec::new();
        }
    };
    let mut paths = Vec::new();
    for f in &fields {
        match f.key {
            "paths" => {
                paths = bounded_names(
                    f.value,
                    f.line,
                    "paths",
                    MAX_ARTIFACT_PATHS,
                    "too_many_artifacts",
                    problems,
                    |p| (!is_artifact_path(p)).then_some("artifact_path_invalid"),
                );
            }
            other => problems.push(Problem::about(f.line, "key_unknown", other)),
        }
    }
    paths
}

/// A list of names, each checked by `bad` (its reason, or `None`), at most
/// `max` of them, without duplicates (`<field>_duplicate` is folded into
/// `field_invalid`).
fn bounded_names(
    node: &Node,
    at: usize,
    field: &'static str,
    max: usize,
    too_many: &'static str,
    problems: &mut Vec<Problem>,
    bad: impl Fn(&str) -> Option<&'static str>,
) -> Vec<String> {
    let Some(items) = texts(node, at, field, problems) else {
        return Vec::new();
    };
    if items.len() > max {
        problems.push(Problem::about(at, too_many, field));
        return Vec::new();
    }
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(items.len());
    for (line, item) in items {
        if let Some(reason) = bad(&item) {
            problems.push(Problem::about(line, reason, item));
        } else if !seen.insert(item.clone()) {
            // clone: the set owns its keys
            problems.push(Problem::about(line, "field_invalid", item));
        } else {
            out.push(item);
        }
    }
    out
}

/// A list of text values with their lines. An empty (`null`) value is an
/// empty list.
fn texts(
    node: &Node,
    at: usize,
    field: &'static str,
    problems: &mut Vec<Problem>,
) -> Option<Vec<(usize, String)>> {
    match &node.kind {
        Kind::Null => Some(Vec::new()),
        Kind::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match &item.kind {
                    Kind::Text(t) => out.push((item.line, t.clone())),
                    Kind::Null | Kind::List(_) | Kind::Map(_) => {
                        problems.push(Problem::about(item.line, "field_invalid", field));
                        return None;
                    }
                }
            }
            Some(out)
        }
        Kind::Text(_) | Kind::Map(_) => {
            problems.push(Problem::about(node.line.max(at), "field_invalid", field));
            None
        }
    }
}

fn one_text(
    node: &Node,
    at: usize,
    field: &'static str,
    problems: &mut Vec<Problem>,
) -> Option<String> {
    match yaml::text(node, at, "field_missing") {
        Ok(t) if !t.trim().is_empty() => Some(t.to_owned()),
        Ok(_) => {
            problems.push(Problem::about(at, "field_missing", field));
            None
        }
        Err(p) if p.reason == "field_missing" => {
            problems.push(Problem::about(p.line, "field_missing", field));
            None
        }
        Err(p) => {
            problems.push(Problem::about(p.line, "field_invalid", field));
            None
        }
    }
}

/// Job, stage and tag names: `[a-z0-9][a-z0-9._-]*`, at most 63 bytes.
fn is_name(s: &str) -> bool {
    let mut chars = s.chars();
    s.len() <= MAX_NAME
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))
}

/// Secret names: `[A-Z][A-Z0-9_]*`, at most 63 bytes.
fn is_secret_name(s: &str) -> bool {
    let mut chars = s.chars();
    s.len() <= MAX_NAME
        && chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// `<name>@sha256:<64 lowercase hex>`; the name has no space or control
/// character.
fn is_pinned_image(s: &str) -> bool {
    let Some((name, digest)) = s.split_once("@sha256:") else {
        return false;
    };
    !name.is_empty()
        && name.len() <= MAX_PATH
        && !name
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '@')
        && digest.len() == 64
        && digest
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// `<n>s`, `<n>m` or `<n>h`, `n` ≥ 1 (decimal digits).
pub(crate) fn parse_duration(s: &str) -> Option<u32> {
    let unit = match s.chars().last()? {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        _ => return None,
    };
    let digits = s.get(..s.len() - 1)?;
    if digits.is_empty() || digits.len() > 6 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let n: u32 = digits.parse().ok()?;
    if n == 0 {
        return None;
    }
    n.checked_mul(unit)
}

/// A branch pattern: an exact name, or a prefix ending in `*` (`*` alone is
/// every branch). No space, control character, `..`, leading `-`, or `*`
/// elsewhere.
fn is_branch_pattern(s: &str) -> bool {
    let body = s.strip_suffix('*').unwrap_or(s);
    !s.is_empty()
        && s.len() <= MAX_PATH
        && !s.starts_with('-')
        && !s.contains("..")
        && !body.contains('*')
        && !s
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "~^:?[\\".contains(c))
}

/// A relative path inside the work tree: no leading `/`, no `\`, no drive,
/// no `.` / `..` component, no control character.
fn is_artifact_path(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_PATH
        && !s.starts_with('/')
        && !s.contains('\\')
        && !s.contains(':')
        && !s.chars().any(char::is_control)
        && s.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
}
