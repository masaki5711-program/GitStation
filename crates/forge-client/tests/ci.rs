#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly
//! `gstn ci lint` / `gstn ci plan` (plan 14 C1) as a user runs them: the
//! real binary in a git working tree, with no host and no token (all data
//! synthetic).

use std::path::Path;
use std::process::Command;

const D: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn valid() -> String {
    format!(
        "version: 1
stages: [build, test, deploy]
jobs:
  build:
    stage: build
    image: registry.internal/rust@sha256:{D}
    timeout: 30m
    script: [cargo build]
  unit:
    stage: test
    needs: [build]
    image: registry.internal/rust@sha256:{D}
    timeout: 1h
    script: [cargo test]
  deploy:
    stage: deploy
    image: registry.internal/deploy@sha256:{D}
    timeout: 10m
    script: [./deploy.sh]
    secrets: [DEPLOY_KEY]
    rules:
      branches: [main]
      merge_requests: false
"
    )
}

fn repo(ci: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let st = Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir.path())
        .status()
        .expect("git init");
    assert!(st.success());
    std::fs::write(dir.path().join(".forge-ci.yml"), ci).unwrap();
    std::fs::create_dir_all(dir.path().join("sub")).unwrap();
    dir
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

fn gstn(cwd: &Path, args: &[&str]) -> Out {
    let o = Command::new(env!("CARGO_BIN_EXE_gstn"))
        .args(args)
        .current_dir(cwd)
        .env_remove("GSTN_TOKEN")
        .env("GSTN_HOST", "https://unreachable.invalid")
        .env("GSTN_CONFIG_DIR", cwd.join(".no-gstn-config"))
        .output()
        .expect("gstn");
    Out {
        code: o.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

#[test]
fn ci_lint_passes_a_valid_file_from_a_subdirectory_with_the_protected_list() {
    let r = repo(&valid());
    let o = gstn(
        &r.path().join("sub"),
        &["ci", "lint", "--protected", "main"],
    );
    assert_eq!(o.code, 0, "{} {}", o.stdout, o.stderr);
    assert!(
        o.stdout.contains("No problem in .forge-ci.yml."),
        "{}",
        o.stdout
    );
}

#[test]
fn ci_lint_without_the_protected_list_reports_secret_protection_unknown() {
    let r = repo(&valid());
    let o = gstn(r.path(), &["ci", "lint", "--json"]);
    assert_eq!(o.code, 1);
    let v = o.json();
    assert_eq!(v["problems"][0]["reason"], "secret_protection_unknown");
    assert_eq!(v["problems"][0]["subject"], "main");
    assert!(o.stderr.contains("FG-CI-0001"), "{}", o.stderr);
}

#[test]
fn ci_lint_lists_each_problem_with_line_and_reason_and_exits_1() {
    let cyc = valid().replace(
        "    stage: build\n",
        "    stage: build\n    needs: [build]\n",
    );
    let r = repo(&cyc);
    let o = gstn(r.path(), &["ci", "lint", "--protected", "main"]);
    assert_eq!(o.code, 1);
    assert!(
        o.stdout
            .contains(".forge-ci.yml:4: needs_cycle — build [FG-CI-0001]"),
        "{}",
        o.stdout
    );
    assert!(o.stderr.contains("FG-CI-0001"), "{}", o.stderr);
}

#[test]
fn ci_lint_reads_a_given_path() {
    let r = repo("");
    std::fs::write(r.path().join("other.yml"), valid()).unwrap();
    let o = gstn(
        r.path(),
        &["ci", "lint", "other.yml", "--protected", "main,release/*"],
    );
    assert_eq!(o.code, 0, "{} {}", o.stdout, o.stderr);
    // The default file is empty: empty_file.
    let o = gstn(r.path(), &["ci", "lint", "--json"]);
    assert_eq!(o.json()["problems"][0]["reason"], "empty_file");
}

#[test]
fn ci_lint_on_a_missing_file_is_an_input_failure() {
    let r = repo("");
    let o = gstn(r.path(), &["ci", "lint", "nope.yml"]);
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("FG-CLI-"), "{}", o.stderr);
    assert!(!o.stderr.contains("FG-CI-0001"), "{}", o.stderr);
}

#[test]
fn ci_plan_prints_the_waves_as_json() {
    let r = repo(&valid());
    let o = gstn(
        r.path(),
        &[
            "ci",
            "plan",
            "--branch",
            "main",
            "--protected",
            "main",
            "--json",
        ],
    );
    assert_eq!(o.code, 0, "{} {}", o.stdout, o.stderr);
    let v = o.json();
    assert_eq!(v["event"], "push");
    assert_eq!(v["protected"], true);
    let jobs: Vec<(String, u64)> = v["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|j| {
            (
                j["name"].as_str().unwrap().to_owned(),
                j["wave"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        jobs,
        [
            ("build".into(), 0),
            ("unit".into(), 1),
            ("deploy".into(), 2)
        ]
    );
}

#[test]
fn ci_plan_for_a_merge_request_leaves_out_the_secret_job() {
    let r = repo(&valid());
    let o = gstn(
        r.path(),
        &[
            "ci",
            "plan",
            "--ref",
            "feature-x",
            "--event",
            "mr",
            "--protected",
            "main",
        ],
    );
    assert_eq!(o.code, 0, "{} {}", o.stdout, o.stderr);
    assert!(
        o.stdout.contains("merge_request on feature-x — 2 job(s)"),
        "{}",
        o.stdout
    );
    assert!(!o.stdout.contains("deploy"), "{}", o.stdout);
}

#[test]
fn ci_plan_refuses_secrets_on_a_branch_not_protected() {
    // `deploy` runs on every branch but the list protects only `main`: the
    // lint already refuses it, so the plan says so too.
    let wide = valid().replace("branches: [main]", "branches: [\"*\"]");
    let r = repo(&wide);
    let o = gstn(
        r.path(),
        &[
            "ci",
            "plan",
            "--branch",
            "dev",
            "--protected",
            "*",
            "--json",
        ],
    );
    // `*` protects every branch: the lint passes and `dev` is protected.
    assert_eq!(o.code, 0, "{} {}", o.stdout, o.stderr);
    let o = gstn(
        r.path(),
        &[
            "ci",
            "plan",
            "--branch",
            "dev",
            "--protected",
            "main",
            "--json",
        ],
    );
    assert_eq!(o.code, 1);
    assert_eq!(
        o.json()["problems"][0]["reason"],
        "secret_on_unprotected_ref"
    );
}

/// Plan 14 C5s: a job's services are linted (each rule a reason) and named
/// in the plan.
#[test]
fn ci_lint_and_plan_show_a_jobs_services() {
    let with_db = valid().replace(
        "    script: [cargo test]\n",
        &format!(
            "    script: [cargo test]\n    services:\n      - name: db\n        image: registry.internal/postgres@sha256:{D}\n        user: \"999:999\"\n        tmpfs: [/var/run/postgresql]\n"
        ),
    );
    let r = repo(&with_db);
    let o = gstn(
        r.path(),
        &[
            "ci",
            "plan",
            "--branch",
            "main",
            "--protected",
            "main",
            "--json",
        ],
    );
    assert_eq!(o.code, 0, "{} {}", o.stdout, o.stderr);
    let v = o.json();
    assert_eq!(v["jobs"][1]["name"], "unit");
    assert_eq!(v["jobs"][1]["services"], serde_json::json!(["db"]));
    assert_eq!(v["jobs"][0]["services"], serde_json::json!([]));
    let o = gstn(
        r.path(),
        &["ci", "plan", "--branch", "main", "--protected", "main"],
    );
    assert!(
        o.stdout
            .contains("unit [test] timeout 3600s after build services db"),
        "{}",
        o.stdout
    );
    // An unpinned service image and a bad tmpfs path: one line each.
    let bad = with_db
        .replace("postgres@sha256:", "postgres:16-")
        .replace("/var/run/postgresql", "/proc/x");
    std::fs::write(r.path().join(".forge-ci.yml"), bad).unwrap();
    let o = gstn(r.path(), &["ci", "lint", "--protected", "main", "--json"]);
    assert_eq!(o.code, 1);
    let reasons: Vec<String> = o.json()["problems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["reason"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(reasons, ["image_not_pinned", "service_tmpfs_invalid"]);
    assert!(o.stderr.contains("FG-CI-0001"), "{}", o.stderr);
}

/// Security review: a root service user passes the lint (exit 0) with a
/// warning line; `--json` lists it under `warnings`.
#[test]
fn ci_lint_warns_about_a_root_service_user() {
    let text = valid().replace(
        "    script: [cargo test]\n",
        &format!(
            "    script: [cargo test]\n    services:\n      - name: db\n        image: registry.internal/postgres@sha256:{D}\n        user: root\n"
        ),
    );
    let r = repo(&text);
    let o = gstn(r.path(), &["ci", "lint", "--protected", "main"]);
    assert_eq!(o.code, 0, "{} {}", o.stdout, o.stderr);
    assert!(
        o.stdout
            .contains(".forge-ci.yml:18: warning service_user_root — db"),
        "{}",
        o.stdout
    );
    let o = gstn(r.path(), &["ci", "lint", "--protected", "main", "--json"]);
    assert_eq!(o.code, 0);
    let v = o.json();
    assert_eq!(v["problems"], serde_json::json!([]));
    assert_eq!(v["warnings"][0]["reason"], "service_user_root");
    assert_eq!(v["warnings"][0]["subject"], "db");
    // A clean file has an empty list.
    let r = repo(&valid());
    let o = gstn(r.path(), &["ci", "lint", "--protected", "main", "--json"]);
    assert_eq!(o.json()["warnings"], serde_json::json!([]));
}
