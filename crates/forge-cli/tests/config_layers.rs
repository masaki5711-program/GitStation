#![allow(clippy::expect_used)] // tests may fail loudly (rust/testing.md)
//! The `forge` binary resolves its configuration file ← environment ← flags.

use std::process::{Command, Output};

/// `forge` with no inherited `FORGE_*` variables.
fn forge(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_forge"));
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("FORGE_") {
            cmd.env_remove(name);
        }
    }
    cmd.args(["--lang", "en"])
        .args(args)
        .envs(env.iter().copied());
    cmd.output().expect("run forge")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn repo_path_without_any_storage_root_is_config_invalid() {
    let out = forge(&["admin", "repo-path", "7"], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("FG-SYS-0002"), "{}", stderr(&out));
}

#[test]
fn storage_root_comes_from_the_config_file_and_env_overrides_it() {
    let from_file = tempfile::tempdir().expect("dir");
    let from_env = tempfile::tempdir().expect("dir");
    let cfg = from_file.path().join("forge.toml");
    let root = from_file.path().to_string_lossy().replace('\\', "/");
    std::fs::write(&cfg, format!("[storage]\nroot = \"{root}\"\n")).expect("write");
    let cfg = cfg.to_string_lossy().into_owned();

    let out = forge(&["--config", &cfg, "admin", "repo-path", "7"], &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("missing"), "{}", stdout(&out));
    let printed = stdout(&out).replace('\\', "/");
    assert!(printed.starts_with(&root), "{printed} vs {root}");

    let env_root = from_env.path().to_string_lossy().into_owned();
    let out = forge(
        &["admin", "repo-path", "7"],
        &[("FORGE_CONFIG", &cfg), ("FORGE_STORAGE_ROOT", &env_root)],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let env_root = env_root.replace('\\', "/");
    assert!(stdout(&out).replace('\\', "/").starts_with(&env_root));
}

#[test]
fn a_typo_in_the_config_file_stops_the_command() {
    let dir = tempfile::tempdir().expect("dir");
    let cfg = dir.path().join("forge.toml");
    std::fs::write(&cfg, "[storage]\nrot = \"/srv\"\n").expect("write");
    let cfg = cfg.to_string_lossy().into_owned();
    let out = forge(&["--config", &cfg, "admin", "repo-path", "7"], &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("FG-SYS-0002"), "{}", stderr(&out));
}

#[test]
fn explain_needs_no_configuration() {
    let out = forge(
        &["explain", "FG-SYS-0002"],
        &[("FORGE_DATABASE_URLL", "typo")],
    );
    assert!(out.status.success(), "{}", stderr(&out));
}
