use forge_errors::ErrorCode;

use crate::dedup_key::{DedupKey, DedupPart, MAX_CHARS};

fn assert_invalid(result: Result<DedupKey, forge_errors::ForgeError>) {
    let err = result.expect_err("invalid");
    assert_eq!(err.code(), ErrorCode::ChangeRejected, "{err}");
    assert!(
        err.to_string().contains("reason=invalid_dedup_key"),
        "{err}"
    );
    assert!(err.to_string().contains("prefix="), "{err}");
}

#[test]
fn a_dedup_key_is_built_from_server_side_ids_hashes_and_fixed_words() {
    let hash = [0xab_u8; 32];
    let key = DedupKey::new(
        "push",
        &[
            DedupPart::Id(42),
            DedupPart::Hash(&hash),
            DedupPart::Word("v1"),
        ],
    )
    .expect("key");
    assert_eq!(key.as_str(), format!("push:42:{}:v1", "ab".repeat(32)));
    assert!(DedupKey::is_well_formed(key.as_str()));
    // Negative ids keep the shape (`-` is allowed in a part).
    let neg = DedupKey::new("t", &[DedupPart::Id(-5)]).expect("key");
    assert_eq!(neg.as_str(), "t:-5");
}

#[test]
fn a_dedup_key_from_a_raw_client_value_cannot_be_built() {
    // The only constructor takes `&'static str` words, ids and 32-byte
    // hashes: a client string (a `String`) has no way in. What remains is
    // checked: a bad prefix, a bad word, no part, more than four parts.
    assert_invalid(DedupKey::new("Push", &[DedupPart::Id(1)]));
    assert_invalid(DedupKey::new("push-x", &[DedupPart::Id(1)]));
    assert_invalid(DedupKey::new("", &[DedupPart::Id(1)]));
    assert_invalid(DedupKey::new("push", &[DedupPart::Word("A b")]));
    assert_invalid(DedupKey::new("push", &[DedupPart::Word("")]));
    // A `:` inside a word or the prefix would silently add a part.
    assert_invalid(DedupKey::new("push", &[DedupPart::Word("a:b")]));
    assert_invalid(DedupKey::new("a:b", &[DedupPart::Id(1)]));
    assert_invalid(DedupKey::new("push", &[]));
    assert_invalid(DedupKey::new(
        "push",
        &[
            DedupPart::Id(1),
            DedupPart::Id(2),
            DedupPart::Id(3),
            DedupPart::Id(4),
            DedupPart::Id(5),
        ],
    ));
}

#[test]
fn a_dedup_key_longer_than_its_bound_is_refused() {
    let long: &'static str = Box::leak("p".repeat(MAX_CHARS).into_boxed_str());
    assert_invalid(DedupKey::new(long, &[DedupPart::Id(1)]));
}

#[test]
fn is_well_formed_matches_the_documented_shape() {
    for ok in [
        "push:1",
        "a_b:x-y:0",
        "t:1:2:3:4",
        &format!("k:{}", "z".repeat(64)),
    ] {
        assert!(DedupKey::is_well_formed(ok), "{ok:?}");
    }
    for bad in [
        "",
        "push",
        "push:",
        "Push:1",
        "1push:1",
        "push:1:2:3:4:5",
        "push:A",
        "push:a b",
        "push:a.b",
        &format!("k:{}", "z".repeat(65)),
        &format!("{}:1", "a".repeat(MAX_CHARS)),
    ] {
        assert!(!DedupKey::is_well_formed(bad), "{bad:?}");
    }
}
