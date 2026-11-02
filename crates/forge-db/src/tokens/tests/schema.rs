//! The tokens table itself (migration 20261030100000): audit enforcement
//! and privileges. These hold with the migration alone, independent of the
//! tokens module.

use super::*;
use crate::audited_tests::{assert_refused, commit, sql};

fn insert_token(public_id: &str) -> String {
    format!(
        "INSERT INTO forge.tokens (user_id, public_id, secret_sha256, name, scopes, expires_at) \
         VALUES (1, '{public_id}', decode(repeat('ab', 32), 'hex'), 'ci', ARRAY['read_api'], \
         now() + interval '30 days')"
    )
}

async fn seed_user(t: &FreshDb) {
    crate::audited_tests::commit(
        t,
        &[
            audit("user", 1, "succeeded"),
            sql(
                "INSERT INTO forge.users (id, kind, username, display_name, email) \
                 OVERRIDING SYSTEM VALUE VALUES (1, 'human', 'alice', 'A', 'alice@example.test')",
            ),
        ],
    )
    .await
    .expect("seed user");
}

#[tokio::test]
async fn a_token_change_without_its_audit_row_cannot_commit() {
    let t = FreshDb::migrated().await;
    seed_user(&t).await;
    let add = insert_token("00000000000000aa");
    let err = commit(&t, std::slice::from_ref(&sql(&add)))
        .await
        .expect_err("no audit row");
    assert_refused(&err, "tokens");
    // An audit row of another user does not cover it.
    let err = commit(&t, &[audit("user", 2, "succeeded"), sql(&add)])
        .await
        .expect_err("other user");
    assert_refused(&err, "tokens");
    commit(&t, &[audit("user", 1, "succeeded"), sql(&add)])
        .await
        .expect("with its audit row");
    let revoke_sql = "UPDATE forge.tokens SET revoked_at = now(), revoked_reason = 'revoked'";
    let err = commit(&t, &[sql(revoke_sql)]).await.expect_err("revoke");
    assert_refused(&err, "tokens");
    commit(&t, &[sql("UPDATE forge.tokens SET last_used_at = now()")])
        .await
        .expect("last_used_at is unaudited");
    commit(&t, &[audit("user", 1, "succeeded"), sql(revoke_sql)])
        .await
        .expect("revoke with its audit row");
    t.drop_now().await;
}

#[tokio::test]
async fn the_read_pool_cannot_read_token_hashes() {
    let t = FreshDb::migrated().await;
    seed_user(&t).await;
    commit(
        &t,
        &[
            audit("user", 1, "succeeded"),
            sql(&insert_token("00000000000000bb")),
        ],
    )
    .await
    .expect("seed token");
    let ro = t.db.pool().executor();
    for query in [
        "SELECT secret_sha256 FROM forge.tokens",
        "SELECT * FROM forge.tokens",
        "SELECT public_id, secret_sha256 FROM forge.tokens",
    ] {
        let err = sqlx::query(query).fetch_all(ro).await.expect_err(query);
        assert!(
            err.to_string().contains("permission denied"),
            "{query}: {err}"
        );
    }
    let rows = sqlx::query("SELECT id, user_id, public_id, name, scopes FROM forge.tokens")
        .fetch_all(ro)
        .await
        .expect("the other columns are readable");
    assert_eq!(rows.len(), 1);
    t.drop_now().await;
}

#[tokio::test]
async fn the_app_role_cannot_delete_tokens_or_change_their_hash() {
    let t = FreshDb::migrated().await;
    seed_user(&t).await;
    commit(
        &t,
        &[
            audit("user", 1, "succeeded"),
            sql(&insert_token("00000000000000cc")),
        ],
    )
    .await
    .expect("seed token");
    for change in [
        "DELETE FROM forge.tokens",
        "UPDATE forge.tokens SET secret_sha256 = decode(repeat('cd', 32), 'hex')",
        "UPDATE forge.tokens SET scopes = ARRAY['admin']",
        "UPDATE forge.tokens SET expires_at = expires_at + interval '1 day'",
    ] {
        let err = commit(&t, &[audit("user", 1, "succeeded"), sql(change)])
            .await
            .expect_err(change);
        assert!(chain(&err).contains("permission denied"), "{change}: {err}");
    }
    t.drop_now().await;
}
