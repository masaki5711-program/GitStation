//! The agent marketplace's guards against a migrated database (plan 16
//! A3): published parts never change, files are append-only and inserted
//! with their version only, keys retire and revoke once.

use forge_core::RequestId;
use forge_errors::ErrorCode;
use sqlx::Connection as _;

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::test_support::*;
use crate::users::{self, NewUser};

/// A 32-byte public key in base64 (synthetic).
const PUBLIC_KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
/// A 64-byte signature in base64 (synthetic; the database stores, A5 checks).
const SIGNATURE: &str =
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==";
const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const DIGEST: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";

struct World {
    t: FreshDb,
    alice: i64,
}

/// One transaction as forge_app: the audit row of `cause`, then
/// `statements`, then COMMIT — the error instead of a panic.
async fn audited(
    t: &FreshDb,
    cause: (&str, i64),
    statements: &[String],
) -> Result<(), sqlx::Error> {
    let mut tx = t.db.write_pool().begin().await.expect("begin");
    let audit = format!(
        "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, \
         resource_id, request_id) VALUES ('system', 'probe.changed', 'succeeded', '{}', {}, \
         'r-fixture')",
        cause.0, cause.1
    );
    for sql in std::iter::once(&audit).chain(statements) {
        sqlx::raw_sql(sql).execute(&mut *tx).await?;
    }
    tx.commit().await
}

fn sqlstate(e: &sqlx::Error) -> String {
    e.as_database_error()
        .and_then(|d| d.code())
        .map(|c| c.into_owned())
        .unwrap_or_default()
}

fn insert_key(id: i64, kid: &str, public_key: &str) -> String {
    format!(
        "INSERT INTO forge.agent_signing_keys (id, kid, public_key) OVERRIDING SYSTEM VALUE \
         VALUES ({id}, '{kid}', '{public_key}')"
    )
}

fn insert_part(id: i64, slug: &str, user: i64) -> String {
    format!(
        "INSERT INTO forge.agent_parts (id, slug, kind, source_project_id, created_by) \
         OVERRIDING SYSTEM VALUE VALUES ({id}, '{slug}', 'hook', 1, {user})"
    )
}

fn insert_version(id: i64, part: i64, version: &str, kid: &str, user: i64) -> String {
    format!(
        "INSERT INTO forge.agent_part_versions (id, part_id, version, channel, source_sha, tag, \
         manifest, digest, signature, key_id, capabilities, published_by, ci_pipeline_id, state) \
         OVERRIDING SYSTEM VALUE VALUES ({id}, {part}, '{version}', 'beta', '{SHA}', \
         'p/v{version}', '{{}}', '{DIGEST}', '{SIGNATURE}', '{kid}', '{{exec}}', {user}, 7, \
         'published')"
    )
}

fn insert_file(version: i64, path: &str, content_hex: &str) -> String {
    format!(
        "INSERT INTO forge.agent_part_files (version_id, path, mode, sha256, content) \
         SELECT {version}, '{path}', '100644', encode(sha256(c), 'hex'), c \
         FROM (SELECT decode('{content_hex}', 'hex') AS c) AS f"
    )
}

/// A key (id 1, kid 1) and a part (id 1) with version 1.0.0 (id 1) and
/// one file, published by alice.
async fn world() -> World {
    let t = FreshDb::migrated().await;
    let ctx = ChangeContext::new(
        Actor::System,
        RequestId::parse("req-user-alice").expect("id"),
    );
    let alice = users::create(
        &t.db,
        &ctx,
        &NewUser::human("alice", "alice", "alice@example.test"),
    )
    .await
    .expect("alice")
    .user_id
    .get();
    audited(&t, (KEY_RESOURCE, 1), &[insert_key(1, "1", PUBLIC_KEY)])
        .await
        .expect("key");
    audited(
        &t,
        (PART_RESOURCE, 1),
        &[
            insert_part(1, "guard-hook", alice),
            insert_version(1, 1, "1.0.0", "1", alice),
            insert_file(1, ".claude/hooks/guard.js", "6869"),
        ],
    )
    .await
    .expect("published");
    World { t, alice }
}

