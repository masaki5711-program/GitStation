//! The signed release file and its manifest (plan 21 §5.2, ADR-0041 §2).
//!
//! ```json
//! {"manifest": {"schema_version": 1, "release_id": "…", "project": "…",
//!   "version": "…", "source_sha": "<40|64 hex>", "sequence": 7,
//!   "created_at": "2026-11-24T03:00:00Z", "arch": "amd64",
//!   "os": "ubuntu-24.04", "payloads": [{"kind": "oci-image",
//!   "blobs": [{"sha256": "<64 hex>", "size": 123, "path": "…"}]}],
//!   "max_install_seconds": 900, "health_soak_seconds": 600,
//!   "ci_pipeline_id": 42},
//!  "digest": "sha256:<64 hex>", "key_id": "k1", "signature": "<base64>"}
//! ```
//!
//! Unknown and duplicate keys are refused (nothing is ignored). Every
//! string has a bound and an alphabet; blob paths follow the artifact path
//! rules of `edge-release.toml` and are unique in the whole release (also
//! by letter case, and no path is a directory of another), so the device
//! can lay them out under one directory. The order of payloads and blobs is
//! not signed: a parsed [`Manifest`] always comes back in the digest's
//! canonical order (payloads by kind, blobs by SHA-256, path, size).

use std::collections::BTreeSet;

use forge_errors::ErrorCode;
use jiff::Timestamp;
use serde::Deserialize;

use crate::release_def::{check_path, is_secret_like_path, path_conflicts};
use crate::{unhex32, Digest, Problem};

/// Most bytes of a release file a caller should read (and [`parse_release`]
/// accepts).
pub const MAX_RELEASE_BYTES: usize = 4 * 1024 * 1024;
/// Most blobs of one payload.
pub const MAX_BLOBS_PER_PAYLOAD: usize = 4096;
/// Largest blob (8 GiB).
pub const MAX_BLOB_SIZE: u64 = 8 * 1024 * 1024 * 1024;
/// Longest install a release may ask for (2 hours).
pub(crate) const MAX_INSTALL_SECONDS: u32 = 7200;
/// Longest health soak (1 day).
pub(crate) const MAX_HEALTH_SOAK_SECONDS: u32 = 86_400;

/// The CPU architecture a release is built for (plan 21 §11 Q2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Arch {
    Amd64,
    Arm64,
}

impl Arch {
    pub fn as_str(self) -> &'static str {
        match self {
            Arch::Amd64 => "amd64",
            Arch::Arm64 => "arm64",
        }
    }

    pub fn parse(s: &str) -> Option<Arch> {
        match s {
            "amd64" => Some(Arch::Amd64),
            "arm64" => Some(Arch::Arm64),
            _ => None,
        }
    }
}

/// What a payload is and how the device installs it (plan 21 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PayloadKind {
    /// A set of files handed to the `install` hook.
    Files,
    /// An OCI image layout; each layer is a blob.
    OciImage,
    /// A device agent itself (one binary; a release of its own). Unused since
    /// the 2026-11-25 re-scope; a v2 manifest (optional E4) drops it.
    Agent,
}

impl PayloadKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PayloadKind::Files => "files",
            PayloadKind::OciImage => "oci-image",
            PayloadKind::Agent => "agent",
        }
    }

    pub fn parse(s: &str) -> Option<PayloadKind> {
        match s {
            "files" => Some(PayloadKind::Files),
            "oci-image" => Some(PayloadKind::OciImage),
            "agent" => Some(PayloadKind::Agent),
            _ => None,
        }
    }
}

/// One content-addressed blob and where it goes in the release directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    pub sha256: [u8; 32],
    pub size: u64,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payload {
    pub kind: PayloadKind,
    pub blobs: Vec<Blob>,
}

