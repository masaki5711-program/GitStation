//! `gstn part import` (plan 16 A6, ADR-0032 Amendment A6): seed the
//! `marketplace/parts` repository from parts written in another shape.
//! **Local only**: nothing is sent, no token is read.
//!
//! - `--format marketplace`: a W1 tree (`<from>/<category>/<id>/part.json`,
//!   the `marketplace/` the agent models share). The two adapter parts
//!   (`adapter-claude-code-tmux`, `adapter-claude-code-v2`) are made too,
//!   with the overlays the parts carried.
//! - `--format claude`: a repository's `.claude/` — every skill directory,
//!   agent, command and rule file becomes a `1.0.0` part. Other entries of
//!   `.claude/` (`settings.json`, `mistakes/`, `state/`, …) are the model's
//!   own harness and are listed under `skipped`, never read.
//!
//! Every part goes through the same lint and static scan as
//! `gstn agent verify --part` (with `--mcp-allowed-hosts`). Capabilities
//! the scan asks for are declared only when named in
//! `--declare-from-scan exec,network,fs-write` (never `permissions`), and
//! the report says which findings each came from. Without
//! `--out` it is a dry run: the report only. With `--out` (a new or empty
//! directory) every part is written as `<out>/<slug>/part.toml` plus its
//! files — the layout of `marketplace/parts` — and only when no part has a
//! problem (all or nothing), with the report as `import-report.json`. `--only` keeps the named slugs (and drops the
//! sources that are not parts at all: they have no slug to name).
//!
//! Exit 1 with the most serious problem's code; reading a source is
//! `FG-CLI-0009`, writing the output `FG-CLI-0007` (`stream=file`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use forge_agent::import::{
    adapter_part, check_set, import_claude_item, import_marketplace_part, kind_of_category,
    marketplace_sources, seed, Imported, Overlay, Seeded, BASE_MODELS,
};
use forge_agent::{Capability, EntryKind, Kind, Problem, ScanPolicy, MAX_PART_BYTES};
use forge_errors::ErrorCode;
use serde_json::{json, Value};

use crate::agent::{problem_json, read_part_dir, read_small, report, MAX_SMALL_FILE};
use crate::app::Io;
use crate::cli::Cli;
use crate::error::{ClientError, Result};

/// The import's report, written beside the parts with `--out`.
pub const REPORT_FILE: &str = "import-report.json";
/// Most parts one import reads.
const MAX_PARTS: usize = 1_000;

fn input(reason: &str) -> ClientError {
    ClientError::local(ErrorCode::CliInputFailed, format!("reason={reason}"))
}

fn output(reason: &str) -> ClientError {
    ClientError::local(
        ErrorCode::CliOutputFailed,
        format!("stream=file reason={reason}"),
    )
}

/// A source that could not become a part: where, and why.
struct Failed {
    source: String,
    problems: Vec<Problem>,
}

#[derive(Default)]
struct Sources {
    parts: Vec<Imported>,
    failed: Vec<Failed>,
    overlays: Vec<Overlay>,
    skipped: Vec<String>,
}

fn source_problem(reason: &'static str, subject: &str) -> Problem {
    Problem {
        code: ErrorCode::PartManifestInvalid,
        reason,
        subject: Some(subject.to_owned()),
    }
}

/// The entries of `dir`, sorted by name: (name, path, is_dir, is_file).
fn listing(dir: &Path) -> Result<Vec<(String, PathBuf, bool, bool)>> {
    let mut out = Vec::new();
    let items = std::fs::read_dir(dir)
        .map_err(|e| input(&format!("import_unreadable kind={:?}", e.kind())))?;
    for item in items {
        let item = item.map_err(|e| input(&format!("import_unreadable kind={:?}", e.kind())))?;
        let path = item.path();
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| input(&format!("import_unreadable kind={:?}", e.kind())))?;
        let name = item
            .file_name()
            .to_str()
            .ok_or_else(|| input("import_path_not_utf8"))?
            .to_owned();
        out.push((name, path, meta.is_dir(), meta.is_file()));
        if out.len() > MAX_PARTS {
            return Err(input("import_too_many_entries"));
        }
    }
    out.sort();
    Ok(out)
}

/// A regular file under `base` (never followed through a symlink).
fn read_regular(base: &Path, rel: &str) -> std::result::Result<Vec<u8>, &'static str> {
    let mut path = base.to_path_buf();
    for seg in rel.split('/') {
        path.push(seg);
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_symlink() => return Err("not_a_regular_file"),
            Ok(_) => {}
            Err(_) => return Err("source_file_missing"),
        }
    }
    // Security review A6 (MEDIUM): what the OS opens must still be under
    // `base` (Windows renames segments it normalises).
    match (std::fs::canonicalize(base), std::fs::canonicalize(&path)) {
        (Ok(b), Ok(p)) if p.starts_with(&b) => {}
        (Ok(_), Ok(_)) => return Err("source_path_invalid"),
        _ => return Err("source_file_missing"),
    }
    match read_small(&path, MAX_PART_BYTES, "import_file") {
        Ok(Some(b)) => Ok(b),
        Ok(None) => Err("source_file_missing"),
        Err(_) => Err("source_file_unreadable"),
    }
}

