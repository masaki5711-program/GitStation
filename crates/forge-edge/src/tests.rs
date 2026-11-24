#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // tests may fail loudly
// Tests sign with a synthetic seed (clippy.toml: only forge-fleet signs).
#![allow(clippy::disallowed_methods)]

use forge_errors::ErrorCode;
use jiff::civil::Weekday;
use jiff::Timestamp;
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

// ── waves ──

#[test]
fn wave_plan_must_end_at_100_percent() {
    let bp = |v| Wave::Percent(BasisPoints::new(v).unwrap());
    let e = WavePlan::new(vec![Wave::Canary, bp(500), bp(2500)]).unwrap_err();
    assert_eq!(
        (e.code.id(), e.reason),
        ("FG-EDGE-0005", "last_wave_not_100")
    );
    for (waves, reason) in [
        (vec![], "no_waves"),
        (vec![bp(500), Wave::Canary, bp(10_000)], "canary_not_first"),
        (
            vec![bp(2500), bp(500), bp(10_000)],
            "percent_not_increasing",
        ),
        (vec![bp(500), bp(500), bp(10_000)], "percent_not_increasing"),
    ] {
        assert_eq!(WavePlan::new(waves).unwrap_err().reason, reason);
    }
    assert_eq!(
        WavePlan::from_basis_points(&[None, Some(0), Some(10_000)])
            .unwrap_err()
            .reason,
        "percent_range"
    );
    assert_eq!(
        WavePlan::from_basis_points(&[None, Some(500), Some(2500), Some(10_000)]).unwrap(),
        WavePlan::standard()
    );
}

proptest! {
    #[test]
    fn wave_membership_is_monotonic(
        rollout in "[a-z0-9-]{1,36}",
        device in "[a-z0-9-]{1,36}",
        canary in any::<bool>(),
        cuts in proptest::collection::btree_set(1u16..10_000, 0..6),
    ) {
        let mut cuts = cuts;
        cuts.insert(10_000);
        let mut waves = vec![Wave::Canary];
        waves.extend(cuts.into_iter().map(|c| Wave::Percent(BasisPoints::new(c).unwrap())));
        let plan = WavePlan::new(waves).unwrap();
        let bucket = wave_bucket(&rollout, &device);
        prop_assert!(bucket < 10_000);
        prop_assert_eq!(bucket, wave_bucket(&rollout, &device), "stable");
        let n = plan.waves().len();
        let first = first_wave(&plan, canary, bucket);
        prop_assert!(first < n);
        for k in 0..n {
            prop_assert_eq!(in_wave(&plan, k, canary, bucket), k >= first);
            if k + 1 < n && in_wave(&plan, k, canary, bucket) {
                prop_assert!(in_wave(&plan, k + 1, canary, bucket));
            }
        }
        // The last wave (100 %) holds everyone.
        prop_assert!(in_wave(&plan, n - 1, canary, bucket));
        prop_assert_eq!(in_wave(&plan, 0, canary, bucket), canary);
    }
}

#[test]
fn wave_buckets_spread_and_depend_on_the_rollout() {
    let plan = WavePlan::standard();
    let in_5 = (0..10_000)
        .filter(|i| first_wave(&plan, false, wave_bucket("r1", &format!("d{i}"))) <= 1)
        .count();
    // 5 % of 10 000 is 500; a fair hash stays well within ±20 %.
    assert!((400..=600).contains(&in_5), "{in_5}");
    let moved = (0..1000)
        .filter(|i| wave_bucket("r1", &format!("d{i}")) != wave_bucket("r2", &format!("d{i}")))
        .count();
    assert!(moved > 900, "{moved}");
    // The ids are length-prefixed: no ambiguity between ("ab","c") and ("a","bc").
    assert_ne!(wave_bucket("ab", "c"), wave_bucket("a", "bc"));
}

// ── decide ──

fn t(s: &str) -> Timestamp {
    s.parse().unwrap()
}

fn stats(members: u32, attempted: u32, ok: u32, failed: u32, unresponsive: u32) -> WaveStats {
    WaveStats {
        members,
        attempted,
        succeeded: ok,
        failed,
        rollback_failed: 0,
        unresponsive,
        started_at: t("2026-11-01T00:00:00Z"),
    }
}

const LATER: &str = "2026-11-02T06:00:00Z"; // 30 h after the start: every soak over

fn halt_reason(d: Decision) -> Option<HaltReason> {
    match d {
        Decision::Halt(h) => Some(h.reason),
        Decision::Advance | Decision::Wait { .. } => None,
    }
}

