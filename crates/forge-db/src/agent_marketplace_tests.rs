//! The agent marketplace's guards against a migrated database (plan 16
//! A3): published parts never change, files are append-only and inserted
//! with their version only, keys are the operator's (forge_maint through
//! `forge.agent_signing_key_manage`) and retire and revoke once.

use forge_core::RequestId;
use forge_errors::ErrorCode;
use sqlx::Connection as _;

use super::*;
use crate::change::{Actor, ChangeContext};
use crate::test_support::*;
use crate::users::{self, NewUser};

/// 32-byte public keys in base64 (synthetic).
const PUBLIC_KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
const PUBLIC_KEY_2: &str = "AQECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
/// A 64-byte signature in base64 (synthetic; the database stores, A5 checks).
const SIGNATURE: &str =
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==";
const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const DIGEST: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
const PART: &str = "(SELECT id FROM forge.agent_parts WHERE slug = 'guard-hook')";

struct World {
    t: FreshDb,
    alice: i64,
}

/// One transaction as forge_app: `statements`, then the audit row of
/// `resource` (type, SQL expression of its id), then COMMIT — the error
/// instead of a panic.
async fn audited(
    t: &FreshDb,
    resource: (&str, &str),
    statements: &[String],
) -> Result<(), sqlx::Error> {
    let mut tx = t.db.write_pool().begin().await.expect("begin");
    let audit = format!(
        "INSERT INTO forge.audit_log (actor_kind, action, outcome, resource_type, \
         resource_id, request_id) VALUES ('system', 'probe.changed', 'succeeded', '{}', {}, \
         'r-fixture')",
        resource.0, resource.1
    );
    for sql in statements.iter().chain(std::iter::once(&audit)) {
        sqlx::raw_sql(sql).execute(&mut *tx).await?;
    }
    tx.commit().await
}

/// `forge.agent_signing_key_manage(...)` as `role` (a superuser session
/// with `SET LOCAL ROLE`).
async fn manage_as(t: &FreshDb, role: &str, call: &str) -> Result<(), sqlx::Error> {
    let mut su = t.superuser().await;
    let mut tx = su.begin().await.expect("begin");
    sqlx::raw_sql(&format!("SET LOCAL ROLE {role}"))
        .execute(&mut *tx)
        .await
        .expect("role");
    let result = sqlx::raw_sql(&format!("SELECT forge.agent_signing_key_manage({call})"))
        .execute(&mut *tx)
        .await;
    match result {
        Ok(_) => tx.commit().await,
        Err(e) => Err(e),
    }
}

fn sqlstate(e: &sqlx::Error) -> String {
    e.as_database_error()
        .and_then(|d| d.code())
        .map(|c| c.into_owned())
        .unwrap_or_default()
}

fn insert_part(slug: &str, user: i64) -> String {
    format!(
        "INSERT INTO forge.agent_parts (slug, kind, source_project_id, created_by) \
         VALUES ('{slug}', 'hook', 1, {user})"
    )
}

fn insert_version(version: &str, kid: &str, user: i64) -> String {
    format!(
        "INSERT INTO forge.agent_part_versions (part_id, version, channel, source_sha, tag, \
         manifest, digest, signature, key_id, capabilities, published_by, ci_pipeline_id, state) \
         VALUES ({PART}, '{version}', 'beta', '{SHA}', 'p/v{version}', '{{}}', '{DIGEST}', \
         '{SIGNATURE}', '{kid}', '{{exec}}', {user}, 7, 'published')"
    )
}

fn insert_file(version: &str, path: &str, content_hex: &str) -> String {
    format!(
        "INSERT INTO forge.agent_part_files (version_id, path, mode, sha256, content) \
         SELECT v.id, '{path}', '100644', encode(sha256(c), 'hex'), c \
         FROM forge.agent_part_versions AS v, (SELECT decode('{content_hex}', 'hex') AS c) AS f \
         WHERE v.version = '{version}'"
    )
}

/// A key (kid 1, registered by the operator) and a part `guard-hook` with
/// version 1.0.0 and one file, published by alice.
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
    manage_as(
        &t,
        "forge_maint",
        &format!("'register', '1', '{PUBLIC_KEY}', NULL, 'req-key-1'"),
    )
    .await
    .expect("key");
    audited(
        &t,
        (PART_RESOURCE, PART),
        &[
            insert_part("guard-hook", alice),
            insert_version("1.0.0", "1", alice),
            insert_file("1.0.0", ".claude/hooks/guard.js", "6869"),
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
        let err = audited(&w.t, (PART_RESOURCE, PART), &[sql.to_owned()])
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
    let content: Vec<u8> = sqlx::query_scalar("SELECT content FROM forge.agent_part_files")
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
        (PART_RESOURCE, PART),
        &[insert_file("1.0.0", ".claude/hooks/extra.js", "00")],
    )
    .await
    .expect_err("later file");
    assert_eq!(sqlstate(&err), "XF001", "{err}");
    // A file whose sha256 is not its content's.
    let forged = format!(
        "INSERT INTO forge.agent_part_files (version_id, path, mode, sha256, content) \
         SELECT v.id, '.claude/a.md', '100644', '{}', 'x'::bytea \
         FROM forge.agent_part_versions AS v WHERE v.version = '1.0.1'",
        "0".repeat(64)
    );
    let err = audited(
        &w.t,
        (PART_RESOURCE, PART),
        &[insert_version("1.0.1", "1", w.alice), forged],
    )
    .await
    .expect_err("forged hash");
    assert_eq!(sqlstate(&err), "XF001", "{err}");
    // More than 1 MiB in one version.
    let half = "00".repeat(600 * 1024);
    let err = audited(
        &w.t,
        (PART_RESOURCE, PART),
        &[
            insert_version("1.0.1", "1", w.alice),
            insert_file("1.0.1", ".claude/a.md", &half),
            insert_file("1.0.1", ".claude/b.md", &half),
        ],
    )
    .await
    .expect_err("too large");
    assert_eq!(sqlstate(&err), "XF001", "{err}");
    w.t.drop_now().await;
}

