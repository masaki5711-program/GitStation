//! `gstn edge release verify` (plan 21 E1, ADR-0041 §8): checks a signed
//! edge release file on this computer — the same check the device agent
//! runs before it installs (E6). **Local only**: nothing is sent to the
//! server, no token is read.
//!
//! - The release file (`--release`, `{manifest, digest, key_id,
//!   signature}`) is parsed with forge-edge's manifest rules
//!   (`FG-EDGE-0002`), its digest recomputed and compared, and its Ed25519
//!   signature verified in the edge domain (`FG-EDGE-0004`) with the public
//!   keys in `edge-keys.json` in the gstn configuration directory (or
//!   `--keys`; missing, unreadable or without the kid: `FG-EDGE-0003`).
//! - With `--blobs <dir>`, every blob of the verified manifest is read from
//!   `<dir>/<path>` and its size and SHA-256 compared (`FG-EDGE-0004`
//!   `blob_mismatch` / `blob_missing` / `blob_not_a_regular_file`). Blobs
//!   are only checked against a manifest whose signature verified.
//!
//! Exit 1 with the most serious problem's code (FG-EDGE-0004 before 0003
//! before 0002). The output says the keys are local and that revocation
//! was not checked against the server (that comes with E4's key list).

use std::io::Read;
use std::path::Path;

use forge_edge::{
    parse_edge_keys, parse_release, verify_release, KeyRing, Manifest, Problem, MAX_RELEASE_BYTES,
};
use forge_errors::ErrorCode;
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};

use crate::agent::read_small;
use crate::app::{emit, EnvFn, Io};
use crate::cli::{Cli, EdgeCommand, EdgeReleaseCommand};
use crate::error::{ClientError, Result};

/// The edge key list's file name in the gstn configuration directory.
pub const EDGE_KEYS_FILE: &str = "edge-keys.json";
/// Most bytes of the key list.
const MAX_KEYS_FILE: usize = 1024 * 1024;

/// `gstn edge …` (module docs).
pub fn edge(cli: &Cli, cmd: &EdgeCommand, env: EnvFn<'_>, io: &mut Io<'_>) -> Result<()> {
    match cmd {
        EdgeCommand::Release {
            command:
                EdgeReleaseCommand::Verify {
                    release,
                    keys,
                    blobs,
                },
        } => verify(cli, env, io, release, keys.as_deref(), blobs.as_deref()),
    }
}

fn input(reason: &str) -> ClientError {
    ClientError::local(ErrorCode::CliInputFailed, format!("reason={reason}"))
}

fn keys_problem(reason: &'static str) -> Problem {
    Problem {
        code: ErrorCode::EdgeSigningKeyUnavailable,
        reason,
        subject: None,
    }
}

/// The edge key list: `--keys`, or `edge-keys.json` in the configuration
/// directory.
fn load_keys(env: EnvFn<'_>, over: Option<&Path>) -> std::result::Result<KeyRing, Problem> {
    let path = match over {
        Some(p) => p.to_path_buf(),
        None => crate::config::config_dir(env, None)
            .map_err(|_| keys_problem("keys_missing"))?
            .join(EDGE_KEYS_FILE),
    };
    match read_small(&path, MAX_KEYS_FILE, "keys") {
        Ok(Some(bytes)) => parse_edge_keys(&bytes),
        Ok(None) => Err(keys_problem("keys_missing")),
        Err(_) => Err(keys_problem("keys_unreadable")),
    }
}

fn blob_problem(reason: &'static str, path: &str) -> Problem {
    Problem {
        code: ErrorCode::EdgeSignatureInvalid,
        reason,
        subject: Some(path.to_owned()),
    }
}

/// Every blob of `manifest` under `dir`: size first (no hashing of a file
/// of the wrong length), then SHA-256 streamed. A blob that cannot be read
/// for another reason stops the command (`FG-CLI-0009`), never a pass.
fn check_blobs(dir: &Path, manifest: &Manifest) -> Result<(usize, Vec<Problem>)> {
    let mut checked = 0;
    let mut problems = Vec::new();
    for blob in manifest.payloads.iter().flat_map(|p| p.blobs.iter()) {
        // The path passed the manifest rules: relative, no `..`.
        let file = dir.join(&blob.path);
        let meta = match std::fs::symlink_metadata(&file) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                problems.push(blob_problem("blob_missing", &blob.path));
                continue;
            }
            Err(e) => return Err(input(&format!("blob_unreadable kind={:?}", e.kind()))),
        };
        if !meta.is_file() {
            problems.push(blob_problem("blob_not_a_regular_file", &blob.path));
            continue;
        }
        if meta.len() != blob.size {
            problems.push(blob_problem("blob_mismatch", &blob.path));
            continue;
        }
        let mut f = std::fs::File::open(&file)
            .map_err(|e| input(&format!("blob_unreadable kind={:?}", e.kind())))?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = f
                .read(&mut buf)
                .map_err(|e| input(&format!("blob_unreadable kind={:?}", e.kind())))?;
            if n == 0 {
                break;
            }
            hasher.update(buf.get(..n).unwrap_or_default());
        }
        let got: [u8; 32] = hasher.finalize().into();
        if got != blob.sha256 {
            problems.push(blob_problem("blob_mismatch", &blob.path));
        }
        checked += 1;
    }
    Ok((checked, problems))
}

