//! Tests of `webhook`: the URL rules, the target policy (SSRF defences:
//! loopback, link-local, metadata, userinfo, the allowlist, the resolved
//! addresses), the signature, the sealed secret and the event allowlist.

use std::net::IpAddr;

use forge_errors::{ErrorCode, ForgeError};

use crate::event_type::{EventType, VisibilityClass};
use crate::ids::{ProjectId, WebhookId};
use crate::webhook::{
    admitted_event_types, response_head, signature, signature_header, webhook_event_types,
    TargetPolicy, WebhookDeliverySet, WebhookSecret, WebhookSecretKeys, WebhookUrl,
};

fn ctx<'a>(e: &'a ForgeError, key: &str) -> Option<&'a str> {
    e.context()
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.as_str())
}

fn policy(targets: &str, allow_http: bool) -> TargetPolicy {
    TargetPolicy::parse(targets, allow_http).expect("policy")
}

/// The reason a URL is refused for, under `p` (statically and, for a name,
/// once resolved to `resolved`).
fn refused(p: &TargetPolicy, raw: &str, resolved: &[&str]) -> String {
    let url = match WebhookUrl::parse(raw, p.allow_http()) {
        Ok(url) => url,
        Err(e) => {
            assert_eq!(e.code(), ErrorCode::WebhookTargetForbidden, "{raw}");
            return ctx(&e, "reason").unwrap_or("none").to_owned();
        }
    };
    if let Err(e) = p.check_url(&url) {
        assert_eq!(e.code(), ErrorCode::WebhookTargetForbidden, "{raw}");
        return ctx(&e, "reason").unwrap_or("none").to_owned();
    }
    let addrs: Vec<IpAddr> = resolved.iter().map(|a| a.parse().expect("ip")).collect();
    match p.check_resolved(&url, &addrs) {
        Ok(_) => "allowed".to_owned(),
        Err(e) => {
            assert_eq!(e.code(), ErrorCode::WebhookTargetForbidden, "{raw}");
            ctx(&e, "reason").unwrap_or("none").to_owned()
        }
    }
}

#[test]
fn loopback_target_is_int_0010() {
    // Allowlisting a loopback range does not help: it is always refused.
    let p = policy("127.0.0.0/8:8080, [::1/128]:8080, localhost:8080", true);
    for raw in [
        "http://127.0.0.1:8080/hook",
        "http://127.1.2.3:8080/hook",
        "http://[::1]:8080/hook",
        "http://[::ffff:127.0.0.1]:8080/hook",
        "http://localhost:8080/hook",
        "http://api.localhost:8080/hook",
    ] {
        assert_eq!(refused(&p, raw, &[]), "loopback", "{raw}");
    }
    // A name that resolves to loopback at connect time (DNS rebinding).
    let p = policy("kanflow.internal:443", false);
    assert_eq!(
        refused(&p, "https://kanflow.internal/hook", &["127.0.0.1"]),
        "resolved_denied"
    );
}

#[test]
fn link_local_and_metadata_targets_are_refused() {
    let p = policy("169.254.0.0/16:80, [fe80::/10]:80", true);
    for raw in [
        "http://169.254.169.254/latest/meta-data",
        "http://169.254.1.1/",
        "http://[fe80::1]/",
        "http://[::ffff:169.254.169.254]/",
    ] {
        assert_eq!(refused(&p, raw, &[]), "link_local", "{raw}");
    }
}

#[test]
fn other_always_refused_ranges() {
    let p = policy("0.0.0.0/0:80, [::/0]:80", true);
    for (raw, reason) in [
        ("http://0.0.0.0/", "unspecified"),
        ("http://0.1.2.3/", "unspecified"),
        ("http://224.0.0.1/", "multicast"),
        ("http://255.255.255.255/", "broadcast"),
        ("http://240.0.0.1/", "reserved"),
        ("http://[::]/", "unspecified"),
        ("http://[ff02::1]/", "multicast"),
        ("http://[64:ff9b::a00:1]/", "nat64"),
        ("http://[2002:a00:1::1]/", "six_to_four"),
        ("http://[::a00:1]/", "ipv4_compatible"),
        // I2 review: Teredo, local-use NAT64, site-local, CGNAT, benchmarking.
        ("http://[2001:0:4136:e378::1]/", "teredo"),
        ("http://[64:ff9b:1::a00:1]/", "nat64"),
        ("http://[fec0::1]/", "site_local"),
        ("http://100.64.0.1/", "cgnat"),
        ("http://100.127.255.254/", "cgnat"),
        ("http://198.18.0.1/", "benchmarking"),
        ("http://198.19.255.1/", "benchmarking"),
    ] {
        assert_eq!(refused(&p, raw, &[]), reason, "{raw}");
    }
    // Private addresses pass once allowlisted (and only then).
    assert_eq!(refused(&p, "http://10.1.2.3/", &[]), "allowed");
    let none = policy("", true);
    assert_eq!(refused(&none, "http://10.1.2.3/", &[]), "not_allowlisted");
    // Next to the refused ranges, still allowed once listed.
    assert_eq!(refused(&p, "http://100.128.0.1/", &[]), "allowed");
    assert_eq!(refused(&p, "http://198.20.0.1/", &[]), "allowed");
    assert_eq!(refused(&p, "http://[2001:db9::1]/", &[]), "allowed");
}