fn read_marketplace(from: &Path) -> Result<Sources> {
    let mut s = Sources::default();
    for (category, cat_path, is_dir, _) in listing(from)? {
        if !is_dir {
            continue; // README.md and other notes beside the categories
        }
        if kind_of_category(&category).is_none() {
            s.failed.push(Failed {
                source: category.clone(),
                problems: vec![source_problem("source_category_unknown", &category)],
            });
            continue;
        }
        for (id, dir, is_dir, _) in listing(&cat_path)? {
            let origin = format!("{category}/{id}");
            if !is_dir {
                s.failed.push(Failed {
                    problems: vec![source_problem("unexpected_entry", &origin)],
                    source: origin,
                });
                continue;
            }
            match read_one_marketplace(&category, &id, &dir)? {
                Ok((part, mut overlays)) => {
                    s.parts.push(part);
                    s.overlays.append(&mut overlays);
                }
                Err(problems) => s.failed.push(Failed {
                    source: origin,
                    problems,
                }),
            }
        }
    }
    for adapter in BASE_MODELS {
        match adapter_part(adapter, &s.overlays) {
            Ok(p) => s.parts.push(p),
            Err(problems) => s.failed.push(Failed {
                source: format!("adapter {adapter}"),
                problems,
            }),
        }
    }
    Ok(s)
}

type OneResult = std::result::Result<(Imported, Vec<Overlay>), Vec<Problem>>;

fn read_one_marketplace(category: &str, id: &str, dir: &Path) -> Result<OneResult> {
    // Never through a symlink (security review A6, LOW).
    let manifest = match read_regular(dir, "part.json") {
        Ok(m) if m.len() <= MAX_SMALL_FILE => m,
        Ok(_) => {
            return Ok(Err(vec![source_problem(
                "source_manifest_too_large",
                "part.json",
            )]))
        }
        Err("source_file_missing") => {
            return Ok(Err(vec![source_problem(
                "source_manifest_missing",
                "part.json",
            )]))
        }
        Err(reason) => return Ok(Err(vec![source_problem(reason, "part.json")])),
    };
    let srcs = match marketplace_sources(&manifest) {
        Ok(s) => s,
        Err(p) => return Ok(Err(p)),
    };
    let mut files = BTreeMap::new();
    let mut problems = Vec::new();
    for src in srcs {
        match read_regular(dir, &src) {
            Ok(bytes) => {
                files.insert(src, bytes);
            }
            Err(reason) => problems.push(source_problem(reason, &src)),
        }
    }
    if !problems.is_empty() {
        return Ok(Err(problems));
    }
    Ok(import_marketplace_part(category, id, &manifest, &files).map(|m| (m.part, m.overlays)))
}

/// One part from install path → bytes, or why not.
fn claude_part(
    s: &mut Sources,
    kind: Kind,
    id: &str,
    source: &str,
    files: BTreeMap<String, Vec<u8>>,
) {
    match import_claude_item(kind, id, source, files) {
        Ok(p) => s.parts.push(p),
        Err(problems) => s.failed.push(Failed {
            source: source.to_owned(),
            problems,
        }),
    }
}

