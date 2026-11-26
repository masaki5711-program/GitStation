//! The approval summary's pure parts (M5 L5-8): the facts' lists, their
//! text and hash, staleness and when to ask again.
//!
//! A synthetic private-key header for the redaction test (no real key):
//! forge:allow-secret (the push secret check, FG-GIT-0022).

use forge_core::risk::{ChangeStatus, ChangedFile};

use super::*;

fn file(path: &str, status: ChangeStatus) -> ChangedFile {
    ChangedFile {
        path: path.to_owned(),
        old_path: None,
        status,
        old_mode: if status == ChangeStatus::Added {
            0
        } else {
            0o100_644
        },
        new_mode: if status == ChangeStatus::Deleted {
            0
        } else {
            0o100_644
        },
        added: None,
        deleted: None,
    }
}

fn facts() -> ApprovalFacts {
    ApprovalFacts {
        deployment: 7,
        tier: DeployTier::Prod,
        sha: "b".repeat(40),
        base_sha: Some("a".repeat(40)),
        range: RangeState::Read,
        commits: 2,
        commits_truncated: false,
        merge_requests: vec![MergeRequestFact {
            iid: 12,
            title: "Add the order table".to_owned(),
            risk: Some("neutral".to_owned()),
        }],
        migrations: vec!["db/migrations/0002_orders.sql".to_owned()],
        protected_paths: Vec::new(),
        ci_config: Vec::new(),
        permissions: Vec::new(),
        lists_truncated: false,
        risk: None,
        failed_checks: vec![CheckFact {
            name: "ci/pipeline".to_owned(),
            conclusion: "failure".to_owned(),
        }],
        freeze_windows: Vec::new(),
    }
}

fn label(head: &str, input: Option<&str>) -> AdviceLabel {
    AdviceLabel {
        id: 1,
        llm_request_id: Some(1),
        head_sha: Some(head.to_owned()),
        input_sha256: input.map(str::to_owned),
        model: "qwen3:27b".to_owned(),
        prompt_version: 1,
        created_at: "2026-11-26T00:00:00Z".to_owned(),
    }
}

#[test]
fn the_path_rules_sort_changes_into_the_fact_lists() {
    let (migrations, protected, ci, perms, cut) = classify(&[
        file("db/migrations/0002_orders.sql", ChangeStatus::Added),
        file(".forge-ci.yml", ChangeStatus::Modified),
        file("src/auth/session.rs", ChangeStatus::Modified),
        file(".claude/settings.json", ChangeStatus::Modified),
        file("CODEOWNERS", ChangeStatus::Modified),
        file("src/cart.rs", ChangeStatus::Modified),
    ]);
    assert_eq!(migrations, ["db/migrations/0002_orders.sql"]);
    assert_eq!(ci, [".forge-ci.yml"]);
    // CODEOWNERS is permission code to the risk rules too.
    assert_eq!(perms, ["src/auth/session.rs", "CODEOWNERS"]);
    let rules: Vec<(&str, &str)> = protected
        .iter()
        .map(|p| (p.path.as_str(), p.rule.as_str()))
        .collect();
    assert_eq!(
        rules,
        [
            (".claude/settings.json", "agent_config"),
            ("CODEOWNERS", "repository_rules")
        ]
    );
    assert!(!cut);
}

#[test]
fn a_list_over_the_cap_says_it_was_cut() {
    let files: Vec<ChangedFile> = (0..=MAX_LISTED)
        .map(|i| file(&format!("db/migrations/{i:04}.sql"), ChangeStatus::Added))
        .collect();
    let (migrations, _, _, _, cut) = classify(&files);
    assert_eq!(migrations.len(), MAX_LISTED);
    assert!(cut, "a cut list is said so, never shown as complete");
}

#[test]
fn every_fact_is_in_the_text_the_model_reads() {
    let text = render_facts(&facts());
    for needle in [
        "environment: prod",
        "db/migrations/0002_orders.sql",
        "!12 Add the order table (forge/risk: neutral)",
        "ci/pipeline (failure)",
    ] {
        assert!(text.contains(needle), "{needle} missing from:\n{text}");
    }
    // The same facts give the same input (the hash binds an advice).
    let a = summary_input(&facts()).expect("prepared");
    let b = summary_input(&facts()).expect("prepared");
    assert_eq!(a.sha256(), b.sha256());
    assert_eq!(a.class(), InputClass::Internal);
}

