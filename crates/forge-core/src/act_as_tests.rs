#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)

use base64::Engine;
use forge_errors::ErrorCode;

use super::*;

fn key_line(kid: i16, byte: u8, marker: &str) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode([byte; 32]);
    format!("{kid}:{b64} {marker}")
}

fn ring(lines: &[String]) -> ActAsKeys {
    ActAsKeys::parse(&lines.join("\n")).expect("ring")
}

const HEAD: &str = "1111111111111111111111111111111111111111";
const TIP: &str = "2222222222222222222222222222222222222222";

fn mr<'a>(id: i64, head: &'a str, target_ref: &'a str, tip: &'a str) -> SealedSubject<'a> {
    SealedSubject::MergeRequest {
        merge_request_id: id,
        head_sha: head,
        target_ref,
        target_sha: tip,
    }
}

fn binding(target_ref: &str) -> AssertionBinding<'_> {
    AssertionBinding {
        public_id: "0123456789abcdef",
        integration_id: 3,
        user_id: 7,
        intent: Intent::Approve,
        project_id: 11,
        subject: mr(13, HEAD, target_ref, TIP),
        expires_at_micros: 1_790_000_000_000_000,
    }
}

fn set_binding(external: &str, resolves: Option<bool>) -> AssertionBinding<'_> {
    AssertionBinding {
        intent: if resolves.is_some() {
            Intent::BindingSet
        } else {
            Intent::BindingRemove
        },
        subject: SealedSubject::Binding {
            external_project_id: external,
            resolves_work_items: resolves,
        },
        ..binding("refs/heads/main")
    }
}

#[test]
fn act_as_email_form_is_refused() {
    assert_eq!(
        ActAsTarget::parse("user:alice@example.invalid"),
        Err(TargetRefusal::EmailForm)
    );
    let err = TargetRefusal::EmailForm.error();
    assert_eq!(err.code(), ErrorCode::ActAsTargetInvalid);
    assert_eq!(err.code().id(), "FG-INT-0006");
    assert!(err
        .context()
        .iter()
        .any(|(k, v)| *k == "reason" && v == "email_form"));
}

#[test]
fn the_header_grammar_takes_a_user_id_or_an_employee_number() {
    assert_eq!(
        ActAsTarget::parse("user:42"),
        Ok(ActAsTarget::User(UserId::new(42)))
    );
    assert_eq!(
        ActAsTarget::parse("employee:E-1.2_x"),
        Ok(ActAsTarget::Employee("E-1.2_x".into()))
    );
    // Case-sensitive, kept as sent.
    assert_eq!(
        ActAsTarget::parse("employee:ab"),
        Ok(ActAsTarget::Employee("ab".into()))
    );
    assert_ne!(
        ActAsTarget::parse("employee:AB"),
        ActAsTarget::parse("employee:ab")
    );
}

#[test]
fn every_other_value_is_refused_with_its_reason() {
    use TargetRefusal as R;
    let long = format!("employee:{}", "a".repeat(65));
    let cases: &[(&str, R)] = &[
        ("", R::MissingValue),
        (" user:1", R::Malformed),
        ("user:1 ", R::Malformed),
        ("user:1,user:2", R::Malformed),
        ("employee:a,b", R::Malformed),
        ("user:", R::Malformed),
        ("user:0", R::Malformed),
        ("user:01", R::Malformed),
        ("user:-1", R::Malformed),
        ("user:12345678901234567890", R::Malformed),
        ("user:abc", R::Malformed),
        ("USER:1", R::Malformed),
        ("employee:", R::Malformed),
        ("employee:a b", R::Malformed),
        ("employee:ä", R::Malformed),
        (&long, R::Malformed),
        ("email:a@b", R::Malformed),
        ("user:a@b", R::EmailForm),
        ("1", R::Malformed),
    ];
    for (raw, want) in cases {
        assert_eq!(ActAsTarget::parse(raw), Err(*want), "{raw:?}");
    }
    let names: std::collections::HashSet<_> =
        TargetRefusal::ALL.iter().map(|r| r.as_str()).collect();
    assert_eq!(names.len(), TargetRefusal::ALL.len());
}

#[test]
fn an_employee_target_never_prints_its_number() {
    let t = ActAsTarget::parse("employee:S12345").expect("parse");
    assert!(!format!("{t:?}").contains("S12345"));
    assert_eq!(t.form(), "employee");
}