#[test]
fn canary_single_failure_halts() {
    let p = HaltPolicy::default();
    // One of five canaries failed, even below the minimum sample.
    for (attempted, ok) in [(1, 0), (5, 4)] {
        let d = decide(
            &stats(5, attempted, ok, 1, 0),
            WaveKind::Canary,
            &p,
            t(LATER),
        );
        assert_eq!(
            halt_reason(d),
            Some(HaltReason::CanaryFailure),
            "{attempted}"
        );
    }
    // The same in a percentage wave is under the minimum failure count.
    let d = decide(&stats(100, 50, 49, 1, 0), WaveKind::Percent, &p, t(LATER));
    assert_eq!(halt_reason(d), None);
    // A rollback failure halts any wave at once.
    let mut s = stats(1000, 1, 0, 0, 0);
    s.rollback_failed = 1;
    let d = decide(&s, WaveKind::Percent, &p, t("2026-11-01T00:01:00Z"));
    match d {
        Decision::Halt(h) => {
            assert_eq!(
                (h.reason, h.rollback_failed),
                (HaltReason::RollbackFailed, 1)
            );
            assert_eq!(h.reason.as_str(), "rollback_failed");
        }
        other @ (Decision::Advance | Decision::Wait { .. }) => panic!("{other:?}"),
    }
}

#[test]
fn below_min_sample_waits() {
    let p = HaltPolicy::default();
    // Two attempts, both failed: a 100 % failure rate, still only a wait.
    let d = decide(&stats(500, 2, 0, 2, 0), WaveKind::Percent, &p, t(LATER));
    assert_eq!(
        d,
        Decision::Wait {
            reason: WaitReason::BelowMinSample,
            stalled: false
        }
    );
    // A wave smaller than the sample can still finish.
    let d = decide(&stats(2, 2, 2, 0, 0), WaveKind::Percent, &p, t(LATER));
    assert_eq!(d, Decision::Advance);
}

#[test]
fn an_empty_wave_waits_and_never_advances() {
    let p = HaltPolicy::default();
    // Nobody online (or a canary wave without canary devices): no evidence,
    // no advance; after 7 days it is stalled.
    for kind in [WaveKind::Canary, WaveKind::Percent] {
        let d = decide(&stats(0, 0, 0, 0, 0), kind, &p, t("2026-11-08T00:00:00Z"));
        assert_eq!(
            d,
            Decision::Wait {
                reason: WaitReason::NoMembers,
                stalled: true
            }
        );
    }
    assert_eq!(WaitReason::NoMembers.as_str(), "no_members");
}

#[test]
fn failure_rate_halts_before_a_large_min_sample() {
    // min_sample 10, three of three failed: the failure count guards the
    // rate on its own, the sample does not delay the halt.
    let p = HaltPolicy::new(HaltPolicySpec {
        min_sample: 10,
        ..HaltPolicy::default().spec()
    })
    .unwrap();
    let d = decide(&stats(1000, 3, 0, 3, 0), WaveKind::Percent, &p, t(LATER));
    assert_eq!(halt_reason(d), Some(HaltReason::FailureRate));
    // Unresponsive devices still wait for the sample: 1 of 3 is not a rate.
    let d = decide(&stats(1000, 3, 2, 0, 1), WaveKind::Percent, &p, t(LATER));
    assert_eq!(
        d,
        Decision::Wait {
            reason: WaitReason::BelowMinSample,
            stalled: false
        }
    );
}

#[test]
fn failure_rate_boundary_2_percent() {
    let p = HaltPolicy::default();
    // Exactly 2 %: 3 of 150 — does not halt.
    let d = decide(
        &stats(1000, 150, 147, 3, 0),
        WaveKind::Percent,
        &p,
        t(LATER),
    );
    assert_eq!(halt_reason(d), None, "{d:?}");
    // Just over: 3 of 149 (2.01 %) — halts, with the numbers.
    match decide(
        &stats(1000, 149, 146, 3, 0),
        WaveKind::Percent,
        &p,
        t(LATER),
    ) {
        Decision::Halt(h) => {
            assert_eq!(h.reason, HaltReason::FailureRate);
            assert_eq!((h.failed, h.attempted, h.failure_rate_bp), (3, 149, 201));
        }
        other @ (Decision::Advance | Decision::Wait { .. }) => panic!("{other:?}"),
    }
    // Over 2 % but under 3 failed + unresponsive: no halt yet.
    let d = decide(&stats(1000, 50, 48, 2, 0), WaveKind::Percent, &p, t(LATER));
    assert_eq!(halt_reason(d), None);
    // Unresponsive devices count toward the 3, not toward the rate.
    let d = decide(&stats(1000, 60, 57, 2, 1), WaveKind::Percent, &p, t(LATER));
    assert_eq!(halt_reason(d), Some(HaltReason::FailureRate));
}

