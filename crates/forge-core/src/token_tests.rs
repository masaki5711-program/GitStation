use crate::token::{generate, parse, sha256, verify, TokenError, TokenSecret};
use proptest::prelude::*;

fn is_lower_hex(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn valid() -> String {
    format!("fgp_{}_{}", "0123456789abcdef", "ab".repeat(32))
}

#[test]
fn token_has_the_fgp_public_id_secret_shape() {
    let token = generate().unwrap().reveal();
    let parts: Vec<&str> = token.split('_').collect();
    assert_eq!(parts.len(), 3, "token: three `_` separated parts");
    assert_eq!(parts[0], "fgp");
    assert_eq!(parts[1].len(), 16);
    assert_eq!(parts[2].len(), 64);
    assert!(
        is_lower_hex(parts[1]) && is_lower_hex(parts[2]),
        "lowercase hex"
    );
}

#[test]
fn generated_secrets_are_256_bits_and_distinct() {
    let a = generate().unwrap();
    let b = generate().unwrap();
    assert_eq!(a.secret().as_bytes().len(), 32);
    assert_ne!(a.secret().as_bytes(), b.secret().as_bytes());
    assert_ne!(a.secret().as_bytes(), &[0u8; 32], "not an all-zero secret");
    assert_ne!(a.public_id(), b.public_id());
}

#[test]
fn parse_round_trips_a_generated_and_a_fixed_token() {
    let token = generate().unwrap();
    let parsed = parse(&token.reveal()).unwrap();
    assert_eq!(parsed.public_id(), token.public_id());
    assert_eq!(parsed.secret().as_bytes(), token.secret().as_bytes());
    let fixed = parse(&valid()).unwrap();
    assert_eq!(fixed.reveal(), valid());
}

proptest! {
    #[test]
    fn parse_never_panics_and_accepts_only_canonical_tokens(input in ".*") {
        if let Ok(token) = parse(&input) {
            prop_assert_eq!(token.reveal(), input);
        }
    }

    #[test]
    fn every_well_formed_token_round_trips(
        id in "[0-9a-f]{16}",
        secret in "[0-9a-f]{64}",
    ) {
        let text = format!("fgp_{id}_{secret}");
        let token = parse(&text).map_err(|e| TestCaseError::fail(format!("{e:?}")))?;
        prop_assert_eq!(token.public_id().as_str(), id.as_str());
        prop_assert_eq!(token.reveal(), text);
    }
}

#[test]
fn parse_rejects_prefix_length_charset_and_extra_separators() {
    let id = "0123456789abcdef";
    let sec = "ab".repeat(32);
    let hostile: Vec<String> = vec![
        String::new(),
        "fgp".into(),
        "fgp__".into(),
        format!("fgx_{id}_{sec}"),
        format!("FGP_{id}_{sec}"),
        format!("glpat_{id}_{sec}"),
        format!("_{id}_{sec}"),
        format!("fgp_{}_{sec}", &id[..15]),
        format!("fgp_{id}0_{sec}"),
        format!("fgp_{id}_{}", &sec[..63]),
        format!("fgp_{id}_{sec}0"),
        format!("fgp_{}_{sec}", id.to_uppercase()),
        format!("fgp_{id}_{}", sec.to_uppercase()),
        format!("fgp_{id}_{}g", &sec[..63]),
        format!("fgp_{id}_{sec}_extra"),
        format!("fgp_{id}__{sec}"),
        format!("fgp-{id}-{sec}"),
        format!(" fgp_{id}_{sec}"),
        format!("fgp_{id}_{sec}\n"),
        format!("fgp_{id}_{sec}\0"),
        format!("fgp_{id}_{}\u{ff10}", &sec[..63]),
        format!("fgp_{}\u{e9}_{sec}", &id[..15]),
        "\u{1f511}".repeat(40),
        "_".repeat(100),
        "a".repeat(100_000),
    ];
    for input in &hostile {
        assert_eq!(
            parse(input).err(),
            Some(TokenError::Malformed),
            "accepted: {input:?}"
        );
    }
    assert!(
        parse(&valid()).is_ok(),
        "control: the canonical shape is accepted"
    );
}

#[test]
fn token_debug_never_shows_the_secret() {
    let token = parse(&valid()).unwrap();
    let secret_hex = "ab".repeat(32);
    for shown in [
        format!("{token:?}"),
        format!("{:?}", token.secret()),
        format!("{token:#?}"),
    ] {
        assert!(!shown.contains(&secret_hex), "secret leaked: {shown}");
        assert!(!shown.contains("171"), "decimal bytes leaked: {shown}");
        assert!(shown.contains("redacted"), "marker missing: {shown}");
    }
}

#[test]
fn sha256_matches_the_golden_vector() {
    // sha256 of 32 zero bytes (well-known vector):
    // 66687aadf862bd776c8fc18b8e9f8e20089714856ee233b3902a591d0d5f2925.
    let zero = TokenSecret::from_bytes([0u8; 32]);
    let expected: [u8; 32] = [
        0x66, 0x68, 0x7a, 0xad, 0xf8, 0x62, 0xbd, 0x77, 0x6c, 0x8f, 0xc1, 0x8b, 0x8e, 0x9f, 0x8e,
        0x20, 0x08, 0x97, 0x14, 0x85, 0x6e, 0xe2, 0x33, 0xb3, 0x90, 0x2a, 0x59, 0x1d, 0x0d, 0x5f,
        0x29, 0x25,
    ];
    assert_eq!(sha256(&zero), expected);
    let ones = TokenSecret::from_bytes([1u8; 32]);
    assert_ne!(
        sha256(&ones),
        sha256(&zero),
        "different secrets hash differently"
    );
}

#[test]
fn verify_accepts_only_the_exact_secret() {
    let secret = TokenSecret::from_bytes([7u8; 32]);
    let stored = sha256(&secret);
    assert!(verify(&stored, &secret), "exact secret accepted");
    assert!(
        verify(&stored, &TokenSecret::from_bytes([7u8; 32])),
        "equal copy accepted"
    );
    let mut off_by_one = [7u8; 32];
    off_by_one[31] = 8;
    assert!(!verify(&stored, &TokenSecret::from_bytes(off_by_one)));
    assert!(!verify(&stored, &TokenSecret::from_bytes([0u8; 32])));
    assert!(
        !verify(&[0u8; 32], &secret),
        "a zero hash matches nothing real"
    );
}
