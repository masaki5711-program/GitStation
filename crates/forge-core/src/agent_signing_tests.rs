#![allow(clippy::expect_used, clippy::unwrap_used)] // tests may fail loudly

use super::*;

/// Synthetic seeds (never real keys).
const SEED_1: &str = "0101010101010101010101010101010101010101010101010101010101010101";
const SEED_2: &str = "0202020202020202020202020202020202020202020202020202020202020202";

fn reason(e: &ForgeError) -> String {
    assert_eq!(e.code(), ErrorCode::SigningKeyUnavailable, "{e}");
    e.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone())
        .expect("reason")
}

fn context(e: &ForgeError, key: &str) -> Option<String> {
    e.context()
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.clone())
}

#[test]
fn the_highest_kid_signs_and_the_signature_verifies() {
    let keys =
        AgentSigningKeys::parse(&format!("# rotation: 2 is new\n\n2 {SEED_2}\n1 {SEED_1}\n"))
            .expect("keys");
    assert_eq!(keys.signing_kid(), "2");
    let public: Vec<String> = keys.public_keys().into_iter().map(|(k, _)| k).collect();
    assert_eq!(public, ["2", "1"]);
    let digest = forge_agent::Digest([7u8; 32]);
    let (kid, signature) = keys.sign(&digest).expect("sign");
    assert_eq!(kid, "2");
    let ring = forge_agent::KeyRing {
        keys: keys
            .public_keys()
            .into_iter()
            .map(|(kid, key)| {
                use base64::Engine as _;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(key)
                    .expect("base64");
                forge_agent::PublicKey {
                    kid,
                    key: <[u8; 32]>::try_from(bytes.as_slice()).expect("32 bytes"),
                    revoked: false,
                }
            })
            .collect(),
    };
    forge_agent::verify_signature(&ring, &kid, &digest, &signature).expect("verifies");
    // Never the seed in Debug.
    let shown = format!("{keys:?}");
    assert!(!shown.contains("0202"), "{shown}");
}

#[test]
fn malformed_key_files_are_refused_without_their_content() {
    for (text, rule) in [
        ("", "empty"),
        ("# only a comment\n", "empty"),
        ("1", "invalid_line"),
        (&format!("1 {SEED_1} extra") as &str, "invalid_line"),
        (&format!("0 {SEED_1}"), "invalid_key_id"),
        (&format!("01 {SEED_1}"), "invalid_key_id"),
        (&format!("65536 {SEED_1}"), "invalid_key_id"),
        (&format!("k1 {SEED_1}"), "invalid_key_id"),
        ("1 0101", "invalid_key"),
        (&format!("1 {SEED_1}zz"), "invalid_key"),
        (&format!("1 {SEED_1}\n1 {SEED_2}"), "duplicate"),
    ] {
        let err = AgentSigningKeys::parse(text).expect_err(rule);
        assert_eq!(reason(&err), "keys_invalid", "{text}");
        assert_eq!(context(&err, "rule").as_deref(), Some(rule), "{text}");
        assert!(!err.to_string().contains("0101010101"), "{err}");
    }
}

/// Plan 16 A3: a key file readable by group or others is refused with
/// FG-AGENT-0003 `insecure_permissions` (the secret file reader's check,
/// Unix only — on Windows the ACL is the operator's, ADR-0013), and so is
/// a relative or missing path (`keys_unreadable`).
#[test]
fn key_file_insecure_permissions_refused() {
    // The reader's refusal becomes FG-AGENT-0003 on every platform.
    let refused = from_secret_error(
        ForgeError::new(ErrorCode::ConfigInvalid)
            .with_context("config_key", SIGNING_KEYS_KEY)
            .with_context("reason", "insecure_permissions")
            .with_context("path", "/etc/forge/agent-signing.keys")
            .with_context("mode", "640"),
    );
    assert_eq!(reason(&refused), "insecure_permissions");
    assert_eq!(context(&refused, "mode").as_deref(), Some("640"));
    assert_eq!(
        context(&refused, "config_key").as_deref(),
        Some(SIGNING_KEYS_KEY)
    );
    let err = AgentSigningKeys::read(std::path::Path::new("relative.keys")).expect_err("relative");
    assert_eq!(reason(&err), "keys_unreadable");
    assert_eq!(
        context(&err, "config_reason").as_deref(),
        Some("not_absolute")
    );
    let dir = tempfile::tempdir().expect("dir");
    let missing = dir.path().join("missing.keys");
    let err = AgentSigningKeys::read(&missing).expect_err("missing");
    assert_eq!(
        context(&err, "config_reason").as_deref(),
        Some("unreadable")
    );

    let path = dir.path().join("agent-signing.keys");
    std::fs::write(&path, format!("1 {SEED_1}\n")).expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        for mode in [0o640, 0o604, 0o644] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
            let err = AgentSigningKeys::read(&path).expect_err("readable by others");
            assert_eq!(reason(&err), "insecure_permissions");
            assert!(!err.to_string().contains("0101010101"), "{err}");
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
    let keys = AgentSigningKeys::read(&path).expect("owner only");
    assert_eq!(keys.signing_kid(), "1");
}
