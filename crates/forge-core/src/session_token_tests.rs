#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! The web session cookie value and its CSRF token (ADR-0021 §5–§6).

use crate::session_token::{csrf_matches, csrf_token, generate, parse, SESSION_PREFIX};
use crate::token::TokenError;

const VALUE: &str =
    "gss_00000000000000a1_1111111111111111111111111111111111111111111111111111111111111111";

#[test]
fn a_generated_session_round_trips_and_differs_from_a_token() {
    let s = generate().expect("rng");
    let text = s.reveal();
    assert!(text.starts_with("gss_"));
    assert_eq!(text.len(), SESSION_PREFIX.len() + 1 + 16 + 1 + 64);
    let back = parse(&text).expect("parse");
    assert_eq!(back.public_id(), s.public_id());
    assert_eq!(back.secret().as_bytes(), s.secret().as_bytes());
    assert!(!format!("{s:?}").contains(&text[22..]), "Debug is redacted");
    let token_form = text.replacen("gss_", "fgp_", 1);
    assert_eq!(
        parse(&token_form).err().map(|e| e == TokenError::Malformed),
        Some(true),
        "a token is not a session"
    );
    assert!(
        crate::token::parse(&text).is_err(),
        "a session is not a token"
    );
}

#[test]
fn malformed_values_are_refused() {
    for bad in [
        "",
        "gss_",
        "gss_00000000000000a1",
        "gss_00000000000000A1_1111111111111111111111111111111111111111111111111111111111111111",
        "gss_00000000000000a1_111",
        "GSS_00000000000000a1_1111111111111111111111111111111111111111111111111111111111111111",
    ] {
        assert!(matches!(parse(bad), Err(TokenError::Malformed)), "{bad:?}");
    }
}

#[test]
fn the_csrf_token_is_the_hmac_of_the_secret_and_is_compared_whole() {
    let s = parse(VALUE).expect("parse");
    let token = csrf_token(s.secret()).expect("csrf");
    assert_eq!(token.len(), 64);
    assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(s.csrf_token().expect("csrf"), token, "stable per secret");
    assert!(csrf_matches(s.secret(), &token).expect("mac"));
    assert!(!csrf_matches(s.secret(), &token[..63]).expect("mac"));
    assert!(!csrf_matches(s.secret(), "").expect("mac"));
    let other = generate().expect("rng");
    assert!(
        !csrf_matches(other.secret(), &token).expect("mac"),
        "another session's token"
    );
    assert!(
        !token.contains("1111111111"),
        "the token does not reveal the secret"
    );
}
