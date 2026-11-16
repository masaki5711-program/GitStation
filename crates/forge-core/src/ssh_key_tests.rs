#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_errors::ErrorCode;

use super::*;

// Synthetic keys made for these tests (`ssh-keygen -t …`, private halves
// discarded); fingerprints as `ssh-keygen -l -E sha256` printed them.
const ED25519: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAGuL+srU7BTyMdIqbgh4NRqXH1vOCQWfZxL4x1ZB/uN synthetic-ed25519";
const ED25519_FP: &str = "SHA256:5ytgilOc3oe+dzOfNlMNUlML18fMocF7QAyCsvhkxkA";
const P256: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBAzhwPmR4JxQG+Uj4orL/XazsYd1yWhA+0YVMc9DTd+8+9495bPKu9gkcFn6emrPIX9f0pSCwze/yOGSi7XKLKo= synthetic-ecdsab256";
const P256_FP: &str = "SHA256:BBzJlbExy/SmEN1S+4oief/lMY7kER74q+4+xHH8Cvg";
const P384: &str = "ecdsa-sha2-nistp384 AAAAE2VjZHNhLXNoYTItbmlzdHAzODQAAAAIbmlzdHAzODQAAABhBGx+LuJpWc0tByVyezV3IKQyhs8v9JUccwvz8QiI63QuiqQPGYRFzhs3heivCcp96dc1nmk3HRdW7mASMyR/gBBHGhOVvOBYDJgoQGS1XduWrCg7SwLwoGyP7gxnsx4IRQ== synthetic-ecdsab384";
const P384_FP: &str = "SHA256:uwKabcoNblJH02xtXAJEbq9r/LWG0UJNF0ClGRJmyf0";
const P521: &str = "ecdsa-sha2-nistp521 AAAAE2VjZHNhLXNoYTItbmlzdHA1MjEAAAAIbmlzdHA1MjEAAACFBAErKuH+h5qyA3SElHshwD0wrrZeajwCXa3E2lnJMGrh9K3z9wyE+oySKnfIXXIloZwfhTqgvNMXmjh+FxVLl6JfDgCt/nGgLIpCX0RR13dJ7C6tr5STgBxapX9BN4VHhQUd/HFlSImClLP9rINjjifKzz2Z93Z1ncjyNMqrBGd+RxQmsA== synthetic-ecdsab521";
const P521_FP: &str = "SHA256:N7M0hSw4xHiQldHsJjLoX3L/T01g3aLkiyaRKsHOvI4";
const RSA: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQC/Tf+xcCkPoabfWK2H+xhbpgYxrt5T4m3Nb1IUjE3kUOhZcvddmFF8LabBsXeojOHuWtyOs4y08VleLT2wr5+uyQ36pqfkGVVHz0IgZ4VwRQVwNWY2U5JqALQdjf8WF/SVezb+BEELKcWVWoG5UTWIHx/oS3gSsjkmdLg+EAJkOCMawGxTew7gZmCb3g8k0P+BsDgW1Ft1Pxe/8krhkQ6+356TrwwaqREHG3zjZYrvZqKCUQdQZBPFMIzyzHHmm0bWBf94dq19rPS5ecNFfiC/r9LzfwgKrNOBOSQlXNCpzUpuLIx0TYxJJOoco8UEEvnHXBNcgt665y1O+xkAT48D synthetic-rsab2048";

fn reason(err: &forge_errors::ForgeError) -> (String, Option<String>) {
    let ctx = err.context();
    let get = |k: &str| {
        ctx.iter()
            .find(|(key, _)| *key == k)
            .map(|(_, v)| v.clone())
    };
    (get("reason").unwrap_or_default(), get("detail"))
}

#[test]
fn every_accepted_algorithm_parses_with_ssh_keygens_fingerprint() {
    for (line, alg, fp, comment) in [
        (
            ED25519,
            SshKeyAlgorithm::Ed25519,
            ED25519_FP,
            "synthetic-ed25519",
        ),
        (
            P256,
            SshKeyAlgorithm::EcdsaP256,
            P256_FP,
            "synthetic-ecdsab256",
        ),
        (
            P384,
            SshKeyAlgorithm::EcdsaP384,
            P384_FP,
            "synthetic-ecdsab384",
        ),
        (
            P521,
            SshKeyAlgorithm::EcdsaP521,
            P521_FP,
            "synthetic-ecdsab521",
        ),
    ] {
        let key = parse_public_key(line).unwrap();
        assert_eq!(key.algorithm, alg);
        assert_eq!(key.fingerprint(), fp, "{line}");
        assert!(is_fingerprint(&key.fingerprint()));
        assert_eq!(key.comment, comment);
        // The canonical line round-trips to the same key.
        let again = parse_public_key(&key.openssh()).unwrap();
        assert_eq!(again.blob, key.blob);
        assert_eq!(again.comment, "");
    }
}