/// A validated manifest (`schema_version` 1), in canonical order. Built by
/// [`parse_release`] only: a hand-built one with two payloads of one kind
/// would not have an order-independent digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub release_id: String,
    pub project: String,
    pub version: String,
    pub source_sha: String,
    /// Grows within a project; an installer must never take a lower one as
    /// an upgrade.
    pub sequence: u64,
    pub created_at: Timestamp,
    pub arch: Arch,
    pub os: String,
    pub payloads: Vec<Payload>,
    pub max_install_seconds: u32,
    pub health_soak_seconds: u32,
    pub ci_pipeline_id: Option<u64>,
}

/// A parsed release file: the manifest and what the server says it signed.
/// Nothing here is trusted until [`crate::verify_release`] says so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub manifest: Manifest,
    pub digest: Digest,
    pub key_id: String,
    pub signature: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRelease {
    manifest: RawManifest,
    digest: String,
    key_id: String,
    signature: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawManifest {
    schema_version: u32,
    release_id: String,
    project: String,
    version: String,
    source_sha: String,
    sequence: u64,
    created_at: String,
    arch: String,
    os: String,
    payloads: Vec<RawPayload>,
    max_install_seconds: u32,
    health_soak_seconds: u32,
    #[serde(default)]
    ci_pipeline_id: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPayload {
    kind: String,
    blobs: Vec<RawBlob>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBlob {
    sha256: String,
    size: u64,
    path: String,
}

fn bad(reason: &'static str, subject: &str) -> Problem {
    Problem::about(ErrorCode::EdgeManifestInvalid, reason, subject)
}

/// `s` is 1..=`max` bytes, every one accepted by `ok`.
fn token(s: &str, max: usize, ok: fn(u8) -> bool) -> bool {
    !s.is_empty() && s.len() <= max && s.bytes().all(ok)
}

fn id_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')
}

fn version_byte(b: u8) -> bool {
    id_byte(b) || b == b'+'
}

fn project_ok(s: &str) -> bool {
    token(s, 255, |b| id_byte(b) || b == b'/')
        && !s.starts_with('/')
        && !s.ends_with('/')
        && !s.contains("//")
}

pub(crate) fn os_ok(s: &str) -> bool {
    token(s, 32, |b| {
        b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-')
    })
}

fn sha_ok(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64)
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A key id as the key list allows it: 1..=64 printable ASCII.
pub(crate) fn kid_ok(s: &str) -> bool {
    token(s, 64, |b| b.is_ascii_graphic())
}

/// Parse a release file (module docs). The manifest is validated; the
/// digest and signature are only parsed — [`crate::verify_release`] checks
/// them.
pub fn parse_release(bytes: &[u8]) -> Result<Release, Problem> {
    if bytes.len() > MAX_RELEASE_BYTES {
        return Err(bad("too_large", "release"));
    }
    let raw: RawRelease = serde_json::from_slice(bytes)
        .map_err(|e| bad("json_syntax", &format!("line {}", e.line())))?;
    let manifest = validate(raw.manifest)?;
    let digest = Digest::parse(&raw.digest).ok_or_else(|| bad("digest_syntax", "digest"))?;
    if !kid_ok(&raw.key_id) {
        return Err(bad("key_id", "key_id"));
    }
    Ok(Release {
        manifest,
        digest,
        key_id: raw.key_id,
        signature: raw.signature,
    })
}

fn validate(raw: RawManifest) -> Result<Manifest, Problem> {
    if raw.schema_version != 1 {
        return Err(bad("schema_version", "schema_version"));
    }
    if !token(&raw.release_id, 64, id_byte) {
        return Err(bad("release_id", "release_id"));
    }
    if !project_ok(&raw.project) {
        return Err(bad("project", "project"));
    }
    if !token(&raw.version, 64, version_byte) {
        return Err(bad("version", "version"));
    }
    if !sha_ok(&raw.source_sha) {
        return Err(bad("source_sha", "source_sha"));
    }
    if raw.sequence == 0 {
        return Err(bad("sequence", "sequence"));
    }
    let created_at: Timestamp = raw
        .created_at
        .parse()
        .map_err(|_| bad("created_at", "created_at"))?;
    let arch = Arch::parse(&raw.arch).ok_or_else(|| bad("arch", "arch"))?;
    if !os_ok(&raw.os) {
        return Err(bad("os", "os"));
    }
    if raw.max_install_seconds == 0 || raw.max_install_seconds > MAX_INSTALL_SECONDS {
        return Err(bad("max_install_seconds", "max_install_seconds"));
    }
    if raw.health_soak_seconds > MAX_HEALTH_SOAK_SECONDS {
        return Err(bad("health_soak_seconds", "health_soak_seconds"));
    }
    if raw.ci_pipeline_id == Some(0) {
        return Err(bad("ci_pipeline_id", "ci_pipeline_id"));
    }
    let payloads = payloads(raw.payloads)?;
    Ok(Manifest {
        release_id: raw.release_id,
        project: raw.project,
        version: raw.version,
        source_sha: raw.source_sha,
        sequence: raw.sequence,
        created_at,
        arch,
        os: raw.os,
        payloads,
        max_install_seconds: raw.max_install_seconds,
        health_soak_seconds: raw.health_soak_seconds,
        ci_pipeline_id: raw.ci_pipeline_id,
    })
}

fn payloads(raw: Vec<RawPayload>) -> Result<Vec<Payload>, Problem> {
    if raw.is_empty() {
        return Err(bad("no_payloads", "payloads"));
    }
    let mut kinds = BTreeSet::new();
    // Case-folded paths of the whole release: one directory on the device.
    let mut paths = BTreeSet::new();
    let mut out = Vec::with_capacity(raw.len());
    for p in raw {
        let kind = PayloadKind::parse(&p.kind).ok_or_else(|| bad("payload_kind", &p.kind))?;
        if !kinds.insert(kind) {
            return Err(bad("duplicate_kind", kind.as_str()));
        }
        if p.blobs.is_empty() {
            return Err(bad("no_blobs", kind.as_str()));
        }
        if p.blobs.len() > MAX_BLOBS_PER_PAYLOAD {
            return Err(bad("too_many_blobs", kind.as_str()));
        }
        if kind == PayloadKind::Agent && p.blobs.len() != 1 {
            return Err(bad("agent_blob_count", kind.as_str()));
        }
        let mut blobs = Vec::with_capacity(p.blobs.len());
        for b in p.blobs {
            check_path(&b.path).map_err(|_| bad("blob_path", &b.path))?;
            if is_secret_like_path(&b.path) {
                return Err(bad("secret_like_path", &b.path));
            }
            let folded = b.path.to_ascii_lowercase();
            if paths.contains(&folded) {
                return Err(bad("duplicate_blob_path", &b.path));
            }
            if path_conflicts(&paths, &folded) {
                return Err(bad("blob_path_conflict", &b.path));
            }
            paths.insert(folded);
            let sha256 = unhex32(&b.sha256).ok_or_else(|| bad("blob_sha256", &b.path))?;
            if b.size == 0 || b.size > MAX_BLOB_SIZE {
                return Err(bad("blob_size", &b.path));
            }
            blobs.push(Blob {
                sha256,
                size: b.size,
                path: b.path,
            });
        }
        // The canonical order, the digest's: the order a file lists blobs
        // in is not signed, so nobody downstream may depend on it.
        blobs.sort_unstable_by(|a, b| {
            a.sha256
                .cmp(&b.sha256)
                .then_with(|| a.path.as_bytes().cmp(b.path.as_bytes()))
                .then(a.size.cmp(&b.size))
        });
        out.push(Payload { kind, blobs });
    }
    if kinds.contains(&PayloadKind::Agent) && kinds.len() > 1 {
        return Err(bad("agent_not_alone", "agent"));
    }
    out.sort_unstable_by_key(|p| p.kind);
    Ok(out)
}