fn read_claude(from: &Path) -> Result<Sources> {
    let mut s = Sources::default();
    let claude = from.join(".claude");
    match std::fs::symlink_metadata(&claude) {
        Ok(m) if m.is_dir() => {}
        _ => return Err(input("import_no_claude_dir")),
    }
    for (name, path, is_dir, _) in listing(&claude)? {
        let base = format!(".claude/{name}");
        match (name.as_str(), is_dir) {
            ("skills", true) => {
                for (id, dir, is_dir, _) in listing(&path)? {
                    let source = format!("{base}/{id}");
                    if !is_dir {
                        s.failed.push(Failed {
                            problems: vec![source_problem("unexpected_entry", &source)],
                            source,
                        });
                        continue;
                    }
                    let (files, problems) = tree_files(&dir, &source)?;
                    if problems.is_empty() {
                        claude_part(&mut s, Kind::Skill, &id, &source, files);
                    } else {
                        s.failed.push(Failed { source, problems });
                    }
                }
            }
            ("agents" | "commands", true) => {
                let kind = if name == "agents" {
                    Kind::Agent
                } else {
                    Kind::Command
                };
                for (file, fpath, _, is_file) in listing(&path)? {
                    let source = format!("{base}/{file}");
                    let Some(id) = file.strip_suffix(".md").filter(|_| is_file) else {
                        s.failed.push(Failed {
                            problems: vec![source_problem("unexpected_entry", &source)],
                            source,
                        });
                        continue;
                    };
                    // Gone since the listing: a problem, never an empty part.
                    let Some(bytes) = read_small(&fpath, MAX_PART_BYTES, "import_file")? else {
                        s.failed.push(Failed {
                            problems: vec![source_problem("source_file_missing", &source)],
                            source,
                        });
                        continue;
                    };
                    claude_part(
                        &mut s,
                        kind,
                        id,
                        &source,
                        BTreeMap::from([(source.clone(), bytes)]),
                    );
                }
            }
            ("rules", true) => {
                let (files, problems) = tree_files(&path, &base)?;
                if !problems.is_empty() {
                    s.failed.push(Failed {
                        source: base.clone(),
                        problems,
                    });
                }
                for (install, bytes) in files {
                    let rel = install
                        .strip_prefix(&format!("{base}/"))
                        .unwrap_or(&install);
                    let Some(stem) = rel.strip_suffix(".md") else {
                        s.failed.push(Failed {
                            problems: vec![source_problem("unexpected_entry", &install)],
                            source: install,
                        });
                        continue;
                    };
                    let id = stem.replace('/', "-");
                    let source = install.clone();
                    claude_part(
                        &mut s,
                        Kind::Rule,
                        &id,
                        &source,
                        BTreeMap::from([(install, bytes)]),
                    );
                }
            }
            _ => s.skipped.push(base),
        }
    }
    Ok(s)
}

/// Install path → bytes.
type Files = BTreeMap<String, Vec<u8>>;

/// Every regular file under `dir` as `<prefix>/<rel>` → bytes; a symlink or
/// other entry is a problem (never followed).
fn tree_files(dir: &Path, prefix: &str) -> Result<(Files, Vec<Problem>)> {
    let mut files = BTreeMap::new();
    let mut problems = Vec::new();
    let entries = read_part_dir(dir)?;
    // read_part_dir stops one past the part limit: say so, never drop files.
    if entries.len() > forge_agent::MAX_PART_FILES {
        problems.push(source_problem("too_many_files", prefix));
    }
    for e in entries {
        let path = format!("{prefix}/{}", e.path);
        if e.kind == EntryKind::File {
            files.insert(path, e.content);
        } else {
            problems.push(source_problem("not_a_regular_file", &path));
        }
    }
    Ok((files, problems))
}

fn seeded_json(s: &Seeded) -> Value {
    let part = s.part.as_ref();
    json!({
        "slug": s.slug,
        "source": s.source,
        "kind": part.map(|p| p.manifest.kind.as_str()),
        "version": part.map(|p| p.manifest.version.clone()),
        "files": part.map_or(0, |p| p.files.len()),
        "capabilities": part.map(|p| p.manifest.capabilities.iter().map(|c| c.as_str()).collect::<Vec<_>>()),
        "inferred_capabilities": s.inferred.iter().map(|c| c.as_str()).collect::<Vec<_>>(),
        "inferred_from": s.evidence.iter().map(|(c, reason, path)| json!({
            "capability": c.as_str(), "reason": reason, "path": path,
        })).collect::<Vec<_>>(),
        "requires": part.map(|p| p.manifest.requires.clone()),
        "digest": part.map(|p| p.digest.to_string()),
        "problems": s.problems.iter().map(problem_json).collect::<Vec<_>>(),
    })
}

/// `<out>` must be new or an empty directory (never overwrite a tree).
fn prepare_out(out: &Path) -> Result<()> {
    match std::fs::symlink_metadata(out) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir_all(out)
            .map_err(|e| output(&format!("write_failed kind={:?}", e.kind()))),
        Ok(m) if m.is_dir() => {
            let mut items = std::fs::read_dir(out)
                .map_err(|e| output(&format!("write_failed kind={:?}", e.kind())))?;
            if items.next().is_some() {
                Err(output("out_not_empty"))
            } else {
                Ok(())
            }
        }
        Ok(_) => Err(output("out_not_a_directory")),
        Err(e) => Err(output(&format!("write_failed kind={:?}", e.kind()))),
    }
}

fn write_parts(out: &Path, parts: &[Seeded]) -> Result<()> {
    prepare_out(out)?;
    for s in parts {
        // The slug and every path passed the lint: no `..`, no absolute.
        let root = out.join(&s.slug);
        for e in &s.entries {
            let mut path = root.clone();
            path.extend(e.path.split('/'));
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| output(&format!("write_failed kind={:?}", e.kind())))?;
            }
            std::fs::write(&path, &e.content)
                .map_err(|e| output(&format!("write_failed kind={:?}", e.kind())))?;
        }
    }
    Ok(())
}