#[test]
fn surrounding_whitespace_and_a_missing_comment_are_fine() {
    let bare = ED25519.rsplit_once(' ').unwrap().0;
    let key = parse_public_key(&format!("  {bare}\t \n")).unwrap();
    assert_eq!(key.fingerprint(), ED25519_FP);
    assert_eq!(key.comment, "");
    let spaced = parse_public_key(&format!("{bare} two  words")).unwrap();
    assert_eq!(spaced.comment, "two words");
}

/// The errors.toml test of FG-AUTH-0023 (the parser's reasons; the
/// route's `duplicate` / `limit` / `title_invalid` are forge-db's and
/// forge-web's tests).
#[test]
fn an_unparseable_or_unsupported_ssh_key_is_refused() {
    let bare = ED25519.rsplit_once(' ').unwrap().0;
    let (alg, b64) = bare.split_once(' ').unwrap();
    let blob = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .unwrap();
    let enc = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let mut trailing = blob.clone();
    trailing.push(0);
    let truncated = &blob[..blob.len() - 1];
    let p256_blob = base64::engine::general_purpose::STANDARD
        .decode(P256.split(' ').nth(1).unwrap())
        .unwrap();
    let cases: Vec<(String, &str, Option<&str>)> = vec![
        (RSA.to_owned(), "algorithm_unsupported", None),
        (
            "ssh-dss AAAAB3NzaC1kc3M= x".to_owned(),
            "algorithm_unsupported",
            None,
        ),
        (
            format!("sk-ssh-ed25519@openssh.com {b64}"),
            "algorithm_unsupported",
            None,
        ),
        (
            format!("ssh-ed25519-cert-v01@openssh.com {b64}"),
            "algorithm_unsupported",
            None,
        ),
        (format!("no-pty {bare}"), "algorithm_unsupported", None),
        (String::new(), "malformed", Some("empty")),
        ("   ".to_owned(), "malformed", Some("empty")),
        (alg.to_owned(), "malformed", Some("no_key")),
        (format!("{alg} !!!notbase64"), "malformed", Some("base64")),
        (
            format!("{alg} {}", enc(&trailing)),
            "malformed",
            Some("trailing_bytes"),
        ),
        (
            format!("{alg} {}", enc(truncated)),
            "malformed",
            Some("blob_truncated"),
        ),
        // The line says Ed25519, the blob is a P-256 key.
        (
            format!("{alg} {}", enc(&p256_blob)),
            "malformed",
            Some("blob_algorithm_mismatch"),
        ),
        (
            format!("{bare}\n{bare}"),
            "malformed",
            Some("more_than_one_line"),
        ),
        (
            format!("{bare} {}", "x".repeat(MAX_LINE_BYTES)),
            "too_long",
            None,
        ),
    ];
    for (line, want, detail) in cases {
        let err = parse_public_key(&line).expect_err(&line);
        assert_eq!(err.code(), ErrorCode::SshKeyInvalid, "{line}");
        let (got, got_detail) = reason(&err);
        assert_eq!(got, want, "{line}");
        if let Some(d) = detail {
            assert_eq!(got_detail.as_deref(), Some(d), "{line}");
        }
    }
}

#[test]
fn an_ecdsa_blob_with_another_curve_or_a_compressed_point_is_malformed() {
    let blob = base64::engine::general_purpose::STANDARD
        .decode(P256.split(' ').nth(1).unwrap())
        .unwrap();
    // The curve string "nistp256" → "nistp257".
    let mut curve = blob.clone();
    let at = curve
        .windows(8)
        .rposition(|w| w == b"nistp256")
        .expect("curve name");
    curve[at + 7] = b'7';
    let line = format!(
        "ecdsa-sha2-nistp256 {}",
        base64::engine::general_purpose::STANDARD.encode(&curve)
    );
    let (r, d) = reason(&parse_public_key(&line).unwrap_err());
    assert_eq!(
        (r.as_str(), d.as_deref()),
        ("malformed", Some("blob_curve_mismatch"))
    );
    // The point's 0x04 prefix → 0x02 (compressed).
    let mut point = blob.clone();
    let first = point.len() - 65;
    point[first] = 0x02;
    let line = format!(
        "ecdsa-sha2-nistp256 {}",
        base64::engine::general_purpose::STANDARD.encode(&point)
    );
    let (r, d) = reason(&parse_public_key(&line).unwrap_err());
    assert_eq!(
        (r.as_str(), d.as_deref()),
        ("malformed", Some("point_not_uncompressed"))
    );
}

#[test]
fn control_characters_never_reach_the_comment_and_long_ones_are_cut() {
    let bare = ED25519.rsplit_once(' ').unwrap().0;
    let key = parse_public_key(&format!("{bare} a\u{1b}[31mb {}", "c".repeat(300))).unwrap();
    assert!(!key.comment.contains('\u{1b}'));
    assert_eq!(key.comment.chars().count(), MAX_COMMENT_CHARS);
}

#[test]
fn fingerprint_shape_is_checked() {
    assert!(is_fingerprint(ED25519_FP));
    assert!(!is_fingerprint("MD5:aa:bb"));
    assert!(!is_fingerprint("SHA256:short"));
    assert!(!is_fingerprint(&format!("SHA256:{}", "-".repeat(43))));
}