fn verify(
    cli: &Cli,
    env: EnvFn<'_>,
    io: &mut Io<'_>,
    release: &Path,
    keys_over: Option<&Path>,
    blobs_dir: Option<&Path>,
) -> Result<()> {
    let shown = release.display().to_string();
    let bytes = read_small(release, MAX_RELEASE_BYTES, "release")?
        .ok_or_else(|| input("release_missing"))?;
    let mut problems = Vec::new();
    let mut verified = None;
    // The manifest's own form first, so a malformed file is FG-EDGE-0002
    // whether or not keys are present.
    let parsed = parse_release(&bytes);
    let summary_of = parsed.as_ref().ok().map(|r| r.manifest.clone());
    match parsed {
        Err(p) => problems.push(p),
        Ok(_) => match load_keys(env, keys_over) {
            Err(p) => problems.push(p),
            Ok(ring_) => match verify_release(&bytes, &ring_) {
                Ok(v) => verified = Some(v),
                Err(p) => problems.push(p),
            },
        },
    }
    let mut blobs_checked = 0;
    if let (Some(dir), Some(v)) = (blobs_dir, verified.as_ref()) {
        let (n, found) = check_blobs(dir, &v.manifest)?;
        blobs_checked = n;
        problems.extend(found);
    }
    let m = summary_of.as_ref();
    let blob_count: usize = m.map_or(0, |m| m.payloads.iter().map(|p| p.blobs.len()).sum());
    let value = json!({
        "release": shown,
        "release_id": m.map(|m| m.release_id.clone()),
        "project": m.map(|m| m.project.clone()),
        "version": m.map(|m| m.version.clone()),
        "sequence": m.map(|m| m.sequence),
        "arch": m.map(|m| m.arch.as_str()),
        "os": m.map(|m| m.os.clone()),
        "payloads": m.map(|m| m.payloads.iter().map(|p| p.kind.as_str()).collect::<Vec<_>>()),
        "blobs": blob_count,
        "blobs_checked": blobs_checked,
        "digest": verified.as_ref().map(|v| v.digest.to_string()),
        "key_id": verified.as_ref().map(|v| v.key_id.clone()),
        "verified": verified.is_some() && problems.is_empty(),
        "keys": "local",
        "revocation_checked": false,
        "problems": problems.iter().map(problem_json).collect::<Vec<_>>(),
    });
    let summary = match (m, verified.as_ref()) {
        (Some(m), Some(v)) => format!(
            "Release {} ({} {}, sequence {}, {}/{}): signature verified with local key {}, digest {}; {} of {} blob(s) checked; revocation not checked (offline).",
            crate::error::sanitize(&m.release_id),
            crate::error::sanitize(&m.project),
            crate::error::sanitize(&m.version),
            m.sequence,
            m.arch.as_str(),
            m.os,
            crate::error::sanitize(&v.key_id),
            v.digest,
            blobs_checked,
            blob_count,
        ),
        (Some(m), None) => format!(
            "{shown}: release {} {} NOT verified.",
            crate::error::sanitize(&m.release_id),
            crate::error::sanitize(&m.version)
        ),
        (None, _) => format!("{shown}: not a valid edge release file."),
    };
    report(cli, io, value, &problems, &summary)
}

fn problem_json(p: &Problem) -> Value {
    json!({"error_code": p.code.id(), "reason": p.reason, "subject": p.subject})
}

fn problem_text(p: &Problem) -> String {
    let subject = p
        .subject
        .as_deref()
        .map(|s| format!("{}: ", crate::error::sanitize(s)))
        .unwrap_or_default();
    format!("{subject}{} [{}]", p.reason, p.code.id())
}

fn report(
    cli: &Cli,
    io: &mut Io<'_>,
    value: Value,
    problems: &[Problem],
    summary: &str,
) -> Result<()> {
    let mut human: Vec<String> = problems.iter().map(problem_text).collect();
    human.push(if problems.is_empty() {
        summary.to_owned()
    } else {
        format!("{} problem(s). {summary}", problems.len())
    });
    emit(io, cli, value, &human.join("\n"))?;
    match forge_edge::worst(problems) {
        None => Ok(()),
        Some(p) => Err(ClientError::local(
            p.code,
            format!(
                "reason={} subject={} problems={}",
                p.reason,
                p.subject.as_deref().unwrap_or("-"),
                problems.len()
            ),
        )),
    }
}