/// Review A3: a file's path is relative and under `.claude/`, or
/// `CLAUDE.md` / `.mcp.json`; a manifest is at most 1 MiB.
#[tokio::test]
async fn file_paths_and_manifests_are_bounded() {
    let w = world().await;
    for path in [
        "/etc/passwd",
        ".claude/../.git/config",
        ".claude/./a.md",
        ".claude//a.md",
        ".claude/",
        ".claude\\\\hooks\\\\a.js",
        "C:/x.md",
        ".claude/c:a.md",
        "src/main.rs",
        ".git/hooks/pre-commit",
        "claude.md",
    ] {
        let err = audited(
            &w.t,
            (PART_RESOURCE, PART),
            &[
                insert_version("1.0.1", "1", w.alice),
                insert_file("1.0.1", path, "00"),
            ],
        )
        .await
        .expect_err(path);
        assert_eq!(sqlstate(&err), "23514", "{path}: {err}");
    }
    // The three allowed shapes, in one new version.
    audited(
        &w.t,
        (PART_RESOURCE, PART),
        &[
            insert_version("1.0.1", "1", w.alice),
            insert_file("1.0.1", ".claude/hooks/a.js", "00"),
            insert_file("1.0.1", "CLAUDE.md", "00"),
            insert_file("1.0.1", ".mcp.json", "00"),
        ],
    )
    .await
    .expect("allowed paths");
    let big = format!("{{\"x\": \"{}\"}}", "a".repeat(1024 * 1024));
    let err = audited(
        &w.t,
        (PART_RESOURCE, PART),
        &[insert_version("2.0.0", "1", w.alice).replace("'{}'", &format!("'{big}'"))],
    )
    .await
    .expect_err("manifest too large");
    assert_eq!(sqlstate(&err), "23514", "{err}");
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_published_version_only_moves_forward() {
    let w = world().await;
    for sql in [
        "UPDATE forge.agent_part_versions SET digest = 'sha256:' || repeat('1', 64)",
        "UPDATE forge.agent_part_versions SET key_id = '1', signature = signature, tag = 'x'",
        "UPDATE forge.agent_part_versions SET capabilities = '{}'",
        // Not even the identity columns on INSERT (txid is the guard's).
        "INSERT INTO forge.agent_part_versions (id) OVERRIDING SYSTEM VALUE VALUES (99)",
    ] {
        // Not granted to forge_app (column privileges).
        let err = audited(&w.t, (PART_RESOURCE, PART), &[sql.to_owned()])
            .await
            .expect_err("content");
        assert_eq!(sqlstate(&err), "42501", "{sql}: {err}");
    }
    // A version is never deleted.
    let err = audited(
        &w.t,
        (PART_RESOURCE, PART),
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
        (PART_RESOURCE, PART),
        &["UPDATE forge.agent_part_versions SET channel = 'stable'".to_owned()],
    )
    .await
    .expect("promote");
    // Never back to beta.
    let err = audited(
        &w.t,
        (PART_RESOURCE, PART),
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
    audited(&w.t, (PART_RESOURCE, PART), &[revoke])
        .await
        .expect("revoke");
    let err = audited(
        &w.t,
        (PART_RESOURCE, PART),
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
    let stable = insert_version("2.0.0", "1", w.alice).replace("'beta'", "'stable'");
    let err = audited(&w.t, (PART_RESOURCE, PART), &[stable])
        .await
        .expect_err("stable first");
    assert_eq!(sqlstate(&err), "XF001", "{err}");
    // The same version again: the unique constraint.
    let err = audited(
        &w.t,
        (PART_RESOURCE, PART),
        &[insert_version("1.0.0", "1", w.alice)],
    )
    .await
    .expect_err("republish");
    assert_eq!(sqlstate(&err), "23505", "{err}");
    w.t.drop_now().await;
}

/// Review A3 (HIGH): the keys are the operator's — forge_app reads them
/// only and cannot run the management function; forge_maint registers,
/// retires and revokes through it, each with its audit row.
#[tokio::test]
async fn only_the_operator_manages_signing_keys() {
    let w = world().await;
    for sql in [
        format!(
            "INSERT INTO forge.agent_signing_keys (kid, public_key) VALUES ('2', '{PUBLIC_KEY_2}')"
        ),
        "UPDATE forge.agent_signing_keys SET retired_at = now()".to_owned(),
        "UPDATE forge.agent_signing_keys SET revoked_at = now(), revoked_reason = 'x'".to_owned(),
        "SELECT forge.agent_signing_key_manage('retire', '1', NULL, NULL, 'req-x')".to_owned(),
    ] {
        let err = audited(&w.t, (KEY_RESOURCE, "1"), std::slice::from_ref(&sql))
            .await
            .expect_err("forge_app");
        assert_eq!(sqlstate(&err), "42501", "{sql}: {err}");
    }
    for role in ["forge_app", "forge_ro", "forge_purge"] {
        let err = manage_as(&w.t, role, "'retire', '1', NULL, NULL, 'req-x'")
            .await
            .expect_err(role);
        assert_eq!(sqlstate(&err), "42501", "{role}: {err}");
    }
    // Bad arguments; an unknown key.
    for call in [
        "'delete', '1', NULL, NULL, 'req-x'",
        "'revoke', '1', NULL, NULL, 'req-x'",
        "'retire', '1', NULL, NULL, ''",
    ] {
        let err = manage_as(&w.t, "forge_maint", call).await.expect_err(call);
        assert_eq!(sqlstate(&err), "22023", "{call}: {err}");
    }
    let err = manage_as(&w.t, "forge_maint", "'retire', '9', NULL, NULL, 'req-x'")
        .await
        .expect_err("unknown");
    assert_eq!(sqlstate(&err), "P0002", "{err}");
    // Retire once (a second retirement finds no current row).
    manage_as(
        &w.t,
        "forge_maint",
        "'retire', '1', NULL, NULL, 'req-retire'",
    )
    .await
    .expect("retire");
    let err = manage_as(&w.t, "forge_maint", "'retire', '1', NULL, NULL, 'req-x'")
        .await
        .expect_err("twice");
    assert_eq!(sqlstate(&err), "P0002", "{err}");
    manage_as(
        &w.t,
        "forge_maint",
        "'revoke', '1', NULL, 'leaked', 'req-revoke'",
    )
    .await
    .expect("revoke");
    let mut su = w.t.superuser().await;
    let actions: Vec<(String, String)> = sqlx::query_as(
        "SELECT action, request_id FROM forge.audit_log \
         WHERE resource_type = 'agent_signing_key' ORDER BY id",
    )
    .fetch_all(&mut su)
    .await
    .expect("audit");
    su.close().await.expect("close");
    assert_eq!(
        actions,
        [
            (
                "agent_signing_key.registered".to_owned(),
                "req-key-1".to_owned()
            ),
            (
                "agent_signing_key.retired".to_owned(),
                "req-retire".to_owned()
            ),
            (
                "agent_signing_key.revoked".to_owned(),
                "req-revoke".to_owned()
            ),
        ]
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn keys_retire_and_revoke_once_and_sign_only_while_current() {
    let w = world().await;
    // The kid and the public key never change; nothing is deleted (the
    // owner meets the guard).
    let mut su = w.t.superuser().await;
    for sql in [
        "UPDATE forge.agent_signing_keys SET kid = '2'".to_owned(),
        format!("UPDATE forge.agent_signing_keys SET public_key = '{PUBLIC_KEY_2}'"),
        "DELETE FROM forge.agent_signing_keys".to_owned(),
        "DELETE FROM forge.agent_parts".to_owned(),
    ] {
        let err = sqlx::raw_sql(&sql)
            .execute(&mut su)
            .await
            .expect_err("guard");
        assert_eq!(sqlstate(&err), "XF001", "{sql}: {err}");
    }
    su.close().await.expect("close");
    // A retired key signs no new version.
    manage_as(
        &w.t,
        "forge_maint",
        "'retire', '1', NULL, NULL, 'req-retire'",
    )
    .await
    .expect("retire");
    let err = audited(
        &w.t,
        (PART_RESOURCE, PART),
        &[insert_version("1.1.0", "1", w.alice)],
    )
    .await
    .expect_err("retired key");
    assert_eq!(sqlstate(&err), "XF001", "{err}");
    // A deprecated part gets no new version (with a current key).
    manage_as(
        &w.t,
        "forge_maint",
        &format!("'register', '2', '{PUBLIC_KEY_2}', NULL, 'req-key-2'"),
    )
    .await
    .expect("key 2");
    let deprecate = format!(
        "UPDATE forge.agent_parts SET deprecated_at = now(), deprecated_by = {}",
        w.alice
    );
    audited(&w.t, (PART_RESOURCE, PART), &[deprecate])
        .await
        .expect("deprecate");
    let err = audited(
        &w.t,
        (PART_RESOURCE, PART),
        &[insert_version("1.1.0", "2", w.alice)],
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
