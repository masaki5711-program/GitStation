//! Unit tests: registry ↔ clap, C-1 route coverage, exit mapping, host
//! matching, store fallback, the dependency decision.

use crate::cli::command_paths;
use crate::config::{parse_host, Config, Overrides};
use crate::coverage::{non_api_coverage, Coverage, API_ROUTES};
use crate::credential::{answer_get, matches_host, parse_request};
use crate::error::{ClientError, Exit};
use crate::registry::{find, COMMANDS};
use crate::store::{FileBackend, SecretBackend, Token, TokenStore};
use forge_errors::ErrorCode;
use std::cell::RefCell;

fn cfg(host: &str) -> Config {
    let dir = tempfile::tempdir().expect("tempdir").keep();
    let host = host.to_string();
    let env = move |k: &str| (k == "GSTN_HOST").then(|| host.clone());
    Config::load(
        &env,
        &Overrides {
            host: None,
            config_dir: Some(dir),
        },
    )
    .expect("config")
}

#[test]
fn registry_and_clap_describe_the_same_commands() {
    let mut clap_paths = command_paths();
    clap_paths.retain(|p| !p.is_empty());
    let mut reg: Vec<String> = COMMANDS.iter().map(|c| c.name.to_string()).collect();
    reg.sort();
    assert_eq!(
        clap_paths, reg,
        "a command exists in clap xor in registry.rs"
    );
    for c in COMMANDS {
        assert!(
            serde_json::from_str::<serde_json::Value>(c.output).is_ok(),
            "{}: output schema is not JSON",
            c.name
        );
        assert!(!c.examples.is_empty(), "{}: needs an example", c.name);
        for s in c.scopes {
            assert!(
                [
                    "read_repository",
                    "write_repository",
                    "read_api",
                    "api",
                    "admin"
                ]
                .contains(s),
                "{}: unknown scope {s}",
                c.name
            );
        }
    }
}

#[test]
fn registry_routes_are_real_server_routes() {
    let routes = server_routes();
    for c in COMMANDS {
        for r in c.routes {
            assert!(
                routes.contains(&r.to_string()),
                "{} claims {r}, which the server does not have",
                c.name
            );
        }
    }
}

fn server_routes() -> Vec<String> {
    let text = include_str!("../../../docs/authz/routes.generated.json");
    let v: serde_json::Value = serde_json::from_str(text).expect("routes json");
    v.as_array()
        .expect("array")
        .iter()
        .map(|r| {
            format!(
                "{} {}",
                r["method"].as_str().unwrap_or_default(),
                r["path"].as_str().unwrap_or_default()
            )
        })
        .collect()
}

/// C-1 gate: a new REST route without a row here fails the build.
#[test]
fn every_api_route_is_covered_or_planned() {
    let routes = server_routes();
    let api: Vec<&String> = routes
        .iter()
        .filter(|r| {
            r.split(' ')
                .nth(1)
                .is_some_and(|p| p.starts_with("/api/v1"))
        })
        .collect();
    assert!(!api.is_empty());
    for r in &api {
        let (m, p) = r.split_once(' ').expect("method path");
        assert!(
            API_ROUTES.iter().any(|(rm, rp, _)| *rm == m && *rp == p),
            "route {r} has no gstn coverage row: add a command or list it as Planned in coverage.rs (C-1)"
        );
    }
    for (m, p, cov) in API_ROUTES {
        assert!(
            routes.contains(&format!("{m} {p}")),
            "coverage row {m} {p} is not a server route any more"
        );
        if let Coverage::Command(name) = cov {
            let spec = find(name).unwrap_or_else(|| panic!("{m} {p}: no command {name}"));
            assert!(
                spec.routes.contains(&format!("{m} {p}").as_str()),
                "{name} does not call {m} {p}"
            );
        }
    }
    // Every non-/api/v1 route has a stated category (api, git, or web).
    for r in routes.iter().filter(|r| !r.contains(" /api/v1")) {
        let p = r.split(' ').nth(1).unwrap_or_default();
        assert!(!non_api_coverage(p).is_empty());
    }
}

