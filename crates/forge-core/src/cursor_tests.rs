//! The opaque keyset cursor: round trip, binding to the purpose, rotation,
//! and every way a cursor fails (`FG-WEB-0006 reason=cursor_invalid`,
//! never the value).

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use forge_errors::{ErrorCode, ForgeError};
use std::time::Duration;

use crate::cursor::{CursorKeys, MAX_CURSOR_CHARS, MAX_PAYLOAD_BYTES};

const PURPOSE: &str = "projects.list:user=42";
const AGE: Duration = Duration::from_secs(3600);

fn keys(lines: &[(u16, &str)]) -> CursorKeys {
    let text: String = lines
        .iter()
        .map(|(kid, byte)| format!("{kid} {}\n", byte.repeat(32)))
        .collect();
    CursorKeys::parse(&text).expect("keys")
}

fn rule(err: &ForgeError) -> String {
    assert_eq!(err.code(), ErrorCode::RequestInvalid, "{err}");
    let text = err.to_string();
    assert!(text.contains("reason=cursor_invalid"), "{text}");
    text.split("rule=")
        .nth(1)
        .and_then(|r| r.split([',', ' ', ')', ']']).next())
        .unwrap_or_default()
        .to_owned()
}

#[test]
fn a_cursor_round_trips_and_reveals_nothing_of_its_payload() {
    let k = keys(&[(1, "ab")]);
    let payload = br#"{"after_id":1234,"seen":17}"#;
    let cursor = k.encode(PURPOSE, payload).expect("encode");
    assert!(cursor
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    assert!(!cursor.contains("1234"));
    assert_eq!(k.decode(PURPOSE, &cursor, AGE).expect("decode"), payload);
    // A fresh nonce each time: the same position never yields the same text.
    let again = k.encode(PURPOSE, payload).expect("encode");
    assert_ne!(cursor, again);
    let full = k.encode(PURPOSE, &[7u8; MAX_PAYLOAD_BYTES]).expect("max");
    assert!(full.len() <= MAX_CURSOR_CHARS, "{}", full.len());
    assert_eq!(
        k.decode(PURPOSE, &full, AGE).expect("max").len(),
        MAX_PAYLOAD_BYTES
    );
    let over = k
        .encode(PURPOSE, &[0u8; MAX_PAYLOAD_BYTES + 1])
        .expect_err("too large");
    assert_eq!(over.code(), ErrorCode::Internal);
}

/// Request validation's cursor rule (the `test` field of FG-WEB-0006 is the
/// web layer's; this pins every cursor rule).
#[test]
fn tampered_truncated_foreign_and_rotated_out_cursors_are_cursor_invalid() {
    let k = keys(&[(1, "ab")]);
    let cursor = k.encode(PURPOSE, b"position").expect("encode");
    let mut bytes = URL_SAFE_NO_PAD.decode(&cursor).expect("b64");

    // Every single flipped bit fails the tag (header bits: version / kid).
    for i in 0..bytes.len() {
        let mut flipped = bytes.clone();
        flipped[i] ^= 0x01;
        let err = k
            .decode(PURPOSE, &URL_SAFE_NO_PAD.encode(&flipped), AGE)
            .expect_err("flipped");
        let r = rule(&err);
        let want = match i {
            0 => "version",
            1 | 2 => "kid",
            _ => "tag",
        };
        assert_eq!(r, want, "byte {i}");
    }
    // Another list or another caller: the tag does not verify.
    let err = k
        .decode("projects.list:user=43", &cursor, AGE)
        .expect_err("other caller");
    assert_eq!(rule(&err), "tag");
    // Truncation: shorter than header + tag, or a cut tag.
    let err = k
        .decode(PURPOSE, &URL_SAFE_NO_PAD.encode(&bytes[..20]), AGE)
        .expect_err("short");
    assert_eq!(rule(&err), "truncated");
    bytes.pop();
    let err = k
        .decode(PURPOSE, &URL_SAFE_NO_PAD.encode(&bytes), AGE)
        .expect_err("cut tag");
    assert_eq!(rule(&err), "tag");
    // Not base64url (padding, `+`, `/`, spaces) and absurd lengths.
    for bad in [
        format!("{cursor}="),
        format!("+{}", &cursor[1..]),
        format!(" {cursor}"),
        String::new(),
    ] {
        let err = k.decode(PURPOSE, &bad, AGE).expect_err("encoding");
        assert!(
            ["encoding", "truncated"].contains(&rule(&err).as_str()),
            "{bad}"
        );
    }
    let err = k
        .decode(PURPOSE, &"A".repeat(MAX_CURSOR_CHARS + 1), AGE)
        .expect_err("long");
    assert_eq!(rule(&err), "too_long");

    // A key from another installation, and a key rotated out.
    let other = keys(&[(1, "cd")]);
    assert_eq!(
        rule(&other.decode(PURPOSE, &cursor, AGE).expect_err("foreign")),
        "tag"
    );
    let rotated = keys(&[(2, "ef")]);
    assert_eq!(
        rule(&rotated.decode(PURPOSE, &cursor, AGE).expect_err("gone")),
        "kid"
    );
    // The error never carries the cursor.
    let err = other.decode(PURPOSE, &cursor, AGE).expect_err("foreign");
    assert!(!format!("{err} {err:?}").contains(&cursor));
}

#[test]
fn rotation_encrypts_with_the_newest_kid_and_still_opens_the_older_ones() {
    let old = keys(&[(1, "ab")]);
    let before = old.encode(PURPOSE, b"old position").expect("encode");
    let both = keys(&[(2, "cd"), (1, "ab")]);
    assert_eq!(both.encrypting().kid(), 2);
    assert_eq!(
        both.decode(PURPOSE, &before, AGE).expect("old kid"),
        b"old position"
    );
    let after = both.encode(PURPOSE, b"new position").expect("encode");
    assert_eq!(URL_SAFE_NO_PAD.decode(&after).expect("b64")[1..3], [0, 2]);
    assert_eq!(
        rule(&old.decode(PURPOSE, &after, AGE).expect_err("new kid")),
        "kid"
    );
}

#[test]
fn a_bad_cursor_key_file_names_the_line_but_never_its_content() {
    let short = format!("1 {}", "ab".repeat(31));
    let long = format!("1 {}", "ab".repeat(33));
    let not_hex = format!("1 {}zz", "ab".repeat(31));
    let dup = format!("1 {}\n1 {}", "ab".repeat(32), "cd".repeat(32));
    let cases: &[(&str, &str)] = &[
        ("", "empty"),
        ("# only a comment\n", "empty"),
        ("1", "invalid"),
        (&short, "invalid_key"),
        (&long, "invalid_key"),
        (&not_hex, "invalid_key"),
        (&dup, "duplicate"),
    ];
    for (text, reason) in cases {
        let err = CursorKeys::parse(text).expect_err(reason);
        let shown = err.to_string();
        assert_eq!(err.code(), ErrorCode::ConfigInvalid);
        assert!(shown.contains(&format!("reason={reason}")), "{shown}");
        assert!(shown.contains("api.cursor_keys_file"), "{shown}");
        assert!(!shown.contains("abab"), "{shown}");
    }
    for kid in ["0", "-1", "+1", "65536", "x"] {
        let err = CursorKeys::parse(&format!("{kid} {}", "ab".repeat(32))).expect_err(kid);
        assert!(err.to_string().contains("reason=invalid_key_id"), "{err}");
    }
    let k = keys(&[(5, "ab")]);
    assert!(!format!("{k:?}").contains("ab"), "{k:?}");
}

/// The issue time is sealed with the payload: a cursor older than `max_age`
/// is `rule=expired`, the boundary and a clock stepped back are still valid.
#[test]
fn a_cursor_older_than_the_max_age_is_expired() {
    let k = keys(&[(1, "ab")]);
    let cursor = k.encode_at(PURPOSE, b"position", 1_000).expect("encode");
    let ok = |now| k.decode_at(PURPOSE, &cursor, AGE, now);
    assert_eq!(ok(1_000).expect("fresh"), b"position");
    assert_eq!(ok(1_000 + 3_600).expect("at the limit"), b"position");
    assert_eq!(ok(500).expect("clock stepped back"), b"position");
    assert_eq!(rule(&ok(1_000 + 3_601).expect_err("expired")), "expired");
    assert_eq!(rule(&ok(u64::MAX).expect_err("long after")), "expired");
    // The expiry check never hides the other failures: a foreign caller is
    // still `tag`, whatever the age.
    let err = k
        .decode_at("projects.list:user=43", &cursor, AGE, 1_000 + 10_000)
        .expect_err("other caller");
    assert_eq!(rule(&err), "tag");
}
