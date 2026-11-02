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
fn the_storage_root_flag_overrides_env_and_file() {
    let from_file = tempfile::tempdir().expect("dir");
    let from_env = tempfile::tempdir().expect("dir");
    let from_flag = tempfile::tempdir().expect("dir");
    let cfg = from_file.path().join("forge.toml");
    let root = from_file.path().to_string_lossy().replace('\\', "/");
    std::fs::write(&cfg, format!("[storage]\nroot = \"{root}\"\n")).expect("write");
    let cfg = cfg.to_string_lossy().into_owned();
    let env_root = from_env.path().to_string_lossy().into_owned();
    let flag_root = from_flag.path().to_string_lossy().into_owned();
    let out = forge(
        &["admin", "repo-path", "7", "--storage-root", &flag_root],
        &[("FORGE_CONFIG", &cfg), ("FORGE_STORAGE_ROOT", &env_root)],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let printed = stdout(&out).replace('\\', "/");
    assert!(
        printed.starts_with(&flag_root.replace('\\', "/")),
        "{printed}"
    );
}

#[test]
fn serve_with_an_invalid_configuration_exits_before_binding() {
    for env in [
        ("FORGE_SERVER_BINDX", "127.0.0.1:1"),
        ("FORGE_BIND", "127.0.0.1:1"),
        ("FORGE_SERVER_BIND", "nonsense"),
    ] {
        let out = forge(&["serve"], &[env]);
        assert_eq!(out.status.code(), Some(1), "{env:?}: {}", stderr(&out));
        assert!(stderr(&out).contains("FG-SYS-0002"), "{}", stderr(&out));
    }
}

#[test]
fn a_missing_config_file_from_the_environment_stops_the_command() {
    let dir = tempfile::tempdir().expect("dir");
    let missing = dir.path().join("forge.toml").to_string_lossy().into_owned();
    let out = forge(&["admin", "repo-path", "7"], &[("FORGE_CONFIG", &missing)]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("FG-SYS-0002"), "{}", stderr(&out));
}

#[cfg(unix)]
#[test]
fn a_non_utf8_config_variable_is_rejected_and_a_foreign_one_ignored() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt as _;
    let bad = OsString::from_vec(vec![0x66, 0xff, 0x6f]);
    let run = |name: &str| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_forge"));
        cmd.args(["--lang", "en", "admin", "repo-path", "7"])
            .env("FORGE_STORAGE_ROOT", "/nonexistent-root")
            .env(name, &bad)
            .output()
            .expect("run forge")
    };
    let out = run("FORGE_DATABASE_URL");
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("FG-SYS-0002"), "{}", stderr(&out));
    let out = run("FORGE_REQUEST_ID");
    assert!(!stderr(&out).contains("FG-SYS-0002"), "{}", stderr(&out));
}

#[test]
fn explain_needs_no_configuration() {
    let out = forge(
        &["explain", "FG-SYS-0002"],
        &[("FORGE_DATABASE_URLL", "typo")],
    );
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn serve_and_db_commands_need_a_database_url() {
    for args in [&["serve"][..], &["db", "status"], &["db", "migrate"]] {
        let out = forge(args, &[]);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", stderr(&out));
        let err = stderr(&out);
        assert!(err.contains("FG-SYS-0002"), "{args:?}: {err}");
    }
}

/// M1 phase 8b MR7b: the list routes need `api.cursor_keys_file`; `serve`
/// reads it before it connects or binds — missing and invalid both stop
/// the start with FG-SYS-0002 naming the key (never the file's content).
#[test]
fn serve_needs_valid_cursor_keys_before_it_connects() {
    let root = tempfile::tempdir().expect("dir");
    let root_path = root.path().to_string_lossy().into_owned();
    // Port 1: nothing listens, so reaching the database would be another error.
    let base = [
        (
            "FORGE_DATABASE_URL",
            "postgres://forge_app@127.0.0.1:1/forge",
        ),
        ("FORGE_STORAGE_ROOT", root_path.as_str()),
    ];
    let out = forge(&["serve"], &base);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("FG-SYS-0002"), "{err}");
    assert!(err.contains("api.cursor_keys_file"), "{err}");
    assert!(err.contains("missing"), "{err}");

    let keys = root.path().join("cursor.keys");
    std::fs::write(&keys, "1 not-a-synthetic-secret-key\n").expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&keys, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
    let keys_path = keys.to_string_lossy().into_owned();
    let mut env = base.to_vec();
    env.push(("FORGE_API_CURSOR_KEYS_FILE", keys_path.as_str()));
    let out = forge(&["serve"], &env);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("FG-SYS-0002"), "{err}");
    assert!(err.contains("api.cursor_keys_file"), "{err}");
    assert!(err.contains("invalid_key"), "{err}");
    assert!(!err.contains("not-a-synthetic-secret-key"), "{err}");
}

/// M2 MR3 (ADR-0021 §12): `serve` needs `web.base_url` (the origin of the
/// CSRF check) and reads `web.session_key_file` before it connects — a
/// missing base URL and a broken key file both stop the start with
/// FG-SYS-0002 naming the key (never the file's content).
#[test]
fn serve_needs_the_web_base_url_and_a_valid_session_key_file() {
    let root = tempfile::tempdir().expect("dir");
    let root_path = root.path().to_string_lossy().into_owned();
    let secret_file = |name: &str, text: &str| {
        let path = root.path().join(name);
        std::fs::write(&path, text).expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        }
        path.to_string_lossy().into_owned()
    };
    // Synthetic keys (test-only values).
    let cursor = secret_file("cursor.keys", &format!("1 {}\n", "11".repeat(32)));
    let base = [
        (
            "FORGE_DATABASE_URL",
            "postgres://forge_app@127.0.0.1:1/forge",
        ),
        ("FORGE_STORAGE_ROOT", root_path.as_str()),
        ("FORGE_API_CURSOR_KEYS_FILE", cursor.as_str()),
    ];
    let out = forge(&["serve"], &base);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("FG-SYS-0002"), "{err}");
    assert!(err.contains("web.base_url"), "{err}");
    assert!(err.contains("missing"), "{err}");

    let mut env = base.to_vec();
    env.push(("FORGE_WEB_BASE_URL", "http://git.example.test"));
    let err = stderr(&forge(&["serve"], &env));
    assert!(
        err.contains("web.base_url") && err.contains("insecure_http"),
        "{err}"
    );

    let keys = secret_file("session.keys", "1 not-a-synthetic-session-key\n");
    let mut env = base.to_vec();
    env.push(("FORGE_WEB_BASE_URL", "https://git.example.test"));
    env.push(("FORGE_WEB_SESSION_KEY_FILE", keys.as_str()));
    let out = forge(&["serve"], &env);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("web.session_key_file"), "{err}");
    assert!(err.contains("invalid_key"), "{err}");
    assert!(!err.contains("not-a-synthetic-session-key"), "{err}");
}