/// The arguments of `gstn part import`.
pub struct ImportArgs<'a> {
    pub from: &'a Path,
    pub format: &'a str,
    pub out: Option<&'a Path>,
    pub only: Option<&'a [String]>,
    pub mcp_allowed_hosts: &'a [String],
    /// Capabilities `seed` may declare from the scan (never permissions).
    pub declare_from_scan: &'a [String],
}

/// `gstn part import` (module docs).
pub fn import(cli: &Cli, io: &mut Io<'_>, args: &ImportArgs<'_>) -> Result<()> {
    let mut sources = match args.format {
        "marketplace" => read_marketplace(args.from)?,
        "claude" => read_claude(args.from)?,
        _ => return Err(input("import_format_unknown")),
    };
    if let Some(only) = args.only {
        let wanted: BTreeSet<&str> = only.iter().map(String::as_str).collect();
        let known: BTreeSet<&str> = sources.parts.iter().map(|p| p.slug.as_str()).collect();
        if let Some(missing) = wanted.iter().find(|w| !known.contains(**w)) {
            return Err(input(&format!("only_unknown_slug slug={missing}")));
        }
        sources.parts.retain(|p| wanted.contains(p.slug.as_str()));
        sources.failed.clear();
    }
    let policy = ScanPolicy {
        // clone: the policy owns its hosts
        mcp_allowed_hosts: args.mcp_allowed_hosts.to_vec(),
    };
    let mut allow = BTreeSet::new();
    for c in args.declare_from_scan {
        match Capability::parse(c) {
            Some(Capability::Permissions) | None => {
                return Err(input(&format!(
                    "declare_from_scan_not_allowed capability={c}"
                )))
            }
            Some(cap) => {
                allow.insert(cap);
            }
        }
    }
    let mut seeded: Vec<Seeded> = sources
        .parts
        .iter()
        .map(|p| seed(p, &policy, &allow))
        .collect();
    for (i, p) in check_set(&sources.parts) {
        if let Some(s) = seeded.get_mut(i) {
            s.problems.push(p);
        }
    }
    let mut problems: Vec<Problem> = seeded.iter().flat_map(|s| s.problems.clone()).collect();
    problems.extend(sources.failed.iter().flat_map(|f| f.problems.clone()));

    let wrote = args.out.filter(|_| problems.is_empty());
    if let Some(out) = wrote {
        write_parts(out, &seeded)?;
    }
    let written = wrote.map(|o| o.display().to_string());
    let ok = seeded.iter().filter(|s| s.problems.is_empty()).count();
    let value = json!({
        "from": args.from.display().to_string(),
        "format": args.format,
        "dry_run": args.out.is_none(),
        "written": written,
        "parts": seeded.iter().map(seeded_json).collect::<Vec<_>>(),
        "failed": sources.failed.iter().map(|f| json!({
            "source": f.source,
            "problems": f.problems.iter().map(problem_json).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "skipped": sources.skipped,
        "ok": ok,
        "problems": problems.len(),
    });
    // The report beside the parts, for the reviewers of the parts MR
    // (security review A6: which findings each declared capability came from).
    if let Some(out) = wrote {
        let mut bytes =
            serde_json::to_vec_pretty(&value).map_err(|_| output("write_failed kind=serialize"))?;
        bytes.push(b'\n');
        std::fs::write(out.join(REPORT_FILE), bytes)
            .map_err(|e| output(&format!("write_failed kind={:?}", e.kind())))?;
    }
    let mut lines: Vec<String> = seeded
        .iter()
        .map(|s| {
            let state = if s.problems.is_empty() { "ok" } else { "FAIL" };
            let inferred = if s.inferred.is_empty() {
                String::new()
            } else {
                format!(
                    " (declared from the scan: {})",
                    s.inferred
                        .iter()
                        .map(|c| c.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            format!("{state} {} <- {}{inferred}", s.slug, s.source)
        })
        .collect();
    lines.extend(
        sources
            .failed
            .iter()
            .map(|f| format!("FAIL {} (not a part)", f.source)),
    );
    let tail = match (&written, args.out) {
        (Some(out), _) => format!("wrote {} part(s) to {out}", seeded.len()),
        (None, Some(_)) => "nothing written: every part must pass first".to_owned(),
        (None, None) => "dry run: nothing written".to_owned(),
    };
    let summary = format!(
        "{}\n{ok} of {} part(s) pass; {tail}.",
        lines.join("\n"),
        seeded.len() + sources.failed.len()
    );
    report(cli, io, value, &problems, &summary)
}