#[test]
fn an_assertion_round_trips_and_never_prints_its_secret() {
    let a = AssertionToken::generate().expect("rng");
    let text = a.reveal();
    assert!(text.starts_with("fga_"));
    assert_eq!(text.len(), 4 + 16 + 1 + 64);
    let back = AssertionToken::parse(&text).expect("parse");
    assert_eq!(back.public_id(), a.public_id());
    assert_eq!(back.secret(), a.secret());
    let shown = format!("{a:?}");
    assert!(shown.contains("[redacted]") && !shown.contains(&text[21..]));
    for bad in [
        "",
        "fga_",
        "fgp_0123456789abcdef_",
        &text.replace("fga_", "fgp_"),
        &text.to_uppercase(),
        &text[..text.len() - 1],
    ] {
        assert_eq!(
            AssertionToken::parse(bad).err(),
            Some("malformed"),
            "{bad:?}"
        );
    }
    let err = assertion_invalid("consumed");
    assert_eq!(err.code().id(), "FG-AUTH-0020");
}

#[test]
fn every_bound_field_changes_the_mac() {
    let keys = ring(&[key_line(1, 7, "active")]);
    let secret = [9u8; 32];
    let base = binding("refs/heads/main");
    let (kid, mac) = keys.seal(&secret, &base).expect("seal");
    assert_eq!(kid, 1);
    assert_eq!(keys.verify(kid, &secret, &base, &mac), Ok(()));
    let variants = [
        AssertionBinding {
            public_id: "fedcba9876543210",
            ..base.clone()
        },
        AssertionBinding {
            integration_id: 4,
            ..base.clone()
        },
        AssertionBinding {
            user_id: 8,
            ..base.clone()
        },
        AssertionBinding {
            intent: Intent::Unapprove,
            ..base.clone()
        },
        AssertionBinding {
            project_id: 12,
            ..base.clone()
        },
        AssertionBinding {
            subject: mr(14, HEAD, "refs/heads/main", TIP),
            ..base.clone()
        },
        AssertionBinding {
            subject: mr(
                13,
                "3333333333333333333333333333333333333333",
                "refs/heads/main",
                TIP,
            ),
            ..base.clone()
        },
        binding("refs/heads/release"),
        AssertionBinding {
            subject: mr(
                13,
                HEAD,
                "refs/heads/main",
                "4444444444444444444444444444444444444444",
            ),
            ..base.clone()
        },
        AssertionBinding {
            expires_at_micros: base.expires_at_micros + 1,
            ..base.clone()
        },
    ];
    for v in &variants {
        assert_eq!(
            keys.verify(kid, &secret, v, &mac),
            Err(MacRefusal::MacMismatch),
            "{v:?}"
        );
    }
    // Another secret with the same binding never verifies either.
    assert_eq!(
        keys.verify(kid, &[8u8; 32], &base, &mac),
        Err(MacRefusal::MacMismatch)
    );
    // Length-prefixed: moving bytes between fields changes the input.
    assert_ne!(
        binding_bytes(&AssertionBinding {
            subject: mr(13, "ab", "c", TIP),
            ..base.clone()
        }),
        binding_bytes(&AssertionBinding {
            subject: mr(13, "a", "bc", TIP),
            ..base
        })
    );
}

/// M3 X3: the binding intents seal the kanflow project and the setting; a
/// merge request's bytes are those of S2.
#[test]
fn a_binding_assertion_seals_its_external_project_and_setting() {
    let keys = ring(&[key_line(1, 7, "active")]);
    let secret = [9u8; 32];
    let base = set_binding("P1", Some(false));
    let (kid, mac) = keys.seal(&secret, &base).expect("seal");
    assert_eq!(keys.verify(kid, &secret, &base, &mac), Ok(()));
    for v in [
        set_binding("P2", Some(false)),
        set_binding("P1", Some(true)),
        AssertionBinding {
            intent: Intent::BindingSet,
            ..set_binding("P1", None)
        },
        AssertionBinding {
            project_id: 12,
            ..base.clone()
        },
    ] {
        assert_eq!(
            keys.verify(kid, &secret, &v, &mac),
            Err(MacRefusal::MacMismatch),
            "{v:?}"
        );
    }
    // The S2 layout of a merge request assertion: one more field after
    // the project, never one of the binding shape.
    let bytes = binding_bytes(&binding("refs/heads/main"));
    let mut expected = Vec::new();
    for f in [
        "0123456789abcdef".as_bytes().to_vec(),
        3i64.to_be_bytes().to_vec(),
        7i64.to_be_bytes().to_vec(),
        b"approve".to_vec(),
        11i64.to_be_bytes().to_vec(),
        13i64.to_be_bytes().to_vec(),
        HEAD.as_bytes().to_vec(),
        b"refs/heads/main".to_vec(),
        TIP.as_bytes().to_vec(),
        1_790_000_000_000_000i64.to_be_bytes().to_vec(),
    ] {
        expected.extend_from_slice(&u32::try_from(f.len()).expect("len").to_be_bytes());
        expected.extend_from_slice(&f);
    }
    assert_eq!(bytes, expected);
    assert!(Intent::ALL
        .iter()
        .all(|i| Intent::parse(i.as_str()) == Some(*i)));
    assert!(!Intent::BindingSet.is_merge_request());
}

