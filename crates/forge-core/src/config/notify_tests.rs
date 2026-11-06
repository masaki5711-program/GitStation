//! Tests of `notify.*` (M3 N1). A separate file: `config_tests.rs` is edited by
//! other branches.

#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly

use std::io::Write as _;
use std::time::Duration;

use forge_errors::{ErrorCode, ForgeError};

use crate::config::{load, Env};

fn env(pairs: &[(&str, &str)]) -> Env {
    Env::from_pairs(pairs.iter().copied())
}

fn file(body: &str) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().expect("temp file");
    f.write_all(body.as_bytes()).expect("write");
    f
}

fn assert_invalid(err: &ForgeError, key: &str, reason: &str) {
    assert_eq!(err.code(), ErrorCode::ConfigInvalid, "{err}");
    let text = err.to_string();
    assert!(text.contains(&format!("config_key={key}")), "{text}");
    assert!(text.contains(&format!("reason={reason}")), "{text}");
}

fn ca_path() -> &'static str {
    static PATH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        std::env::temp_dir()
            .join("forge-n1-relay-ca.pem")
            .to_string_lossy()
            .into_owned()
    })
}

fn notify_env<'a>(extra: &[(&'a str, &'a str)]) -> Env {
    let mut pairs = vec![
        ("FORGE_NOTIFY_ENABLED", "true"),
        ("FORGE_NOTIFY_SMTP_HOST", "mail.example.test"),
        ("FORGE_NOTIFY_FROM_ADDRESS", "forge@example.test"),
    ];
    // TLS needs the operator's CA file (no OS store): the default env has one.
    let plain = extra.contains(&("FORGE_NOTIFY_SMTP_TLS", "none"));
    if !plain {
        pairs.push(("FORGE_NOTIFY_SMTP_CA_FILE", ca_path()));
    }
    pairs.retain(|(k, _)| !extra.iter().any(|(ek, _)| ek == k));
    pairs.extend_from_slice(extra);
    Env::from_pairs(pairs)
}

/// Nothing is sent until the operator turns it on: the defaults are off,
/// every bound is the documented one, and a relay is not required while off.
#[test]
fn notify_is_off_by_default_with_documented_bounds() {
    use super::{NotifyConfig, SmtpTls};
    let cfg = load(None, &env(&[])).expect("defaults");
    assert_eq!(cfg.notify, NotifyConfig::default());
    assert!(!cfg.notify.enabled);
    assert!(cfg.notify.smtp.is_none());
    assert_eq!(cfg.notify.default_language, forge_errors::Lang::Ja);
    assert_eq!(cfg.notify.connect_timeout, Duration::from_millis(5_000));
    assert_eq!(cfg.notify.send_timeout, Duration::from_millis(30_000));
    assert_eq!(cfg.notify.max_attempts, 5);

    let on = load(None, &notify_env(&[])).expect("enabled");
    let smtp = on.notify.smtp.as_ref().expect("relay");
    assert!(on.notify.enabled);
    assert_eq!(smtp.host, "mail.example.test");
    assert_eq!(smtp.port, 587);
    assert_eq!(smtp.tls, SmtpTls::StartTls);
    assert_eq!(smtp.from_address, "forge@example.test");
    assert_eq!(smtp.from_name, "GitStation");
    assert!(smtp.username.is_none() && smtp.ca_file.is_some());

    // The default port follows the protection.
    for (tls, port) in [("tls", 465), ("none", 25), ("starttls", 587)] {
        let cfg = load(None, &notify_env(&[("FORGE_NOTIFY_SMTP_TLS", tls)])).expect(tls);
        assert_eq!(cfg.notify.smtp.expect("relay").port, port, "{tls}");
    }
    let custom = load(
        None,
        &notify_env(&[
            ("FORGE_NOTIFY_SMTP_PORT", "2525"),
            ("FORGE_NOTIFY_SMTP_TLS", "none"),
            ("FORGE_NOTIFY_DEFAULT_LANGUAGE", "id"),
            ("FORGE_NOTIFY_FROM_NAME", "フォージ"),
            ("FORGE_NOTIFY_MAX_ATTEMPTS", "3"),
            ("FORGE_NOTIFY_CONNECT_TIMEOUT_MS", "1000"),
            ("FORGE_NOTIFY_SEND_TIMEOUT_MS", "2000"),
        ]),
    )
    .expect("custom");
    assert_eq!(custom.notify.smtp.as_ref().expect("relay").port, 2525);
    assert_eq!(custom.notify.default_language, forge_errors::Lang::Id);
    assert_eq!(custom.notify.max_attempts, 3);
    assert_eq!(custom.notify.send_timeout, Duration::from_millis(2_000));

    // The TOML file takes the same keys.
    let toml = file(
        "[notify]\nenabled = true\nsmtp_host = \"10.0.0.5\"\nsmtp_port = 25\nsmtp_tls = \"none\"\nfrom_address = \"forge@example.test\"\n",
    );
    let from_file = load(Some(toml.path()), &env(&[])).expect("file");
    assert_eq!(from_file.notify.smtp.expect("relay").host, "10.0.0.5");
}

