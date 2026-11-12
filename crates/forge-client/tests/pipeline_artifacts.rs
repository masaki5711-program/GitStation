//! End-to-end tests of `gstn pipeline artifacts` / `artifact-download` (M4
//! C6) against a scripted loopback server: the routes, the file written
//! new and checked against the listed sha256, and nothing left behind when
//! the bytes do not match. Synthetic data only.

#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

mod common;
use common::{Mock, Out, Resp, Sandbox, TOKEN};

fn gstn(sb: &Sandbox, srv: &Mock, args: &[&str]) -> Out {
    sb.run_env(&srv.url, args, "", &[("GSTN_TOKEN", TOKEN)])
}

fn list(sha: &str) -> String {
    format!(
        r#"{{"artifacts":[{{"id":31,"pipeline_id":42,"job_id":9,"job":"build","attempt":1,"path":"out/app.bin","size_bytes":15,"sha256":"{sha}","keep":false,"created_at":"2026-11-12T00:00:00Z","expires_at":"2026-12-12T00:00:00Z"}}]}}"#
    )
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn artifact_download_writes_a_new_file_and_checks_its_sha256() {
    let want = sha256_hex(b"synthetic bytes");
    let body = list(&want);
    let srv = Mock::start(move |r| {
        if r.target.ends_with("/artifacts") {
            Resp::json(200, &body)
        } else {
            Resp {
                status: 200,
                headers: vec![("Content-Type".into(), "application/octet-stream".into())],
                body: "synthetic bytes".into(),
            }
        }
    });
    let sb = Sandbox::new();
    let dir = tempfile::tempdir().expect("dir");
    let out = dir.path().join("app.bin");
    let o = gstn(
        &sb,
        &srv,
        &[
            "pipeline",
            "artifact-download",
            "--project",
            "7",
            "--pipeline",
            "42",
            "--job",
            "9",
            "31",
            "--output",
            out.to_str().expect("utf-8"),
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(std::fs::read(&out).expect("file"), b"synthetic bytes");
    let seen: Vec<String> = srv.seen().into_iter().map(|r| r.target).collect();
    assert_eq!(
        seen,
        [
            "/api/v1/projects/7/pipelines/42/jobs/9/artifacts",
            "/api/v1/projects/7/pipelines/42/jobs/9/artifacts/31"
        ]
    );
    // Never over an existing file.
    let again = gstn(
        &sb,
        &srv,
        &[
            "pipeline",
            "artifact-download",
            "--project",
            "7",
            "--pipeline",
            "42",
            "--job",
            "9",
            "31",
            "--output",
            out.to_str().expect("utf-8"),
        ],
    );
    assert_ne!(again.code, 0);
}

#[test]
fn a_download_that_does_not_match_its_sha256_leaves_no_file() {
    let body = list(&"0".repeat(64));
    let srv = Mock::start(move |r| {
        if r.target.ends_with("/artifacts") {
            Resp::json(200, &body)
        } else {
            Resp {
                status: 200,
                headers: vec![("Content-Type".into(), "application/octet-stream".into())],
                body: "synthetic bytes".into(),
            }
        }
    });
    let sb = Sandbox::new();
    let dir = tempfile::tempdir().expect("dir");
    let out = dir.path().join("app.bin");
    let o = gstn(
        &sb,
        &srv,
        &[
            "pipeline",
            "artifact-download",
            "--project",
            "7",
            "--pipeline",
            "42",
            "--job",
            "9",
            "31",
            "--output",
            out.to_str().expect("utf-8"),
        ],
    );
    assert_ne!(o.code, 0, "{}", o.stdout);
    assert!(!out.exists(), "a wrong file is removed");
}

#[test]
fn pipeline_artifacts_lists_a_jobs_files() {
    let body = list(&sha256_hex(b"synthetic bytes"));
    let srv = Mock::start(move |_| Resp::json(200, &body));
    let sb = Sandbox::new();
    let o = gstn(
        &sb,
        &srv,
        &[
            "pipeline",
            "artifacts",
            "--project",
            "7",
            "--pipeline",
            "42",
            "--job",
            "9",
        ],
    );
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.contains("31 out/app.bin 15 bytes"), "{}", o.stdout);
}