#[test]
fn exit_codes_map_from_registered_codes_and_status() {
    let m = |status, code| ClientError::from_server(status, code, "m", None).exit();
    assert_eq!(m(401, "FG-AUTH-0001"), Exit::Auth);
    assert_eq!(m(403, "FG-AUTH-0003"), Exit::Auth);
    assert_eq!(m(404, "FG-REPO-0005"), Exit::NotFound);
    assert_eq!(m(409, "FG-REPO-0010"), Exit::Conflict);
    assert_eq!(m(400, "FG-WEB-0006"), Exit::Usage);
    assert_eq!(m(503, "FG-DB-0001"), Exit::Retryable);
    assert_eq!(m(500, "FG-SYS-0001"), Exit::Other);
    // the registry class wins over a transport status (a proxy rewriting 404 to 200)
    assert_eq!(m(200, "FG-REPO-0005"), Exit::NotFound);
    // unknown code: status decides
    assert_eq!(m(409, "FG-NEW-0001"), Exit::Conflict);
    assert_eq!(
        [
            Exit::Ok.code(),
            Exit::Other.code(),
            Exit::Usage.code(),
            Exit::Auth.code(),
            Exit::NotFound.code(),
            Exit::Conflict.code(),
            Exit::Retryable.code()
        ],
        [0, 1, 2, 3, 4, 5, 6]
    );
    assert_eq!(
        ClientError::local(ErrorCode::CliServerUnreachable, "").exit(),
        Exit::Retryable
    );
}

#[test]
fn host_parsing_rules() {
    assert!(parse_host("https://git.example.internal", false).is_ok());
    assert!(parse_host("https://git.example.internal:8443/", false).is_ok());
    assert!(parse_host("http://127.0.0.1:3000", false).is_ok());
    assert!(parse_host("http://localhost", false).is_ok());
    assert!(parse_host("http://[::1]:3000", false).is_ok());
    assert!(parse_host("https://git-1.example.internal", false).is_ok());
    assert!(parse_host("http://git.example.internal", false).is_err());
    assert!(parse_host("http://git.example.internal", true).is_ok());
    for bad in [
        "git.example.internal",
        "ftp://x",
        "https://user:pw@x",
        "https://x/path",
        "https://x?q=1",
        "",
    ] {
        assert!(parse_host(bad, true).is_err(), "{bad}");
    }
}

#[test]
fn credential_host_matching_is_exact() {
    let c = cfg("https://Git.Example.internal:8443");
    let req = |s: &str| parse_request(s);
    assert!(matches_host(
        &c,
        &req("protocol=https\nhost=git.example.internal:8443\n\n")
    ));
    assert!(matches_host(
        &c,
        &req("protocol=HTTPS\nhost=GIT.example.internal:8443\n\n")
    ));
    assert!(
        !matches_host(&c, &req("protocol=https\nhost=git.example.internal\n\n")),
        "port differs"
    );
    assert!(!matches_host(
        &c,
        &req("protocol=http\nhost=git.example.internal:8443\n\n")
    ));
    assert!(!matches_host(
        &c,
        &req("protocol=https\nhost=evilgit.example.internal:8443\n\n")
    ));
    let tok = Token::parse("gst_abc").unwrap();
    assert!(answer_get(&c, &req("protocol=https\nhost=other\n\n"), Some(&tok)).is_empty());
    assert!(answer_get(
        &c,
        &req("protocol=https\nhost=git.example.internal:8443\n\n"),
        None
    )
    .is_empty());
    assert!(answer_get(
        &c,
        &req("protocol=https\nhost=git.example.internal:8443\nusername=bob\n\n"),
        Some(&tok)
    )
    .contains("username=bob\n"));
}

#[test]
fn token_debug_is_redacted_and_parse_rejects_junk() {
    let t = Token::parse(" gst_abc123 \n").unwrap();
    assert_eq!(format!("{t:?}"), "Token(<redacted>)");
    for bad in ["", "   ", "two words", "tab\tx", "ü", &"a".repeat(513)] {
        assert!(Token::parse(bad).is_err(), "{bad:?}");
    }
}

struct Failing(&'static str);
impl SecretBackend for Failing {
    fn name(&self) -> &'static str {
        "keyring"
    }
    fn get(&self, _: &str) -> Result<Option<Token>, String> {
        Err(self.0.to_string())
    }
    fn set(&self, _: &str, _: &Token) -> Result<(), String> {
        Err(self.0.to_string())
    }
    fn delete(&self, _: &str) -> Result<bool, String> {
        Err(self.0.to_string())
    }
}