#[test]
fn userinfo_in_url_is_refused() {
    let p = policy("kanflow.internal:443", false);
    for raw in [
        "https://user:secret@kanflow.internal/hook",
        "https://user@kanflow.internal/hook",
        "https://:secret@kanflow.internal/hook",
    ] {
        assert_eq!(refused(&p, raw, &["10.0.0.5"]), "userinfo", "{raw}");
    }
    // The refusal never carries the URL (it may hold the secret).
    let e = WebhookUrl::parse("https://user:secret@kanflow.internal/hook", false)
        .expect_err("userinfo");
    assert!(!format!("{e:?}").contains("secret"), "{e:?}");
}

#[test]
fn empty_allowlist_refuses_every_target() {
    let p = policy("", false);
    assert!(p.is_empty());
    assert_eq!(
        refused(&p, "https://kanflow.internal/hook", &["10.0.0.5"]),
        "not_allowlisted"
    );
}

#[test]
fn allowlisted_host_and_port_pass_and_nothing_else() {
    let p = policy("kanflow.internal:443, 10.20.0.0/16:8443", false);
    assert_eq!(
        refused(&p, "https://kanflow.internal/hook", &["10.0.0.5"]),
        "allowed"
    );
    assert_eq!(
        refused(&p, "https://KANFLOW.internal/hook", &["10.0.0.5"]),
        "allowed"
    );
    // Port 8443 has a listed range: decided on the resolved address.
    assert_eq!(
        refused(&p, "https://kanflow.internal:8443/hook", &["10.0.0.5"]),
        "resolved_not_allowlisted"
    );
    assert_eq!(
        refused(&p, "https://other.internal/hook", &["10.0.0.5"]),
        "not_allowlisted"
    );
    // A name whose addresses fall in an allowlisted CIDR with its port.
    assert_eq!(
        refused(&p, "https://ci.internal:8443/hook", &["10.20.3.4"]),
        "allowed"
    );
    assert_eq!(
        refused(
            &p,
            "https://ci.internal:8443/hook",
            &["10.20.3.4", "10.99.0.1"]
        ),
        "resolved_not_allowlisted"
    );
    assert_eq!(refused(&p, "https://ci.internal:8443/hook", &[]), "dns");
}

#[test]
fn the_checked_address_is_the_one_returned() {
    let p = policy("kanflow.internal:443", false);
    let url = WebhookUrl::parse("https://kanflow.internal/hook", false).expect("url");
    let addrs: Vec<IpAddr> = vec![
        "10.0.0.5".parse().expect("ip"),
        "10.0.0.6".parse().expect("ip"),
    ];
    let pinned = p.check_resolved(&url, &addrs).expect("allowed");
    assert_eq!(pinned, addrs[0]);
    // Any always-refused address among them refuses the whole name.
    let mixed: Vec<IpAddr> = vec![
        "10.0.0.5".parse().expect("ip"),
        "169.254.169.254".parse().expect("ip"),
    ];
    let e = p.check_resolved(&url, &mixed).expect_err("refused");
    assert_eq!(ctx(&e, "reason"), Some("resolved_denied"));
    assert_eq!(ctx(&e, "address"), Some("169.254.169.254"));
}