#[test]
fn unresponsive_rate_halts_and_done_waves_advance_after_soak() {
    let p = HaltPolicy::default();
    // 11 of 100 attempted unresponsive (> 10 %).
    let d = decide(&stats(100, 100, 89, 0, 11), WaveKind::Percent, &p, t(LATER));
    assert_eq!(halt_reason(d), Some(HaltReason::UnresponsiveRate));
    let d = decide(&stats(100, 100, 90, 0, 10), WaveKind::Percent, &p, t(LATER));
    assert_eq!(d, Decision::Advance);
    // 89 % final: in progress.
    let d = decide(&stats(100, 95, 89, 0, 0), WaveKind::Percent, &p, t(LATER));
    assert_eq!(
        d,
        Decision::Wait {
            reason: WaitReason::InProgress,
            stalled: false
        }
    );
    // Canary: done, but the 24 h soak is not over at 23 h.
    let d = decide(
        &stats(5, 5, 5, 0, 0),
        WaveKind::Canary,
        &p,
        t("2026-11-01T23:00:00Z"),
    );
    assert_eq!(
        d,
        Decision::Wait {
            reason: WaitReason::Soaking,
            stalled: false
        }
    );
    // An inconsistent count (more final than attempted) is not a lower rate.
    let d = decide(&stats(1000, 3, 146, 3, 0), WaveKind::Percent, &p, t(LATER));
    assert_eq!(halt_reason(d), Some(HaltReason::FailureRate));
}

#[test]
fn halt_policy_ranges() {
    let spec = HaltPolicy::default().spec();
    assert_eq!(HaltPolicy::new(spec).unwrap(), HaltPolicy::default());
    for (bad, reason) in [
        (
            HaltPolicySpec {
                min_sample: 0,
                ..spec
            },
            "min_sample",
        ),
        (
            HaltPolicySpec {
                failure_rate_bp: 0,
                ..spec
            },
            "failure_rate",
        ),
        (
            HaltPolicySpec {
                unresponsive_rate_bp: 10_001,
                ..spec
            },
            "unresponsive_rate",
        ),
        (
            HaltPolicySpec {
                done_ratio_bp: 99,
                ..spec
            },
            "done_ratio",
        ),
        (
            HaltPolicySpec {
                soak_canary_secs: -1,
                ..spec
            },
            "soak",
        ),
        (
            HaltPolicySpec {
                stall_after_secs: 60,
                ..spec
            },
            "stall_after",
        ),
    ] {
        let e = HaltPolicy::new(bad).unwrap_err();
        assert_eq!(
            (e.code, e.reason),
            (ErrorCode::EdgeRolloutPolicyInvalid, reason)
        );
    }
}

// ── windows ──

fn window(w: Weekday, start: &str, minutes: u16) -> WeeklyWindow {
    WeeklyWindow::new(w, start, minutes).unwrap()
}

fn win(from: &str, until: &str) -> Option<InstallWindow> {
    Some(InstallWindow {
        not_before: t(from),
        not_after: t(until),
    })
}

#[test]
fn window_across_dst_and_midnight() {
    // Saturday 23:00 for 4 hours, Berlin: ends Sunday 03:00 on the clock.
    let s = Schedule::new(
        "Europe/Berlin",
        vec![window(Weekday::Saturday, "23:00", 240)],
        vec![],
    )
    .unwrap();
    // Spring forward (Sunday 2026-03-29, 02:00 CET → 03:00 CEST): 3 h long.
    assert_eq!(
        install_window(&s, t("2026-03-28T12:00:00Z")),
        win("2026-03-28T22:00:00Z", "2026-03-29T01:00:00Z")
    );
    // Asked after midnight, inside the window: the same window.
    assert_eq!(
        install_window(&s, t("2026-03-28T23:30:00Z")),
        win("2026-03-28T22:00:00Z", "2026-03-29T01:00:00Z")
    );
    // Fall back (Sunday 2026-10-25, 03:00 CEST → 02:00 CET): 5 h long.
    assert_eq!(
        install_window(&s, t("2026-10-24T12:00:00Z")),
        win("2026-10-24T21:00:00Z", "2026-10-25T02:00:00Z")
    );
    // An ordinary week: 4 h.
    assert_eq!(
        install_window(&s, t("2026-11-21T12:00:00Z")),
        win("2026-11-21T22:00:00Z", "2026-11-22T02:00:00Z")
    );
    // After the window: the next Saturday.
    assert_eq!(
        install_window(&s, t("2026-11-22T02:00:00Z")),
        win("2026-11-28T22:00:00Z", "2026-11-29T02:00:00Z")
    );
    // A start inside the gap (02:30 does not exist) moves forward by the
    // gap: 03:30 CEST; the end stays 04:30 on the clock, so 1 h is left.
    let gap = Schedule::new(
        "Europe/Berlin",
        vec![window(Weekday::Sunday, "02:30", 120)],
        vec![],
    )
    .unwrap();
    assert_eq!(
        install_window(&gap, t("2026-03-28T12:00:00Z")),
        win("2026-03-29T01:30:00Z", "2026-03-29T02:30:00Z")
    );
    // Shorter than the shift: nothing is left that night (the safe side),
    // the next Sunday's window is answered.
    let lost = Schedule::new(
        "Europe/Berlin",
        vec![window(Weekday::Sunday, "02:30", 60)],
        vec![],
    )
    .unwrap();
    assert_eq!(
        install_window(&lost, t("2026-03-28T12:00:00Z")),
        win("2026-04-05T00:30:00Z", "2026-04-05T01:30:00Z")
    );
    // Jakarta (no DST), a window crossing midnight into Monday.
    let jkt = Schedule::new(
        "Asia/Jakarta",
        vec![window(Weekday::Sunday, "22:00", 180)],
        vec![],
    )
    .unwrap();
    assert_eq!(
        install_window(&jkt, t("2026-11-22T16:00:00Z")),
        win("2026-11-22T15:00:00Z", "2026-11-22T18:00:00Z")
    );
}

