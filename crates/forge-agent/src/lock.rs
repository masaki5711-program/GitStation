//! `.gitstation/agent.lock.json` (plan 09 §4, plan 16 §3, ADR-0032 §6) and
//! its verification against the working tree.
//!
//! ```json
//! {
//!   "schema_version": 1,
//!   "project_id": 12, "project_type": "system", "linked_specs": ["spec/a.md"],
//!   "parts": [{
//!     "slug": "format-on-save", "kind": "hook", "version": "1.2.3",
//!     "requires": [], "capabilities": ["exec"],
//!     "digest": "sha256:…", "key_id": "k1", "signature": "<base64>",
//!     "selected_by": "rule", "reason": "project_type=system",
//!     "files": [{"path": ".claude/hooks/format-on-save.js", "sha256": "…"}]
//!   }],
//!   "custom": [{"path": ".claude/rules/local.md", "sha256": "…", "approved_by": "alice"}]
//! }
//! ```
//!
//! The chain checked: each working-tree file's SHA-256 = the lock's → the
//! part's digest recomputed from its manifest fields and file hashes = the
//! lock's digest → the Ed25519 signature of that digest by `key_id` holds.
//! Every managed file (`.claude/**`, `CLAUDE.md`, `.mcp.json`) is either a
//! part's file or a `custom` one; anything else is `unmanaged_file`.

use std::collections::{BTreeMap, BTreeSet};

use forge_errors::ErrorCode;
use serde::Deserialize;

use crate::manifest::manifest_from_parts;
use crate::{
    check_install_path, digest, is_managed_path, sha256, unhex32, verify_signature, Digest,
    KeyRing, Problem,
};

/// Where the lock lives in a repository.
pub const LOCK_PATH: &str = ".gitstation/agent.lock.json";

const MAX_LOCK_PARTS: usize = 500;

/// The lock file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lock {
    pub schema_version: u32,
    #[serde(default)]
    pub project_id: Option<i64>,
    #[serde(default)]
    pub project_type: Option<String>,
    #[serde(default)]
    pub linked_specs: Vec<String>,
    #[serde(default)]
    pub parts: Vec<LockPart>,
    #[serde(default)]
    pub custom: Vec<LockFile>,
}

/// One part version the repository uses.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockPart {
    pub slug: String,
    pub kind: String,
    pub version: String,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub digest: String,
    pub key_id: String,
    pub signature: String,
    /// `rule` / `user` / `llm` (the builder's choice, plan 09 §4).
    #[serde(default)]
    pub selected_by: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    pub files: Vec<LockFile>,
}

/// One file the lock pins.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockFile {
    pub path: String,
    pub sha256: String,
    /// For a `custom` file: who approved it (checked by the server, A7).
    #[serde(default)]
    pub approved_by: Option<String>,
}

impl Lock {
    /// Parse the lock; a malformed one is `FG-AGENT-0007 lock_invalid`.
    pub fn parse(bytes: &[u8]) -> Result<Lock, Problem> {
        let lock: Lock = serde_json::from_slice(bytes).map_err(|e| {
            Problem::new(
                ErrorCode::AgentLockMismatch,
                "lock_invalid",
                Some(format!("line {} column {}", e.line(), e.column())),
            )
        })?;
        if lock.schema_version != 1 {
            return Err(Problem::new(
                ErrorCode::AgentLockMismatch,
                "lock_version_unsupported",
                Some(lock.schema_version.to_string()),
            ));
        }
        if lock.parts.len() > MAX_LOCK_PARTS {
            return Err(Problem::new(
                ErrorCode::AgentLockMismatch,
                "lock_invalid",
                Some("too_many_parts".to_owned()),
            ));
        }
        Ok(lock)
    }
}

/// What [`verify_lock`] checked and found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LockReport {
    pub parts: usize,
    pub files: usize,
    pub problems: Vec<Problem>,
}

fn mismatch(reason: &'static str, subject: &str) -> Problem {
    Problem::about(ErrorCode::AgentLockMismatch, reason, subject)
}

/// Verify `lock` against `tree` (every managed file of the working tree:
/// path → bytes) with `keys`.
pub fn verify_lock(lock: &Lock, keys: &KeyRing, tree: &BTreeMap<String, Vec<u8>>) -> LockReport {
    let mut report = LockReport::default();
    let mut pinned: BTreeSet<&str> = BTreeSet::new();

    for part in &lock.parts {
        report.parts += 1;
        let label = format!("{}@{}", part.slug, part.version);
        verify_part(part, &label, keys, &mut report.problems);
        for f in &part.files {
            check_file(f, tree, &mut pinned, &mut report);
        }
    }
    for f in &lock.custom {
        check_file(f, tree, &mut pinned, &mut report);
    }
    for path in tree.keys() {
        if is_managed_path(path) && !pinned.contains(path.as_str()) {
            report.problems.push(mismatch("unmanaged_file", path));
        }
    }
    report
}

/// The part's own chain: manifest fields → digest → signature.
fn verify_part(part: &LockPart, label: &str, keys: &KeyRing, problems: &mut Vec<Problem>) {
    let paths: Vec<String> = part.files.iter().map(|f| f.path.clone()).collect();
    let manifest = match manifest_from_parts(
        &part.slug,
        &part.kind,
        &part.version,
        &part.requires,
        &part.capabilities,
        &paths,
    ) {
        Ok(m) => m,
        Err(mut found) => {
            problems.append(&mut found);
            return;
        }
    };
    let mut hashes = Vec::with_capacity(part.files.len());
    for f in &part.files {
        match unhex32(&f.sha256) {
            Some(h) => hashes.push((f.path.clone(), h)),
            None => {
                problems.push(mismatch("lock_invalid", &f.path));
                return;
            }
        }
    }
    let recomputed = digest(&manifest, &hashes);
    match Digest::parse(&part.digest) {
        None => {
            problems.push(mismatch("lock_invalid", label));
            return;
        }
        Some(d) if d != recomputed => {
            problems.push(Problem::about(
                ErrorCode::PartSignatureInvalid,
                "digest_mismatch",
                label,
            ));
            return;
        }
        Some(_) => {}
    }
    if let Err(p) = verify_signature(keys, &part.key_id, &recomputed, &part.signature) {
        let what = p.subject.unwrap_or_default();
        problems.push(Problem::new(
            p.code,
            p.reason,
            Some(format!("{label} key {what}")),
        ));
    }
}

fn check_file<'a>(
    f: &'a LockFile,
    tree: &BTreeMap<String, Vec<u8>>,
    pinned: &mut BTreeSet<&'a str>,
    report: &mut LockReport,
) {
    report.files += 1;
    if let Err(reason) = check_install_path(&f.path) {
        report.problems.push(Problem::about(
            ErrorCode::PartManifestInvalid,
            reason,
            &f.path,
        ));
        return;
    }
    if !pinned.insert(f.path.as_str()) {
        report.problems.push(mismatch("duplicate_path", &f.path));
        return;
    }
    let Some(expected) = unhex32(&f.sha256) else {
        report.problems.push(mismatch("lock_invalid", &f.path));
        return;
    };
    match tree.get(&f.path) {
        None => report.problems.push(mismatch("file_missing", &f.path)),
        Some(bytes) if sha256(bytes) != expected => {
            report.problems.push(mismatch("file_changed", &f.path));
        }
        Some(_) => {}
    }
}
