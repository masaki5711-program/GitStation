//! `edge-release.toml` (plan 21 §5.2, ADR-0041 §4): what a repository
//! releases to its edge devices.
//!
//! ```toml
//! schema_version = 1
//! arch = "amd64"                 # amd64 | arm64
//! os = "ubuntu-24.04"
//! max_install_seconds = 900      # 1..=7200
//! health_soak_seconds = 600      # 0..=86400
//!
//! [[payloads]]
//! kind = "oci-image"             # files | oci-image | agent
//! artifacts = ["dist/pfe-image"] # CI artifact paths of the tag's pipeline
//! ```
//!
//! The lint lists every problem (`FG-EDGE-0001`); unknown keys are refused.
//! Secrets never belong in a release: a path whose name looks like a
//! secret is refused here, and again by the release builder for every file
//! it expands (E4) and by the manifest rules.

use std::collections::BTreeSet;

use forge_errors::ErrorCode;
use serde::Deserialize;

use crate::manifest::{os_ok, MAX_HEALTH_SOAK_SECONDS, MAX_INSTALL_SECONDS};
use crate::{Arch, PayloadKind, Problem};

/// Most bytes of `edge-release.toml`.
pub const MAX_DEFINITION_BYTES: usize = 64 * 1024;
const MAX_ARTIFACTS: usize = 256;
const MAX_PATH_BYTES: usize = 255;

/// A linted release definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseDefinition {
    pub arch: Arch,
    pub os: String,
    pub max_install_seconds: u32,
    pub health_soak_seconds: u32,
    pub payloads: Vec<PayloadDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadDefinition {
    pub kind: PayloadKind,
    pub artifacts: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDefinition {
    schema_version: u32,
    arch: String,
    os: String,
    max_install_seconds: u32,
    health_soak_seconds: u32,
    #[serde(default)]
    payloads: Vec<RawPayload>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPayload {
    kind: String,
    #[serde(default)]
    artifacts: Vec<String>,
}

/// A relative path with `/` separators: no empty, `.` or `..` segment, no
/// leading `/`, drive letter, backslash, colon, control or non-ASCII
/// character, no trailing dot or space in a segment (Windows would name it
/// differently), at most 255 bytes. Why it failed is not needed: the
/// caller has one reason per field.
pub(crate) fn check_path(path: &str) -> Result<(), ()> {
    if path.is_empty() || path.len() > MAX_PATH_BYTES {
        return Err(());
    }
    let bytes_ok = path
        .bytes()
        .all(|b| (b.is_ascii_graphic() && b != b'\\' && b != b':') || b == b' ');
    if !bytes_ok {
        return Err(());
    }
    for seg in path.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." || seg.ends_with('.') || seg.ends_with(' ') {
            return Err(());
        }
    }
    Ok(())
}

/// Whether `path` (relative, `/`-separated) names what looks like a secret
/// (module docs; the list is FG-EDGE-0001's). A heuristic against
/// mistakes, not a guarantee: secrets belong to the device's provisioning.
pub fn is_secret_like_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let mut segs: Vec<&str> = lower.split('/').collect();
    let name = segs.pop().unwrap_or("");
    if segs
        .iter()
        .any(|d| matches!(*d, ".ssh" | ".gnupg" | ".aws" | ".docker"))
    {
        return true;
    }
    const NAMES: [&str; 6] = [
        ".netrc",
        ".pgpass",
        ".npmrc",
        ".htpasswd",
        ".git-credentials",
        ".env",
    ];
    const EXTS: [&str; 9] = [
        ".env",
        ".pem",
        ".key",
        ".p12",
        ".pfx",
        ".jks",
        ".keystore",
        ".ppk",
        ".kdbx",
    ];
    const PREFIXES: [&str; 6] = [
        ".env.",
        "id_rsa",
        "id_dsa",
        "id_ecdsa",
        "id_ed25519",
        "credentials",
    ];
    NAMES.contains(&name)
        || EXTS.iter().any(|e| name.ends_with(e))
        || PREFIXES.iter().any(|p| name.starts_with(p))
}

fn problem(reason: &'static str, subject: &str) -> Problem {
    Problem::about(ErrorCode::EdgeReleaseDefinitionInvalid, reason, subject)
}

/// Lint `edge-release.toml` (module docs): the definition, or every
/// problem found.
pub fn lint_release_definition(bytes: &[u8]) -> Result<ReleaseDefinition, Vec<Problem>> {
    if bytes.len() > MAX_DEFINITION_BYTES {
        return Err(vec![problem("too_large", "edge-release.toml")]);
    }
    let text =
        std::str::from_utf8(bytes).map_err(|_| vec![problem("not_utf8", "edge-release.toml")])?;
    let raw: RawDefinition = toml::from_str(text).map_err(|e| {
        let at = e
            .span()
            .and_then(|s| text.get(..s.start))
            .map_or(0, |t| t.matches('\n').count() + 1);
        vec![problem("toml_syntax", &format!("line {at}"))]
    })?;
    let mut problems = Vec::new();
    if raw.schema_version != 1 {
        problems.push(problem("schema_version", "schema_version"));
    }
    let arch = Arch::parse(&raw.arch);
    if arch.is_none() {
        problems.push(problem("arch", &raw.arch));
    }
    if !os_ok(&raw.os) {
        problems.push(problem("os", "os"));
    }
    if raw.max_install_seconds == 0 || raw.max_install_seconds > MAX_INSTALL_SECONDS {
        problems.push(problem("max_install_seconds", "max_install_seconds"));
    }
    if raw.health_soak_seconds > MAX_HEALTH_SOAK_SECONDS {
        problems.push(problem("health_soak_seconds", "health_soak_seconds"));
    }
    let payloads = lint_payloads(raw.payloads, &mut problems);
    match arch {
        Some(arch) if problems.is_empty() => Ok(ReleaseDefinition {
            arch,
            os: raw.os,
            max_install_seconds: raw.max_install_seconds,
            health_soak_seconds: raw.health_soak_seconds,
            payloads,
        }),
        Some(_) | None => Err(problems),
    }
}

fn lint_payloads(raw: Vec<RawPayload>, problems: &mut Vec<Problem>) -> Vec<PayloadDefinition> {
    if raw.is_empty() {
        problems.push(problem("no_payloads", "payloads"));
    }
    let mut kinds = BTreeSet::new();
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(raw.len());
    for p in raw {
        let Some(kind) = PayloadKind::parse(&p.kind) else {
            problems.push(problem("payload_kind", &p.kind));
            continue;
        };
        if !kinds.insert(kind) {
            problems.push(problem("duplicate_kind", kind.as_str()));
        }
        if p.artifacts.is_empty() {
            problems.push(problem("no_artifacts", kind.as_str()));
        }
        if p.artifacts.len() > MAX_ARTIFACTS {
            problems.push(problem("too_many_artifacts", kind.as_str()));
        }
        for a in p.artifacts.iter().take(MAX_ARTIFACTS) {
            if check_path(a).is_err() {
                problems.push(problem("artifact_path", a));
            } else if is_secret_like_path(a) {
                problems.push(problem("secret_like_path", a));
            } else if !seen.insert(a.to_ascii_lowercase()) {
                problems.push(problem("duplicate_artifact", a));
            }
        }
        out.push(PayloadDefinition {
            kind,
            artifacts: p.artifacts,
        });
    }
    if kinds.contains(&PayloadKind::Agent) && kinds.len() > 1 {
        problems.push(problem("agent_not_alone", "agent"));
    }
    out
}