#[test]
fn http_needs_allow_http_and_urls_are_shaped() {
    let p = policy("kanflow.internal:80, kanflow.internal:443", false);
    assert_eq!(
        refused(&p, "http://kanflow.internal/hook", &["10.0.0.5"]),
        "scheme"
    );
    for (raw, reason) in [
        ("ftp://kanflow.internal/hook", "scheme"),
        ("kanflow.internal/hook", "invalid_url"),
        ("https://kanflow.internal/hook#frag", "fragment"),
        ("https:///hook", "invalid_url"),
    ] {
        assert_eq!(refused(&p, raw, &["10.0.0.5"]), reason, "{raw}");
    }
    let long = format!("https://kanflow.internal/{}", "a".repeat(2048));
    assert_eq!(refused(&p, &long, &["10.0.0.5"]), "invalid_url");
    let p = policy("kanflow.internal:80", true);
    assert_eq!(
        refused(&p, "http://kanflow.internal/hook", &["10.0.0.5"]),
        "allowed"
    );
}

#[test]
fn the_origin_never_shows_path_query_or_userinfo() {
    let url = WebhookUrl::parse("https://kanflow.internal:8443/a/b?token=x", false).expect("url");
    assert_eq!(url.origin(), "https://kanflow.internal:8443");
    let url = WebhookUrl::parse("https://kanflow.internal/hook?x=1", false).expect("url");
    assert_eq!(url.origin(), "https://kanflow.internal:443");
    assert_eq!(url.as_str(), "https://kanflow.internal/hook?x=1");
}

#[test]
fn allowlist_entries_are_parsed_strictly() {
    for bad in [
        "kanflow.internal",
        "kanflow.internal:0",
        "kanflow.internal:99999",
        "10.0.0.0/33:443",
        "[fd00::/129]:443",
        "user@kanflow.internal:443",
        "*.internal:443",
        "http://kanflow.internal:443",
    ] {
        let e = TargetPolicy::parse(bad, false).expect_err(bad);
        assert_eq!(e.code(), ErrorCode::ConfigInvalid, "{bad}");
        assert_eq!(ctx(&e, "config_key"), Some("webhook.allowed_targets"));
        assert!(!format!("{e:?}").contains("user@"), "{bad}");
    }
    let p = TargetPolicy::parse(
        " kanflow.internal:443 ,10.0.0.0/8:443\n[fd00::/8]:8443 ",
        false,
    )
    .expect("policy");
    assert_eq!(p.rules().len(), 3);
}

#[test]
fn signature_is_hmac_sha256_of_t_delivery_and_body() {
    // Python: hmac.new(b'whsec_test', b'1700000000.d1.{}', sha256).hexdigest()
    assert_eq!(
        signature("whsec_test", 1_700_000_000, "d1", b"{}").expect("sig"),
        "76df35ac8708c6f27c6b13af2390d900137a6c952b938e805affd62c6ec2bb96"
    );
}

#[test]
fn the_signature_header_carries_both_secrets_during_the_overlap() {
    let new = signature("whsec_test", 1_700_000_000, "d1", b"{}").expect("sig");
    let old = signature("whsec_old", 1_700_000_000, "d1", b"{}").expect("sig");
    assert_eq!(
        old,
        "ab46f1a66df8d7c27ace8dfd9df3871d8272149741f0e47a895b933dcecc441d"
    );
    assert_eq!(
        signature_header(1_700_000_000, &[new.clone(), old.clone()]), // clone: compared below
        format!("t=1700000000,v1={new},v1={old}")
    );
    assert_eq!(
        signature_header(1_700_000_000, std::slice::from_ref(&new)),
        format!("t=1700000000,v1={new}")
    );
}

#[test]
fn a_new_secret_is_random_prefixed_and_never_printed() {
    let a = WebhookSecret::generate().expect("secret");
    let b = WebhookSecret::generate().expect("secret");
    assert!(a.expose().starts_with("whsec_"));
    assert!(a.expose().len() >= 40);
    assert_ne!(a.expose(), b.expose());
    assert!(!format!("{a:?}").contains(a.expose()));
}

const KEYS: &str = "# webhook secret keys\n1 0000000000000000000000000000000000000000000000000000000000000001\n2 0000000000000000000000000000000000000000000000000000000000000002\n";

