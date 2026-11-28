//! The `pfs-ai-bundle` release manifest (plan 17 P17-7 §1.8, ADR-0031
//! Amendment P17-7): what a release of a PFS AI asset repository ships,
//! its digest, and its Ed25519 signature with the agent signing key (plan
//! 16 A1: the same key, the same length-prefixed SHA-256 encoding).
//!
//! ```text
//! "forge-pfs-ai-bundle-digest-v1\n"
//! field(kind) field(project) field(version) field(commit)
//! u32 n_files  { field(path) sha256[32] u64 size }   sorted by path bytes
//! u32 n_models { field(model) }                       sorted, unique
//! field(bench.check) field(bench.conclusion) field(bench.summary)
//! field(x) = u32 big-endian byte length ‖ x
//! ```
//!
//! The signed message is [`PFS_AI_BUNDLE_DOMAIN`] ‖ the 32 digest bytes,
//! so a part signature (`forge-agent-part-signature-v1\0`) or an edge
//! manifest signature never verifies as a bundle, even with the same key
//! and the same 32 bytes. The manifest's JSON ([`BundleManifest::to_json`])
//! lists the files sorted, so a verifier (the PFS loader, PFS-5) recomputes
//! the digest from the JSON alone ([`parse_manifest`], [`verify_bundle`]).
//! Problems are `FG-AIASSET-0008` (the manifest) and the agent signature
//! codes (`FG-AGENT-0002` / `FG-AGENT-0003`).

use std::collections::BTreeSet;

use forge_errors::ErrorCode;
use serde::{Deserialize, Serialize};

use crate::sign::{sign_digest_in, verify_signature_in, KeyRing, SignatureDomain};
use crate::{hex, sha256, unhex32, Digest, Problem};

/// The release kind (`ai_bundle_releases.kind`, the manifest's `kind`).
pub const BUNDLE_KIND: &str = "pfs-ai-bundle";

/// The prefix of every bundle signature's message.
pub const PFS_AI_BUNDLE_DOMAIN: SignatureDomain =
    SignatureDomain::new(b"forge-pfs-ai-bundle-signature-v1\0");

const _: () = assert!(PFS_AI_BUNDLE_DOMAIN.is_well_formed());

const DIGEST_DOMAIN: &[u8] = b"forge-pfs-ai-bundle-digest-v1\n";

/// The directories a bundle ships (plan 17 §2): never `evals/` (the bench's
/// data), `thresholds/` (a person's decision in PFS) or `manifest/` (the
/// server's).
pub const BUNDLE_DIRS: [&str; 6] = ["skills", "playbooks", "agents", "tools", "models", "bench"];

/// At most this many files in a bundle.
pub const MAX_BUNDLE_FILES: usize = 2000;
/// At most this many bytes of files in a bundle.
pub const MAX_BUNDLE_BYTES: u64 = 32 * 1024 * 1024;
/// The bench summary is cut to this many bytes (on a char boundary).
pub const MAX_BENCH_SUMMARY: usize = 4096;

/// One shipped file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleFile {
    /// From the repository's root, `/`-separated, under a [`BUNDLE_DIRS`]
    /// directory.
    pub path: String,
    pub sha256: [u8; 32],
    pub size: u64,
}

/// The release candidate's bench (`forge/ai-eval` on the commit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleBench {
    pub check: String,
    pub conclusion: String,
    pub summary: String,
}

/// What a `pfs-ai-bundle` release ships.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleManifest {
    /// The project's full path (`pfs-ai/assets`).
    pub project: String,
    /// `assets/v<yyyy>.<ww>.<n>` without `assets/`: `v2026.48.1`.
    pub version: String,
    /// The commit the tag named when the release was made (lowercase hex).
    pub commit: String,
    /// Sorted by path bytes, unique.
    pub files: Vec<BundleFile>,
    /// `name:tag@sha256:<hex>` of `models/models.toml`, sorted, unique.
    pub models: Vec<String>,
    pub bench: BundleBench,
}

fn invalid(reason: &'static str, subject: Option<String>) -> Problem {
    Problem::new(ErrorCode::BundleReleaseRefused, reason, subject)
}

