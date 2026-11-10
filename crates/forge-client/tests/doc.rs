#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly
//! `gstn doc fmt` / `gstn doc lint` (plan 13 stage 2) as a user runs them:
//! the real binary in a git working tree (all documents synthetic). Test
//! names that match `test = …` fields of FG-DOC-* in errors.toml are
//! load-bearing.

use std::path::Path;
use std::process::Command;

const MANUAL_T: &str = "name: qms.manual\nid_pattern: '^QM-\\d{3}$'\n";

fn doc(id: &str, body: &str) -> String {
    format!("---\ndoctype: qms.manual\nid: {id}\ntitle: t\nowner: qa-team\n---\n\n{body}\n")
}

/// A git working tree with `files` (not committed: `git ls-files --others`
/// lists them, as it lists what a commit would hold).
fn repo(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let st = Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir.path())
        .status()
        .expect("git init");
    assert!(st.success());
    for (path, text) in files {
        let p = dir.path().join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
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
    fn err_json(&self) -> serde_json::Value {
        serde_json::from_str(self.stderr.trim())
            .unwrap_or_else(|e| panic!("stderr is not JSON ({e}): {}", self.stderr))
    }
}

/// Run gstn in `cwd` with no host, no token and an unreachable host: the
/// doc commands must never need any of them.
fn gstn(cwd: &Path, args: &[&str]) -> Out {
    gstn_env(cwd, args, &[])
}

