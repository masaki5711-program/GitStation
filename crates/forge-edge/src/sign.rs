//! Edge release signatures (plan 21 §5.2, ADR-0041 §3): Ed25519 over the
//! manifest digest in the edge domain, through forge-agent's signer.
//!
//! The signed message is [`EDGE_MANIFEST_DOMAIN`] ‖ the 32 digest bytes. A
//! signature made for an agent part (`forge-agent-part-signature-v1\0` ‖
//! digest) or in any other domain never verifies as an edge manifest, even
//! with the same key and the same 32 bytes — and the edge keys are a list of
//! their own (`edge-keys.json`, `edge.signing_keys_file`), so an agent key
//! is not even looked up.
//!
//! forge-agent's problems are mapped to the edge codes by their stable
//! `reason`: `FG-AGENT-0002` → `FG-EDGE-0004`, `FG-AGENT-0003` →
//! `FG-EDGE-0003`. Nothing falls back to "unsigned is fine".

use forge_agent::{sign_digest_in, verify_signature_in, SignatureDomain};
use forge_errors::ErrorCode;

use crate::{manifest_digest, parse_release, Digest, KeyRing, Manifest, Problem};

/// The prefix of every edge manifest signature's message.
pub const EDGE_MANIFEST_DOMAIN: SignatureDomain =
    SignatureDomain::new(b"forge-edge-manifest-signature-v1\0");

const _: () = assert!(EDGE_MANIFEST_DOMAIN.is_well_formed());

/// A release whose digest and signature verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedRelease {
    pub manifest: Manifest,
    pub digest: Digest,
    pub key_id: String,
}

/// forge-agent's signature problem as the edge code with the same reason.
fn edge(p: forge_agent::Problem) -> Problem {
    let code = if p.code == ErrorCode::PartSignatureInvalid {
        ErrorCode::EdgeSignatureInvalid
    } else {
        // SigningKeyUnavailable, and anything else the signer may add:
        // the key side, never a pass.
        ErrorCode::EdgeSigningKeyUnavailable
    };
    Problem::new(code, p.reason, p.subject)
}

/// Parse the edge public key list (the agent key list's JSON form:
/// `{"keys":[{"kid","public_key",…}]}`); problems are `FG-EDGE-0003`.
pub fn parse_edge_keys(bytes: &[u8]) -> Result<KeyRing, Problem> {
    KeyRing::parse(bytes).map_err(edge)
}

/// Parse and verify a release file: the manifest's rules
/// (`FG-EDGE-0002`), the recomputed digest against the envelope's
/// (`FG-EDGE-0004 digest_mismatch`), then the signature by `key_id` in the
/// edge domain (`FG-EDGE-0004` / `FG-EDGE-0003`).
pub fn verify_release(bytes: &[u8], keys: &KeyRing) -> Result<VerifiedRelease, Problem> {
    let release = parse_release(bytes)?;
    let digest = manifest_digest(&release.manifest);
    if digest != release.digest {
        return Err(Problem::about(
            ErrorCode::EdgeSignatureInvalid,
            "digest_mismatch",
            &release.manifest.release_id,
        ));
    }
    verify_signature_in(
        EDGE_MANIFEST_DOMAIN,
        keys,
        &release.key_id,
        &digest,
        &release.signature,
    )
    .map_err(edge)?;
    Ok(VerifiedRelease {
        manifest: release.manifest,
        digest,
        key_id: release.key_id,
    })
}

/// Sign a manifest digest in the edge domain with the Ed25519 seed (base64
/// of the 64-byte signature). Only forge-fleet's release builder signs
/// (E4; `clippy.toml` refuses every other caller); tests sign with a
/// synthetic seed.
// The signed kind's own wrapper of the domain signer (clippy.toml).
#[allow(clippy::disallowed_methods)]
pub fn sign_manifest(seed: &[u8; 32], digest: &Digest) -> Result<String, Problem> {
    sign_digest_in(EDGE_MANIFEST_DOMAIN, seed, digest).map_err(edge)
}