#[test]
fn a_retired_or_unknown_key_is_key_retired() {
    let old = ring(&[key_line(1, 1, "active")]);
    let secret = [5u8; 32];
    let b = binding("refs/heads/main");
    let (kid, mac) = old.seal(&secret, &b).expect("seal");
    let rotating = ring(&[key_line(1, 1, ""), key_line(2, 2, "active")]);
    assert_eq!(rotating.verify(kid, &secret, &b, &mac), Ok(()));
    let retired = ring(&[key_line(1, 1, "retired"), key_line(2, 2, "active")]);
    assert_eq!(
        retired.verify(kid, &secret, &b, &mac),
        Err(MacRefusal::KeyRetired)
    );
    let gone = ring(&[key_line(2, 2, "active")]);
    assert_eq!(
        gone.verify(kid, &secret, &b, &mac),
        Err(MacRefusal::KeyRetired)
    );
    assert_eq!(
        retired.counts(),
        KeyCounts {
            active: 1,
            valid: 0,
            retired: 1
        }
    );
}

#[test]
fn the_key_file_needs_exactly_one_active_key() {
    let reason = |text: &str| {
        let err = ActAsKeys::parse(text).expect_err("refused");
        assert_eq!(err.code(), ErrorCode::ConfigInvalid, "{text:?}");
        let reason = err
            .context()
            .iter()
            .find(|(k, _)| *k == "reason")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let text = format!("{err:?}");
        assert!(!text.contains("AQEB"), "never quotes a key: {text}");
        reason
    };
    assert_eq!(reason(""), "no_active_key");
    assert_eq!(reason(&key_line(1, 1, "")), "no_active_key");
    assert_eq!(
        reason(&[key_line(1, 1, "active"), key_line(2, 2, "active")].join("\n")),
        "several_active_keys"
    );
    assert_eq!(
        reason(&[key_line(1, 1, "active"), key_line(1, 2, "")].join("\n")),
        "kid_duplicate"
    );
    assert_eq!(reason(&key_line(0, 1, "active")), "kid_invalid");
    assert_eq!(reason(&key_line(1, 1, "primary")), "unknown_marker");
    assert_eq!(reason("1:AQEB active"), "key_length");
    assert_eq!(reason("1:!!! active"), "key_not_base64");
    assert_eq!(reason("nonsense"), "malformed_line");
    let ok = ActAsKeys::parse(&format!("# comment\n\n{}\n", key_line(3, 1, "active")))
        .expect("comments and blank lines are ignored");
    assert_eq!(ok.active_kid().expect("active"), 3);
    assert!(!format!("{ok:?}").contains("AQEB"));
}

#[test]
fn the_claimed_mac_is_keyed_and_stable() {
    let keys = ring(&[key_line(1, 1, ""), key_line(2, 2, "active")]);
    let (kid, a) = keys.claimed("employee:S1", None).expect("mac");
    assert_eq!(kid, 2);
    assert_eq!(a.len(), 64);
    assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(keys.claimed("employee:S1", None).expect("mac").1, a);
    assert_ne!(keys.claimed("employee:S2", None).expect("mac").1, a);
    let (old, b) = keys.claimed("employee:S1", Some(1)).expect("kid 1");
    assert_eq!(old, 1);
    assert_ne!(a, b);
    assert!(keys.claimed("employee:S1", Some(9)).is_err());
    // The claimed purpose differs from the assertion's: the same key never
    // yields the same value for both.
    assert!(!a.contains("S1"));
}
