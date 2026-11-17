#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly (rust/testing.md)

//! GitStation's own pipeline (`.forge-ci.yml`) runs with no network, so the
//! pinned job image must already hold every crate of `Cargo.lock`. An image
//! older than the lockfile fails every job at dependency resolution
//! (`no matching package named … found`, 2026-11-17: `russh`).
//!
//! `ops/ci-image/build-and-push.mjs` records what it built the image from in
//! `ops/ci-image/image.toml` and pins the digests in `.forge-ci.yml`. These
//! tests fail when the four files disagree: a dependency change without a new
//! image, a hand-edited digest, or a toolchain bump without a new image.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

const REBUILD: &str = "run `node ops/ci-image/build-and-push.mjs` and commit \
                       .forge-ci.yml and ops/ci-image/image.toml (ops/ci-image/README.md)";

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(rel: &str) -> String {
    let path = root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {rel}: {e}"))
}

fn record() -> toml::Value {
    read("ops/ci-image/image.toml")
        .parse()
        .expect("ops/ci-image/image.toml parses")
}

fn recorded<'a>(record: &'a toml::Value, key: &str) -> &'a str {
    record
        .get(key)
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| panic!("ops/ci-image/image.toml has no string `{key}`"))
}

/// The crates a job needs from the image: every package of `Cargo.lock` with
/// a `source` (workspace members have none). One line per package,
/// `name version source checksum`, sorted, each ended by a line feed. Returns
/// the number of lines and the SHA-256 of the text. The same rule is in
/// `ops/ci-image/build-and-push.mjs` (`lockedCrates`).
fn locked_crates(lock: &str) -> (usize, String) {
    fn flush(pkg: &mut Option<[String; 4]>, lines: &mut Vec<String>) {
        if let Some([name, version, source, checksum]) = pkg.take() {
            if !source.is_empty() {
                lines.push(format!("{name} {version} {source} {checksum}"));
            }
        }
    }

    let mut lines = Vec::new();
    let mut pkg: Option<[String; 4]> = None;
    for line in lock.lines() {
        if line == "[[package]]" {
            flush(&mut pkg, &mut lines);
            pkg = Some(Default::default());
            continue;
        }
        let Some(fields) = pkg.as_mut() else { continue };
        for (i, key) in ["name", "version", "source", "checksum"]
            .into_iter()
            .enumerate()
        {
            if let Some(value) = line
                .strip_prefix(key)
                .and_then(|rest| rest.strip_prefix(" = \""))
                .and_then(|rest| rest.strip_suffix('"'))
            {
                fields[i] = value.to_owned();
            }
        }
    }
    flush(&mut pkg, &mut lines);
    lines.sort();

    let mut hasher = Sha256::new();
    for line in &lines {
        hasher.update(line.as_bytes());
        hasher.update(b"\n");
    }
    let hex = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    (lines.len(), hex)
}

#[test]
fn the_fingerprint_covers_registry_packages_only() {
    let lock = "version = 4\n\n\
                [[package]]\nname = \"b\"\nversion = \"1.0.0\"\n\
                source = \"registry+https://example.invalid/index\"\nchecksum = \"00\"\n\
                dependencies = [\n \"a\",\n]\n\n\
                [[package]]\nname = \"forge-x\"\nversion = \"0.1.0\"\ndependencies = [\n \"b\",\n]\n\n\
                [[package]]\nname = \"a\"\nversion = \"2.0.0\"\n\
                source = \"registry+https://example.invalid/index\"\nchecksum = \"11\"\n";
    let (count, sha) = locked_crates(lock);
    assert_eq!(count, 2, "the workspace member has no source");

    // A new edge between crates already in the image needs no new image.
    let more_edges = lock.replace(
        " \"b\",\n]\n\n[[package]]\nname = \"a\"",
        " \"a\",\n \"b\",\n]\n\n[[package]]\nname = \"a\"",
    );
    assert_ne!(more_edges, lock);
    assert_eq!(locked_crates(&more_edges), (2, sha.clone()));

    // CRLF (a checkout without .gitattributes) gives the same fingerprint.
    assert_eq!(locked_crates(&lock.replace('\n', "\r\n")), (2, sha.clone()));

    // A new version of a crate does.
    let bumped = lock.replace("2.0.0", "2.0.1");
    assert_ne!(locked_crates(&bumped).1, sha);
}

#[test]
fn the_pinned_image_holds_every_crate_of_cargo_lock() {
    let record = record();
    let (count, sha) = locked_crates(&read("Cargo.lock"));
    assert!(count > 100, "Cargo.lock lists only {count} crates");
    assert_eq!(
        sha,
        recorded(&record, "crates_sha256"),
        "Cargo.lock has {count} crates that differ from the ones the pinned CI image was built \
         with ({} crates): the jobs of .forge-ci.yml have no network and would fail with \
         `no matching package named … found`; {REBUILD}",
        record
            .get("crates")
            .and_then(toml::Value::as_integer)
            .unwrap_or(-1),
    );
}

#[test]
fn the_pinned_image_has_the_toolchain_of_rust_toolchain_toml() {
    let record = record();
    let toolchain: toml::Value = read("rust-toolchain.toml")
        .parse()
        .expect("rust-toolchain.toml parses");
    let channel = toolchain
        .get("toolchain")
        .and_then(|t| t.get("channel"))
        .and_then(toml::Value::as_str)
        .expect("rust-toolchain.toml has toolchain.channel");
    assert_eq!(
        channel,
        recorded(&record, "rust_channel"),
        "the pinned CI image has another toolchain than rust-toolchain.toml: change RUST_VERSION \
         and the base digest in ops/ci-image/Dockerfile, then {REBUILD}"
    );
}

#[test]
fn forge_ci_yml_pins_exactly_the_recorded_images() {
    let record = record();
    let rust = format!("registry:5000/ci/rust@{}", recorded(&record, "rust_image"));
    let postgres = format!(
        "registry:5000/ci/postgres@{}",
        recorded(&record, "postgres_image")
    );
    for digest in [&rust, &postgres] {
        let hex = digest.rsplit("@sha256:").next().unwrap_or_default();
        assert!(
            hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
            "ops/ci-image/image.toml records no sha256 digest: {digest}"
        );
    }

    let ci = read(".forge-ci.yml");
    let images: Vec<&str> = ci
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("image: "))
        .map(str::trim)
        .collect();
    assert!(
        images.len() >= 3,
        ".forge-ci.yml names only {} images",
        images.len()
    );
    assert!(images.contains(&rust.as_str()), "no job uses {rust}");
    let other: Vec<&&str> = images
        .iter()
        .filter(|image| **image != rust && **image != postgres)
        .collect();
    assert!(
        other.is_empty(),
        ".forge-ci.yml pins images that ops/ci-image/image.toml does not record: {other:?}; {REBUILD}"
    );
}
