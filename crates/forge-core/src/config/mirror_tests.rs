use std::collections::HashMap;
use std::time::Duration;

use forge_errors::ErrorCode;

use super::*;

fn load(pairs: &[(&str, &str)]) -> Result<MirrorConfig, ForgeError> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    MirrorConfig::load(|k| map.get(k).map(String::as_str))
}

fn reason(e: &ForgeError) -> String {
    e.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

#[test]
fn the_defaults_mirror_nothing_until_configured() {
    let c = load(&[]).expect("defaults");
    assert_eq!(c, MirrorConfig::default());
    assert!(c.credentials_dir.is_none());
    assert!(c.policy.hosts().is_empty());
    assert_eq!(c.push_timeout, Duration::from_secs(300));
    assert_eq!(c.lag_alert, Duration::from_secs(900));
}

#[test]
fn bad_values_are_config_invalid_with_their_key() {
    for (pairs, key, why) in [
        (
            vec![(CREDENTIALS_DIR_KEY, "relative/dir")],
            CREDENTIALS_DIR_KEY,
            "not_absolute",
        ),
        (vec![(CA_FILE_KEY, "")], CA_FILE_KEY, "empty"),
        (vec![(ALLOW_HTTP_KEY, "yes")], ALLOW_HTTP_KEY, "invalid"),
        (
            vec![(PUSH_TIMEOUT_KEY, "10")],
            PUSH_TIMEOUT_KEY,
            "below_min",
        ),
        (
            vec![(PUSH_TIMEOUT_KEY, "1501")],
            PUSH_TIMEOUT_KEY,
            "above_max",
        ),
        (vec![(LAG_ALERT_KEY, "x")], LAG_ALERT_KEY, "invalid"),
        (
            vec![(ALLOWED_HOSTS_KEY, "a/b")],
            ALLOWED_HOSTS_KEY,
            "invalid",
        ),
    ] {
        let e = load(&pairs).expect_err(key);
        assert_eq!(e.code(), ErrorCode::ConfigInvalid, "{key}");
        assert!(
            e.context()
                .iter()
                .any(|(k, v)| *k == "config_key" && v == key),
            "{key}: {e:?}"
        );
        assert_eq!(reason(&e), why, "{key}");
    }
}

#[cfg(unix)]
#[test]
fn a_credential_is_read_from_a_private_file_and_problems_are_classified() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().expect("dir");
    let d = dir.path().to_str().expect("utf8").to_owned();
    let c = load(&[(CREDENTIALS_DIR_KEY, &d)]).expect("config");
    let ok = dir.path().join("gitlab");
    std::fs::write(&ok, "mirror-bot:glpat-synthetic\n").expect("write");
    std::fs::set_permissions(&ok, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let name = CredentialName::parse("gitlab").expect("name");
    let cred = c.credential(&name).expect("read");
    assert_eq!(cred.username(), "mirror-bot");

    std::fs::set_permissions(&ok, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let (problem, e) = c.credential(&name).expect_err("insecure");
    assert_eq!(problem, CredentialProblem::Insecure);
    assert!(!format!("{e:?}").contains("glpat"));

    let missing = CredentialName::parse("nope").expect("name");
    assert_eq!(
        c.credential(&missing).expect_err("missing").0,
        CredentialProblem::Missing
    );
    let bad = dir.path().join("bad");
    std::fs::write(&bad, "no-separator\n").expect("write");
    std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    assert_eq!(
        c.credential(&CredentialName::parse("bad").expect("name"))
            .expect_err("invalid")
            .0,
        CredentialProblem::Invalid
    );
    assert_eq!(
        MirrorConfig::default()
            .credential(&name)
            .expect_err("not configured")
            .0,
        CredentialProblem::NotConfigured
    );
}
