#![allow(clippy::expect_used, clippy::unwrap_used)] // tests may fail loudly (rust/testing.md)

//! `RefName` agrees with the real `git check-ref-format` (the oracle), except
//! that forge additionally rejects components starting with `-`.

use proptest::prelude::*;

fn git_accepts(name: &str) -> bool {
    std::process::Command::new("git")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env(
            "SystemRoot",
            std::env::var_os("SystemRoot").unwrap_or_default(),
        )
        // check-ref-format takes no `--`; names always start with `refs/`.
        .args(["check-ref-format", name])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn component() -> impl Strategy<Value = String> {
    // Mostly plausible characters plus every character class git cares about.
    proptest::string::string_regex(r"[a-zA-Z0-9._\-@{}~^:?*\[\\ ]{1,8}").expect("regex")
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, ..ProptestConfig::default() })]

    #[test]
    fn ref_name_validation_matches_git_check_ref_format(parts in proptest::collection::vec(component(), 1..4)) {
        let name = format!("refs/heads/{}", parts.join("/"));
        let ours = forge_core::RefName::parse(&name).is_ok();
        let dash_component = name.split('/').any(|c| c.starts_with('-'));
        let theirs = git_accepts(&name) && !dash_component;
        prop_assert_eq!(ours, theirs, "name: {:?}", name);
    }
}