/// Enabled needs the relay and the sender; the error names the key.
#[test]
fn notify_enabled_requires_a_relay_and_a_sender() {
    let err = load(None, &env(&[("FORGE_NOTIFY_ENABLED", "true")])).expect_err("no host");
    assert_invalid(&err, "notify.smtp_host", "missing");
    let err = load(
        None,
        &env(&[
            ("FORGE_NOTIFY_ENABLED", "true"),
            ("FORGE_NOTIFY_SMTP_HOST", "mail.example.test"),
        ]),
    )
    .expect_err("no sender");
    assert_invalid(&err, "notify.from_address", "missing");
    // Off: the relay may stay unnamed, but what is set must still be sane.
    let err = load(None, &env(&[("FORGE_NOTIFY_SMTP_PORT", "0")])).expect_err("port 0 while off");
    assert_invalid(&err, "notify.smtp_port", "below_min");
}

/// Every refused value names its key and reason (FG-SYS-0002); nothing
/// reaches an SMTP command line unchecked.
#[test]
fn notify_refuses_bad_values() {
    let cases: &[(&str, &str, &str, &str)] = &[
        ("FORGE_NOTIFY_ENABLED", "yes", "notify.enabled", "invalid"),
        (
            "FORGE_NOTIFY_SMTP_PORT",
            "0",
            "notify.smtp_port",
            "below_min",
        ),
        (
            "FORGE_NOTIFY_SMTP_PORT",
            "70000",
            "notify.smtp_port",
            "above_max",
        ),
        ("FORGE_NOTIFY_SMTP_TLS", "ssl", "notify.smtp_tls", "invalid"),
        (
            "FORGE_NOTIFY_SMTP_HOST",
            "mail host",
            "notify.smtp_host",
            "invalid",
        ),
        (
            "FORGE_NOTIFY_SMTP_HOST",
            "mail/host",
            "notify.smtp_host",
            "invalid",
        ),
        (
            "FORGE_NOTIFY_SMTP_HOST",
            "mail.example.test\r\nQUIT",
            "notify.smtp_host",
            "invalid",
        ),
        (
            "FORGE_NOTIFY_FROM_ADDRESS",
            "no-at-sign",
            "notify.from_address",
            "invalid",
        ),
        (
            "FORGE_NOTIFY_FROM_ADDRESS",
            "a@b@c.test",
            "notify.from_address",
            "invalid",
        ),
        (
            "FORGE_NOTIFY_FROM_ADDRESS",
            "a b@c.test",
            "notify.from_address",
            "invalid",
        ),
        (
            "FORGE_NOTIFY_FROM_ADDRESS",
            "<a@c.test>",
            "notify.from_address",
            "invalid",
        ),
        (
            "FORGE_NOTIFY_FROM_ADDRESS",
            "a@c.test\r\nBcc: x@y.test",
            "notify.from_address",
            "invalid",
        ),
        (
            "FORGE_NOTIFY_FROM_ADDRESS",
            "é@c.test",
            "notify.from_address",
            "invalid",
        ),
        (
            "FORGE_NOTIFY_FROM_ADDRESS",
            "a@localhost",
            "notify.from_address",
            "invalid",
        ),
        (
            "FORGE_NOTIFY_FROM_NAME",
            "Forge\r\nBcc: x@y.test",
            "notify.from_name",
            "invalid",
        ),
        (
            "FORGE_NOTIFY_DEFAULT_LANGUAGE",
            "fr",
            "notify.default_language",
            "invalid",
        ),
        (
            "FORGE_NOTIFY_CONNECT_TIMEOUT_MS",
            "50",
            "notify.connect_timeout_ms",
            "below_min",
        ),
        (
            "FORGE_NOTIFY_CONNECT_TIMEOUT_MS",
            "60001",
            "notify.connect_timeout_ms",
            "above_max",
        ),
        (
            "FORGE_NOTIFY_SEND_TIMEOUT_MS",
            "300001",
            "notify.send_timeout_ms",
            "above_max",
        ),
        (
            "FORGE_NOTIFY_SEND_TIMEOUT_MS",
            "4000",
            "notify.send_timeout_ms",
            "below_connect_timeout",
        ),
        (
            "FORGE_NOTIFY_MAX_ATTEMPTS",
            "0",
            "notify.max_attempts",
            "below_min",
        ),
        (
            "FORGE_NOTIFY_MAX_ATTEMPTS",
            "26",
            "notify.max_attempts",
            "above_max",
        ),
        (
            "FORGE_NOTIFY_SMTP_CA_FILE",
            "relative/ca.pem",
            "notify.smtp_ca_file",
            "not_absolute",
        ),
    ];
    for (var, value, key, reason) in cases {
        let err = load(None, &notify_env(&[(var, value)])).expect_err(value);
        assert_invalid(&err, key, reason);
    }
}

