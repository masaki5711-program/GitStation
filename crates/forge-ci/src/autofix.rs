//! `autofix.fixers` of `.forge-ci.yml` (M4 C14, ADR-0017 §1 ① and its
//! Amendment C14, plan 04 C-5): the deterministic fixers a project
//! registers — `fmt`, `clippy --fix`, the regeneration of a lock file or a
//! generated file. forge reads them **from the target branch's** file only
//! (never from the merge request's tree) and runs one as a CI job whose
//! output is a tree; the server builds the commit and checks its scope.
//!
//! ```yaml
//! autofix:
//!   fixers:
//!     fmt:
//!       kind: format          # format | lint | regenerate
//!       image: rust:1.86@sha256:<64 hex>
//!       script: [cargo fmt --all]
//!       timeout: 10m
//!       paths: ["**/*.rs"]    # what it may change (`**`, `*`)
//!     lockfile:
//!       kind: regenerate      # also resolves conflicts on its paths (C-5)
//!       image: rust:1.86@sha256:<64 hex>
//!       script: [cargo generate-lockfile --offline]
//!       timeout: 10m
//!       paths: [Cargo.lock]
//! ```
//!
//! A fixer is a job without stage, needs, services, secrets, artifacts or
//! tags (none of those keys is accepted): no secret ever reaches it and it
//! uploads nothing but the runner's own bundle. At most [`MAX_FIXERS`].

use forge_doc::yaml::{self, Kind, Node};

use crate::definition::{is_pinned_image, parse_duration, read_script, Job, Rules};
use crate::{Problem, MAX_TIMEOUT_SECS};

/// The most fixers one file registers.
pub const MAX_FIXERS: usize = 8;
/// The most `paths` patterns of one fixer.
pub const MAX_FIXER_PATHS: usize = 16;
/// The stage name a fixer's job carries (no stage of `stages:`).
pub const FIXER_STAGE: &str = "autofix";

/// What a fixer is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FixerKind {
    /// A formatter (`cargo fmt`, `prettier --write`).
    Format,
    /// A linter's own fixes (`cargo clippy --fix`, `eslint --fix`).
    Lint,
    /// Regenerates a lock file or a generated file; the only kind that
    /// resolves a conflict (plan 04 C-5), on its own `paths` only.
    Regenerate,
}

impl FixerKind {
    pub const ALL: [FixerKind; 3] = [Self::Format, Self::Lint, Self::Regenerate];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Format => "format",
            Self::Lint => "lint",
            Self::Regenerate => "regenerate",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == raw)
    }

    /// Whether the fixer may resolve a conflict.
    pub fn resolves_conflicts(self) -> bool {
        matches!(self, Self::Regenerate)
    }
}

/// One fixer of a valid definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fixer {
    pub(crate) kind: FixerKind,
    pub(crate) paths: Vec<String>,
    /// The job the runner runs (stage [`FIXER_STAGE`], nothing but the
    /// image, script and timeout).
    pub(crate) job: Job,
}

impl Fixer {
    pub fn name(&self) -> &str {
        &self.job.name
    }

    pub fn kind(&self) -> FixerKind {
        self.kind
    }

    /// The patterns of the paths it may change.
    pub fn paths(&self) -> &[String] {
        &self.paths
    }

    /// The job the runner runs.
    pub fn job(&self) -> &Job {
        &self.job
    }
}

/// A `paths` pattern: a relative path of `/`-separated segments, each
/// `**` or `[A-Za-z0-9._*-]+` (no `..`, no `.git`, no leading `/`).
fn is_fixer_path(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && !s.starts_with('/')
        && s.split('/').all(|seg| {
            !seg.is_empty()
                && seg != "."
                && seg != ".."
                && !seg.eq_ignore_ascii_case(".git")
                && (seg == "**"
                    || (!seg.contains("**")
                        && seg
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || "._*-".contains(c))))
        })
}

/// `autofix:` — a mapping with `fixers:` only.
pub(crate) fn read_autofix(node: &Node, at: usize, problems: &mut Vec<Problem>) -> Vec<Fixer> {
    let fields = match yaml::mapping(node, "field_invalid") {
        Ok(f) => f,
        Err(p) => {
            problems.push(Problem::about(p.line.max(at), p.reason, "autofix"));
            return Vec::new();
        }
    };
    let mut fixers = Vec::new();
    for f in &fields {
        match f.key {
            "fixers" => fixers = read_fixers(f.value, f.line, problems),
            other => problems.push(Problem::about(f.line, "key_unknown", other)),
        }
    }
    fixers
}

fn read_fixers(node: &Node, at: usize, problems: &mut Vec<Problem>) -> Vec<Fixer> {
    let entries = match yaml::mapping(node, "field_invalid") {
        Ok(e) => e,
        Err(p) => {
            problems.push(Problem::about(p.line.max(at), "field_invalid", "fixers"));
            return Vec::new();
        }
    };
    if entries.len() > MAX_FIXERS {
        problems.push(Problem::new(at, "too_many_fixers"));
        return Vec::new();
    }
    let mut out = Vec::with_capacity(entries.len());
    for e in &entries {
        if !crate::definition::is_name(e.key) {
            problems.push(Problem::about(e.line, "fixer_name_invalid", e.key));
            continue;
        }
        if let Some(fixer) = read_fixer(e.key, e.line, e.value, problems) {
            out.push(fixer);
        }
    }
    out
}

