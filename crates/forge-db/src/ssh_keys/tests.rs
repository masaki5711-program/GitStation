//! SSH key commands against a migrated database (M4 C13).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_core::ids::UserId;
use forge_core::ssh_key::parse_public_key;
use forge_core::RequestId;
use forge_errors::ErrorCode;
use sqlx::postgres::PgConnection;

use super::*;
use crate::change::Actor;
use crate::test_support::*;
use crate::users::{self, NewUser};

// Synthetic keys (ssh-keygen, private halves discarded).
const ED25519: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAGuL+srU7BTyMdIqbgh4NRqXH1vOCQWfZxL4x1ZB/uN synthetic-ed25519";
const ED25519_FP: &str = "SHA256:5ytgilOc3oe+dzOfNlMNUlML18fMocF7QAyCsvhkxkA";
const P256: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBAzhwPmR4JxQG+Uj4orL/XazsYd1yWhA+0YVMc9DTd+8+9495bPKu9gkcFn6emrPIX9f0pSCwze/yOGSi7XKLKo=";

fn ctx(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, rid(request))
}

fn rid(request: &str) -> RequestId {
    RequestId::parse(request).expect("request id")
}

async fn human(t: &FreshDb, name: &str) -> UserId {
    users::create(
        &t.db,
        &ctx(&format!("req-user-{name}")),
        &NewUser::human(name, "Someone", &format!("{name}@example.test")),
    )
    .await
    .expect("create user")
    .user_id
}

fn new_key(user: UserId, line: &str, title: &str) -> NewSshKey {
    NewSshKey {
        user,
        title: title.to_owned(),
        key: parse_public_key(line).expect("key"),
    }
}

fn reason(err: &forge_errors::ForgeError) -> String {
    err.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

async fn audit_rows(
    su: &mut PgConnection,
    request: &str,
) -> Vec<(String, String, i64, serde_json::Value)> {
    sqlx::query_as(
        "SELECT action, resource_type, resource_id, details FROM forge.audit_log \
         WHERE request_id = $1 ORDER BY id",
    )
    .bind(request)
    .fetch_all(su)
    .await
    .expect("audit rows")
}

#[tokio::test]
async fn add_list_authenticate_and_delete_are_audited_on_the_owner() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice").await;
    let added = add(&t.db, &ctx("req-ssh-add"), &new_key(alice, ED25519, ""))
        .await
        .expect("add");
    assert_eq!(added.fingerprint, ED25519_FP);
    // No title given: the key's comment.
    assert_eq!(added.title, "synthetic-ed25519");
    assert_eq!(added.algorithm, "ssh-ed25519");
    assert!(added.public_key.starts_with("ssh-ed25519 AAAA"));
    assert!(
        !added.public_key.contains("synthetic"),
        "no comment stored in the key"
    );
    let titled = add(
        &t.db,
        &ctx("req-ssh-add-2"),
        &new_key(alice, P256, "  laptop  "),
    )
    .await
    .expect("add p256");
    assert_eq!(titled.title, "laptop");

    let mut su = t.superuser().await;
    let rows = audit_rows(&mut su, "req-ssh-add").await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "ssh_key.added");
    assert_eq!(rows[0].1, "user");
    assert_eq!(rows[0].2, alice.get());
    assert_eq!(rows[0].3["fingerprint"], ED25519_FP);
    assert_eq!(rows[0].3["ssh_key_id"], added.key_id);
    assert!(rows[0].3.get("title").is_none(), "never the title");

    let listed = list(&t.db, alice).await.expect("list");
    assert_eq!(
        listed.iter().map(|k| k.key_id).collect::<Vec<_>>(),
        vec![titled.key_id, added.key_id],
        "newest first"
    );
    assert_eq!(
        owner(&t.db, added.key_id).await.expect("owner"),
        Some(alice)
    );
    assert_eq!(owner(&t.db, added.key_id + 100).await.expect("owner"), None);

    let who = authenticate(&t.db, &rid("req-ssh-auth"), ED25519_FP, true)
        .await
        .expect("authenticate");
    assert_eq!(who.key_id, added.key_id);
    assert_eq!(who.user_id, alice);
    assert_eq!(who.username, "alice");
    let used: Option<String> =
        sqlx::query_scalar("SELECT last_used_at::text FROM forge.user_ssh_keys WHERE id = $1")
            .bind(added.key_id)
            .fetch_one(&mut su)
            .await
            .expect("last used");
    assert!(used.is_some(), "the use was recorded");
    // An offer (no proof yet) does not record a use.
    let p256_fp = parse_public_key(P256).unwrap().fingerprint();
    authenticate(&t.db, &rid("req-ssh-offer"), &p256_fp, false)
        .await
        .expect("offer");
    let unused: Option<String> =
        sqlx::query_scalar("SELECT last_used_at::text FROM forge.user_ssh_keys WHERE id = $1")
            .bind(titled.key_id)
            .fetch_one(&mut su)
            .await
            .expect("last used");
    assert!(unused.is_none());

    assert_eq!(
        delete(&t.db, &ctx("req-ssh-del"), added.key_id)
            .await
            .expect("delete"),
        DeleteOutcome::Deleted
    );
    let rows = audit_rows(&mut su, "req-ssh-del").await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "ssh_key.deleted");
    assert_eq!(rows[0].2, alice.get());
    assert_eq!(
        delete(&t.db, &ctx("req-ssh-del-2"), added.key_id)
            .await
            .expect("delete again"),
        DeleteOutcome::Absent
    );
    assert!(audit_rows(&mut su, "req-ssh-del-2").await.is_empty());
    let err = authenticate(&t.db, &rid("req-ssh-auth-2"), ED25519_FP, true)
        .await
        .expect_err("deleted key");
    assert_eq!(err.code(), ErrorCode::Unauthenticated);
    assert_eq!(reason(&err), "ssh_key_unknown");
    t.drop_now().await;
}