#[tokio::test]
async fn part_files_update_refused() {
    let w = world().await;
    // forge_app holds no UPDATE or DELETE on the files at all.
    for sql in [
        "UPDATE forge.agent_part_files SET content = 'x'::bytea",
        "DELETE FROM forge.agent_part_files",
    ] {
        let err = audited(&w.t, (PART_RESOURCE, 1), &[sql.to_owned()])
            .await
            .expect_err("forge_app");
        assert_eq!(sqlstate(&err), "42501", "{err}");
    }
    // Even the owner's write is refused by the guard (ENABLE ALWAYS).
    let mut su = w.t.superuser().await;
    for sql in [
        "UPDATE forge.agent_part_files SET content = 'x'::bytea, \
         sha256 = encode(sha256('x'::bytea), 'hex')",
        "DELETE FROM forge.agent_part_files",
    ] {
        let err = sqlx::raw_sql(sql)
            .execute(&mut su)
            .await
            .expect_err("guard");
        assert_eq!(sqlstate(&err), "XF001", "{err}");
        let mapped = crate::query_error("test.agent_part_files", err);
        assert_eq!(mapped.code(), ErrorCode::AppendOnlyViolation);
    }
    let content: Vec<u8> =
        sqlx::query_scalar("SELECT content FROM forge.agent_part_files WHERE version_id = 1")
            .fetch_one(&mut su)
            .await
            .expect("content");
    assert_eq!(content, b"hi");
    su.close().await.expect("close");
    w.t.drop_now().await;
}

