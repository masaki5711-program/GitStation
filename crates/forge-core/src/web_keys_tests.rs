#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! `web.session_key_file` and the per-purpose keys (ADR-0021 §7).

use crate::web_keys::{hkdf_sha256, KeyPurpose, OpenError, WebKeys};
use forge_errors::ErrorCode;

// Synthetic keys (test-only values, never deployed).
const K1: &str = "0101010101010101010101010101010101010101010101010101010101010101";
const K2: &str = "0202020202020202020202020202020202020202020202020202020202020202";

fn keys(text: &str) -> WebKeys {
    WebKeys::parse(text).expect("keys")
}

fn reason(text: &str) -> String {
    let err = WebKeys::parse(text).expect_err("invalid");
    assert_eq!(err.code(), ErrorCode::ConfigInvalid);
    let ctx = err.context();
    assert!(ctx
        .iter()
        .any(|(k, v)| *k == "config_key" && v == "web.session_key_file"));
    assert!(
        !format!("{err:?}").contains("0303"),
        "the key never reaches the error"
    );
    ctx.iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone())
        .expect("reason")
}

/// RFC 5869 A.3 (SHA-256, no salt, no info): the first 32 bytes of OKM.
#[test]
fn hkdf_matches_the_rfc_5869_vector() {
    let ikm = [0x0bu8; 22];
    let okm = hkdf_sha256(&ikm, b"").expect("hkdf");
    let expected = "8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d";
    assert_eq!(crate::token::to_hex(&okm), expected);
}

#[test]
fn every_purpose_has_its_own_info_and_key() {
    let infos: Vec<String> = KeyPurpose::ALL.iter().map(|p| p.info()).collect();
    assert_eq!(
        infos,
        [
            "gitstation/web/oidc-state/v1",
            "gitstation/web/refresh-token/v1",
            "gitstation/web/id-token/v1"
        ]
    );
    let ikm = [7u8; 32];
    let derived: Vec<[u8; 32]> = KeyPurpose::ALL
        .iter()
        .map(|p| hkdf_sha256(&ikm, p.info().as_bytes()).expect("hkdf"))
        .collect();
    for (i, a) in derived.iter().enumerate() {
        assert_ne!(a, &ikm, "the file key is never used directly");
        for b in derived.iter().skip(i + 1) {
            assert_ne!(a, b);
        }
    }
}

/// HKDF key separation: a ciphertext of one purpose does not open under
/// another, even with the same file key and the same associated data.
#[test]
fn a_ciphertext_of_one_purpose_never_opens_under_another() {
    let keys = keys(&format!("1 {K1}\n"));
    let state = keys.purpose(KeyPurpose::OidcState).expect("state");
    let refresh = keys.purpose(KeyPurpose::RefreshToken).expect("refresh");
    let id = keys.purpose(KeyPurpose::IdToken).expect("id");
    let sealed = refresh
        .seal(b"00000000000000a1", b"synthetic-refresh")
        .expect("seal");
    assert_eq!(
        refresh.open(b"00000000000000a1", &sealed).expect("open"),
        b"synthetic-refresh"
    );
    assert_eq!(
        state.open(b"00000000000000a1", &sealed),
        Err(OpenError::Tag)
    );
    assert_eq!(id.open(b"00000000000000a1", &sealed), Err(OpenError::Tag));
    // Another row's associated data (a ciphertext copied to another session).
    assert_eq!(
        refresh.open(b"00000000000000b2", &sealed),
        Err(OpenError::Tag)
    );
}

#[test]
fn the_newest_key_seals_and_every_listed_key_opens() {
    let old = keys(&format!("1 {K1}\n"));
    let sealed_old = old
        .purpose(KeyPurpose::IdToken)
        .expect("purpose")
        .seal(b"a", b"x")
        .expect("seal");
    let rotated = keys(&format!("# rotation\n2 {K2}\n\n1 {K1}\n"));
    assert_eq!(rotated.kids(), vec![1, 2]);
    let id = rotated.purpose(KeyPurpose::IdToken).expect("purpose");
    assert_eq!(id.open(b"a", &sealed_old).expect("old kid opens"), b"x");
    let sealed_new = id.seal(b"a", b"y").expect("seal");
    assert_eq!(&sealed_new[1..3], &2u16.to_be_bytes(), "kid 2 seals");
    let dropped = keys(&format!("2 {K2}\n"));
    assert_eq!(
        dropped
            .purpose(KeyPurpose::IdToken)
            .expect("purpose")
            .open(b"a", &sealed_old),
        Err(OpenError::Kid)
    );
}

#[test]
fn tampered_or_short_input_does_not_open() {
    let keys = keys(&format!("1 {K1}\n"));
    let p = keys.purpose(KeyPurpose::OidcState).expect("purpose");
    let mut sealed = p.seal(b"", b"payload").expect("seal");
    assert_eq!(p.open(b"", &sealed[..10]), Err(OpenError::Truncated));
    let last = sealed.len() - 1;
    sealed[last] ^= 1;
    assert_eq!(p.open(b"", &sealed), Err(OpenError::Tag));
    sealed[0] = 9;
    assert_eq!(p.open(b"", &sealed), Err(OpenError::Version));
}

#[test]
fn invalid_key_files_are_config_invalid_without_their_content() {
    assert_eq!(reason(""), "empty");
    assert_eq!(reason("# only a comment\n"), "empty");
    assert_eq!(reason("1\n"), "invalid");
    assert_eq!(reason(&format!("0 {K1}\n")), "invalid_key_id");
    assert_eq!(reason(&format!("+1 {K1}\n")), "invalid_key_id");
    assert_eq!(reason("1 0303\n"), "invalid_key");
    assert_eq!(reason(&format!("1 {K1}\n1 {K2}\n")), "duplicate");
    assert!(format!("{:?}", keys(&format!("1 {K1}\n"))).contains("redacted"));
}
