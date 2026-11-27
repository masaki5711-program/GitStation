#![allow(clippy::disallowed_methods)] // tests sign with a synthetic seed

use super::*;
use crate::{public_key_from_seed, sign_digest};

const SEED: [u8; 32] = [7u8; 32];

fn file(path: &str, body: &[u8]) -> BundleFile {
    BundleFile {
        path: path.to_owned(),
        sha256: sha256(body),
        size: body.len() as u64,
    }
}

fn manifest() -> BundleManifest {
    BundleManifest {
        project: "pfs-ai/assets".to_owned(),
        version: "v2026.48.1".to_owned(),
        commit: "a".repeat(40),
        files: vec![
            file("skills/pe-vibration.md", b"---\nname: pe-vibration\n---\n"),
            file("playbooks/line.toml", b"[[playbook]]\n"),
            file("models/models.toml", b"small = \"qwen3:14b@sha256:00\"\n"),
        ],
        models: vec![format!("qwen3:14b@sha256:{}", "0".repeat(64))],
        bench: BundleBench {
            check: "forge/ai-eval".to_owned(),
            conclusion: "success".to_owned(),
            summary: "all safety cases passed".to_owned(),
        },
    }
}

fn ring() -> KeyRing {
    let pk = public_key_from_seed(&SEED).expect("public key");
    KeyRing::parse(
        serde_json::json!({ "keys": [{ "kid": "1", "public_key": pk }] })
            .to_string()
            .as_bytes(),
    )
    .expect("ring")
}

#[test]
fn digest_is_independent_of_file_order() {
    let a = manifest().canonical().expect("canonical");
    let mut reversed = manifest();
    reversed.files.reverse();
    let b = reversed.canonical().expect("canonical");
    assert_eq!(a.digest(), b.digest());
    assert_eq!(a.files[0].path, "models/models.toml");
}

#[test]
fn every_field_changes_the_digest() {
    let base = manifest().canonical().expect("canonical").digest();
    let mut m = manifest();
    m.files[0] = file("skills/pe-vibration.md", b"changed");
    assert_ne!(m.canonical().expect("c").digest(), base);
    let mut m = manifest();
    m.commit = "b".repeat(40);
    assert_ne!(m.canonical().expect("c").digest(), base);
    let mut m = manifest();
    m.version = "v2026.48.2".to_owned();
    assert_ne!(m.canonical().expect("c").digest(), base);
    let mut m = manifest();
    m.bench.conclusion = "failure".to_owned();
    assert_ne!(m.canonical().expect("c").digest(), base);
    let mut m = manifest();
    m.models.clear();
    assert_ne!(m.canonical().expect("c").digest(), base);
}

#[test]
fn a_signed_bundle_verifies_from_its_json_alone() {
    let m = manifest().canonical().expect("canonical");
    let digest = m.digest();
    let sig = sign_bundle(&SEED, &digest).expect("sign");
    let json = m.to_json().to_string();
    let verified =
        verify_bundle(json.as_bytes(), &digest.to_string(), "1", &sig, &ring()).expect("verifies");
    assert_eq!(verified, m);
}

#[test]
fn a_part_signature_never_verifies_as_a_bundle() {
    let m = manifest().canonical().expect("canonical");
    let digest = m.digest();
    let part_sig = sign_digest(&SEED, &digest).expect("part signature");
    let json = m.to_json().to_string();
    let e = verify_bundle(
        json.as_bytes(),
        &digest.to_string(),
        "1",
        &part_sig,
        &ring(),
    )
    .expect_err("another domain");
    assert_eq!(e.code, ErrorCode::PartSignatureInvalid);
    assert_eq!(e.reason, "bad_signature");
}

#[test]
fn a_changed_manifest_is_a_digest_mismatch() {
    let m = manifest().canonical().expect("canonical");
    let digest = m.digest();
    let sig = sign_bundle(&SEED, &digest).expect("sign");
    let mut tampered = m.clone();
    tampered.files[0].size += 1;
    let json = tampered.to_json().to_string();
    let e = verify_bundle(json.as_bytes(), &digest.to_string(), "1", &sig, &ring())
        .expect_err("tampered");
    assert_eq!(e.reason, "digest_mismatch");
}

#[test]
fn an_unsorted_manifest_json_is_refused() {
    let m = manifest().canonical().expect("canonical");
    let mut json = m.to_json();
    json["files"].as_array_mut().expect("files").reverse();
    let e = parse_manifest(json.to_string().as_bytes()).expect_err("not canonical");
    assert_eq!(e.code, ErrorCode::BundleReleaseRefused);
}

#[test]
fn files_outside_the_bundle_directories_are_refused() {
    for path in [
        "thresholds/proposals/x.md",
        "evals/cases/a/b.json",
        "manifest/m.json",
        "skills/../x",
        "skills/",
        "README.md",
        "skills/..\\..\\loader\\x.md",
        "skills/c:x.md",
        "skills/x.md.",
        "skills/a b.md",
        "skills/x\u{7}.md",
        "skills/é.md",
    ] {
        let mut m = manifest();
        m.files.push(file(path, b"x"));
        let e = m.canonical().expect_err(path);
        assert_eq!(e.reason, "manifest_invalid", "{path}");
    }
}

#[test]
fn paths_differing_only_in_case_are_refused() {
    let mut m = manifest();
    m.files.push(file("skills/PE-vibration.md", b"x"));
    assert_eq!(
        m.canonical().expect_err("case collision").reason,
        "manifest_invalid"
    );
}

#[test]
fn sizes_that_overflow_are_too_large() {
    let mut m = manifest();
    m.files[0].size = u64::MAX;
    m.files[1].size = 2;
    assert_eq!(m.canonical().expect_err("overflow").reason, "too_large");
}

#[test]
fn release_versions_are_weekly_tags() {
    assert_eq!(version_of_tag("assets/v2026.48.1"), Some("v2026.48.1"));
    assert_eq!(version_of_tag("assets/v2026.01.12"), Some("v2026.01.12"));
    for bad in [
        "v2026.48.1",
        "assets/v2026.48",
        "assets/v2026.54.1",
        "assets/v2026.00.1",
        "assets/v2026.4.1",
        "assets/v2026.48.0",
        "assets/v2026.48.01",
        "assets/2026.48.1",
        "assets/v2026.48.1-rc",
        "assets/v02026.48.1",
    ] {
        assert_eq!(version_of_tag(bad), None, "{bad}");
    }
}

#[test]
fn the_summary_is_cut_on_a_char_boundary() {
    let long = "あ".repeat(MAX_BENCH_SUMMARY);
    let cut = cut_summary(&long);
    assert!(cut.len() <= MAX_BENCH_SUMMARY);
    assert!(long.starts_with(&cut));
}