#[tokio::test]
async fn files_are_inserted_with_their_version_only() {
    let w = world().await;
    // A later transaction cannot add a file to a published version.
    let err = audited(
        &w.t,
        (PART_RESOURCE, 1),
        &[insert_file(1, ".claude/hooks/extra.js", "00")],
    )
    .await
    .expect_err("later file");
    assert_eq!(sqlstate(&err), "XF001", "{err}");
    // A file whose sha256 is not its content's.
    let forged = format!(
        "INSERT INTO forge.agent_part_files (version_id, path, mode, sha256, content) \
         VALUES (2, '.claude/a.md', '100644', '{}', 'x'::bytea)",
        "0".repeat(64)
    );
    let err = audited(
        &w.t,
        (PART_RESOURCE, 1),
        &[insert_version(2, 1, "1.0.1", "1", w.alice), forged],
    )
    .await
    .expect_err("forged hash");
    assert_eq!(sqlstate(&err), "XF001", "{err}");
    // More than 1 MiB in one version.
    let half = "00".repeat(600 * 1024);
    let err = audited(
        &w.t,
        (PART_RESOURCE, 1),
        &[
            insert_version(2, 1, "1.0.1", "1", w.alice),
            insert_file(2, ".claude/a.md", &half),
            insert_file(2, ".claude/b.md", &half),
        ],
    )
    .await
    .expect_err("too large");
    assert_eq!(sqlstate(&err), "XF001", "{err}");
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_published_version_only_moves_forward() {
    let w = world().await;
    let refused = |sql: &str| sql.to_owned();
    for sql in [
        refused("UPDATE forge.agent_part_versions SET digest = 'sha256:' || repeat('1', 64)"),
        refused(
            "UPDATE forge.agent_part_versions SET key_id = '1', signature = signature, tag = 'x'",
        ),
        refused("UPDATE forge.agent_part_versions SET capabilities = '{}'"),
    ] {
        // Not even granted to forge_app (column privileges).
        let err = audited(&w.t, (PART_RESOURCE, 1), &[sql])
            .await
            .expect_err("content");
        assert_eq!(sqlstate(&err), "42501", "{err}");
    }
    // A version is never deleted.
    let err = audited(
        &w.t,
        (PART_RESOURCE, 1),
        &["DELETE FROM forge.agent_part_versions".to_owned()],
    )
    .await
    .expect_err("delete");
    assert_eq!(sqlstate(&err), "42501", "{err}");
    // A change without its audit row fails at COMMIT (XF002).
    let mut tx = w.t.db.write_pool().begin().await.expect("begin");
    sqlx::raw_sql("UPDATE forge.agent_part_versions SET channel = 'stable'")
        .execute(&mut *tx)
        .await
        .expect("update");
    let err = tx.commit().await.expect_err("unaudited");
    assert_eq!(sqlstate(&err), "XF002", "{err}");
    // beta -> stable, audited.
    audited(
        &w.t,
        (PART_RESOURCE, 1),
        &["UPDATE forge.agent_part_versions SET channel = 'stable'".to_owned()],
    )
    .await
    .expect("promote");
    // Never back to beta.
    let err = audited(
        &w.t,
        (PART_RESOURCE, 1),
        &["UPDATE forge.agent_part_versions SET channel = 'beta'".to_owned()],
    )
    .await
    .expect_err("demote");
    assert_eq!(sqlstate(&err), "XF001", "{err}");
    // Revoked with its reason, by, at; then final.
    let revoke = format!(
        "UPDATE forge.agent_part_versions SET state = 'revoked', revoked_reason = 'suspicious', \
         revoked_by = {}, revoked_at = now()",
        w.alice
    );
    audited(&w.t, (PART_RESOURCE, 1), &[revoke])
        .await
        .expect("revoke");
    let err = audited(
        &w.t,
        (PART_RESOURCE, 1),
        &[
            "UPDATE forge.agent_part_versions SET state = 'published', revoked_reason = NULL, \
           revoked_by = NULL, revoked_at = NULL"
                .to_owned(),
        ],
    )
    .await
    .expect_err("unrevoke");
    assert_eq!(sqlstate(&err), "XF001", "{err}");
    // Inserted as stable: refused (beta first).
    let stable = insert_version(3, 1, "2.0.0", "1", w.alice).replace("'beta'", "'stable'");
    let err = audited(&w.t, (PART_RESOURCE, 1), &[stable])
        .await
        .expect_err("stable first");
    assert_eq!(sqlstate(&err), "XF001", "{err}");
    // The same version again: the unique constraint.
    let err = audited(
        &w.t,
        (PART_RESOURCE, 1),
        &[insert_version(4, 1, "1.0.0", "1", w.alice)],
    )
    .await
    .expect_err("republish");
    assert_eq!(sqlstate(&err), "23505", "{err}");
    w.t.drop_now().await;
}

#[tokio::test]
async fn keys_retire_and_revoke_once_and_sign_only_while_current() {
    let w = world().await;
    // The kid and the public key never change (no privilege; the owner
    // meets the guard).
    let err = audited(
        &w.t,
        (KEY_RESOURCE, 1),
        &["UPDATE forge.agent_signing_keys SET kid = '2'".to_owned()],
    )
    .await
    .expect_err("kid");
    assert_eq!(sqlstate(&err), "42501", "{err}");
    let mut su = w.t.superuser().await;
    for sql in [
        "UPDATE forge.agent_signing_keys SET public_key = \
         'AQECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8='",
        "DELETE FROM forge.agent_signing_keys",
        "DELETE FROM forge.agent_parts",
    ] {
        let err = sqlx::raw_sql(sql)
            .execute(&mut su)
            .await
            .expect_err("guard");
        assert_eq!(sqlstate(&err), "XF001", "{err}");
    }
    su.close().await.expect("close");
    // Retired: once; a retired key signs no new version.
    audited(
        &w.t,
        (KEY_RESOURCE, 1),
        &["UPDATE forge.agent_signing_keys SET retired_at = now()".to_owned()],
    )
    .await
    .expect("retire");
    let err = audited(
        &w.t,
        (KEY_RESOURCE, 1),
        &["UPDATE forge.agent_signing_keys SET retired_at = now() - interval '1 day'".to_owned()],
    )
    .await
    .expect_err("retire twice");
    assert_eq!(sqlstate(&err), "XF001", "{err}");
    let err = audited(
        &w.t,
        (PART_RESOURCE, 1),
        &[insert_version(2, 1, "1.1.0", "1", w.alice)],
    )
    .await
    .expect_err("retired key");
    assert_eq!(sqlstate(&err), "XF001", "{err}");
    // A deprecated part gets no new version (with a current key).
    audited(
        &w.t,
        (KEY_RESOURCE, 2),
        &[insert_key(
            2,
            "2",
            "AQECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=",
        )],
    )
    .await
    .expect("key 2");
    let deprecate = format!(
        "UPDATE forge.agent_parts SET deprecated_at = now(), deprecated_by = {}",
        w.alice
    );
    audited(&w.t, (PART_RESOURCE, 1), &[deprecate])
        .await
        .expect("deprecate");
    let err = audited(
        &w.t,
        (PART_RESOURCE, 1),
        &[insert_version(2, 1, "1.1.0", "2", w.alice)],
    )
    .await
    .expect_err("deprecated part");
    assert_eq!(sqlstate(&err), "XF001", "{err}");
    // The read doctor uses.
    let keys = signing_keys(&w.t.db).await.expect("keys");
    assert_eq!(
        keys.iter()
            .map(|k| (k.kid.as_str(), k.retired, k.revoked))
            .collect::<Vec<_>>(),
        [("1", true, false), ("2", false, false)]
    );
    w.t.drop_now().await;
}
