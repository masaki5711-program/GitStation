//! The part digest (plan 16 §2.2, ADR-0032 §3): SHA-256 over a canonical,
//! length-prefixed encoding of the manifest and every file's path and
//! SHA-256, files sorted by path. The order in which files were listed in
//! `part.toml` or read from disk never changes it; any byte of any file,
//! a renamed file, or a changed manifest field does.
//!
//! ```text
//! "forge-agent-digest-v1\n"
//! field(slug) field(kind) field(version)
//! u32 n_requires     { field(require) }        sorted, unique
//! u32 n_capabilities { field(capability) }     sorted, unique
//! u32 n_files        { field(path) sha256[32] } sorted by path bytes
//! field(x) = u32 big-endian byte length ‖ x
//! ```

use std::fmt;

use crate::{hex, sha256, unhex32, Manifest};

const DOMAIN: &[u8] = b"forge-agent-digest-v1\n";

/// A part digest; shown as `sha256:<64 hex>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Digest(pub [u8; 32]);

impl Digest {
    pub fn parse(s: &str) -> Option<Digest> {
        s.strip_prefix("sha256:").and_then(unhex32).map(Digest)
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sha256:{}", hex(&self.0))
    }
}

fn field(out: &mut Vec<u8>, bytes: &[u8]) {
    // A path is at most 255 bytes and a slug or version far less; a field
    // past u32 cannot occur, and saturating keeps the encoding total.
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

fn count(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(&u32::try_from(n).unwrap_or(u32::MAX).to_be_bytes());
}

/// The digest of a part: its manifest and its files' `(path, sha256)`, in
/// any order.
pub fn digest(manifest: &Manifest, files: &[(String, [u8; 32])]) -> Digest {
    let mut out = Vec::with_capacity(256 + files.len() * 300);
    out.extend_from_slice(DOMAIN);
    field(&mut out, manifest.slug.as_bytes());
    field(&mut out, manifest.kind.as_str().as_bytes());
    field(&mut out, manifest.version.as_bytes());

    let mut requires: Vec<&str> = manifest.requires.iter().map(String::as_str).collect();
    requires.sort_unstable();
    requires.dedup();
    count(&mut out, requires.len());
    for r in requires {
        field(&mut out, r.as_bytes());
    }

    // A BTreeSet: already sorted and unique.
    count(&mut out, manifest.capabilities.len());
    for c in &manifest.capabilities {
        field(&mut out, c.as_str().as_bytes());
    }

    let mut sorted: Vec<&(String, [u8; 32])> = files.iter().collect();
    // Duplicates are refused by both lints before this; if one reaches here
    // it is encoded (sorted by path, then hash), never dropped.
    sorted.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()).then(a.1.cmp(&b.1)));
    count(&mut out, sorted.len());
    for (path, hash) in sorted {
        field(&mut out, path.as_bytes());
        out.extend_from_slice(hash);
    }
    Digest(sha256(&out))
}
