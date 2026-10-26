#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly (rust/testing.md)

//! `RefName` agrees with the real `git check-ref-format` (the oracle), except
//! that forge additionally rejects components starting with `-`.
//!
//! The oracle calls `std::process::Command` directly (not the forge-git
//! wrapper) on purpose: it must not depend on the code under test.

use proptest::prelude::*;

fn git_accepts(name: &str) -> bool {
    let status = std::process::Command::new("git")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env(
            "SystemRoot",
            std::env::var_os("SystemRoot").unwrap_or_default(),
        )
        // check-ref-format takes no `--`; names always start with `refs/`.
        .args(["check-ref-format", name])
        .status();
    match status {
        Ok(s) => s.success(),
        // A missing git would make every name "invalid" and the oracle vacuous.
        Err(e) => panic!("cannot run git for the oracle: {e}"),
    }
}

fn agrees(name: &str) -> Result<(), String> {
    let ours = forge_core::RefName::parse(name).is_ok();
    let dash_component = name.split('/').any(|c| c.starts_with('-'));
    let theirs = git_accepts(name) && !dash_component;
    if ours == theirs {
        Ok(())
    } else {
        Err(format!("{name:?}: forge={ours} git={theirs}"))
    }
}

fn component() -> impl Strategy<Value = String> {
    // Plausible characters, every character class git cares about, control
    // characters and non-ASCII.
    proptest::string::string_regex(
        r"[a-zA-Z0-9._\-@{}~^:?*\[\\ /\x01\x1f\x7f\u{e9}\u{65e5}\u{202e}]{1,10}",
    )
    .expect("regex")
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn ref_name_validation_matches_git_check_ref_format(parts in proptest::collection::vec(component(), 1..4)) {
        let name = format!("refs/heads/{}", parts.join("/"));
        let verdict = agrees(&name);
        prop_assert!(verdict.is_ok(), "{:?}", verdict);
    }
}

#[test]
fn fixed_edge_cases_match_git() {
    for name in [
        "refs/heads/a@",
        "refs/heads/@",
        "refs/heads/a@{",
        "refs/heads/x.lock/y",
        "refs/heads/a/.lock",
        "refs/heads/a.lock.b",
        "refs/tags/v1.0",
        "refs/heads/a/b/c",
        "refs/heads/\u{e9}",
        "refs/",
        "refs/heads",
    ] {
        if let Err(msg) = agrees(name) {
            panic!("{msg}");
        }
    }
}
