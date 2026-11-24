#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly
// The test signs with a synthetic seed (clippy.toml).
#![allow(clippy::disallowed_methods)]
//! `gstn edge release verify` (plan 21 E1, ADR-0041) as a user runs it: the
//! real binary, no host and no token; the release, the blobs and the keys
//! are synthetic (a fixed test seed).

use std::path::Path;
use std::process::Command;

use forge_edge::{manifest_digest, parse_release, sha256, sign_manifest};

const SEED: [u8; 32] = [21u8; 32];
const LAYER: &[u8] = b"synthetic layer bytes";
const CONFIG: &[u8] = b"line = \"L3\"\n";

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Out {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {}", self.stdout))
    }
}

fn gstn(config: &Path, args: &[&str]) -> Out {
    let o = Command::new(env!("CARGO_BIN_EXE_gstn"))
        .args(args)
        .env_remove("GSTN_TOKEN")
        .env("GSTN_HOST", "https://unreachable.invalid")
        .env("GSTN_CONFIG_DIR", config)
        .output()
        .expect("gstn");
    Out {
        code: o.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

fn manifest() -> String {
    format!(
        r#"{{"schema_version":1,"release_id":"r-1","project":"pfs/pfe","version":"2.0.0","source_sha":"{}","sequence":3,"created_at":"2026-11-24T03:00:00Z","arch":"amd64","os":"ubuntu-24.04","payloads":[{{"kind":"oci-image","blobs":[{{"sha256":"{}","size":{},"path":"image/layer"}}]}},{{"kind":"files","blobs":[{{"sha256":"{}","size":{},"path":"config/app.toml"}}]}}],"max_install_seconds":900,"health_soak_seconds":600}}"#,
        "b".repeat(40),
        hex(&sha256(LAYER)),
        LAYER.len(),
        hex(&sha256(CONFIG)),
        CONFIG.len()
    )
}

fn envelope(manifest: &str, digest: &str, sig: &str) -> String {
    format!(r#"{{"manifest":{manifest},"digest":"{digest}","key_id":"k1","signature":"{sig}"}}"#)
}

/// A temp dir with release.json, blobs/ and a config dir holding the key.
struct Fixture {
    dir: tempfile::TempDir,
    config: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let m = manifest();
        let placeholder = envelope(&m, &format!("sha256:{}", "0".repeat(64)), "x");
        let digest = manifest_digest(&parse_release(placeholder.as_bytes()).unwrap().manifest);
        let sig = sign_manifest(&SEED, &digest).unwrap();
        std::fs::write(
            dir.path().join("release.json"),
            envelope(&m, &digest.to_string(), &sig),
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("blobs/image")).unwrap();
        std::fs::create_dir_all(dir.path().join("blobs/config")).unwrap();
        std::fs::write(dir.path().join("blobs/image/layer"), LAYER).unwrap();
        std::fs::write(dir.path().join("blobs/config/app.toml"), CONFIG).unwrap();
        let pk = forge_agent::public_key_from_seed(&SEED).unwrap();
        std::fs::write(
            config.path().join("edge-keys.json"),
            format!(r#"{{"keys":[{{"kid":"k1","public_key":"{pk}"}}]}}"#),
        )
        .unwrap();
        Fixture { dir, config }
    }

    fn path(&self, p: &str) -> String {
        self.dir.path().join(p).display().to_string()
    }

    fn run(&self, extra: &[&str]) -> Out {
        let release = self.path("release.json");
        let mut args = vec!["--json", "edge", "release", "verify", "--release", &release];
        args.extend_from_slice(extra);
        gstn(self.config.path(), &args)
    }
}

#[test]
fn a_signed_release_and_its_blobs_verify() {
    let f = Fixture::new();
    let blobs = f.path("blobs");
    let out = f.run(&["--blobs", &blobs]);
    assert_eq!(out.code, 0, "{} {}", out.stdout, out.stderr);
    let v = out.json();
    assert_eq!(v["verified"], true);
    assert_eq!(v["release_id"], "r-1");
    assert_eq!(v["key_id"], "k1");
    assert_eq!(
        (v["blobs"].as_u64(), v["blobs_checked"].as_u64()),
        (Some(2), Some(2))
    );
    assert_eq!(v["revocation_checked"], false);
    assert!(v["digest"].as_str().unwrap().starts_with("sha256:"));
}

#[test]
fn a_changed_blob_fails_with_signature_invalid() {
    let f = Fixture::new();
    std::fs::write(
        f.dir.path().join("blobs/config/app.toml"),
        b"line = \"L4\"\n",
    )
    .unwrap();
    std::fs::remove_file(f.dir.path().join("blobs/image/layer")).unwrap();
    let blobs = f.path("blobs");
    let out = f.run(&["--blobs", &blobs]);
    assert_eq!(out.code, 1, "{}", out.stdout);
    let v = out.json();
    assert_eq!(v["verified"], false);
    // Blobs are checked in the manifest's canonical order (by SHA-256), not
    // the order the file lists them in.
    let mut reasons: Vec<(&str, &str)> = v["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["error_code"].as_str().unwrap(),
                p["reason"].as_str().unwrap(),
            )
        })
        .collect();
    reasons.sort_unstable();
    assert_eq!(
        reasons,
        [
            ("FG-EDGE-0004", "blob_mismatch"),
            ("FG-EDGE-0004", "blob_missing")
        ]
    );
    assert!(out.stderr.contains("FG-EDGE-0004"), "{}", out.stderr);
}

#[test]
fn an_agent_part_signature_or_missing_keys_never_verify() {
    let f = Fixture::new();
    // The same key over the same digest, in the agent part domain.
    let text = std::fs::read_to_string(f.path("release.json")).unwrap();
    let rel = parse_release(text.as_bytes()).unwrap();
    let part_sig = forge_agent::sign_digest(&SEED, &rel.digest).unwrap();
    std::fs::write(
        f.path("release.json"),
        envelope(&manifest(), &rel.digest.to_string(), &part_sig),
    )
    .unwrap();
    let out = f.run(&[]);
    assert_eq!(out.code, 1);
    let v = out.json();
    assert_eq!(v["problems"][0]["error_code"], "FG-EDGE-0004");
    assert_eq!(v["problems"][0]["reason"], "bad_signature");
    // Nothing of an unverified manifest is shown as a fact.
    assert_eq!(v["verified"], false);
    assert!(v["release_id"].is_null() && v["version"].is_null(), "{v}");

    // No key list: FG-EDGE-0003 keys_missing.
    std::fs::remove_file(f.config.path().join("edge-keys.json")).unwrap();
    let out = f.run(&[]);
    assert_eq!(out.code, 1);
    let v = out.json();
    assert_eq!(v["problems"][0]["error_code"], "FG-EDGE-0003");
    assert_eq!(v["problems"][0]["reason"], "keys_missing");
    assert_eq!(v["problems"][0]["subject"], "edge-keys.json");

    // Not a release file at all: FG-EDGE-0002.
    std::fs::write(f.path("release.json"), b"{}").unwrap();
    let out = f.run(&[]);
    assert_eq!(out.code, 1);
    assert_eq!(out.json()["problems"][0]["error_code"], "FG-EDGE-0002");
}