#[test]
fn a_sealed_secret_round_trips_and_is_bound_to_its_webhook() {
    let keys = WebhookSecretKeys::parse(KEYS).expect("keys");
    let uid = uuid::Uuid::new_v4();
    let secret = WebhookSecret::generate().expect("secret");
    let sealed = keys.seal(&uid, &secret).expect("seal");
    assert_eq!(sealed.key_id, 2, "the highest key id seals");
    assert!(!sealed.ciphertext.windows(6).any(|w| w == b"whsec_"));
    let opened = keys
        .open(&uid, sealed.key_id, &sealed.ciphertext)
        .expect("open");
    assert_eq!(opened.expose(), secret.expose());
    // Another webhook's row cannot borrow it.
    let other = uuid::Uuid::new_v4();
    assert!(keys
        .open(&other, sealed.key_id, &sealed.ciphertext)
        .is_err());
    // A key removed from the file is a configuration error, named.
    let only_one = WebhookSecretKeys::parse(
        "1 0000000000000000000000000000000000000000000000000000000000000001\n",
    )
    .expect("keys");
    let e = only_one
        .open(&uid, sealed.key_id, &sealed.ciphertext)
        .expect_err("key gone");
    assert_eq!(e.code(), ErrorCode::ConfigInvalid);
    assert_eq!(ctx(&e, "config_key"), Some("webhook.secret_keys_file"));
    for bad in ["", "0 00", "1 abcd", "x 00\n", "1 00 extra"] {
        assert!(WebhookSecretKeys::parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn webhook_event_types_name_maintainer_types_only_explicitly() {
    let ok = webhook_event_types(&[
        "merge_request.*".to_owned(),
        "member.added".to_owned(),
        "repository.pushed".to_owned(),
        "repository.pushed".to_owned(),
    ])
    .expect("valid");
    assert_eq!(
        ok,
        vec!["member.added", "merge_request.*", "repository.pushed"]
    );
    for bad in [
        "token.created",
        "project.purged",
        "member.*",
        "nope.nope",
        "*.*",
        "",
        "Merge_request.opened",
    ] {
        assert!(webhook_event_types(&[bad.to_owned()]).is_err(), "{bad}");
    }
    assert!(webhook_event_types(&[]).is_err());
    let many: Vec<String> = (0..65).map(|_| "*".to_owned()).collect();
    assert!(webhook_event_types(&many).is_err());
    // `*` admits the integration class only.
    let all = admitted_event_types(
        &["*".to_owned()],
        &[VisibilityClass::Integration, VisibilityClass::Maintainer],
    );
    assert!(all.contains(&EventType::MergeRequestMerged));
    assert!(!all.contains(&EventType::MemberAdded));
    assert!(!all.contains(&EventType::TokenCreated));
    // A named maintainer type is admitted only when the class is allowed.
    let named = ["member.added".to_owned()];
    assert_eq!(
        admitted_event_types(
            &named,
            &[VisibilityClass::Integration, VisibilityClass::Maintainer]
        ),
        vec![EventType::MemberAdded]
    );
    assert!(admitted_event_types(&named, &[VisibilityClass::Integration]).is_empty());
    // The admin class is never admitted, whatever the decisions say.
    assert!(admitted_event_types(&["token.created".to_owned()], &VisibilityClass::ALL).is_empty());
}

#[test]
fn a_delivery_set_admits_what_the_decisions_and_allowlist_admit() {
    let set = WebhookDeliverySet::from_authorized_decisions(
        WebhookId::new(3),
        ProjectId::new(7),
        &[VisibilityClass::Integration],
        &["merge_request.*".to_owned(), "member.added".to_owned()],
    );
    assert_eq!(set.webhook(), WebhookId::new(3));
    assert_eq!(set.project(), ProjectId::new(7));
    assert!(set.admits(EventType::MergeRequestOpened));
    assert!(!set.admits(EventType::RepositoryPushed));
    assert!(!set.admits(EventType::MemberAdded));
    assert!(!set.admits(EventType::WebhookDisabled));
}

#[test]
fn the_response_head_is_bounded_printable_and_redacted() {
    let body = format!(
        "error: token ghp_{} rejected\u{7}\n{}",
        "A".repeat(40),
        "x".repeat(500)
    );
    let head = response_head(body.as_bytes());
    assert!(head.chars().count() <= 128, "{head}");
    assert!(head.contains("[redacted]"), "{head}");
    assert!(!head.contains("AAAAAAAAAAAAAAAAAAAA"), "{head}");
    assert!(head.chars().all(|c| (' '..='~').contains(&c)), "{head}");
    assert_eq!(response_head(b""), "");
    assert_eq!(response_head(&[0xff, 0xfe, b'o', b'k']), "??ok");
}