#[test]
fn blackouts_cut_windows_and_adjacent_windows_merge() {
    let s = Schedule::new(
        "UTC",
        vec![
            window(Weekday::Monday, "01:00", 60),
            window(Weekday::Monday, "02:00", 60),
        ],
        vec![],
    )
    .unwrap();
    assert_eq!(
        install_window(&s, t("2026-11-23T00:00:00Z")),
        win("2026-11-23T01:00:00Z", "2026-11-23T03:00:00Z")
    );
    let cut = Schedule::new(
        "UTC",
        vec![window(Weekday::Monday, "01:00", 120)],
        vec![Blackout::new(t("2026-11-23T01:30:00Z"), t("2026-11-23T02:30:00Z")).unwrap()],
    )
    .unwrap();
    assert_eq!(
        install_window(&cut, t("2026-11-23T01:40:00Z")),
        win("2026-11-23T02:30:00Z", "2026-11-23T03:00:00Z")
    );
    // A whole week blacked out: the following week.
    let all = Schedule::new(
        "UTC",
        vec![window(Weekday::Monday, "01:00", 60)],
        vec![Blackout::new(t("2026-11-22T00:00:00Z"), t("2026-11-29T00:00:00Z")).unwrap()],
    )
    .unwrap();
    assert_eq!(
        install_window(&all, t("2026-11-22T00:00:00Z")),
        win("2026-11-30T01:00:00Z", "2026-11-30T02:00:00Z")
    );
    let none = Schedule::new("UTC", vec![], vec![]).unwrap();
    assert_eq!(install_window(&none, t("2026-11-22T00:00:00Z")), None);
}

#[test]
fn install_starts_only_with_enough_time_left() {
    let w = win("2026-11-23T01:00:00Z", "2026-11-23T02:00:00Z").unwrap();
    assert!(may_start_install(&w, t("2026-11-23T01:00:00Z"), 900));
    assert!(may_start_install(&w, t("2026-11-23T01:45:00Z"), 900));
    assert!(!may_start_install(&w, t("2026-11-23T01:45:01Z"), 900));
    assert!(!may_start_install(&w, t("2026-11-23T00:59:59Z"), 60));
    assert!(!may_start_install(&w, t("2026-11-23T02:00:00Z"), 0));
}

#[test]
fn schedule_values_are_checked() {
    for (tz, reason) in [("Mars/Base", "time_zone"), ("", "time_zone")] {
        let e = Schedule::new(tz, vec![], vec![]).unwrap_err();
        assert_eq!(
            (e.code, e.reason),
            (ErrorCode::EdgeRolloutPolicyInvalid, reason)
        );
    }
    for (start, minutes, reason) in [
        ("24:00", 60, "window_start"),
        ("7:00", 60, "window_start"),
        ("07:60", 60, "window_start"),
        ("07:00", 0, "window_duration"),
        ("07:00", 1441, "window_duration"),
    ] {
        let e = WeeklyWindow::new(Weekday::Monday, start, minutes).unwrap_err();
        assert_eq!(e.reason, reason, "{start} {minutes}");
    }
    let e = Blackout::new(t("2026-11-23T01:00:00Z"), t("2026-11-23T01:00:00Z")).unwrap_err();
    assert_eq!(e.reason, "blackout_order");
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