fn read_fixer(name: &str, at: usize, node: &Node, problems: &mut Vec<Problem>) -> Option<Fixer> {
    let fields = match yaml::mapping(node, "field_invalid") {
        Ok(f) => f,
        Err(p) => {
            problems.push(Problem::about(p.line, p.reason, name));
            return None;
        }
    };
    let before = problems.len();
    let (mut kind, mut image, mut script, mut timeout) = (None, None, None, None);
    let mut paths = Vec::new();
    for f in &fields {
        let (line, value) = (f.line, f.value);
        match f.key {
            "kind" => match text(value) {
                Some(k) => match FixerKind::parse(k) {
                    Some(k) => kind = Some(k),
                    None => problems.push(Problem::about(line, "fixer_kind_invalid", k)),
                },
                None => problems.push(Problem::about(line, "field_invalid", "kind")),
            },
            "image" => match text(value) {
                Some(s) if is_pinned_image(s) => image = Some(s.to_owned()),
                Some(_) => problems.push(Problem::about(line, "image_not_pinned", name)),
                None => problems.push(Problem::about(line, "field_invalid", "image")),
            },
            "script" => script = read_script(value, line, name, problems),
            "timeout" => match text(value).map(|s| (s, parse_duration(s))) {
                Some((s, None)) => problems.push(Problem::about(line, "timeout_invalid", s)),
                Some((s, Some(secs))) if secs > MAX_TIMEOUT_SECS => {
                    problems.push(Problem::about(line, "timeout_too_long", s));
                }
                Some((_, Some(secs))) => timeout = Some(secs),
                None => problems.push(Problem::about(line, "field_invalid", "timeout")),
            },
            "paths" => paths = read_paths(value, line, name, problems),
            // A fixer gets nothing a job may ask for beyond its image,
            // script and timeout (module docs): every other key is unknown.
            other => problems.push(Problem::about(line, "key_unknown", other)),
        }
    }
    for (missing, present) in [
        ("kind", kind.is_some()),
        (
            "image",
            image.is_some() || fields.iter().any(|f| f.key == "image"),
        ),
        ("script", fields.iter().any(|f| f.key == "script")),
        ("timeout", fields.iter().any(|f| f.key == "timeout")),
        ("paths", fields.iter().any(|f| f.key == "paths")),
    ] {
        if !present {
            problems.push(Problem::about(at, "field_missing", missing));
        }
    }
    if problems.len() != before {
        return None;
    }
    Some(Fixer {
        kind: kind?,
        paths,
        job: Job {
            name: name.to_owned(),
            line: at,
            stage: FIXER_STAGE.to_owned(),
            stage_index: 0,
            needs: Vec::new(),
            image: image?,
            script: script?,
            timeout_secs: timeout?,
            tags: Vec::new(),
            secrets: Vec::new(),
            rules: Rules::default(),
            artifacts: Vec::new(),
            artifacts_keep: false,
            services: Vec::new(),
            // A fixer always runs when the autofix worker starts it.
            manual: false,
            // A fixer edits the pinned commit's tree only.
            fetch_depth: 1,
        },
    })
}

/// One non-empty text value.
fn text(node: &Node) -> Option<&str> {
    match &node.kind {
        Kind::Text(t) if !t.trim().is_empty() => Some(t.as_str()),
        Kind::Text(_) | Kind::Null | Kind::List(_) | Kind::Map(_) => None,
    }
}

fn read_paths(node: &Node, at: usize, fixer: &str, problems: &mut Vec<Problem>) -> Vec<String> {
    let Kind::List(items) = &node.kind else {
        problems.push(Problem::about(node.line.max(at), "field_invalid", "paths"));
        return Vec::new();
    };
    if items.is_empty() {
        problems.push(Problem::about(at, "fixer_paths_empty", fixer));
        return Vec::new();
    }
    if items.len() > MAX_FIXER_PATHS {
        problems.push(Problem::about(at, "too_many_fixer_paths", fixer));
        return Vec::new();
    }
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match &item.kind {
            Kind::Text(t) if is_fixer_path(t) => {
                if !out.contains(t) {
                    out.push(t.clone()); // clone: the tree is borrowed
                }
            }
            Kind::Text(t) => {
                problems.push(Problem::about(item.line, "fixer_path_invalid", t.as_str()))
            }
            Kind::Null | Kind::List(_) | Kind::Map(_) => {
                problems.push(Problem::about(item.line, "field_invalid", "paths"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::is_fixer_path;

    #[test]
    fn fixer_paths_are_relative_globs() {
        for ok in [
            "**/*.rs",
            "Cargo.lock",
            "web/package-lock.json",
            "src/**",
            "*.md",
        ] {
            assert!(is_fixer_path(ok), "{ok}");
        }
        for bad in [
            "",
            "/etc/x",
            "../x",
            "a/../b",
            ".git/config",
            "a/**b",
            "a b",
            "a\\b",
            "x?",
        ] {
            assert!(!is_fixer_path(bad), "{bad}");
        }
    }
}