/// Credentials never travel in the clear, and half a login is refused.
#[test]
fn notify_credentials_need_protection_and_both_halves() {
    let pw = file("s3cret\n");
    let missing = pw
        .path()
        .with_file_name("forge-n1-no-such-password")
        .to_string_lossy()
        .into_owned();
    let path = pw.path().to_string_lossy().into_owned();
    let err = load(
        None,
        &notify_env(&[
            ("FORGE_NOTIFY_SMTP_TLS", "none"),
            ("FORGE_NOTIFY_SMTP_USERNAME", "forge"),
            ("FORGE_NOTIFY_SMTP_PASSWORD_FILE", path.as_str()),
        ]),
    )
    .expect_err("clear text");
    assert_invalid(&err, "notify.smtp_username", "credentials_without_tls");
    let err = load(
        None,
        &notify_env(&[("FORGE_NOTIFY_SMTP_USERNAME", "forge")]),
    )
    .expect_err("no password");
    assert_invalid(&err, "notify.smtp_password_file", "missing");
    let err = load(
        None,
        &notify_env(&[("FORGE_NOTIFY_SMTP_PASSWORD_FILE", path.as_str())]),
    )
    .expect_err("no user");
    assert_invalid(&err, "notify.smtp_username", "missing");
    let err = load(
        None,
        &notify_env(&[
            ("FORGE_NOTIFY_SMTP_TLS", "none"),
            ("FORGE_NOTIFY_SMTP_CA_FILE", path.as_str()),
        ]),
    )
    .expect_err("ca without tls");
    assert_invalid(&err, "notify.smtp_ca_file", "needs_tls");

    // A good login: the password is read from the file, never printed.
    let cfg = load(
        None,
        &notify_env(&[
            ("FORGE_NOTIFY_SMTP_USERNAME", "forge"),
            ("FORGE_NOTIFY_SMTP_PASSWORD_FILE", path.as_str()),
        ]),
    )
    .expect("login");
    let password = cfg.notify.smtp_password().expect("read").expect("set");
    assert_eq!(password.expose(), "s3cret");
    assert!(!format!("{cfg:?}").contains("s3cret"));
    // A missing file stops the start; a disabled config does not read it.
    let err = load(
        None,
        &notify_env(&[
            ("FORGE_NOTIFY_SMTP_USERNAME", "forge"),
            ("FORGE_NOTIFY_SMTP_PASSWORD_FILE", missing.as_str()),
        ]),
    )
    .expect_err("unreadable");
    assert_invalid(&err, "notify.smtp_password_file", "unreadable");
    let off = load(
        None,
        &env(&[
            ("FORGE_NOTIFY_SMTP_USERNAME", "forge"),
            ("FORGE_NOTIFY_SMTP_PASSWORD_FILE", missing.as_str()),
        ]),
    )
    .expect("off does not read");
    assert!(!off.notify.enabled);
}

/// Only the operator's CA is trusted: TLS without `notify.smtp_ca_file` is
/// refused at load (no fallback to the operating system's store).
#[test]
fn notify_tls_requires_the_ca_file() {
    for tls in ["starttls", "tls"] {
        let err = load(
            None,
            &env(&[
                ("FORGE_NOTIFY_ENABLED", "true"),
                ("FORGE_NOTIFY_SMTP_HOST", "mail.example.test"),
                ("FORGE_NOTIFY_FROM_ADDRESS", "forge@example.test"),
                ("FORGE_NOTIFY_SMTP_TLS", tls),
            ]),
        )
        .expect_err(tls);
        assert_invalid(&err, "notify.smtp_ca_file", "missing");
    }
    // The default mode is starttls, so the default needs it too.
    let err = load(
        None,
        &env(&[
            ("FORGE_NOTIFY_ENABLED", "true"),
            ("FORGE_NOTIFY_SMTP_HOST", "mail.example.test"),
            ("FORGE_NOTIFY_FROM_ADDRESS", "forge@example.test"),
        ]),
    )
    .expect_err("default tls");
    assert_invalid(&err, "notify.smtp_ca_file", "missing");
    // Plain text needs none; disabled needs none.
    assert!(load(None, &notify_env(&[("FORGE_NOTIFY_SMTP_TLS", "none")])).is_ok());
    assert!(load(None, &env(&[("FORGE_NOTIFY_SMTP_TLS", "tls")])).is_ok());
}