#[test]
fn keyring_failure_falls_back_to_a_0600_file_with_a_visible_warning() {
    let dir = tempfile::tempdir().unwrap();
    let store = TokenStore::new(
        Box::new(Failing("secret_service_unavailable")),
        Some(Box::new(FileBackend::new(dir.path()))),
    );
    let t = Token::parse("gst_fallback_token").unwrap();
    let saved = store.save("https://h", &t).unwrap();
    assert_eq!(saved.backend, "file");
    assert_eq!(saved.warnings.len(), 1);
    assert!(
        saved.warnings[0].contains("0600")
            && saved.warnings[0].contains("secret_service_unavailable")
    );
    assert!(!saved.warnings[0].contains("gst_fallback_token"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let m = std::fs::metadata(dir.path().join("credentials.toml"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(m & 0o777, 0o600);
    }
    // Loading: the keyring read error is a warning, the file answers.
    let l = store.load("https://h", None).unwrap().unwrap();
    assert_eq!(l.token.expose(), "gst_fallback_token");
    assert_eq!(l.source, crate::store::Source::File);
    assert_eq!(l.warnings.len(), 1);
    // Logout cleans the file even though the keyring is unreachable.
    let d = store.delete("https://h").unwrap();
    assert!(d.removed && d.warnings.len() == 1);
    assert!(
        store.load("https://h", None).is_err() || store.load("https://h", None).unwrap().is_none()
    );
}

#[test]
fn keyring_success_removes_a_stale_file_copy() {
    #[derive(Default)]
    struct Mem(RefCell<Option<String>>);
    impl SecretBackend for Mem {
        fn name(&self) -> &'static str {
            "keyring"
        }
        fn get(&self, _: &str) -> Result<Option<Token>, String> {
            Ok(self.0.borrow().as_deref().map(|s| Token::parse(s).unwrap()))
        }
        fn set(&self, _: &str, t: &Token) -> Result<(), String> {
            *self.0.borrow_mut() = Some(t.expose().to_string());
            Ok(())
        }
        fn delete(&self, _: &str) -> Result<bool, String> {
            Ok(self.0.borrow_mut().take().is_some())
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let file = FileBackend::new(dir.path());
    file.set("https://h", &Token::parse("old").unwrap())
        .unwrap();
    let store = TokenStore::new(
        Box::new(Mem::default()),
        Some(Box::new(FileBackend::new(dir.path()))),
    );
    let saved = store
        .save("https://h", &Token::parse("new").unwrap())
        .unwrap();
    assert_eq!((saved.backend, saved.warnings.len()), ("keyring", 0));
    assert!(file.get("https://h").unwrap().is_none());
}

#[test]
fn both_stores_failing_is_credential_store_failed() {
    let store = TokenStore::new(Box::new(Failing("a")), Some(Box::new(Failing("b"))));
    let e = store.save("h", &Token::parse("t").unwrap()).unwrap_err();
    assert_eq!(e.code_id(), "FG-CLI-0006");
    assert!(!e.human().contains("\nt"), "no token");
}

/// The dependency decision (lib.rs): forge-client sees only forge-errors, and
/// no server crate depends on forge-client.
#[test]
fn client_depends_only_on_forge_errors() {
    let own = include_str!("../Cargo.toml");
    let deps: Vec<&str> = own
        .lines()
        .filter(|l| l.trim_start().starts_with("forge-"))
        .collect();
    assert_eq!(deps.len(), 1, "{deps:?}");
    assert!(deps[0].starts_with("forge-errors"));
    for (name, text) in [
        ("forge-core", include_str!("../../forge-core/Cargo.toml")),
        ("forge-auth", include_str!("../../forge-auth/Cargo.toml")),
        ("forge-db", include_str!("../../forge-db/Cargo.toml")),
        ("forge-git", include_str!("../../forge-git/Cargo.toml")),
        (
            "forge-hosting",
            include_str!("../../forge-hosting/Cargo.toml"),
        ),
        ("forge-web", include_str!("../../forge-web/Cargo.toml")),
        ("forge-cli", include_str!("../../forge-cli/Cargo.toml")),
    ] {
        assert!(
            !text.contains("forge-client"),
            "{name} must not depend on forge-client"
        );
    }
}
