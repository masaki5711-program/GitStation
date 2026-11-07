use forge_errors::ErrorCode;

use super::*;

fn reason(e: &ForgeError) -> String {
    e.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

fn policy() -> MirrorPolicy {
    MirrorPolicy::parse("gitlab.example.invalid, git.example.invalid:8443", false).expect("policy")
}

#[test]
fn an_invalid_mirror_target_is_400_int_0021() {
    let p = policy();
    for (raw, why) in [
        ("", "invalid_url"),
        ("gitlab.example.invalid/x.git", "invalid_url"),
        ("https:///x.git", "invalid_url"),
        ("ssh://gitlab.example.invalid/x.git", "scheme"),
        ("http://gitlab.example.invalid/x.git", "scheme"),
        (
            "https://oauth2:tok@gitlab.example.invalid/x.git",
            "userinfo",
        ),
        (
            "https://gitlab.example.invalid/x.git?private_token=t",
            "query_or_fragment",
        ),
        (
            "https://gitlab.example.invalid/x.git#f",
            "query_or_fragment",
        ),
        ("https://gitlab.example.invalid/a b.git", "invalid_url"),
        ("https://other.example.invalid/x.git", "not_allowlisted"),
        ("https://git.example.invalid/x.git", "not_allowlisted"),
    ] {
        let e = p.check(raw).expect_err(raw);
        assert_eq!(e.code(), ErrorCode::PushMirrorTargetInvalid, "{raw}");
        assert_eq!(e.code().http_status(), 400, "{raw}");
        assert_eq!(reason(&e), why, "{raw}");
        // The URL never reaches the error (it may hold a path worth hiding).
        assert!(!format!("{e:?}").contains("x.git"), "{raw}: {e:?}");
    }
    let too_long = format!(
        "https://gitlab.example.invalid/{}",
        "a".repeat(MAX_URL_BYTES)
    );
    assert_eq!(
        reason(&p.check(&too_long).expect_err("long")),
        "invalid_url"
    );
}

#[test]
fn an_allowlisted_https_url_is_kept_with_its_origin() {
    let p = policy();
    let u = p
        .check("https://GitLab.example.invalid/ymma/app.git")
        .expect("ok");
    assert_eq!(u.as_str(), "https://gitlab.example.invalid/ymma/app.git");
    assert_eq!(u.origin(), "https://gitlab.example.invalid:443");
    assert!(u.is_https());
    let u = p
        .check("https://git.example.invalid:8443/a/b.git")
        .expect("port");
    assert_eq!(u.origin(), "https://git.example.invalid:8443");
}

#[test]
fn http_needs_allow_http_and_a_listed_port_80() {
    let p = MirrorPolicy::parse("127.0.0.1:80", true).expect("policy");
    assert!(p.allow_http());
    let u = p.check("http://127.0.0.1/r.git").expect("http");
    assert_eq!(u.origin(), "http://127.0.0.1:80");
    assert!(!u.is_https());
}

#[test]
fn the_allowlist_refuses_a_malformed_entry_and_names_its_position() {
    for bad in [
        "a/b",
        "user@host",
        "host:notaport",
        "host:99999",
        "https://x",
        "h?q",
    ] {
        let e = MirrorPolicy::parse(&format!("ok.example.invalid {bad}"), false).expect_err(bad);
        assert_eq!(e.code(), ErrorCode::ConfigInvalid, "{bad}");
        assert!(
            e.context().iter().any(|(k, v)| *k == "entry" && v == "2"),
            "{bad}: {e:?}"
        );
    }
    assert!(MirrorPolicy::parse("", false)
        .expect("empty")
        .hosts()
        .is_empty());
    let p =
        MirrorPolicy::parse("[::1]:8443,A.example.invalid,a.example.invalid", false).expect("v6");
    assert_eq!(p.hosts().len(), 2);
}

#[test]
fn credential_names_are_plain_file_names() {
    for ok in ["gitlab", "gitlab-mirror.token", "a", "0_x"] {
        assert_eq!(CredentialName::parse(ok).expect(ok).as_str(), ok);
    }
    for bad in [
        "",
        ".",
        "..",
        ".hidden",
        "a/b",
        "A",
        "a b",
        "-x",
        &"a".repeat(65),
    ] {
        let e = CredentialName::parse(bad).expect_err(bad);
        assert_eq!(reason(&e), "credential_name", "{bad}");
    }
}

#[test]
fn the_audit_shape_names_the_origin_and_never_the_url() {
    let set = PushMirrorChange::Set {
        project: ProjectId::new(7),
        repo: RepoId::new(3),
        origin: "https://gitlab.example.invalid:443".into(),
        credential_name: "gitlab".into(),
        enabled: true,
        created: true,
    };
    assert_eq!(set.action(), "push_mirror.set");
    let v = set.to_value();
    assert_eq!(v["origin"], "https://gitlab.example.invalid:443");
    assert_eq!(v["created"], true);
    let removed = PushMirrorChange::Removed {
        project: ProjectId::new(7),
        repo: RepoId::new(3),
        origin: "https://gitlab.example.invalid:443".into(),
    };
    assert_eq!(removed.action(), "push_mirror.removed");
    assert_eq!(removed.to_value()["repo_id"], 3);
}

#[test]
fn a_credential_is_one_user_token_line_and_never_shows_the_token() {
    let c = MirrorCredential::parse("mirror-bot:glpat-synthetic-0123\n").expect("ok");
    assert_eq!(c.username(), "mirror-bot");
    // base64("mirror-bot:glpat-synthetic-0123")
    assert_eq!(
        c.authorization().expose(),
        "Basic bWlycm9yLWJvdDpnbHBhdC1zeW50aGV0aWMtMDEyMw=="
    );
    assert!(!format!("{c:?}").contains("glpat"), "{c:?}");
    for (text, detail) in [
        ("", "empty"),
        ("\n", "empty"),
        ("token-only", "no_separator"),
        (":tok", "no_separator"),
        ("user:", "no_separator"),
        ("a:b\nc:d", "several_lines"),
        ("a:b\tc", "control_characters"),
    ] {
        let e = MirrorCredential::parse(text).expect_err(text);
        assert_eq!(e.code(), ErrorCode::PushMirrorFailed, "{text:?}");
        assert!(
            e.context()
                .iter()
                .any(|(k, v)| *k == "detail" && v == detail),
            "{text:?}: {e:?}"
        );
        assert!(!format!("{e:?}").contains("tok"), "{e:?}");
    }
    let big = format!("u:{}", "x".repeat(MAX_CREDENTIAL_BYTES));
    assert!(MirrorCredential::parse(&big).is_err());
}
