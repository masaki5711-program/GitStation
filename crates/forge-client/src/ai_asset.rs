//! `gstn ai-asset lint` (plan 17 P17-3, ADR-0035): the AI asset lint
//! ([`forge_doc::ai_asset::lint`]) on the working tree of an asset
//! repository. **Local only**: nothing is sent to the server, and no
//! configuration or token is read. The CI job of an opted-in repository
//! (`ai_assets.enabled`) runs it; the server's `forge/risk` runs the same
//! raw-data scan of the cases.
//!
//! The files are the asset directories (`skills/`, `playbooks/`, `agents/`,
//! `tools/`, `models/`, `bench/`, `evals/`, `thresholds/`) and `CODEOWNERS`
//! under the repository found upward (or `--dir`). Bounded: at most
//! [`MAX_FILES`] files and [`MAX_TOTAL_BYTES`] in all; each read up to one
//! byte past the lint's limit (it then says `too_large`). A symbolic link
//! is not followed (`symlink_not_followed`).
//!
//! `gstn ai-asset status --project <id>` reads a project's
//! `ai_assets.enabled` (`GET …/ai-assets`, `read_api`).
//!
//! Plan 17 P17-7: `gstn ai-asset release --project <id> --tag
//! assets/v<yyyy>.<ww>.<n>` makes a signed `pfs-ai-bundle` release (`api`,
//! a Maintainer); `releases` lists them (or `--version` shows one with its
//! manifest); `release-deploy` and `rollback` request a stage deployment,
//! which a person approves in the browser.
//!
//! Exit codes: 1 with `FG-AIASSET-0001` when the assets have a problem;
//! `FG-CLI-*` when they cannot be read.

use std::io::Read;
use std::path::{Path, PathBuf};

use forge_doc::ai_asset::{AssetFile, Problem, MAX_FILE_BYTES};
use forge_errors::ErrorCode;
use serde_json::{json, Value};

use crate::app::{emit, EnvFn, Io};
use crate::cli::{AiAssetCommand, Cli};
use crate::error::{ClientError, Result};

/// The directories read (the asset layout, plan 17 §2).
pub const ASSET_DIRS: &[&str] = &[
    "skills",
    "playbooks",
    "agents",
    "tools",
    "models",
    "bench",
    "evals",
    "thresholds",
];
/// At most this many files are read.
pub const MAX_FILES: usize = 10_000;
/// At most this many bytes in all.
pub const MAX_TOTAL_BYTES: u64 = 128 * 1024 * 1024;

fn cli_input(reason: &str) -> ClientError {
    ClientError::local(ErrorCode::CliInputFailed, format!("reason={reason}"))
}

