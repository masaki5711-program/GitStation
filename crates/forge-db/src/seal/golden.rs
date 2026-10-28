//! The golden vector of hash version 1 (ADR-0018 §4). The expected values
//! were computed independently of this crate by
//! `tests/fixtures/audit_seal_v1_golden.py` (Python's json / hashlib /
//! hmac) and are committed here: a change to v1's columns, their reading or
//! their serialization, or to the seal layout, fails this test.

use forge_core::config::SealKeys;
use forge_errors::ErrorCode;

use super::chain::{self, SealFields};
use super::v1::{self, Row};
use super::*;
use crate::test_support::*;

const ROW_HASHES: [&str; 3] = [
    "ffdc239bc22901264327d72531b72e620002f558eb96b28021a85d3a94c78fbe",
    "12f28f808d6617185f0a1c14a0a362d1eaa27e2df1e4f1a08ce5b7b27e6b11a6",
    "460370d730a7781f6b6337477ec6e666386ebb7d8501063f65b9186ba1946497",
];
const ROWS_SHA256: &str = "b91a3c539539130485addc12fc688e3792a28ce3937bb3c0e65b389ca06a98fc";
const GENESIS: &str = "ae80e7261da7b20e1b8233263db92262ca20faaa878236f899f103a3b9cf80e1";
const SEAL_SHA256: &str = "637193f5c928694172f5862d6dda5b89647b231b67edef3760b20f738146d5bf";
const HMAC: &str = "a8e7a42f9e3a3a26841fb3ec804e8873b6a44bb51a49f3df6fda9ab44f79e46c";
/// The script's key: 32 bytes of 0x42, key_id 1.
pub(crate) fn golden_keys() -> SealKeys {
    SealKeys::parse(&format!("1 {}", "42".repeat(32))).expect("golden key")
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn row(seq: i64) -> Row {
    let base = Row {
        seq,
        occurred_at: String::new(),
        actor_kind: String::new(),
        actor_user_id: None,
        actor_via_id: None,
        credential_id: None,
        act_as_claimed: None,
        act_as_key_id: None,
        via_assertion_id: None,
        effective_scope: None,
        action: String::new(),
        outcome: "succeeded".to_owned(),
        resource_type: "repository".to_owned(),
        resource_id: None,
        project_id: None,
        request_id: String::new(),
        error_code: None,
        occurrences: 1,
        details: "{}".to_owned(),
    };
    let s = |v: &str| v.to_owned();
    match seq {
        1 => Row {
            occurred_at: s("2026-10-27T01:02:03.456789Z"),
            actor_kind: s("user"),
            actor_user_id: Some(42),
            effective_scope: Some(vec![Some(s("repo:read")), Some(s("repo:write"))]),
            action: s("repository.created"),
            resource_id: Some(7),
            project_id: Some(3),
            request_id: s("req-golden-1"),
            // As PostgreSQL prints the jsonb.
            details: s(r#"{"name": "設計書", "size": 12}"#),
            ..base
        },
        2 => Row {
            occurred_at: s("2026-10-27T01:02:04.000001Z"),
            actor_kind: s("user_via_service_account"),
            actor_user_id: Some(43),
            actor_via_id: Some(5),
            credential_id: Some(900),
            act_as_claimed: Some(s("alice")),
            act_as_key_id: Some(2),
            via_assertion_id: Some(s("va-1")),
            effective_scope: Some(vec![Some(s("api")), None]),
            action: s("repository.pushed"),
            outcome: s("denied"),
            resource_id: Some(7),
            project_id: Some(3),
            request_id: s("req-golden-2"),
            error_code: Some(s("FG-AUTH-0002")),
            occurrences: 3,
            details: s(
                r#"{"z": [1, null, true], "$n": "lit", "big": 9007199254740993, "neg": -9007199254740991, "ratio": 0.5}"#,
            ),
            ..base
        },
        _ => Row {
            occurred_at: s("2026-10-27T23:59:59.000000Z"),
            actor_kind: s("system"),
            action: s("system.started"),
            resource_type: s("instance"),
            request_id: s("boot-1"),
            ..base
        },
    }
}

fn golden_fields(rows_sha256: chain::Digest32) -> SealFields {
    SealFields {
        hash_version: v1::VERSION,
        seal_epoch: 1,
        key_id: 1,
        kind: chain::SealKind::Normal,
        first_seq: 1,
        last_seq: 3,
        events_high_seq: 10,
        rows_sha256,
        prev_seal_sha256: chain::genesis(),
    }
}

#[test]
fn audit_seal_v1_golden_vector() {
    let rows: Vec<Row> = (1..=3).map(row).collect();
    let hashes: Vec<chain::Digest32> = rows.iter().map(|r| r.sha256().expect("hash")).collect();
    let shown: Vec<String> = hashes.iter().map(|h| hex(h)).collect();
    assert_eq!(shown, ROW_HASHES);
    let rows_sha256 = chain::rows_sha256(&hashes);
    assert_eq!(hex(&rows_sha256), ROWS_SHA256);
    assert_eq!(hex(&chain::genesis()), GENESIS);
    let seal = golden_fields(rows_sha256).seal_sha256();
    assert_eq!(hex(&seal), SEAL_SHA256);
    let keys = golden_keys();
    let mac = chain::hmac(keys.signing(), &seal).expect("hmac");
    assert_eq!(hex(&mac), HMAC);
    assert!(chain::hmac_matches(keys.signing(), &seal, &mac).expect("verify"));
    assert!(!chain::hmac_matches(keys.signing(), &seal, &[0; 32]).expect("verify"));
}

#[test]
fn the_v1_columns_are_every_audit_column_but_the_personal_ones() {
    let canonical = String::from_utf8(row(1).canonical().expect("json")).expect("utf-8");
    let value: serde_json::Value = serde_json::from_str(&canonical).expect("json");
    let keys: Vec<&str> = value
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    let mut expected = v1::COLUMNS.to_vec();
    expected.sort_unstable();
    assert_eq!(keys, expected);
    for personal in v1::UNSEALED {
        assert!(!canonical.contains(personal), "{canonical}");
    }
}

#[test]
fn audit_details_numbers_are_jcs_safe() {
    let details = |text: &str| {
        let r = Row {
            details: text.to_owned(),
            ..row(3)
        };
        let json: serde_json::Value =
            serde_json::from_slice(&r.canonical().expect("canonical")).expect("json");
        json["details"].to_string()
    };
    // Safe integers stay numbers; the rest keep PostgreSQL's exact text.
    assert_eq!(
        details(r#"{"a": 9007199254740991, "b": -9007199254740991, "c": 0, "d": [1, -2]}"#),
        r#"{"a":9007199254740991,"b":-9007199254740991,"c":0,"d":[1,-2]}"#
    );
    assert_eq!(
        details(
            r#"{"big": 9007199254740992, "neg": -9007199254740992, "f": 1.0, "e": 1.5e+300, "z": -0}"#
        ),
        r#"{"big":{"$n":"9007199254740992"},"e":{"$n":"1.5e+300"},"f":{"$n":"1.0"},"neg":{"$n":"-9007199254740992"},"z":{"$n":"-0"}}"#
    );
    // Beyond a double's range is still expressible (kept as text).
    assert_eq!(details(r#"{"x": 1e400}"#), r#"{"x":{"$n":"1e400"}}"#);
    // Keys starting with `$` (raw or escaped) get one more `$`; values and
    // other keys stay; nested containers too.
    assert_eq!(
        details(r#"{"$n": "1.5", "$$a": 1, "$b": 2, "a$": 3, "k": "$n"}"#),
        r#"{"$$$a":1,"$$b":2,"$$n":"1.5","a$":3,"k":"$n"}"#
    );
    assert_eq!(
        details(r#"{"a": [{"$x": [1.5, {"y": -0.0}]}]}"#),
        r#"{"a":[{"$$x":[{"$n":"1.5"},{"y":{"$n":"-0.0"}}]}]}"#
    );
    // Digits a double would round away still change the hash.
    let long = |last: char| {
        Row {
            details: format!(r#"{{"x": 1234567890123456789012345678{last}}}"#),
            ..row(3)
        }
        .sha256()
        .expect("hash")
    };
    assert_ne!(long('8'), long('9'));
    let precise = |text: &str| {
        Row {
            details: text.to_owned(),
            ..row(3)
        }
        .sha256()
        .expect("hash")
    };
    assert_ne!(
        precise(r#"{"x": 0.10000000000000000001}"#),
        precise(r#"{"x": 0.10000000000000000002}"#)
    );
    // Numbers inside strings, escapes and keys are untouched.
    assert_eq!(
        details(r#"{"s": "12.5 \"9007199254740993\"", "9": 1}"#),
        r#"{"9":1,"s":"12.5 \"9007199254740993\""}"#
    );
    // Key order and whitespace of the stored text do not matter.
    assert_eq!(precise(r#"{"a": 1, "b": 2}"#), precise(r#"{"b":2,"a":1}"#));
    // Injective: a number never hashes like the equal string, nor like an
    // object that spells its marker; a safe integer never like its string.
    let number = precise(r#"{"x": 1.5}"#);
    assert_ne!(number, precise(r#"{"x": "1.5"}"#));
    assert_ne!(number, precise(r#"{"x": {"$n": "1.5"}}"#));
    assert_ne!(number, precise(r#"{"x": {"$$n": "1.5"}}"#));
    assert_ne!(precise(r#"{"x": 7}"#), precise(r#"{"x": "7"}"#));
    assert_ne!(
        precise(r#"{"$a": 1}"#),
        precise(r#"{"$$a": 1}"#),
        "escaped keys stay distinct"
    );
}

#[test]
fn details_v1_cannot_read_is_internal_with_its_seq() {
    let deep = format!("{}1{}", "[".repeat(200), "]".repeat(200));
    let err = Row {
        details: format!(r#"{{"x": {deep}}}"#),
        ..row(3)
    }
    .sha256()
    .expect_err("too deep");
    assert_eq!(err.code(), ErrorCode::Internal);
    let text = err.to_string();
    for part in ["operation=seal.v1", "reason=details_unreadable", "seq=3"] {
        assert!(text.contains(part), "{text}");
    }
}

/// The same rows stored in the database: v1's SELECT reads them back into
/// the golden hashes, and seal_pending writes the golden seal.
#[tokio::test]
async fn the_database_reads_and_seals_the_golden_rows_into_the_golden_values() {
    let t = FreshDb::migrated().await;
    t.exec_superuser(
        "INSERT INTO forge.audit_log (actor_kind, actor_user_id, effective_scope, actor_label, \
             action, outcome, resource_type, resource_id, project_id, source_ip, user_agent, \
             request_id, details) \
         VALUES ('user', 42, ARRAY['repo:read', 'repo:write'], 'Alice', 'repository.created', \
             'succeeded', 'repository', 7, 3, '10.0.0.1', 'git/2.47', 'req-golden-1', \
             '{\"name\": \"設計書\", \"size\": 12}'); \
         INSERT INTO forge.audit_log (actor_kind, actor_user_id, actor_via_id, credential_id, \
             act_as_claimed, act_as_key_id, via_assertion_id, effective_scope, action, outcome, \
             resource_type, resource_id, project_id, request_id, error_code, occurrences, details) \
         VALUES ('user_via_service_account', 43, 5, 900, 'alice', 2, 'va-1', ARRAY['api', NULL], \
             'repository.pushed', 'denied', 'repository', 7, 3, 'req-golden-2', 'FG-AUTH-0002', 3, \
             '{\"$n\": \"lit\", \"big\": 9007199254740993, \"neg\": -9007199254740991, \"ratio\": 0.5, \
               \"z\": [1, null, true]}'); \
         INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, request_id) \
         VALUES ('system', 'system.started', 'succeeded', 'instance', 'boot-1'); \
         ALTER TABLE forge.audit_log DISABLE TRIGGER audit_log_append_only; \
         UPDATE forge.audit_log SET seq = 1, sequenced_at = now(), \
             occurred_at = '2026-10-27 10:02:03.456789+09' WHERE request_id = 'req-golden-1'; \
         UPDATE forge.audit_log SET seq = 2, sequenced_at = now(), \
             occurred_at = '2026-10-27 01:02:04.000001+00' WHERE request_id = 'req-golden-2'; \
         UPDATE forge.audit_log SET seq = 3, sequenced_at = now(), \
             occurred_at = '2026-10-27 23:59:59+00' \
             WHERE id = (SELECT max(id) FROM forge.audit_log); \
         ALTER TABLE forge.audit_log ENABLE ALWAYS TRIGGER audit_log_append_only; \
         INSERT INTO forge.event_feed_state (epoch, system_identifier, timeline_id, high_seq, \
             audit_high_seq, sequencer_heartbeat) VALUES (gen_random_uuid(), 1, 1, 10, 3, now());",
    )
    .await;
    let rows: Vec<Row> = sqlx::query(v1::SELECT)
        .bind(1_i64)
        .bind(3_i64)
        .fetch_all(t.db.write_pool())
        .await
        .expect("rows")
        .iter()
        .map(|r| Row::from_pg(r).expect("row"))
        .collect();
    let shown: Vec<String> = rows
        .iter()
        .map(|r| hex(&r.sha256().expect("hash")))
        .collect();
    assert_eq!(shown, ROW_HASHES);

    let outcome = seal_pending(&t.db, &golden_keys()).await.expect("seal");
    let SealOutcome::Sealed(sealed) = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!((sealed.range.first(), sealed.range.last()), (1, 3));
    assert!(!sealed.more);
    let (seal, mac, kind): (Vec<u8>, Vec<u8>, String) =
        sqlx::query_as("SELECT seal_sha256, hmac, kind FROM forge.audit_seal WHERE id = $1")
            .bind(sealed.seal_id)
            .fetch_one(t.db.pool().executor())
            .await
            .expect("seal row");
    assert_eq!(
        (hex(&seal), hex(&mac), kind.as_str()),
        (SEAL_SHA256.to_owned(), HMAC.to_owned(), "normal")
    );
    let verified = verify_range(
        &t.db,
        Some(&golden_keys()),
        VerifyScope::SealedWithin(DOCTOR_WINDOW),
    )
    .await
    .expect("verifies");
    assert_eq!((verified.seals, verified.rows), (1, 3));
    // A key file without key_id 1 cannot vouch for the seal.
    let other = SealKeys::parse(&format!("2 {}", "42".repeat(32))).expect("keys");
    let err = verify_range(
        &t.db,
        Some(&other),
        VerifyScope::SealedWithin(DOCTOR_WINDOW),
    )
    .await
    .expect_err("unknown key");
    assert_reason(&err, ErrorCode::AuditSealMismatch, "hmac");
    assert!(err.to_string().contains("detail=unknown_key_id"), "{err}");
    t.drop_now().await;
}
