#![allow(clippy::expect_used)] // tests may fail loudly

use super::*;

fn modified(path: &str) -> CandidateChange {
    CandidateChange {
        path: path.to_owned(),
        kind: ChangeKind::Modified,
        old_mode: 0o100_644,
        new_mode: 0o100_644,
    }
}

fn rust_policy<'a>(paths: &'a [String]) -> ScopePolicy<'a> {
    ScopePolicy {
        fixer_paths: paths,
        conflicted: None,
        script_paths: &[],
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn triggers_and_states_round_trip() {
    for t in AutofixTrigger::ALL {
        assert_eq!(AutofixTrigger::parse(t.as_str()), Some(t));
    }
    for s in AttemptState::ALL {
        assert_eq!(AttemptState::parse(s.as_str()), Some(s));
    }
    assert!(!AttemptState::Queued.is_terminal());
    assert!(!AttemptState::Running.is_terminal());
    assert!(AttemptState::Applied.is_terminal());
    assert!(AttemptState::Proposed.is_terminal());
}

#[test]
fn glob_patterns_match_segments() {
    assert!(glob_matches("**/*.rs", "src/lib.rs"));
    assert!(glob_matches("**/*.rs", "lib.rs"));
    assert!(glob_matches("crates/*/src/**", "crates/a/src/x/y.rs"));
    assert!(glob_matches("Cargo.lock", "Cargo.lock"));
    assert!(!glob_matches("Cargo.lock", "sub/Cargo.lock"));
    assert!(glob_matches("**/Cargo.lock", "sub/Cargo.lock"));
    assert!(!glob_matches("*.rs", "src/lib.rs"));
    assert!(!glob_matches("**/*.rs", "src/lib.rs.orig"));
}

/// A path the fixer did not declare is refused, not proposed.
#[test]
fn a_path_outside_the_fixer_paths_is_refused() {
    let paths = strings(&["**/*.rs"]);
    let verdict = check_scope(
        &[modified("src/lib.rs"), modified("README.txt")],
        &rust_policy(&paths),
    );
    assert_eq!(
        verdict,
        ScopeVerdict::Refused(ScopeFinding {
            path: "README.txt".to_owned(),
            reason: "outside_fixer_paths"
        })
    );
    assert_eq!(
        check_scope(&[modified("src/lib.rs")], &rust_policy(&paths)),
        ScopeVerdict::Allowed
    );
}

/// ADR-0017 §2.1: the fixed denials hold whatever the fixer declares.
#[test]
fn fixed_denials_and_bad_paths_are_refused_even_inside_the_fixer_paths() {
    let all = strings(&["**"]);
    for (path, reason) in [
        (".forge-ci.yml", "forbidden_path"),
        ("crates/x/Cargo.toml", "forbidden_path"),
        ("build.rs", "forbidden_path"),
        (".claude/settings.json", "forbidden_path"),
        (".forge/repo.yml", "forbidden_path"),
        ("rust-toolchain.toml", "forbidden_path"),
        ("scripts/hooks/pre.js", "forbidden_path"),
        ("config/.env.local", "secret_path"),
        ("keys/server.pem", "secret_path"),
        (".git/config", "git_dir"),
        ("a/.GIT/hooks", "git_dir"),
        ("GIT~1/x", "git_dir"),
        ("a/b./c", "path_invalid"),
        ("a\\b", "path_invalid"),
        ("a/../b", "path_invalid"),
    ] {
        assert_eq!(
            check_scope(&[modified(path)], &rust_policy(&all)),
            ScopeVerdict::Refused(ScopeFinding {
                path: path.to_owned(),
                reason
            }),
            "{path}"
        );
    }
}

#[test]
fn deletions_modes_and_case_collisions_are_refused() {
    let all = strings(&["**"]);
    let deleted = CandidateChange {
        kind: ChangeKind::Deleted,
        new_mode: 0,
        ..modified("src/a.rs")
    };
    let executable = CandidateChange {
        new_mode: 0o100_755,
        ..modified("src/a.rs")
    };
    let symlink = CandidateChange {
        kind: ChangeKind::Added,
        old_mode: 0,
        new_mode: 0o120_000,
        ..modified("src/link")
    };
    for (change, reason) in [
        (deleted, "deleted"),
        (executable, "mode"),
        (symlink, "mode"),
    ] {
        let ScopeVerdict::Refused(f) = check_scope(&[change], &rust_policy(&all)) else {
            panic!("refused");
        };
        assert_eq!(f.reason, reason);
    }
    let ScopeVerdict::Refused(f) = check_scope(
        &[modified("src/A.rs"), modified("src/a.rs")],
        &rust_policy(&all),
    ) else {
        panic!("refused");
    };
    assert_eq!(f.reason, "case_collision");
}

/// A file the target branch's CI runs is never changed by the bot.
#[test]
fn a_file_the_ci_scripts_name_is_refused() {
    let all = strings(&["**"]);
    let words = script_words(&strings(&[
        "sh ./ci/check.sh --strict",
        "python tools/gen.py",
    ]));
    let policy = ScopePolicy {
        fixer_paths: &all,
        conflicted: None,
        script_paths: &words,
    };
    for path in ["ci/check.sh", "tools/gen.py"] {
        let ScopeVerdict::Refused(f) = check_scope(&[modified(path)], &policy) else {
            panic!("refused {path}");
        };
        assert_eq!(f.reason, "ci_file");
    }
    assert_eq!(
        check_scope(&[modified("src/lib.rs")], &policy),
        ScopeVerdict::Allowed
    );
}

/// The task: migrations and protected (authz) paths are a proposal only.
#[test]
fn migrations_and_protected_paths_are_a_proposal_only() {
    let all = strings(&["**"]);
    let verdict = check_scope(
        &[
            modified("src/lib.rs"),
            modified("db/migrations/0001_init.sql"),
            modified("crates/forge-auth/src/authz/rules.rs"),
        ],
        &rust_policy(&all),
    );
    assert_eq!(
        verdict,
        ScopeVerdict::Proposal(vec![
            ScopeFinding {
                path: "db/migrations/0001_init.sql".to_owned(),
                reason: "migration"
            },
            ScopeFinding {
                path: "crates/forge-auth/src/authz/rules.rs".to_owned(),
                reason: "protected_path"
            },
        ])
    );
}

/// For a conflict only the conflicted paths may change.
#[test]
fn a_conflict_candidate_may_change_only_the_conflicted_paths() {
    let all = strings(&["**"]);
    let conflicted = strings(&["Cargo.lock"]);
    let policy = ScopePolicy {
        fixer_paths: &all,
        conflicted: Some(&conflicted),
        script_paths: &[],
    };
    assert_eq!(
        check_scope(&[modified("Cargo.lock")], &policy),
        ScopeVerdict::Allowed
    );
    let ScopeVerdict::Refused(f) =
        check_scope(&[modified("Cargo.lock"), modified("src/lib.rs")], &policy)
    else {
        panic!("refused");
    };
    assert_eq!(
        (f.path.as_str(), f.reason),
        ("src/lib.rs", "outside_conflict")
    );
}

#[test]
fn too_many_files_are_refused() {
    let all = strings(&["**"]);
    let many: Vec<CandidateChange> = (0..=MAX_CHANGED_FILES)
        .map(|i| modified(&format!("src/f{i}.rs")))
        .collect();
    let ScopeVerdict::Refused(f) = check_scope(&many, &rust_policy(&all)) else {
        panic!("refused");
    };
    assert_eq!(f.reason, "too_many_files");
}

const MARKED: &str = "\
[[package]]
name = \"a\"
<<<<<<< ours
version = \"1.1.0\"
=======
version = \"1.2.0\"
>>>>>>> theirs
[[package]]
name = \"b\"
version = \"2.0.0\"
";

/// ADR-0017 §1.2: a resolution inside the conflicted hunk is allowed.
#[test]
fn a_resolution_inside_the_conflicted_hunk_is_allowed() {
    let resolved = "[[package]]\nname = \"a\"\nversion = \"1.2.0\"\n[[package]]\nname = \"b\"\nversion = \"2.0.0\"\n";
    assert!(within_conflict_hunks(
        MARKED.as_bytes(),
        resolved.as_bytes()
    ));
    // Both sides kept, or the hunk emptied: still inside the hunk.
    let both = "[[package]]\nname = \"a\"\nversion = \"1.1.0\"\nversion = \"1.2.0\"\n[[package]]\nname = \"b\"\nversion = \"2.0.0\"\n";
    assert!(within_conflict_hunks(MARKED.as_bytes(), both.as_bytes()));
}

/// ADR-0017 §1.2: a change outside the conflicted hunk is refused.
#[test]
fn a_change_outside_the_conflicted_hunk_is_refused() {
    let sneaky = "[[package]]\nname = \"a\"\nversion = \"1.2.0\"\n[[package]]\nname = \"b\"\nversion = \"6.6.6\"\n";
    assert!(!within_conflict_hunks(MARKED.as_bytes(), sneaky.as_bytes()));
    let prefixed = "evil = true\n[[package]]\nname = \"a\"\nversion = \"1.2.0\"\n[[package]]\nname = \"b\"\nversion = \"2.0.0\"\n";
    assert!(!within_conflict_hunks(
        MARKED.as_bytes(),
        prefixed.as_bytes()
    ));
    // A marker left in the file is no resolution.
    assert!(!within_conflict_hunks(MARKED.as_bytes(), MARKED.as_bytes()));
}

#[test]
fn a_file_without_a_block_must_stay_unchanged() {
    assert!(within_conflict_hunks(b"a\nb\n", b"a\nb\n"));
    assert!(!within_conflict_hunks(b"a\nb\n", b"a\nc\n"));
    // An unterminated block is never resolved.
    assert!(!within_conflict_hunks(b"<<<<<<< x\na\n", b"a\n"));
}

#[test]
fn two_hunks_keep_the_text_between_them() {
    let marked =
        "x\n<<<<<<< a\n1\n=======\n2\n>>>>>>> b\nmid\n<<<<<<< a\n3\n=======\n4\n>>>>>>> b\ny\n";
    assert!(within_conflict_hunks(
        marked.as_bytes(),
        b"x\n2\nmid\n4\ny\n"
    ));
    assert!(!within_conflict_hunks(
        marked.as_bytes(),
        b"x\n2\nMID\n4\ny\n"
    ));
}

#[test]
fn binary_content_is_detected() {
    assert!(is_binary(b"PNG\0\x01"));
    assert!(!is_binary(b"fn main() {}\n"));
}

/// ADR-0017 §2.6: a fixed message with trailers, never `Signed-off-by`.
#[test]
fn the_commit_message_is_fixed_with_trailers() {
    let msg = commit_message(AutofixTrigger::PipelineFailed, "fmt", 42, "req-1");
    assert!(msg.starts_with("autofix(pipeline_failed): apply the fmt fixer\n\n"));
    assert!(msg.contains("Forge-Autofix-Attempt: 42\n"));
    assert!(msg.contains("Forge-Autofix-Fixer: fmt\n"));
    assert!(msg.contains("Forge-Request-Id: req-1\n"));
    assert!(!msg.contains("Signed-off-by"));
}

#[test]
fn the_settings_change_is_audited_without_free_text() {
    let change = AutofixChange::SettingsChanged {
        project: ProjectId::new(3),
        enabled: true,
    };
    assert_eq!(change.action(), "autofix.settings_changed");
    assert_eq!(
        change.to_value(),
        serde_json::json!({"project_id": 3, "enabled": true})
    );
}
