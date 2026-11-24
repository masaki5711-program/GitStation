//! The manifest digest (plan 21 §5.2, ADR-0041 §2): SHA-256 over a
//! canonical, length-prefixed encoding — the same construction as the
//! agent part digest (ADR-0032 §3), with its own domain string.
//!
//! ```text
//! "forge-edge-manifest-digest-v1\n"
//! u32 schema_version (1)
//! field(release_id) field(project) field(version) field(source_sha)
//! u64 sequence
//! i64 created_at seconds ‖ i32 created_at nanoseconds   (the instant, not its text)
//! field(arch) field(os)
//! u32 max_install_seconds ‖ u32 health_soak_seconds
//! u8 0 | u8 1 ‖ u64 ci_pipeline_id
//! u32 n_payloads { field(kind) u32 n_blobs { sha256[32] u64 size field(path) } }
//!   payloads sorted by kind; blobs sorted by (sha256, path bytes, size)
//! field(x) = u32 big-endian byte length ‖ x; integers big-endian
//! ```
//!
//! The order payloads and blobs are listed in never changes the digest;
//! any field, any blob hash, size or path does.

use crate::{sha256, Digest, Manifest, Payload};

const DOMAIN: &[u8] = b"forge-edge-manifest-digest-v1\n";

fn field(out: &mut Vec<u8>, bytes: &[u8]) {
    // Every string is bounded by the manifest rules (≤ 255 bytes); a
    // field past u32 cannot occur, and saturating keeps the encoding total.
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

fn count(out: &mut Vec<u8>, n: usize) {
    out.extend_from_slice(&u32::try_from(n).unwrap_or(u32::MAX).to_be_bytes());
}

/// The digest of a manifest (module docs).
pub fn manifest_digest(m: &Manifest) -> Digest {
    let blobs: usize = m.payloads.iter().map(|p| p.blobs.len()).sum();
    let mut out = Vec::with_capacity(512 + blobs * 300);
    out.extend_from_slice(DOMAIN);
    out.extend_from_slice(&1u32.to_be_bytes());
    field(&mut out, m.release_id.as_bytes());
    field(&mut out, m.project.as_bytes());
    field(&mut out, m.version.as_bytes());
    field(&mut out, m.source_sha.as_bytes());
    out.extend_from_slice(&m.sequence.to_be_bytes());
    out.extend_from_slice(&m.created_at.as_second().to_be_bytes());
    out.extend_from_slice(&m.created_at.subsec_nanosecond().to_be_bytes());
    field(&mut out, m.arch.as_str().as_bytes());
    field(&mut out, m.os.as_bytes());
    out.extend_from_slice(&m.max_install_seconds.to_be_bytes());
    out.extend_from_slice(&m.health_soak_seconds.to_be_bytes());
    match m.ci_pipeline_id {
        None => out.push(0),
        Some(id) => {
            out.push(1);
            out.extend_from_slice(&id.to_be_bytes());
        }
    }
    let mut payloads: Vec<&Payload> = m.payloads.iter().collect();
    // Kinds are unique in a parsed manifest (`duplicate_kind`), so this is
    // a total order.
    payloads.sort_unstable_by_key(|p| p.kind);
    count(&mut out, payloads.len());
    for p in payloads {
        field(&mut out, p.kind.as_str().as_bytes());
        let mut sorted: Vec<_> = p.blobs.iter().collect();
        sorted.sort_unstable_by(|a, b| {
            a.sha256
                .cmp(&b.sha256)
                .then_with(|| a.path.as_bytes().cmp(b.path.as_bytes()))
                .then(a.size.cmp(&b.size))
        });
        count(&mut out, sorted.len());
        for b in sorted {
            out.extend_from_slice(&b.sha256);
            out.extend_from_slice(&b.size.to_be_bytes());
            field(&mut out, b.path.as_bytes());
        }
    }
    Digest(sha256(&out))
}
