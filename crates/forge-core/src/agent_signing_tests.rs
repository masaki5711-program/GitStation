#![allow(clippy::expect_used, clippy::unwrap_used)] // tests may fail loudly

use super::*;

/// Synthetic seeds (never real keys).
const SEED_1: &str = "0101010101010101010101010101010101010101010101010101010101010101";
const SEED_2: &str = "0202020202020202020202020202020202020202020202020202020202020202";

fn reason(e: &ForgeError) -> String {
    assert_eq!(e.code(), ErrorCode::SigningKeyFileInvalid, "{e}");
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

fn seed(hex: &str) -> [u8; 32] {
    <[u8; 32]>::try_from(crate::config::decode_hex(hex).expect("hex").as_slice()).expect("32")
}

#[test]
fn the_file_yields_public_keys_only_highest_kid_first() {
    let keys = AgentKeyFile::parse(&format!("# rotation: 2 is new\n\n2 {SEED_2}\n1 {SEED_1}\n"))
        .expect("keys");
    assert_eq!(keys.signing_kid(), 2);
    let listed: Vec<(u32, String)> = keys.keys().map(|k| (k.kid, k.public_key.clone())).collect();
    assert_eq!(
        listed,
        [
            (
                2,
                forge_agent::public_key_from_seed(&seed(SEED_2)).expect("pk")
            ),
            (
                1,
                forge_agent::public_key_from_seed(&seed(SEED_1)).expect("pk")
            ),
        ]
    );
    // Never a seed in Debug: only the public halves are kept.
    let shown = format!("{keys:?}");
    assert!(
        !shown.contains("0202") && !shown.contains("0101"),
        "{shown}"
    );
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
        let err = AgentKeyFile::parse(text).expect_err(rule);
        assert_eq!(reason(&err), "keys_invalid", "{text}");
        assert_eq!(context(&err, "rule").as_deref(), Some(rule), "{text}");
        assert!(!err.to_string().contains("0101010101"), "{err}");
    }
}

/// Plan 16 A3: a key file readable by group or others is refused with
/// FG-AGENT-0014 `insecure_permissions` (the secret file reader's check,
/// Unix only — on Windows the ACL is the operator's, ADR-0013, and doctor
/// warns), and so is a relative or missing path (`keys_unreadable`).
#[test]
fn key_file_insecure_permissions_refused() {
    // The reader's refusal becomes FG-AGENT-0014 on every platform.
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
    assert_eq!(PERMISSIONS_CHECKED, cfg!(unix));
    let err = AgentKeyFile::read(Path::new("relative.keys")).expect_err("relative");
    assert_eq!(reason(&err), "keys_unreadable");
    assert_eq!(
        context(&err, "config_reason").as_deref(),
        Some("not_absolute")
    );
    let dir = tempfile::tempdir().expect("dir");
    let missing = dir.path().join("missing.keys");
    let err = AgentKeyFile::read(&missing).expect_err("missing");
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
            let err = AgentKeyFile::read(&path).expect_err("readable by others");
            assert_eq!(reason(&err), "insecure_permissions");
            assert!(!err.to_string().contains("0101010101"), "{err}");
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
    // Owned by this user and owner only: read.
    let keys = AgentKeyFile::read(&path).expect("owner only");
    assert_eq!(keys.signing_kid(), 1);
}
