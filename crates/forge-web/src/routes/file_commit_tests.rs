#![allow(clippy::expect_used)] // tests may fail loudly

use super::*;

fn input() -> SingleFileCommit {
    SingleFileCommit {
        branch: "main".into(),
        start_sha: "a".repeat(40),
        path: "docs/spec.md".into(),
        content: "# Spec\n".into(),
        message: "Edit the spec\n\nbody".into(),
        create_branch: None,
        open_merge_request: None,
        title: None,
    }
}

fn rule(e: &ForgeError) -> (ErrorCode, String, String) {
    let get = |key: &str| {
        e.context()
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    (e.code(), get("param"), get("rule"))
}

#[test]
fn a_direct_commit_targets_the_branch_without_a_merge_request() {
    let c = check(&input(), u64::MAX).expect("valid");
    assert_eq!(c.branch.as_str(), "refs/heads/main");
    assert_eq!(c.target, c.branch);
    assert!(!c.created && !c.open_mr);
    assert_eq!(c.title, "Edit the spec");
}

#[test]
fn a_new_branch_opens_a_merge_request_by_default() {
    let mut i = input();
    i.create_branch = Some("refs/heads/edit/spec".into());
    let c = check(&i, u64::MAX).expect("valid");
    assert_eq!(c.target.as_str(), "refs/heads/edit/spec");
    assert!(c.created && c.open_mr);
    i.open_merge_request = Some(false);
    assert!(!check(&i, u64::MAX).expect("valid").open_mr);
}

#[test]
fn bad_fields_are_param_invalid() {
    let w = ErrorCode::RequestInvalid;
    type Change = Box<dyn Fn(&mut SingleFileCommit)>;
    let cases: Vec<(Change, &str, &str)> = vec![
        (
            Box::new(|i| i.branch = "refs/tags/v1".into()),
            "branch",
            "not_a_branch",
        ),
        (
            Box::new(|i| i.branch = "a..b".into()),
            "branch",
            "invalid_ref",
        ),
        (
            Box::new(|i| i.branch = "b".repeat(40)),
            "branch",
            "hex_like",
        ),
        (
            Box::new(|i| i.start_sha = "HEAD".into()),
            "start_sha",
            "oid",
        ),
        (Box::new(|i| i.message = " \n".into()), "message", "blank"),
        (
            Box::new(|i| i.message = "m".repeat(MAX_MESSAGE_BYTES + 1)),
            "message",
            "too_long",
        ),
        (
            Box::new(|i| i.create_branch = Some("main".into())),
            "create_branch",
            "same_as_branch",
        ),
        (
            Box::new(|i| i.open_merge_request = Some(true)),
            "open_merge_request",
            "needs_create_branch",
        ),
        (
            Box::new(|i| {
                i.create_branch = Some("edit/x".into());
                i.title = Some("t".repeat(256));
            }),
            "title",
            "length",
        ),
    ];
    for (change, param, expected) in cases {
        let mut i = input();
        change(&mut i);
        let e = check(&i, u64::MAX).expect_err(param);
        assert_eq!(rule(&e), (w, param.to_owned(), expected.to_owned()), "{e}");
    }
}

#[test]
fn the_content_limit_is_the_lower_of_the_editor_and_the_push() {
    let mut i = input();
    i.content = "x".repeat(MAX_CONTENT_BYTES + 1);
    let e = check(&i, u64::MAX).expect_err("over 512 KiB");
    assert_eq!(e.code(), ErrorCode::FileCommitTooLarge);
    i.content = "x".repeat(100);
    let e = check(&i, 99).expect_err("over the push blob limit");
    assert_eq!(e.code(), ErrorCode::FileCommitTooLarge);
    i.path = ".git/config".into();
    let e = check(&i, u64::MAX).expect_err("path");
    assert_eq!(e.code(), ErrorCode::FileCommitPathInvalid);
}

#[test]
fn a_secret_is_refused_and_a_waived_one_passes() {
    let log = PushLog {
        request_id: forge_core::RequestId::parse("req-scan").expect("id"),
        project_id: ProjectId::new(1),
        repo_id: 1,
        lang: forge_errors::Lang::En,
        route: ROUTE,
    };
    let path = FilePath::parse("keys/id_rsa").expect("path");
    let key = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3Blbn\n";
    let e = scan(key, &path, &log).expect_err("secret");
    assert_eq!(e.code(), ErrorCode::PushSecretDetected);
    scan(&format!("# forge:allow-secret\n{key}"), &path, &log).expect("waived");
    scan("# Spec\n", &path, &log).expect("clean");
}