#[test]
fn a_secret_in_a_title_never_reaches_the_model() {
    let mut f = facts();
    f.merge_requests[0].title =
        "Rotate key -----BEGIN RSA PRIVATE KEY-----\nMIIB\n-----END RSA PRIVATE KEY-----"
            .to_owned();
    match summary_input(&f) {
        Ok(input) => assert!(!input.render().contains("MIIB"), "the key text is redacted"),
        Err(e) => assert_eq!(e.code(), ErrorCode::LlmInputRefused),
    }
}

#[test]
fn new_commit_marks_summary_stale() {
    let sha = "b".repeat(40);
    let input = summary_input(&facts()).expect("prepared");
    let now = Some(input.sha256());
    let fresh = label(&sha, now);
    assert_eq!(staleness(&fresh, &sha, now, None), None);
    // A newer commit requested for the environment.
    let newer = "c".repeat(40);
    assert_eq!(
        staleness(&fresh, &sha, now, Some(&newer)),
        Some(StaleReason::NewerCommit)
    );
    // An advice of another commit is never this approval's.
    assert_eq!(
        staleness(&label(&"a".repeat(40), now), &sha, now, None),
        Some(StaleReason::ShaChanged)
    );
    // The facts moved on (a check finished): stale.
    let mut f = facts();
    f.failed_checks.clear();
    let moved = summary_input(&f).expect("prepared");
    assert_eq!(
        staleness(&fresh, &sha, Some(moved.sha256()), None),
        Some(StaleReason::FactsChanged)
    );
    // Its request was swept: not known to be current.
    assert_eq!(
        staleness(&label(&sha, None), &sha, now, None),
        Some(StaleReason::FactsUnknown)
    );
}

#[test]
fn the_same_facts_are_asked_again_only_after_a_failure_and_a_pause() {
    use forge_db::llm::AskedBefore;
    assert!(should_ask(&AskedBefore::default()));
    for answered in [
        AskedBefore {
            live_or_answered: 1,
            failed_recently: 0,
            total: 1,
        },
        AskedBefore {
            live_or_answered: 0,
            failed_recently: 1,
            total: 1,
        },
        AskedBefore {
            live_or_answered: 0,
            failed_recently: 0,
            total: MAX_ASKS,
        },
    ] {
        assert!(!should_ask(&answered), "{answered:?}");
    }
    assert!(should_ask(&AskedBefore {
        live_or_answered: 0,
        failed_recently: 0,
        total: MAX_ASKS - 1,
    }));
}

#[test]
fn an_agent_is_not_told_secret_file_names() {
    let mut f = facts();
    f.protected_paths = vec![
        PathFact {
            path: "config/prod.key".to_owned(),
            rule: "secret_file".to_owned(),
        },
        PathFact {
            path: ".claude/settings.json".to_owned(),
            rule: "agent_config".to_owned(),
        },
    ];
    withhold_secret_paths(&mut f);
    assert_eq!(f.protected_paths[0].path, WITHHELD_PATH);
    assert_eq!(f.protected_paths[0].rule, "secret_file", "the rule stays");
    assert_eq!(f.protected_paths[1].path, ".claude/settings.json");
}

#[test]
fn the_llm_eval_sets_use_the_rendered_facts_shape() {
    // The approval summary's eval sets (plan 19 L5-10) hand-write the text
    // render_facts makes: a renamed line or range state here must show up
    // there, or the bench measures an input production never sends.
    let mut full = facts();
    full.lists_truncated = true;
    let rendered = render_facts(&full);
    let keys: Vec<&str> = rendered
        .lines()
        .filter(|l| !l.starts_with("- "))
        .filter_map(|l| l.split_once(": ").map(|(k, _)| k))
        .collect();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../forge-llm/evals/approval_summary");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("the eval sets") {
        let path = entry.expect("entry").path();
        let text = std::fs::read_to_string(&path).expect("readable");
        for line in text.lines() {
            // Fact lines only: not TOML keys, tables, comments or list items.
            let skip = line.is_empty()
                || line.starts_with(['#', '[', '"', '-'])
                || line.contains(" = ")
                || line.starts_with("text");
            if skip {
                continue;
            }
            let (key, value) = line.split_once(": ").unwrap_or((line, ""));
            assert!(
                keys.contains(&key),
                "{}: `{key}` is not a line render_facts writes ({keys:?})",
                path.display()
            );
            if key == "range" {
                assert!(
                    ["first_deployment", "read", "unavailable"].contains(&value),
                    "{}: range `{value}`",
                    path.display()
                );
            }
            checked += 1;
        }
    }
    assert!(checked > 50, "only {checked} fact lines found");
}
