#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // tests may fail loudly
// Tests sign with a synthetic seed (clippy.toml: no production signer).
#![allow(clippy::disallowed_methods)]

use forge_errors::ErrorCode;
use proptest::prelude::*;

use crate::*;

const SEED: [u8; 32] = [11u8; 32];
const OTHER_SEED: [u8; 32] = [12u8; 32];

fn h(c: char) -> String {
    std::iter::repeat_n(c, 64).collect()
}

fn blob(sha: &str, size: u64, path: &str) -> String {
    format!(r#"{{"sha256":"{sha}","size":{size},"path":"{path}"}}"#)
}

fn manifest_json(payloads: &[(&str, Vec<String>)]) -> String {
    let payloads: Vec<String> = payloads
        .iter()
        .map(|(k, b)| format!(r#"{{"kind":"{k}","blobs":[{}]}}"#, b.join(",")))
        .collect();
    format!(
        r#"{{"schema_version":1,"release_id":"r-7","project":"pfs/pfe","version":"1.4.0","source_sha":"{}","sequence":7,"created_at":"2026-11-24T03:00:00Z","arch":"amd64","os":"ubuntu-24.04","payloads":[{}],"max_install_seconds":900,"health_soak_seconds":600,"ci_pipeline_id":42}}"#,
        "a".repeat(40),
        payloads.join(",")
    )
}

fn standard_payloads() -> Vec<(&'static str, Vec<String>)> {
    vec![
        (
            "oci-image",
            vec![
                blob(&h('1'), 100, "image/blobs/sha256/1"),
                blob(&h('2'), 200, "image/blobs/sha256/2"),
                blob(&h('3'), 300, "image/index.json"),
            ],
        ),
        ("files", vec![blob(&h('4'), 10, "config/app.toml")]),
    ]
}

fn envelope(manifest: &str, digest: &str, kid: &str, sig: &str) -> String {
    format!(r#"{{"manifest":{manifest},"digest":"{digest}","key_id":"{kid}","signature":"{sig}"}}"#)
}

/// The manifest's digest, through the parser (a placeholder digest).
fn digest_of(manifest: &str) -> Digest {
    let r = parse_release(envelope(manifest, &format!("sha256:{}", h('0')), "k1", "x").as_bytes())
        .unwrap();
    manifest_digest(&r.manifest)
}

fn keys(seed: &[u8; 32], kid: &str) -> KeyRing {
    let pk = forge_agent::public_key_from_seed(seed).unwrap();
    parse_edge_keys(format!(r#"{{"keys":[{{"kid":"{kid}","public_key":"{pk}"}}]}}"#).as_bytes())
        .unwrap()
}

/// A release signed in the edge domain.
fn signed(manifest: &str) -> String {
    let d = digest_of(manifest);
    let sig = sign_manifest(&SEED, &d).unwrap();
    envelope(manifest, &d.to_string(), "k1", &sig)
}

// ── digest ──

#[test]
fn digest_is_independent_of_blob_order() {
    let a = manifest_json(&standard_payloads());
    let mut reordered = standard_payloads();
    reordered.reverse();
    for (_, blobs) in &mut reordered {
        blobs.reverse();
    }
    let b = manifest_json(&reordered);
    assert_ne!(a, b, "the JSON differs");
    assert_eq!(digest_of(&a), digest_of(&b));
}

proptest! {
    #[test]
    fn digest_is_independent_of_any_blob_permutation(seed in any::<u64>()) {
        let mut blobs = vec![
            blob(&h('1'), 1, "a/1"), blob(&h('2'), 2, "a/2"), blob(&h('3'), 3, "a/3"),
            blob(&h('4'), 4, "b/4"), blob(&h('5'), 5, "b/5"),
        ];
        let base = digest_of(&manifest_json(&[("files", blobs.clone())]));
        // A deterministic shuffle from the seed.
        let mut s = seed;
        for i in (1..blobs.len()).rev() {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let j = usize::try_from(s >> 33).unwrap() % (i + 1);
            blobs.swap(i, j);
        }
        prop_assert_eq!(base, digest_of(&manifest_json(&[("files", blobs)])));
    }
}

#[test]
fn digest_changes_with_any_field() {
    let base = manifest_json(&standard_payloads());
    let d = digest_of(&base);
    for (from, to) in [
        ("\"version\":\"1.4.0\"", "\"version\":\"1.4.1\""),
        ("\"sequence\":7", "\"sequence\":8"),
        ("\"arch\":\"amd64\"", "\"arch\":\"arm64\""),
        ("\"max_install_seconds\":900", "\"max_install_seconds\":901"),
        ("\"ci_pipeline_id\":42", "\"ci_pipeline_id\":43"),
        ("\"size\":100", "\"size\":101"),
        ("image/index.json", "image/index2.json"),
        ("2026-11-24T03:00:00Z", "2026-11-24T03:00:01Z"),
    ] {
        let changed = base.replacen(from, to, 1);
        assert_ne!(changed, base, "{from}");
        assert_ne!(digest_of(&changed), d, "{from} must change the digest");
    }
    // The same instant written with an offset is the same manifest.
    let same = base.replacen("2026-11-24T03:00:00Z", "2026-11-24T10:00:00+07:00", 1);
    assert_eq!(digest_of(&same), d);
}

// ── signature ──

#[test]
fn a_signed_release_verifies() {
    let m = manifest_json(&standard_payloads());
    let v = verify_release(signed(&m).as_bytes(), &keys(&SEED, "k1")).unwrap();
    assert_eq!(v.manifest.version, "1.4.0");
    assert_eq!(v.key_id, "k1");
    assert_eq!(v.digest, digest_of(&m));
}

#[test]
fn agent_part_signature_does_not_verify_as_edge_manifest() {
    let m = manifest_json(&standard_payloads());
    let d = digest_of(&m);
    // Same key, same 32 bytes, the agent part domain.
    let part_sig = forge_agent::sign_digest(&SEED, &d).unwrap();
    let e = verify_release(
        envelope(&m, &d.to_string(), "k1", &part_sig).as_bytes(),
        &keys(&SEED, "k1"),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::EdgeSignatureInvalid);
    assert_eq!((e.code.id(), e.reason), ("FG-EDGE-0004", "bad_signature"));
    // And the other way: an edge signature is not an agent part signature.
    let edge_sig = sign_manifest(&SEED, &d).unwrap();
    let pk = forge_agent::public_key_from_seed(&SEED).unwrap();
    let agent_keys =
        KeyRing::parse(format!(r#"{{"keys":[{{"kid":"k1","public_key":"{pk}"}}]}}"#).as_bytes())
            .unwrap();
    let e = forge_agent::verify_signature(&agent_keys, "k1", &d, &edge_sig).unwrap_err();
    assert_eq!(e.code, ErrorCode::PartSignatureInvalid);
}

#[test]
fn edited_manifest_is_digest_mismatch_and_resigned_digest_is_bad_signature() {
    let m = manifest_json(&standard_payloads());
    let good = signed(&m);
    let ring_ = keys(&SEED, "k1");
    // A field edited, the envelope's digest kept.
    let edited = good.replacen("\"size\":100", "\"size\":999", 1);
    let e = verify_release(edited.as_bytes(), &ring_).unwrap_err();
    assert_eq!(
        (e.code, e.reason),
        (ErrorCode::EdgeSignatureInvalid, "digest_mismatch")
    );
    // The digest recomputed too: the signature no longer matches.
    let m2 = m.replacen("\"size\":100", "\"size\":999", 1);
    let old_sig = sign_manifest(&SEED, &digest_of(&m)).unwrap();
    let forged = envelope(&m2, &digest_of(&m2).to_string(), "k1", &old_sig);
    let e = verify_release(forged.as_bytes(), &ring_).unwrap_err();
    assert_eq!(
        (e.code, e.reason),
        (ErrorCode::EdgeSignatureInvalid, "bad_signature")
    );
    // Another key under the same kid.
    let e = verify_release(good.as_bytes(), &keys(&OTHER_SEED, "k1")).unwrap_err();
    assert_eq!(e.reason, "bad_signature");
    let malformed = envelope(&m, &digest_of(&m).to_string(), "k1", "not base64!");
    let e = verify_release(malformed.as_bytes(), &ring_).unwrap_err();
    assert_eq!(
        (e.code, e.reason),
        (ErrorCode::EdgeSignatureInvalid, "signature_malformed")
    );
}

#[test]
fn unknown_edge_key_is_signing_key_unavailable() {
    let m = manifest_json(&standard_payloads());
    let e = verify_release(signed(&m).as_bytes(), &keys(&SEED, "k2")).unwrap_err();
    assert_eq!((e.code.id(), e.reason), ("FG-EDGE-0003", "unknown_key"));
    let pk = forge_agent::public_key_from_seed(&SEED).unwrap();
    let revoked = parse_edge_keys(
        format!(
            r#"{{"keys":[{{"kid":"k1","public_key":"{pk}","revoked_at":"2026-11-01T00:00:00Z"}}]}}"#
        )
        .as_bytes(),
    )
    .unwrap();
    let e = verify_release(signed(&m).as_bytes(), &revoked).unwrap_err();
    assert_eq!((e.code.id(), e.reason), ("FG-EDGE-0004", "key_revoked"));
    let e = parse_edge_keys(b"{}").unwrap_err();
    assert_eq!((e.code.id(), e.reason), ("FG-EDGE-0003", "keys_invalid"));
}

// ── manifest rules ──

#[test]
fn manifest_rejects_unsafe_blob_path() {
    for path in [
        "../etc/passwd",
        "/abs",
        "a//b",
        "a/./b",
        "C:/x",
        "a\\\\b",
        "dir/name.",
        "CON",
        "dir/nul.txt",
        "Com1",
        "lpt9.log",
        "a<b",
        "a|b",
        "what?",
        "star*",
        "",
        "caf\u{e9}",
    ] {
        let m = manifest_json(&[("files", vec![blob(&h('1'), 1, path)])]);
        let e = parse_release(envelope(&m, &format!("sha256:{}", h('0')), "k1", "x").as_bytes())
            .unwrap_err();
        assert_eq!(
            (e.code, e.reason),
            (ErrorCode::EdgeManifestInvalid, "blob_path"),
            "{path}"
        );
    }
}

#[test]
fn manifest_rules_refuse_bad_fields() {
    let ok = manifest_json(&standard_payloads());
    let cases: Vec<(String, &str)> = vec![
        (
            ok.replacen("\"schema_version\":1", "\"schema_version\":2", 1),
            "schema_version",
        ),
        (
            ok.replacen("\"arch\":\"amd64\"", "\"arch\":\"x86\"", 1),
            "arch",
        ),
        (
            ok.replacen("\"sequence\":7", "\"sequence\":0", 1),
            "sequence",
        ),
        (
            ok.replacen("2026-11-24T03:00:00Z", "yesterday", 1),
            "created_at",
        ),
        (
            // An instant needs an offset (E1b: no tz database to guess one).
            ok.replacen("2026-11-24T03:00:00Z", "2026-11-24T03:00:00", 1),
            "created_at",
        ),
        (
            ok.replacen(
                "\"max_install_seconds\":900",
                "\"max_install_seconds\":0",
                1,
            ),
            "max_install_seconds",
        ),
        (
            ok.replacen("\"kind\":\"files\"", "\"kind\":\"oci-image\"", 1),
            "duplicate_kind",
        ),
        (
            ok.replacen("config/app.toml", "image/INDEX.json", 1),
            "duplicate_blob_path",
        ),
        (
            ok.replacen("config/app.toml", "config/.env", 1),
            "secret_like_path",
        ),
        (ok.replacen("\"size\":10,", "\"size\":0,", 1), "blob_size"),
        (
            ok.replacen(
                "\"ci_pipeline_id\":42",
                "\"ci_pipeline_id\":42,\"extra\":1",
                1,
            ),
            "json_syntax",
        ),
        (
            manifest_json(&[("agent", vec![blob(&h('1'), 1, "a"), blob(&h('2'), 1, "b")])]),
            "agent_blob_count",
        ),
        (
            manifest_json(&[
                ("agent", vec![blob(&h('1'), 1, "a")]),
                ("files", vec![blob(&h('2'), 1, "b")]),
            ]),
            "agent_not_alone",
        ),
        (
            manifest_json(&[("files", vec![blob("ABC", 1, "a")])]),
            "blob_sha256",
        ),
        (manifest_json(&[]), "no_payloads"),
    ];
    for (m, reason) in cases {
        let e = parse_release(envelope(&m, &format!("sha256:{}", h('0')), "k1", "x").as_bytes())
            .unwrap_err();
        assert_eq!(
            (e.code, e.reason),
            (ErrorCode::EdgeManifestInvalid, reason),
            "{m}"
        );
    }
    let e = parse_release(envelope(&ok, "md5:00", "k1", "x").as_bytes()).unwrap_err();
    assert_eq!(e.reason, "digest_syntax");
    let e = parse_release(&vec![b' '; MAX_RELEASE_BYTES + 1]).unwrap_err();
    assert_eq!(e.reason, "too_large");
}

#[test]
fn a_blob_path_cannot_be_a_directory_of_another() {
    for (a, b) in [("config", "config/app.toml"), ("config/app.toml", "CONFIG")] {
        let m = manifest_json(&[("files", vec![blob(&h('1'), 1, a), blob(&h('2'), 1, b)])]);
        let e = parse_release(envelope(&m, &format!("sha256:{}", h('0')), "k1", "x").as_bytes())
            .unwrap_err();
        assert_eq!(
            (e.code, e.reason),
            (ErrorCode::EdgeManifestInvalid, "blob_path_conflict"),
            "{a} {b}"
        );
    }
    // Siblings with a common name prefix are fine.
    let m = manifest_json(&[(
        "files",
        vec![blob(&h('1'), 1, "config"), blob(&h('2'), 1, "config.d/x")],
    )]);
    assert!(
        parse_release(envelope(&m, &format!("sha256:{}", h('0')), "k1", "x").as_bytes()).is_ok()
    );
}

#[test]
fn a_parsed_manifest_is_in_canonical_order() {
    // The listed order is not signed, so the parsed manifest never keeps it.
    let a = manifest_json(&standard_payloads());
    let mut reordered = standard_payloads();
    reordered.reverse();
    for (_, blobs) in &mut reordered {
        blobs.reverse();
    }
    let b = manifest_json(&reordered);
    let parse = |m: &str| {
        parse_release(envelope(m, &format!("sha256:{}", h('0')), "k1", "x").as_bytes())
            .unwrap()
            .manifest
    };
    let (ma, mb) = (parse(&a), parse(&b));
    assert_eq!(ma, mb);
    let kinds: Vec<PayloadKind> = ma.payloads.iter().map(|p| p.kind).collect();
    assert_eq!(kinds, [PayloadKind::Files, PayloadKind::OciImage]);
    let oci = &ma.payloads[1].blobs;
    assert!(oci.windows(2).all(|w| w[0].sha256 <= w[1].sha256));
    // A verified release comes back the same way.
    let v = verify_release(signed(&b).as_bytes(), &keys(&SEED, "k1")).unwrap();
    assert_eq!(v.manifest, ma);
}

// ── edge-release.toml ──

const DEFINITION: &str = r#"schema_version = 1
arch = "arm64"
os = "ubuntu-24.04"
max_install_seconds = 900
health_soak_seconds = 600

[[payloads]]
kind = "oci-image"
artifacts = ["dist/pfe-image"]

[[payloads]]
kind = "files"
artifacts = ["dist/config/app.toml", "dist/hooks/can-update"]
"#;

#[test]
fn release_definition_lints_a_good_file() {
    let d = lint_release_definition(DEFINITION.as_bytes()).unwrap();
    assert_eq!(d.arch, Arch::Arm64);
    assert_eq!(d.payloads.len(), 2);
    assert_eq!(d.payloads[1].kind, PayloadKind::Files);
}

#[test]
fn release_definition_refuses_secret_like_file() {
    for secret in [
        "dist/.env",
        "dist/prod.env",
        "dist/.env.local",
        "dist/tls/server.KEY",
        "dist/cert.pem",
        "dist/id_ed25519",
        "dist/.ssh/known_hosts",
        "dist/.docker/config.json",
        "dist/credentials.json",
    ] {
        let text = DEFINITION.replacen("dist/config/app.toml", secret, 1);
        let e = lint_release_definition(text.as_bytes()).unwrap_err();
        assert_eq!(e.len(), 1, "{secret}: {e:?}");
        assert_eq!(e[0].code, ErrorCode::EdgeReleaseDefinitionInvalid);
        assert_eq!(
            (e[0].reason, e[0].subject.as_deref()),
            ("secret_like_path", Some(secret))
        );
    }
    assert!(!is_secret_like_path("dist/keys-readme.txt"));
    assert!(!is_secret_like_path("dist/environment.toml"));
}

#[test]
fn release_definition_lists_every_problem() {
    let text = DEFINITION
        .replacen("arm64", "sparc", 1)
        .replacen("max_install_seconds = 900", "max_install_seconds = 9000", 1)
        .replacen("\"dist/pfe-image\"", "\"../up\"", 1)
        .replacen("kind = \"files\"", "kind = \"oci-image\"", 1);
    let e = lint_release_definition(text.as_bytes()).unwrap_err();
    let reasons: Vec<&str> = e.iter().map(|p| p.reason).collect();
    assert_eq!(
        reasons,
        [
            "arch",
            "max_install_seconds",
            "artifact_path",
            "duplicate_kind"
        ]
    );
    let conflict = DEFINITION.replacen("dist/hooks/can-update", "dist/config", 1);
    let e = lint_release_definition(conflict.as_bytes()).unwrap_err();
    assert_eq!(
        (e[0].reason, e[0].subject.as_deref()),
        ("artifact_path_conflict", Some("dist/config"))
    );
    let e = lint_release_definition(b"schema_version = 1\nunknown = 2\n").unwrap_err();
    assert_eq!(e[0].reason, "toml_syntax");
    let e = lint_release_definition(&[0xff, 0xfe]).unwrap_err();
    assert_eq!(e[0].reason, "not_utf8");
}

#[test]
fn worst_problem_is_tampering_first() {
    let ps = [
        Problem::new(ErrorCode::EdgeManifestInvalid, "x", None),
        Problem::new(ErrorCode::EdgeSignatureInvalid, "y", None),
        Problem::new(ErrorCode::EdgeSigningKeyUnavailable, "z", None),
    ];
    assert_eq!(worst(&ps).map(|p| p.reason), Some("y"));
    assert_eq!(worst(&[]), None);
}

#[test]
fn created_at_is_the_instant_with_or_without_a_zone_annotation() {
    // Plan 21 E1b dropped jiff's tz database: the offset decides the
    // instant, a `[Zone]` annotation is syntax only, and the digest covers
    // the instant, so all three spellings digest alike.
    let base = manifest_json(&standard_payloads());
    let utc = parse_release(signed(&base).as_bytes()).unwrap();
    for other in [
        "2026-11-24T12:00:00+09:00",
        "2026-11-24T12:00:00+09:00[Asia/Tokyo]",
    ] {
        let m = base.replacen("2026-11-24T03:00:00Z", other, 1);
        let r = parse_release(signed(&m).as_bytes()).unwrap();
        assert_eq!(r.manifest.created_at, utc.manifest.created_at, "{other}");
        assert_eq!(manifest_digest(&r.manifest), manifest_digest(&utc.manifest));
    }
}