/// `gstn ai-asset …` (module docs).
pub fn ai_asset(cli: &Cli, cmd: &AiAssetCommand, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    match cmd {
        // The one `ai-asset` subcommand that calls the server.
        AiAssetCommand::Status { project } => {
            let c = crate::hosting::client(cli, env, io)?;
            let path = format!("/api/v1/projects/{project}/ai-assets");
            let out = crate::hosting::call(&c, crate::http::Method::Get, &path, &[], None)?;
            let on = out.get("enabled").and_then(Value::as_bool) == Some(true);
            let human = format!(
                "ai_assets.enabled: {} (AI asset lint, forge/risk ai_case_raw_data / ai_asset, releases {})",
                if on { "on" } else { "off" },
                if on { "apply" } else { "do not apply" }
            );
            emit(io, cli, out, &human)
        }
        AiAssetCommand::Release { project, tag } => {
            let c = crate::hosting::client(cli, env, io)?;
            let path = format!("/api/v1/projects/{project}/ai-asset-releases");
            let out = crate::hosting::call(
                &c,
                crate::http::Method::Post,
                &path,
                &[],
                Some(&json!({ "tag": tag })),
            )?;
            let human = format!("Released {}", release_line(&out));
            emit(io, cli, out, &human)
        }
        AiAssetCommand::Releases { project, version } => {
            let c = crate::hosting::client(cli, env, io)?;
            let base = format!("/api/v1/projects/{project}/ai-asset-releases");
            if let Some(v) = version {
                check_version(v)?;
                let out = crate::hosting::call(
                    &c,
                    crate::http::Method::Get,
                    &format!("{base}/{v}"),
                    &[],
                    None,
                )?;
                let human = release_line(&out);
                return emit(io, cli, out, &human);
            }
            let out = crate::hosting::call(&c, crate::http::Method::Get, &base, &[], None)?;
            let lines: Vec<String> = out
                .get("releases")
                .and_then(Value::as_array)
                .map(|a| a.iter().map(release_line).collect())
                .unwrap_or_default();
            let human = if lines.is_empty() {
                "No release.".to_owned()
            } else {
                lines.join("\n")
            };
            emit(io, cli, out, &human)
        }
        AiAssetCommand::ReleaseDeploy {
            project,
            version,
            environment,
        } => {
            check_version(version)?;
            let c = crate::hosting::client(cli, env, io)?;
            let path =
                format!("/api/v1/projects/{project}/ai-asset-releases/{version}/deployments");
            let out = crate::hosting::call(
                &c,
                crate::http::Method::Post,
                &path,
                &[],
                Some(&json!({ "environment": environment })),
            )?;
            let human = format!(
                "Requested {version} on {environment}: {} (a person approves it in the browser)",
                crate::error::sanitize(out.get("status").and_then(Value::as_str).unwrap_or("?"))
            );
            emit(io, cli, out, &human)
        }
        AiAssetCommand::Rollback {
            project,
            environment,
        } => {
            let c = crate::hosting::client(cli, env, io)?;
            let path = format!("/api/v1/projects/{project}/ai-asset-rollbacks");
            let out = crate::hosting::call(
                &c,
                crate::http::Method::Post,
                &path,
                &[],
                Some(&json!({ "environment": environment })),
            )?;
            let to = out
                .pointer("/release/version")
                .and_then(Value::as_str)
                .unwrap_or("?");
            let human = format!(
                "Requested the rollback of {environment} to {}: a person approves it in the browser",
                crate::error::sanitize(to)
            );
            emit(io, cli, out, &human)
        }
        AiAssetCommand::Lint { dir } => {
            let root = match dir {
                Some(d) => d.clone(), // clone: the walk owns its root
                None => {
                    let cwd = std::env::current_dir().map_err(|_| cli_input("cwd_unreadable"))?;
                    crate::doc::find_root(&cwd)?
                }
            };
            let (files, mut problems) = read_assets(&root)?;
            let assets: Vec<AssetFile<'_>> = files
                .iter()
                .map(|(path, bytes)| AssetFile { path, bytes })
                .collect();
            problems.extend(forge_doc::ai_asset::lint(&assets));
            problems.sort();
            report(cli, io, files.len(), &problems)
        }
    }
}

/// A release version as the API names it (`v<yyyy>.<ww>.<n>`), checked
/// before it reaches a path.
fn check_version(v: &str) -> Result<()> {
    let digits = v.strip_prefix('v').filter(|rest| {
        v.len() <= 16
            && rest.split('.').count() == 3
            && rest
                .split('.')
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    });
    match digits {
        Some(_) => Ok(()),
        None => Err(cli_input("release_version_invalid")),
    }
}

fn release_line(r: &Value) -> String {
    let s = |k: &str| crate::error::sanitize(r.get(k).and_then(Value::as_str).unwrap_or("?"));
    format!(
        "{} {} commit {} {} key {} ({} files)",
        s("version"),
        s("tag"),
        s("commit_sha"),
        s("digest"),
        s("key_id"),
        r.get("files").and_then(Value::as_i64).unwrap_or(0)
    )
}

/// One file read: its path from the root and its bytes.
pub type ReadFile = (String, Vec<u8>);