fn field(out: &mut Vec<u8>, bytes: &[u8]) {
    // Every field is bounded far below u32 (paths, versions, the cut
    // summary); saturating keeps the encoding total.
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

fn count(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(&u32::try_from(n).unwrap_or(u32::MAX).to_be_bytes());
}

/// `summary` cut to [`MAX_BENCH_SUMMARY`] bytes on a char boundary.
pub fn cut_summary(summary: &str) -> String {
    if summary.len() <= MAX_BENCH_SUMMARY {
        return summary.to_owned();
    }
    let mut end = MAX_BENCH_SUMMARY;
    while !summary.is_char_boundary(end) {
        end -= 1;
    }
    summary[..end].to_owned()
}

/// Whether `version` is `v<yyyy>.<ww>.<n>` (plan 17 §1.8: weekly
/// `assets/vYYYY.WW.N`, hotfixes in the same form): a four-digit year,
/// a week 01..=53 and a number 1..=999 without leading zeros.
pub fn is_bundle_version(version: &str) -> bool {
    let Some(rest) = version.strip_prefix('v') else {
        return false;
    };
    let parts: Vec<&str> = rest.split('.').collect();
    let [year, week, n] = parts.as_slice() else {
        return false;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let year_ok = year.len() == 4 && digits(year) && !year.starts_with('0');
    let week_ok =
        week.len() == 2 && digits(week) && week.parse::<u8>().is_ok_and(|w| (1..=53).contains(&w));
    let n_ok = digits(n) && n.len() <= 3 && !n.starts_with('0');
    year_ok && week_ok && n_ok
}

/// The version of a release tag `assets/v<yyyy>.<ww>.<n>` (`None`: not one).
pub fn version_of_tag(tag: &str) -> Option<&str> {
    tag.strip_prefix("assets/").filter(|v| is_bundle_version(v))
}

/// One path segment a loader on any OS writes as itself (review P17-7):
/// ASCII letters, digits, `.`, `_`, `-` only (no `\`, `:`, drive letters,
/// control characters or spaces), never `.` or `..`, never ending in `.`
/// (Windows drops it), at most 255 bytes.
fn segment_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && s != "."
        && s != ".."
        && !s.ends_with('.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn in_bundle_dir(path: &str) -> bool {
    path.split_once('/').is_some_and(|(first, rest)| {
        BUNDLE_DIRS.contains(&first) && !rest.is_empty() && rest.split('/').all(segment_ok)
    })
}

fn is_commit(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl BundleManifest {
    /// Sort the files and models (the canonical order) and check the
    /// shape: a version, a commit, files under the bundle directories,
    /// unique, within the bounds; at least one file.
    pub fn canonical(mut self) -> Result<BundleManifest, Problem> {
        if !is_bundle_version(&self.version) {
            return Err(invalid("tag_invalid", Some(self.version)));
        }
        if !is_commit(&self.commit) {
            return Err(invalid("manifest_invalid", Some("commit".to_owned())));
        }
        if self.project.is_empty() || self.project.len() > 255 {
            return Err(invalid("manifest_invalid", Some("project".to_owned())));
        }
        if self.files.is_empty() {
            return Err(invalid("bundle_empty", None));
        }
        if self.files.len() > MAX_BUNDLE_FILES {
            return Err(invalid(
                "too_many_files",
                Some(self.files.len().to_string()),
            ));
        }
        // Checked: the sizes of a manifest given to `parse_manifest` are
        // untrusted (review P17-7).
        self.files
            .iter()
            .try_fold(0u64, |acc, f| acc.checked_add(f.size))
            .filter(|t| *t <= MAX_BUNDLE_BYTES)
            .ok_or_else(|| invalid("too_large", None))?;
        self.files
            .sort_unstable_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        // Unique, ignoring ASCII case: a case-insensitive file system would
        // write two such files as one.
        let mut seen = BTreeSet::new();
        for f in &self.files {
            if !seen.insert(f.path.to_ascii_lowercase()) {
                return Err(invalid("manifest_invalid", Some(f.path.clone())));
            }
        }
        if let Some(f) = self
            .files
            .iter()
            .find(|f| !in_bundle_dir(&f.path) || f.path.len() > 1024)
        {
            return Err(invalid("manifest_invalid", Some(f.path.clone())));
        }
        let models: BTreeSet<String> = self.models.into_iter().collect();
        self.models = models.into_iter().collect();
        self.bench.summary = cut_summary(&self.bench.summary);
        Ok(self)
    }

    /// The digest (module docs). Call on a [`canonical`](Self::canonical)
    /// manifest; the files are sorted here again so it never depends on
    /// the order they were given in.
    pub fn digest(&self) -> Digest {
        let mut out = Vec::with_capacity(512 + self.files.len() * 160);
        out.extend_from_slice(DIGEST_DOMAIN);
        field(&mut out, BUNDLE_KIND.as_bytes());
        field(&mut out, self.project.as_bytes());
        field(&mut out, self.version.as_bytes());
        field(&mut out, self.commit.as_bytes());
        let mut files: Vec<&BundleFile> = self.files.iter().collect();
        files.sort_unstable_by(|a, b| {
            a.path
                .as_bytes()
                .cmp(b.path.as_bytes())
                .then(a.sha256.cmp(&b.sha256))
        });
        count(&mut out, files.len());
        for f in files {
            field(&mut out, f.path.as_bytes());
            out.extend_from_slice(&f.sha256);
            out.extend_from_slice(&f.size.to_be_bytes());
        }
        let models: BTreeSet<&str> = self.models.iter().map(String::as_str).collect();
        count(&mut out, models.len());
        for m in models {
            field(&mut out, m.as_bytes());
        }
        field(&mut out, self.bench.check.as_bytes());
        field(&mut out, self.bench.conclusion.as_bytes());
        field(&mut out, self.bench.summary.as_bytes());
        Digest(sha256(&out))
    }

    /// The manifest as JSON (the stored `manifest`, what the release API
    /// answers and the loader verifies).
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "kind": BUNDLE_KIND,
            "schema_version": 1,
            "project": self.project,
            "version": self.version,
            "commit": self.commit,
            "files": self.files.iter().map(|f| serde_json::json!({
                "path": f.path,
                "sha256": hex(&f.sha256),
                "size": f.size,
            })).collect::<Vec<_>>(),
            "models": self.models,
            "bench": {
                "check": self.bench.check,
                "conclusion": self.bench.conclusion,
                "summary": self.bench.summary,
            },
        })
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    path: String,
    sha256: String,
    size: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawBench {
    check: String,
    conclusion: String,
    summary: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    kind: String,
    schema_version: u32,
    project: String,
    version: String,
    commit: String,
    files: Vec<RawFile>,
    models: Vec<String>,
    bench: RawBench,
}

/// Parse a manifest's JSON (unknown keys refused, the kind and schema
/// checked, then [`BundleManifest::canonical`]).
pub fn parse_manifest(bytes: &[u8]) -> Result<BundleManifest, Problem> {
    let raw: RawManifest = serde_json::from_slice(bytes)
        .map_err(|e| invalid("manifest_invalid", Some(format!("line {}", e.line()))))?;
    if raw.kind != BUNDLE_KIND || raw.schema_version != 1 {
        return Err(invalid("manifest_invalid", Some("kind".to_owned())));
    }
    let mut files = Vec::with_capacity(raw.files.len());
    for f in raw.files {
        let Some(sha) = unhex32(&f.sha256) else {
            return Err(invalid("manifest_invalid", Some(f.path)));
        };
        files.push(BundleFile {
            path: f.path,
            sha256: sha,
            size: f.size,
        });
    }
    let parsed = BundleManifest {
        project: raw.project,
        version: raw.version,
        commit: raw.commit,
        files,
        models: raw.models,
        bench: BundleBench {
            check: raw.bench.check,
            conclusion: raw.bench.conclusion,
            summary: raw.bench.summary,
        },
    };
    let canonical = parsed.clone().canonical()?;
    // A manifest whose lists were not already canonical (unsorted, a cut
    // summary) is not the one that was signed.
    if canonical != parsed {
        return Err(invalid(
            "manifest_invalid",
            Some("not_canonical".to_owned()),
        ));
    }
    Ok(canonical)
}

/// Sign a bundle `digest` in [`PFS_AI_BUNDLE_DOMAIN`] with the Ed25519 seed
/// (base64 of the 64-byte signature). For the release step of
/// forge-deploy (after the authorized release decision) and tests.
// The signed kind's own wrapper of the domain signer (clippy.toml).
#[allow(clippy::disallowed_methods)]
pub fn sign_bundle(seed: &[u8; 32], digest: &Digest) -> Result<String, Problem> {
    sign_digest_in(PFS_AI_BUNDLE_DOMAIN, seed, digest)
}

/// Verify a release: the manifest's JSON parses canonically, its
/// recomputed digest is `digest` (`FG-AGENT-0002 digest_mismatch`), and
/// `signature` by `kid` verifies in the bundle domain.
pub fn verify_bundle(
    manifest: &[u8],
    digest: &str,
    kid: &str,
    signature: &str,
    keys: &KeyRing,
) -> Result<BundleManifest, Problem> {
    let parsed = parse_manifest(manifest)?;
    let computed = parsed.digest();
    if Digest::parse(digest) != Some(computed) {
        return Err(Problem::about(
            ErrorCode::PartSignatureInvalid,
            "digest_mismatch",
            &parsed.version,
        ));
    }
    verify_signature_in(PFS_AI_BUNDLE_DOMAIN, keys, kid, &computed, signature)?;
    Ok(parsed)
}

#[cfg(test)]
#[path = "bundle_tests.rs"]
mod tests;