#[tokio::test]
async fn a_key_belongs_to_one_person_and_the_refusals_write_nothing() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice").await;
    let bob = human(&t, "bob").await;
    add(&t.db, &ctx("req-dup-1"), &new_key(alice, ED25519, "a"))
        .await
        .expect("add");
    for (who, request) in [(alice, "req-dup-2"), (bob, "req-dup-3")] {
        let err = add(&t.db, &ctx(request), &new_key(who, ED25519, "again"))
            .await
            .expect_err("duplicate");
        assert_eq!(err.code(), ErrorCode::SshKeyInvalid);
        assert_eq!(reason(&err), "duplicate");
    }
    let err = add(
        &t.db,
        &ctx("req-title"),
        &new_key(bob, P256, &"x".repeat(101)),
    )
    .await
    .expect_err("long title");
    assert_eq!(reason(&err), "title_invalid");
    let err = add(
        &t.db,
        &ctx("req-title-2"),
        &new_key(bob, P256, "a\u{202E}b"),
    )
    .await
    .expect_err("invisible");
    assert_eq!(reason(&err), "title_invalid");
    let mut su = t.superuser().await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM forge.user_ssh_keys")
        .fetch_one(&mut su)
        .await
        .expect("count");
    assert_eq!(count, 1);
    for request in ["req-dup-2", "req-dup-3", "req-title", "req-title-2"] {
        assert!(audit_rows(&mut su, request).await.is_empty(), "{request}");
    }
    t.drop_now().await;
}

#[tokio::test]
async fn a_disabled_owner_cannot_add_or_use_a_key() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice").await;
    let _admin = users::create(
        &t.db,
        &ctx("req-user-root"),
        &NewUser {
            is_admin: true,
            ..NewUser::human("root", "Root", "root@example.test")
        },
    )
    .await
    .expect("admin");
    let added = add(&t.db, &ctx("req-dis-1"), &new_key(alice, ED25519, "a"))
        .await
        .expect("add");
    let disabled = users::disable(&t.db, &ctx("req-dis-2"), alice)
        .await
        .expect("disable");
    assert!(
        matches!(disabled, users::DisableOutcome::Disabled),
        "{disabled:?}"
    );
    let err = authenticate(&t.db, &rid("req-dis-3"), ED25519_FP, true)
        .await
        .expect_err("disabled owner");
    assert_eq!(err.code(), ErrorCode::AccountDisabled);
    assert!(err
        .context()
        .iter()
        .any(|(k, v)| *k == "ssh_key_id" && *v == added.key_id.to_string()));
    let err = add(&t.db, &ctx("req-dis-4"), &new_key(alice, P256, "b"))
        .await
        .expect_err("disabled owner adds");
    assert_eq!(reason(&err), "owner_disabled");
    // Not a fingerprint at all: the same answer as an unknown key.
    let err = authenticate(&t.db, &rid("req-dis-5"), "MD5:aa", true)
        .await
        .expect_err("not a fingerprint");
    assert_eq!(reason(&err), "ssh_key_unknown");
    t.drop_now().await;
}

#[tokio::test]
async fn the_per_user_limit_holds() {
    let t = FreshDb::migrated().await;
    let alice = human(&t, "alice").await;
    // Fill up to the limit with synthetic rows (their audit row in the
    // same transaction), then one more through `add`.
    let mut su = t.superuser().await;
    let batch = format!(
        "{}; INSERT INTO forge.user_ssh_keys (user_id, title, algorithm, public_key, fingerprint_sha256) \
         SELECT {uid}, 'k' || g, 'ssh-ed25519', 'ssh-ed25519 AAAA' || g, \
         'SHA256:' || rpad(lpad(g::text, 4, '0'), 43, 'A') FROM generate_series(1, {n}) g",
        crate::audited_tests::audit("user", alice.get(), "succeeded"),
        uid = alice.get(),
        n = MAX_KEYS_PER_USER,
    );
    sqlx::raw_sql(&batch).execute(&mut su).await.expect("fill");
    let err = add(
        &t.db,
        &ctx("req-limit"),
        &new_key(alice, ED25519, "one more"),
    )
    .await
    .expect_err("limit");
    assert_eq!(reason(&err), "limit");
    t.drop_now().await;
}