fn gstn_env(cwd: &Path, args: &[&str], env: &[(&str, &Path)]) -> Out {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gstn"));
    for (k, v) in env {
        cmd.env(k, v);
    }
    let o = cmd
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
fn lint_passes_a_repository_whose_documents_keep_their_types() {
    let r = repo(&[
        (".forge/doctypes/qms.manual.yml", MANUAL_T),
        ("qms/QM-001.md", &doc("QM-001", "本文。[[QM-002]]")),
        ("deep/QM-002.md", &doc("QM-002", "本文。")),
        ("README.md", "# not a document\n"),
    ]);
    let o = gstn(r.path(), &["doc", "lint"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert!(
        o.stdout
            .contains("No problem (3 Markdown file(s) and 1 type definition(s) read)."),
        "{}",
        o.stdout
    );
    // From a subdirectory: the root is found upward.
    let o = gstn(&r.path().join("qms"), &["doc", "lint", "--json"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert_eq!(o.json()["markdown_files"], 3);
}

#[test]
fn lint_exits_1_with_doc_lint_problem_and_lists_each_problem() {
    let r = repo(&[
        (".forge/doctypes/qms.manual.yml", MANUAL_T),
        ("qms/QM-001.md", &doc("QM-001", "本文。[[QM-404]]")),
        ("other/QM-002.md", &doc("QM-002", "本文。[[QM-405]]")),
    ]);
    let o = gstn(r.path(), &["doc", "lint"]);
    assert_eq!(o.code, 1, "{}{}", o.stdout, o.stderr);
    assert!(
        o.stdout
            .contains("qms/QM-001.md:8: error: ref_broken QM-404 [FG-DOC-0001]"),
        "{}",
        o.stdout
    );
    assert!(o.stderr.contains("[FG-DOC-0001]"), "{}", o.stderr);
    // --json: the report on stdout, the error on stderr; a path narrows
    // what is reported, never what is read.
    let o = gstn(r.path(), &["doc", "lint", "qms", "--json"]);
    assert_eq!(o.code, 1);
    let v = o.json();
    assert_eq!(v["errors"], 1);
    assert_eq!(v["markdown_files"], 2);
    assert_eq!(v["incomplete"], serde_json::Value::Null);
    let files = v["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"], "qms/QM-001.md");
    assert_eq!(
        files[0]["problems"][0],
        serde_json::json!({"line": 8, "rule": "ref_broken", "subject": "QM-404", "level": "error", "error_code": "FG-DOC-0001"})
    );
    assert_eq!(o.err_json()["error_code"], "FG-DOC-0001");
}

/// Plan 13 Q13: broken references are errors; `--migration-warnings`
/// lowers them (and only them) to warnings, locally.
#[test]
fn migration_warnings_lower_only_broken_references() {
    let r = repo(&[
        (".forge/doctypes/qms.manual.yml", MANUAL_T),
        ("qms/QM-001.md", &doc("QM-001", "本文。[[QM-404]]")),
    ]);
    let o = gstn(r.path(), &["doc", "lint", "--migration-warnings", "--json"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let v = o.json();
    assert_eq!(
        (v["errors"].as_u64(), v["warnings"].as_u64()),
        (Some(0), Some(1))
    );
    assert_eq!(v["files"][0]["problems"][0]["level"], "warning");
    std::fs::write(
        r.path().join("qms/QM-001.md"),
        doc("QM-1", "本文。[[QM-404]]"),
    )
    .unwrap();
    let o = gstn(r.path(), &["doc", "lint", "--migration-warnings"]);
    assert_eq!(o.code, 1, "{}", o.stdout);
}

#[test]
fn an_invalid_doctype_definition_is_doctype_definition_invalid() {
    let r = repo(&[
        (".forge/doctypes/qms.manual.yml", "name: qms.other\n"),
        ("qms/QM-001.md", &doc("QM-001", "一文目。二文目。")),
    ]);
    let o = gstn(r.path(), &["doc", "lint", "--json"]);
    assert_eq!(o.code, 1, "{}", o.stdout);
    assert_eq!(o.err_json()["error_code"], "FG-DOC-0003");
    let v = o.json();
    assert_eq!(v["files"][0]["path"], ".forge/doctypes/qms.manual.yml");
    assert_eq!(v["files"][0]["problems"][0]["error_code"], "FG-DOC-0003");
    // fmt leaves the documents of that type as they are.
    let o = gstn(r.path(), &["doc", "fmt", "--json"]);
    assert_eq!(o.code, 1, "{}", o.stdout);
    assert_eq!(o.err_json()["error_code"], "FG-DOC-0003");
    assert_eq!(
        o.json()["skipped"][0],
        serde_json::json!({"path": "qms/QM-001.md", "reason": "doctype_invalid"})
    );
    assert_eq!(
        std::fs::read_to_string(r.path().join("qms/QM-001.md")).unwrap(),
        doc("QM-001", "一文目。二文目。")
    );
}

#[test]
fn fmt_check_exits_non_zero_when_a_file_would_change() {
    let unformatted = doc("QM-001", "一文目。二文目。");
    let r = repo(&[
        (".forge/doctypes/qms.manual.yml", MANUAL_T),
        ("qms/QM-001.md", &unformatted),
        ("qms/QM-002.md", &doc("QM-002", "一文目。")),
        ("notes.md", "# not a document。です。\n"),
    ]);
    let o = gstn(r.path(), &["doc", "fmt", "--check", "--json"]);
    assert_eq!(o.code, 1, "{}{}", o.stdout, o.stderr);
    assert_eq!(o.err_json()["error_code"], "FG-DOC-0004");
    let v = o.json();
    assert_eq!(v["changed"], serde_json::json!(["qms/QM-001.md"]));
    assert_eq!(v["unchanged"], 1);
    assert_eq!(
        v["skipped"],
        serde_json::json!([]),
        "a non-document is no concern"
    );
    assert_eq!(
        std::fs::read_to_string(r.path().join("qms/QM-001.md")).unwrap(),
        unformatted,
        "--check writes nothing"
    );
    // Formatting writes the file; then --check passes.
    let o = gstn(r.path(), &["doc", "fmt"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert!(
        o.stdout.contains("formatted: qms/QM-001.md"),
        "{}",
        o.stdout
    );
    assert_eq!(
        std::fs::read_to_string(r.path().join("qms/QM-001.md")).unwrap(),
        doc("QM-001", "一文目。\n二文目。")
    );
    assert_eq!(
        std::fs::read_to_string(r.path().join("notes.md")).unwrap(),
        "# not a document。です。\n",
        "a file without a doctype is never touched"
    );
    let o = gstn(r.path(), &["doc", "fmt", "--check"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
}

#[test]
fn fmt_takes_paths_and_names_a_named_file_it_cannot_format() {
    let r = repo(&[
        (".forge/doctypes/qms.manual.yml", MANUAL_T),
        ("a/QM-001.md", &doc("QM-001", "一文目。二文目。")),
        ("b/QM-002.md", &doc("QM-002", "一文目。二文目。")),
        ("b/notes.md", "# notes\n"),
    ]);
    let o = gstn(
        r.path(),
        &["doc", "fmt", "--check", "a", "b/notes.md", "--json"],
    );
    assert_eq!(o.code, 1);
    let v = o.json();
    assert_eq!(v["changed"], serde_json::json!(["a/QM-001.md"]));
    assert_eq!(
        v["skipped"],
        serde_json::json!([{"path": "b/notes.md", "reason": "not_a_document"}])
    );
}

#[test]
fn doc_outside_a_repository_or_its_paths_is_a_usage_error() {
    let plain = tempfile::tempdir().unwrap();
    let o = gstn(plain.path(), &["doc", "lint", "--json"]);
    assert_eq!(o.code, 2, "{}", o.stderr);
    assert_eq!(o.err_json()["error_code"], "FG-CLI-0001");
    assert!(o.stderr.contains("not_in_a_repository"), "{}", o.stderr);
    let r = repo(&[("README.md", "# r\n")]);
    let o = gstn(r.path(), &["doc", "fmt", "nope"]);
    assert_eq!(o.code, 2);
    assert!(o.stderr.contains("path_not_found"), "{}", o.stderr);
    let o = gstn(r.path(), &["doc", "fmt", plain.path().to_str().unwrap()]);
    assert_eq!(o.code, 2);
    assert!(o.stderr.contains("path_outside_repository"), "{}", o.stderr);
}

/// Review MR3: `GIT_DIR` / `GIT_WORK_TREE` / `GIT_INDEX_FILE` in the
/// caller's environment never point gstn at another repository.
#[test]
fn git_location_variables_do_not_redirect_the_listing() {
    let r = repo(&[
        (".forge/doctypes/qms.manual.yml", MANUAL_T),
        ("qms/QM-001.md", &doc("QM-001", "本文。[[QM-404]]")),
    ]);
    let other = repo(&[("README.md", "# other\n")]);
    let o = gstn_env(
        r.path(),
        &["doc", "lint", "--json"],
        &[
            ("GIT_DIR", &other.path().join(".git")),
            ("GIT_WORK_TREE", other.path()),
            ("GIT_INDEX_FILE", &other.path().join(".git/index")),
        ],
    );
    assert_eq!(o.code, 1, "{}{}", o.stdout, o.stderr);
    let v = o.json();
    assert_eq!(v["markdown_files"], 1);
    assert_eq!(v["files"][0]["path"], "qms/QM-001.md");
}

/// Review MR3: a file fmt could not check is never a pass of --check — a
/// named file that is no document, a document too large to format — while
/// a large Markdown file without a `doctype` is none of its concern.
#[test]
fn fmt_check_fails_for_a_file_it_could_not_check() {
    let big = "x".repeat(1024 * 1024 + 10);
    let r = repo(&[
        (".forge/doctypes/qms.manual.yml", MANUAL_T),
        ("qms/QM-001.md", &doc("QM-001", "一文目。")),
        ("notes.md", "# notes\n"),
        ("BIG-README.md", &format!("# big\n{big}\n")),
    ]);
    let o = gstn(r.path(), &["doc", "fmt", "--check"]);
    assert_eq!(
        o.code, 0,
        "a large non-document is no concern: {}{}",
        o.stdout, o.stderr
    );
    let o = gstn(r.path(), &["doc", "fmt", "--check", "notes.md", "--json"]);
    assert_eq!(o.code, 1, "{}{}", o.stdout, o.stderr);
    assert_eq!(o.err_json()["error_code"], "FG-DOC-0004");
    assert_eq!(
        o.json()["skipped"],
        serde_json::json!([{"path": "notes.md", "reason": "not_a_document"}])
    );
    std::fs::write(r.path().join("qms/QM-002.md"), doc("QM-002", &big)).unwrap();
    let o = gstn(r.path(), &["doc", "fmt", "--check", "--json"]);
    assert_eq!(o.code, 1, "{}{}", o.stdout, o.stderr);
    assert_eq!(o.err_json()["error_code"], "FG-DOC-0004");
    assert_eq!(
        o.json()["skipped"],
        serde_json::json!([{"path": "qms/QM-002.md", "reason": "too_large"}])
    );
    // Plain fmt reports it too.
    let o = gstn(r.path(), &["doc", "fmt"]);
    assert!(
        o.stdout.contains("skipped: qms/QM-002.md (too_large)"),
        "{}",
        o.stdout
    );
}

/// ADR-0026 §7: an older-format specification (`spec:`) is rewritten only
/// with `--migrate`; `--check` works with it; a second run changes nothing.
const OLDER: &str = "---\nspec: zeva-engine/config\ntitle: 設定管理\nstatus: deprecated\nowners: [po@example.internal]\nconfidential: true\n---\n\n::: req ZEVA-CFG-012 priority=must\n設定は 10 秒以内に反映されること。\n:::\n";

#[test]
fn fmt_migrate_rewrites_an_older_specification_only_when_asked() {
    let r = repo(&[
        ("spec/config.md", OLDER),
        (
            "spec/broken.md",
            "---\nspec: a\ntitle: t\nstatus: nope\n---\n",
        ),
        ("qms/QM-001.md", &doc("QM-001", "一文目。")),
    ]);
    // Without --migrate the older files are no concern of fmt.
    let o = gstn(r.path(), &["doc", "fmt", "--check"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    // --migrate --check: nothing written, exit 1 (FG-DOC-0004).
    let o = gstn(r.path(), &["doc", "fmt", "--migrate", "--check", "--json"]);
    assert_eq!(o.code, 1, "{}{}", o.stdout, o.stderr);
    assert_eq!(o.err_json()["error_code"], "FG-DOC-0004");
    let v = o.json();
    assert_eq!(v["changed"], serde_json::json!(["spec/config.md"]));
    assert_eq!(
        v["migrated"],
        serde_json::json!([{"path": "spec/config.md", "notes": ["owner_missing", "retired_date_unknown"]}])
    );
    assert_eq!(
        v["skipped"],
        serde_json::json!([{"path": "spec/broken.md", "reason": "older_format_invalid"}])
    );
    assert_eq!(
        std::fs::read_to_string(r.path().join("spec/config.md")).unwrap(),
        OLDER,
        "--check writes nothing"
    );
    // --migrate writes the file in the plan 13 format.
    let o = gstn(r.path(), &["doc", "fmt", "--migrate", "spec/config.md"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert!(
        o.stdout
            .contains("formatted: spec/config.md (migrated; owner_missing, retired_date_unknown)"),
        "{}",
        o.stdout
    );
    let migrated = std::fs::read_to_string(r.path().join("spec/config.md")).unwrap();
    assert!(
        migrated.starts_with(
            "---\ndoctype: spec.system\nid: zeva-engine/config\ntitle: 設定管理\nclassification: confidential\nretired: {}\n# TODO(gstn doc fmt --migrate): owner: <the owning group>\n"
        ),
        "{migrated}"
    );
    assert!(migrated.ends_with(
        "---\n\n::: req ZEVA-CFG-012 priority=must\n設定は 10 秒以内に反映されること。\n:::\n"
    ));
    // Idempotent: a second run (with or without --migrate) changes nothing.
    for args in [
        &["doc", "fmt", "--migrate", "--check", "spec/config.md"][..],
        &["doc", "fmt", "--check", "spec/config.md"],
    ] {
        let o = gstn(r.path(), args);
        assert_eq!(o.code, 0, "{args:?}: {}{}", o.stdout, o.stderr);
    }
    // The lint asks a person for the owner.
    let o = gstn(r.path(), &["doc", "lint", "spec/config.md"]);
    assert_eq!(o.code, 1);
    assert!(o.stdout.contains("field_missing owner"), "{}", o.stdout);
}

/// Review (0d59521): `status: draft` would read approved after a migration —
/// refused (`status_would_be_lost`) unless `--accept-status-loss`.
#[test]
fn fmt_migrate_refuses_to_lose_a_draft_status_unless_accepted() {
    let draft = "---\nspec: a\ntitle: t\nstatus: draft\n---\n::: req A-1\nx\n:::\n";
    let r = repo(&[("spec/a.md", draft)]);
    let o = gstn(r.path(), &["doc", "fmt", "--migrate", "--json"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert_eq!(
        o.json()["skipped"],
        serde_json::json!([{"path": "spec/a.md", "reason": "status_would_be_lost"}])
    );
    assert_eq!(
        std::fs::read_to_string(r.path().join("spec/a.md")).unwrap(),
        draft
    );
    let o = gstn(r.path(), &["doc", "fmt", "--accept-status-loss"]);
    assert_eq!(
        o.code, 2,
        "--accept-status-loss needs --migrate: {}",
        o.stderr
    );
    let o = gstn(
        r.path(),
        &["doc", "fmt", "--migrate", "--accept-status-loss"],
    );
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    let migrated = std::fs::read_to_string(r.path().join("spec/a.md")).unwrap();
    assert!(
        migrated.contains("# TODO(gstn doc fmt --migrate): the older file said status: draft ("),
        "{migrated}"
    );
}