/// Every file under the asset directories and `CODEOWNERS` (path from the
/// root, bytes), sorted by path, and the problems of files not read.
pub fn read_assets(root: &Path) -> Result<(Vec<ReadFile>, Vec<Problem>)> {
    let mut files = Vec::new();
    let mut problems = Vec::new();
    let mut total: u64 = 0;
    let mut stack: Vec<(PathBuf, String)> = Vec::new();
    for dir in ASSET_DIRS {
        stack.push((root.join(dir), (*dir).to_owned()));
    }
    for owners in ["CODEOWNERS", ".gitstation/CODEOWNERS"] {
        let path = root.join(owners);
        if path.is_file() {
            read_one(&path, owners, &mut files, &mut total)?;
        }
    }
    while let Some((path, rel)) = stack.pop() {
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(ClientError::local(
                    ErrorCode::CliInputFailed,
                    format!("reason=asset_unreadable kind={:?}", e.kind()),
                ))
            }
        };
        if meta.file_type().is_symlink() {
            problems.push(Problem {
                path: rel,
                line: 0,
                rule: "symlink_not_followed",
                subject: None,
            });
            continue;
        }
        if meta.is_dir() {
            let entries = std::fs::read_dir(&path).map_err(|e| {
                ClientError::local(
                    ErrorCode::CliInputFailed,
                    format!("reason=asset_dir_unreadable kind={:?}", e.kind()),
                )
            })?;
            for entry in entries {
                let entry = entry.map_err(|_| cli_input("asset_dir_unreadable"))?;
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    problems.push(Problem {
                        path: rel.clone(), // clone: the problem owns its path
                        line: 0,
                        rule: "not_utf8_name",
                        subject: None,
                    });
                    continue;
                };
                stack.push((entry.path(), format!("{rel}/{name}")));
            }
            continue;
        }
        if files.len() >= MAX_FILES {
            return Err(cli_input("too_many_asset_files"));
        }
        read_one(&path, &rel, &mut files, &mut total)?;
    }
    files.sort();
    Ok((files, problems))
}

fn read_one(path: &Path, rel: &str, files: &mut Vec<ReadFile>, total: &mut u64) -> Result<()> {
    let f = std::fs::File::open(path).map_err(|e| {
        ClientError::local(
            ErrorCode::CliInputFailed,
            format!("reason=asset_unreadable kind={:?}", e.kind()),
        )
    })?;
    let limit = u64::try_from(MAX_FILE_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut bytes = Vec::new();
    f.take(limit).read_to_end(&mut bytes).map_err(|e| {
        ClientError::local(
            ErrorCode::CliInputFailed,
            format!("reason=asset_unreadable kind={:?}", e.kind()),
        )
    })?;
    *total = total.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
    if *total > MAX_TOTAL_BYTES {
        return Err(cli_input("assets_too_large"));
    }
    files.push((rel.to_owned(), bytes));
    Ok(())
}

fn problem_json(p: &Problem) -> Value {
    json!({"path": p.path, "line": p.line, "rule": p.rule, "subject": p.subject})
}

fn problem_text(p: &Problem) -> String {
    let subject = p
        .subject
        .as_deref()
        .map(|s| format!(" — {}", crate::error::sanitize(s)))
        .unwrap_or_default();
    format!(
        "{}:{}: {}{subject} [{}]",
        crate::error::sanitize(&p.path),
        p.line,
        p.rule,
        ErrorCode::AiAssetInvalid.id()
    )
}

fn report(cli: &Cli, io: &mut Io<'_>, files: usize, problems: &[Problem]) -> Result<()> {
    let mut human: Vec<String> = problems.iter().map(problem_text).collect();
    human.push(if problems.is_empty() {
        format!("No problem in {files} asset file(s).")
    } else {
        format!("{} problem(s) in {files} asset file(s).", problems.len())
    });
    let value = json!({
        "files": files,
        "problems": problems.iter().map(problem_json).collect::<Vec<_>>(),
    });
    emit(io, cli, value, &human.join("\n"))?;
    if let Some(first) = problems.first() {
        return Err(ClientError::local(
            ErrorCode::AiAssetInvalid,
            format!(
                "reason={} line={} problems={}",
                first.rule,
                first.line,
                problems.len()
            ),
        ));
    }
    Ok(())
}
