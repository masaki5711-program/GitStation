//! The agent marketplace's tables (plan 16 §3, A3; migration
//! 20270103100000, ADR-0032 A3 amendment).
//!
//! A3 ships the schema and its guards; the commands that write it come
//! with A5 (publish, promote, revoke, `forge admin agent-key`), A7 (the
//! builder's records) and A10 (lesson injections). What the database
//! itself refuses, whoever writes:
//!
//! - `agent_signing_keys`: the kid and public key never change;
//!   retirement and revocation are set once; never deleted.
//! - `agent_parts`: the identity never changes; deprecation is set once;
//!   never deleted.
//! - `agent_part_versions`: inserted as `beta` / `published`, signed by a
//!   key that is neither retired nor revoked, of a part that is not
//!   deprecated; afterwards only beta -> stable (while published) and
//!   published -> revoked; a revoked version is final; never deleted.
//! - `agent_part_files`: append-only, inserted only in the transaction
//!   that inserted the version, `sha256` = the content's, at most 200
//!   files and 1 MiB per version.
//!
//! Every violation is SQLSTATE `XF001`, `FG-DB-0009` AppendOnlyViolation
//! (alert) through [`crate::query_error`]. forge_app holds no DELETE on these
//! four tables, INSERT of the caller's columns and UPDATE only of the
//! columns that may change — and only SELECT on the keys: they are
//! registered, retired and revoked through `forge.agent_signing_key_manage`
//! (SECURITY DEFINER, owner forge_seq, EXECUTE forge_maint only: the
//! operator's `forge admin agent-key`, A5), which writes the audit row.
//!
//! Reads here: [`signing_keys`] (doctor `agent.signing_keys`, the
//! publish step, `GET /api/v1/agent-keys`).
//!
//! A5 (ADR-0032 Amendment A5) adds the marketplace's commands and reads:
//!
//! - [`publish_version`]: the part (created with its first version), the
//!   version, its files and one `agent_part.version_published` audit row
//!   in one transaction. A version that exists is `FG-AGENT-0012`
//!   (`version_exists`); a part whose first version came from another
//!   kind or project is `FG-AGENT-0012` (`kind_changed` /
//!   `source_changed`). The audit row keeps the publication's facts — the
//!   tag object, its pusher and the other people behind it — on which a
//!   promotion decides again ([`version_people`]).
//! - [`set_channel_stable`] / [`revoke_version`]: beta -> stable and
//!   published -> revoked, each with its audit row; a revoked version is
//!   `FG-AGENT-0006`.
//! - Reads: [`list_parts`], [`part`], [`version`], [`version_files`],
//!   [`parts_source`], [`tag_pushers`], [`users_by_email`],
//!   [`newest_pipeline_at`].

use forge_core::{GroupId, ProjectId, UserId};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use crate::change::{AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Outcome, Settle};
use crate::{query_error, read_query_error, Db};

/// The audit resource of a signing key's change (written by
/// `forge.agent_signing_key_manage`).
pub const KEY_RESOURCE: &str = "agent_signing_key";
/// The audit resource of a part's or a version's change (tables.toml).
pub const PART_RESOURCE: &str = "agent_part";

/// A registered signing key: the public half only (the seed is in
/// `agent.signing_keys_file`, never in the database).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct StoredSigningKey {
    /// The key file's key id (decimal text, 1..=65535).
    pub kid: String,
    /// Base64 of the 32-byte Ed25519 public key.
    pub public_key: String,
    /// Retired: signs nothing new, still verifies what it signed.
    pub retired: bool,
    /// Revoked: its signatures are refused.
    pub revoked: bool,
}

/// Every registered signing key, by kid (the read pool).
pub async fn signing_keys(db: &Db) -> Result<Vec<StoredSigningKey>, ForgeError> {
    const QUERY: &str = "agent_marketplace.signing_keys";
    db.within(QUERY, async {
        sqlx::query_as(
            "SELECT kid, public_key, retired_at IS NOT NULL AS retired, \
             revoked_at IS NOT NULL AS revoked \
             FROM forge.agent_signing_keys ORDER BY kid::integer LIMIT 1000",
        )
        .fetch_all(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}

/// A registered key as `GET /api/v1/agent-keys` lists it (and gstn keeps
/// it, ADR-0032 §7): the times in UTC, RFC 3339.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ListedSigningKey {
    pub kid: String,
    pub public_key: String,
    pub created_at: String,
    pub retired_at: Option<String>,
    pub revoked_at: Option<String>,
}

/// Every registered key with its times, by kid (A5).
pub async fn signing_key_list(db: &Db) -> Result<Vec<ListedSigningKey>, ForgeError> {
    const QUERY: &str = "agent_marketplace.signing_key_list";
    db.within(QUERY, async {
        sqlx::query_as(&format!(
            "SELECT kid, public_key, to_char(created_at AT TIME ZONE 'UTC', {TS}) AS created_at,              to_char(retired_at AT TIME ZONE 'UTC', {TS}) AS retired_at,              to_char(revoked_at AT TIME ZONE 'UTC', {TS}) AS revoked_at              FROM forge.agent_signing_keys ORDER BY kid::integer LIMIT 1000"
        ))
        .fetch_all(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}

/// What `forge admin agent-key` asks `forge.agent_signing_key_manage` to
/// do (A5; the function writes the key's audit row).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction<'a> {
    /// Register `public_key` (base64 of 32 bytes) under the kid.
    Register { public_key: &'a str },
    /// Sign nothing new with the kid (it still verifies).
    Retire,
    /// Refuse every signature of the kid, for `reason` (1..=500).
    Revoke { reason: &'a str },
}

/// Register, retire or revoke key `kid` on the maintenance connection
/// (`forge_maint`, the only role that may EXECUTE the function): the key
/// row's id. A refusal of the function (an unknown kid, a retirement or
/// revocation that is already set, a duplicate key) is the database's code
/// (`FG-DB-0009` for the guard, `FG-DB-…` otherwise) with `kid`.
pub async fn manage_signing_key(
    maint: &mut crate::feed::MaintConn,
    kid: &str,
    action: KeyAction<'_>,
    request_id: &forge_core::RequestId,
) -> Result<i64, ForgeError> {
    const QUERY: &str = "agent_marketplace.manage_signing_key";
    let (name, public_key, reason) = match action {
        KeyAction::Register { public_key } => ("register", Some(public_key), None),
        KeyAction::Retire => ("retire", None, None),
        KeyAction::Revoke { reason } => ("revoke", None, Some(reason)),
    };
    let fut = sqlx::query_scalar::<_, i64>(
        "SELECT key_row_id FROM forge.agent_signing_key_manage($1, $2, $3, $4, $5)",
    )
    .bind(name)
    .bind(kid)
    .bind(public_key)
    .bind(reason)
    .bind(request_id.as_str())
    .fetch_one(&mut maint.conn);
    match tokio::time::timeout(std::time::Duration::from_secs(30), fut).await {
        Ok(Ok(id)) => Ok(id),
        Ok(Err(e)) => Err(query_error(QUERY, e)
            .with_context("kid", kid.to_owned())
            .with_context("action", name)),
        Err(_) => Err(ForgeError::new(ErrorCode::DbUnavailable)
            .with_context("reason", "operation_timeout")
            .with_context("operation", QUERY)),
    }
}

// ── A5: publish, promote, revoke (plan 16 A5, ADR-0032 Amendment A5) ──

/// The audit action of a publication.
pub const PUBLISHED: &str = "agent_part.version_published";
/// The audit action of a promotion (beta -> stable).
pub const PROMOTED: &str = "agent_part.version_promoted";
/// The audit action of a revocation.
pub const REVOKED: &str = "agent_part.version_revoked";

/// The most parts one list returns.
pub const MAX_LIST: i64 = 200;

/// One file of a version to publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPartFile {
    pub path: String,
    /// `100644` or `100755`.
    pub mode: &'static str,
    /// Lower-case hex of the content's SHA-256.
    pub sha256: String,
    pub content: Vec<u8>,
}

/// A version to publish (everything the route checked and signed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPartVersion {
    pub slug: String,
    pub kind: String,
    pub source_project: ProjectId,
    pub version: String,
    pub source_sha: String,
    /// The tag's short name (`<slug>/v<version>`).
    pub tag: String,
    pub tag_object: String,
    pub tag_pusher: UserId,
    pub tag_authors: Vec<UserId>,
    pub manifest: serde_json::Value,
    /// `sha256:<hex>`.
    pub digest: String,
    pub signature: String,
    pub key_id: String,
    pub capabilities: Vec<String>,
    pub published_by: UserId,
    pub ci_pipeline_id: i64,
    pub files: Vec<NewPartFile>,
}

/// What a publication wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Published {
    pub part_id: i64,
    pub version_id: i64,
}

fn exists(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::PartVersionExists).with_context("reason", reason)
}

fn unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .and_then(|d| d.code())
        .is_some_and(|c| c == "23505")
}

type PartRow = (i64, String, i64);

async fn part_for_publish(
    conn: &mut PgConnection,
    new: &NewPartVersion,
) -> Result<i64, ForgeError> {
    const QUERY: &str = "agent_marketplace.part_for_publish";
    let found: Option<PartRow> = sqlx::query_as(
        "SELECT id, kind, source_project_id FROM forge.agent_parts WHERE slug = $1 FOR SHARE",
    )
    .bind(&new.slug)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| query_error(QUERY, e))?;
    let Some((id, kind, source)) = found else {
        let inserted: Result<i64, sqlx::Error> = sqlx::query_scalar(
            "INSERT INTO forge.agent_parts (slug, kind, source_project_id, created_by) \
             VALUES ($1, $2, $3, $4) RETURNING id",
        )
        .bind(&new.slug)
        .bind(&new.kind)
        .bind(new.source_project.get())
        .bind(new.published_by.get())
        .fetch_one(&mut *conn)
        .await;
        return match inserted {
            Ok(id) => Ok(id),
            // Another publication created it first: asking again meets the
            // part.
            Err(e) if unique_violation(&e) => Err(exists("part_created_concurrently")),
            Err(e) => Err(query_error(QUERY, e)),
        };
    };
    if kind != new.kind {
        return Err(exists("kind_changed").with_context("published_kind", kind));
    }
    if source != new.source_project.get() {
        return Err(exists("source_changed"));
    }
    Ok(id)
}

async fn insert_version(
    conn: &mut PgConnection,
    part_id: i64,
    new: &NewPartVersion,
) -> Result<i64, ForgeError> {
    const QUERY: &str = "agent_marketplace.insert_version";
    let inserted: Result<i64, sqlx::Error> = sqlx::query_scalar(
        "INSERT INTO forge.agent_part_versions (part_id, version, channel, source_sha, tag, \
         manifest, digest, signature, key_id, capabilities, published_by, ci_pipeline_id, state) \
         VALUES ($1, $2, 'beta', $3, $4, $5, $6, $7, $8, $9, $10, $11, 'published') RETURNING id",
    )
    .bind(part_id)
    .bind(&new.version)
    .bind(&new.source_sha)
    .bind(&new.tag)
    .bind(&new.manifest)
    .bind(&new.digest)
    .bind(&new.signature)
    .bind(&new.key_id)
    .bind(&new.capabilities)
    .bind(new.published_by.get())
    .bind(new.ci_pipeline_id)
    .fetch_one(&mut *conn)
    .await;
    match inserted {
        Ok(id) => Ok(id),
        Err(e) if unique_violation(&e) => Err(exists("version_exists")),
        Err(e) => Err(query_error(QUERY, e)),
    }
}

async fn insert_files(
    conn: &mut PgConnection,
    version_id: i64,
    files: &[NewPartFile],
) -> Result<(), ForgeError> {
    for f in files {
        sqlx::query(
            "INSERT INTO forge.agent_part_files (version_id, path, mode, sha256, content) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(version_id)
        .bind(&f.path)
        .bind(f.mode)
        .bind(&f.sha256)
        .bind(&f.content)
        .execute(&mut *conn)
        .await
        .map_err(|e| query_error("agent_marketplace.insert_file", e))?;
    }
    Ok(())
}

async fn version_taken(
    conn: &mut PgConnection,
    part_id: i64,
    version: &str,
) -> Result<bool, ForgeError> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM forge.agent_part_versions \
         WHERE part_id = $1 AND version = $2)",
    )
    .bind(part_id)
    .bind(version)
    .fetch_one(conn)
    .await
    .map_err(|e| query_error("agent_marketplace.version_taken", e))
}

fn user_ids(users: &[UserId]) -> Vec<i64> {
    users.iter().map(|u| u.get()).collect()
}

fn published_record(new: &NewPartVersion, at: Published) -> ChangeRecord {
    change_record(
        PUBLISHED,
        at.part_id,
        serde_json::json!({
            "slug": new.slug,
            "kind": new.kind,
            "version": new.version,
            "version_id": at.version_id,
            "source_project_id": new.source_project.get(),
            "source_sha": new.source_sha,
            "tag": new.tag,
            "tag_object": new.tag_object,
            "tag_pusher": new.tag_pusher.get(),
            "tag_authors": user_ids(&new.tag_authors),
            "digest": new.digest,
            "key_id": new.key_id,
            "capabilities": new.capabilities,
            "ci_pipeline_id": new.ci_pipeline_id,
            "files": new.files.len(),
        }),
    )
}

/// Publish `new` (module docs): one transaction, one
/// `agent_part.version_published` audit row on (`agent_part`, the part).
pub async fn publish_version(
    db: &Db,
    ctx: &ChangeContext,
    new: &NewPartVersion,
) -> Result<Published, ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let part_id = tx.step(|c| part_for_publish(c, new)).await?;
        let version = new.version.as_str();
        if tx.step(|c| version_taken(c, part_id, version)).await? {
            return Err(exists("version_exists"));
        }
        let version_id = tx.step(|c| insert_version(c, part_id, new)).await?;
        let files = new.files.as_slice();
        tx.step(|c| insert_files(c, version_id, files)).await?;
        let at = Published {
            part_id,
            version_id,
        };
        Ok(Settle::Commit(published_record(new, at), at))
    }
    .await;
    tx.settle(PUBLISHED, outcome).await
}

/// One stored version (its files apart).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct StoredVersion {
    pub id: i64,
    pub part_id: i64,
    pub slug: String,
    pub kind: String,
    pub source_project_id: i64,
    pub version: String,
    /// `beta` or `stable`.
    pub channel: String,
    /// `published` or `revoked`.
    pub state: String,
    pub source_sha: String,
    pub tag: String,
    pub manifest: serde_json::Value,
    pub digest: String,
    pub signature: String,
    pub key_id: String,
    pub capabilities: Vec<String>,
    pub published_by: i64,
    pub ci_pipeline_id: i64,
    pub revoked_reason: Option<String>,
    pub revoked_by: Option<i64>,
    /// UTC, RFC 3339.
    pub revoked_at: Option<String>,
    /// UTC, RFC 3339.
    pub created_at: String,
}

const TS: &str = "'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'";

fn version_sql(filter: &str) -> String {
    format!(
        "SELECT v.id, v.part_id, p.slug, p.kind, p.source_project_id, v.version, v.channel, \
         v.state, v.source_sha, v.tag, v.manifest, v.digest, v.signature, v.key_id, \
         v.capabilities, v.published_by, v.ci_pipeline_id, v.revoked_reason, v.revoked_by, \
         to_char(v.revoked_at AT TIME ZONE 'UTC', {TS}) AS revoked_at, \
         to_char(v.created_at AT TIME ZONE 'UTC', {TS}) AS created_at \
         FROM forge.agent_part_versions AS v JOIN forge.agent_parts AS p ON p.id = v.part_id \
         WHERE {filter}"
    )
}

/// The version `version` of part `slug` (`None`: no such version).
pub async fn version(
    db: &Db,
    slug: &str,
    version: &str,
) -> Result<Option<StoredVersion>, ForgeError> {
    const QUERY: &str = "agent_marketplace.version";
    db.within(QUERY, async {
        sqlx::query_as(&version_sql("p.slug = $1 AND v.version = $2"))
            .bind(slug)
            .bind(version)
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}

/// Every version of part `slug`, newest first, at most 500 (`None`: no
/// such part).
pub async fn part(db: &Db, slug: &str) -> Result<Option<Vec<StoredVersion>>, ForgeError> {
    const QUERY: &str = "agent_marketplace.part";
    db.within(QUERY, async {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM forge.agent_parts WHERE slug = $1)")
                .bind(slug)
                .fetch_one(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))?;
        if !exists {
            return Ok(None);
        }
        let rows: Vec<StoredVersion> =
            sqlx::query_as(&version_sql("p.slug = $1 ORDER BY v.id DESC LIMIT 500"))
                .bind(slug)
                .fetch_all(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))?;
        Ok(Some(rows))
    })
    .await
}

/// A catalog row: slug, kind, deprecated, the newest live version's id.
type CatalogRow = (String, String, bool, Option<i64>);

/// One part of the catalog with its newest live version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartSummary {
    pub slug: String,
    pub kind: String,
    pub deprecated: bool,
    /// The newest version not revoked (on `channel` when one was asked
    /// for); `None`: none.
    pub latest: Option<StoredVersion>,
}

/// The catalog by slug, filtered by `kind`, by a `channel` its newest live
/// version is on (parts with none on it are left out) and by `q` (a
/// substring of the slug); at most [`MAX_LIST`].
pub async fn list_parts(
    db: &Db,
    kind: Option<&str>,
    channel: Option<&str>,
    q: Option<&str>,
) -> Result<Vec<PartSummary>, ForgeError> {
    const QUERY: &str = "agent_marketplace.list_parts";
    let (rows, latest): (Vec<CatalogRow>, Vec<StoredVersion>) = db
        .within(QUERY, async {
            let rows: Vec<CatalogRow> = sqlx::query_as(
                "SELECT p.slug, p.kind, p.deprecated_at IS NOT NULL, \
                 (SELECT v.id FROM forge.agent_part_versions AS v WHERE v.part_id = p.id \
                  AND v.state = 'published' AND ($2::text IS NULL OR v.channel = $2) \
                  ORDER BY v.id DESC LIMIT 1) \
                 FROM forge.agent_parts AS p \
                 WHERE ($1::text IS NULL OR p.kind = $1) \
                 AND ($3::text IS NULL OR strpos(p.slug, $3) > 0) \
                 ORDER BY p.slug LIMIT $4",
            )
            .bind(kind)
            .bind(channel)
            .bind(q)
            .bind(MAX_LIST)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))?;
            let ids: Vec<i64> = rows.iter().filter_map(|r| r.3).collect();
            let latest: Vec<StoredVersion> = sqlx::query_as(&version_sql("v.id = ANY($1)"))
                .bind(&ids)
                .fetch_all(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))?;
            Ok((rows, latest))
        })
        .await?;
    Ok(rows
        .into_iter()
        .filter(|r| channel.is_none() || r.3.is_some())
        .map(|(slug, kind, deprecated, id)| PartSummary {
            slug,
            kind,
            deprecated,
            latest: id.and_then(|id| latest.iter().find(|v| v.id == id).cloned()),
        })
        .collect())
}

/// One stored file of a version (the content stays in the database).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredFile {
    pub path: String,
    pub mode: String,
    pub sha256: String,
    pub size: i64,
}

/// The files of version `version_id`, by path (byte order).
pub async fn version_files(db: &Db, version_id: i64) -> Result<Vec<StoredFile>, ForgeError> {
    const QUERY: &str = "agent_marketplace.version_files";
    let rows: Vec<(String, String, String, i32)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT path, mode, sha256, octet_length(content) FROM forge.agent_part_files \
                 WHERE version_id = $1 ORDER BY path COLLATE \"C\"",
            )
            .bind(version_id)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(|(path, mode, sha256, size)| StoredFile {
            path,
            mode,
            sha256,
            size: i64::from(size),
        })
        .collect())
}

/// The people the publication of `version_id` (of part `part_id`)
/// recorded: the tag's pusher and the other people behind it, from its
/// audit row. `None`: no publication row; a pusher that is not there is
/// `None` (the caller fails closed).
pub async fn version_people(
    db: &Db,
    part_id: i64,
    version_id: i64,
) -> Result<Option<(Option<UserId>, Vec<UserId>)>, ForgeError> {
    const QUERY: &str = "agent_marketplace.version_people";
    let details: Option<serde_json::Value> = db
        .within(QUERY, async {
            sqlx::query_scalar(
                "SELECT details FROM forge.audit_log WHERE action = $1 AND resource_type = $2 \
                 AND resource_id = $3 AND outcome = 'succeeded' \
                 AND details->>'version_id' = $4::text ORDER BY id DESC LIMIT 1",
            )
            .bind(PUBLISHED)
            .bind(PART_RESOURCE)
            .bind(part_id)
            .bind(version_id.to_string())
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    Ok(details.map(|d| {
        let pusher = d
            .get("tag_pusher")
            .and_then(serde_json::Value::as_i64)
            .filter(|id| *id > 0)
            .map(UserId::new);
        let authors = d
            .get("tag_authors")
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(serde_json::Value::as_i64)
                    .map(UserId::new)
                    .collect()
            })
            .unwrap_or_default();
        (pusher, authors)
    }))
}

/// Who pushed `tag_ref` (a full `refs/tags/…` name) to `object` in
/// `project`, from every `repository.pushed` audit row listing that update
/// (oldest first, at most 50): the first pusher and the others (review A5
/// MEDIUM: a later push of the same tag does not clear the first). Any such
/// push that was not a person's (`actor_kind <> 'user'`: a replayed push,
/// a service account's or an act-as push), or none at all (the ref not
/// listed: a push of more than 100 refs), is `(None, [])` and the caller
/// fails closed (plan 16 §4, ADR-0032 Amendments A4 and A5).
pub async fn tag_pushers(
    db: &Db,
    project: ProjectId,
    tag_ref: &str,
    object: &str,
) -> Result<(Option<UserId>, Vec<UserId>), ForgeError> {
    const QUERY: &str = "agent_marketplace.tag_pushers";
    let needle = serde_json::json!([{ "ref": tag_ref, "new": object }]);
    let rows: Vec<(String, Option<i64>)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT actor_kind, actor_user_id FROM forge.audit_log                  WHERE action = 'repository.pushed' AND project_id = $1                  AND outcome = 'succeeded' AND details->'refs' @> $2::jsonb                  ORDER BY id LIMIT 50",
            )
            .bind(project.get())
            .bind(&needle)
            .fetch_all(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    let mut people = Vec::with_capacity(rows.len());
    for (kind, id) in rows {
        match id {
            Some(id) if kind == "user" && id > 0 => people.push(UserId::new(id)),
            _ => return Ok((None, Vec::new())),
        }
    }
    let mut people = people.into_iter();
    let first = people.next();
    Ok((first, people.filter(|u| Some(*u) != first).collect()))
}

/// The accounts whose e-mail address (ignoring case) is one of `emails`
/// (at most 16 addresses are looked up).
pub async fn users_by_email(db: &Db, emails: &[String]) -> Result<Vec<UserId>, ForgeError> {
    const QUERY: &str = "agent_marketplace.users_by_email";
    let lowered: Vec<String> = emails.iter().take(16).map(|e| e.to_lowercase()).collect();
    if lowered.is_empty() {
        return Ok(Vec::new());
    }
    // The read pool (forge_ro) has no e-mail column: the write pool.
    let ids: Vec<i64> = db
        .within(QUERY, async {
            sqlx::query_scalar(
                "SELECT id FROM forge.users WHERE email IS NOT NULL \
                 AND lower(email) = ANY($1) ORDER BY id",
            )
            .bind(&lowered)
            .fetch_all(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    Ok(ids.into_iter().map(UserId::new).collect())
}

/// The parts project at `path` (`group/…/project`) and the group it is in
/// (`None`: no project there, or it is not in a group).
pub async fn parts_source(db: &Db, path: &str) -> Result<Option<(ProjectId, GroupId)>, ForgeError> {
    const QUERY: &str = "agent_marketplace.parts_source";
    let row: Option<(i64, i64)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT p.owner_id, g.owner_id FROM forge.namespaces AS p \
                 JOIN forge.namespaces AS g ON g.id = p.parent_id AND g.kind = 'group' \
                 WHERE p.kind = 'project' AND p.full_path = $1",
            )
            .bind(path)
            .fetch_optional(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(row.map(|(p, g)| (ProjectId::new(p), GroupId::new(g))))
}

/// The newest pipeline of `project` on commit `sha`: its id and status
/// (`None`: none ran there).
pub async fn newest_pipeline_at(
    db: &Db,
    project: ProjectId,
    sha: &str,
) -> Result<Option<(i64, String)>, ForgeError> {
    const QUERY: &str = "agent_marketplace.newest_pipeline_at";
    db.within(QUERY, async {
        sqlx::query_as(
            "SELECT id, status FROM forge.ci_pipelines WHERE project_id = $1 AND sha = $2 \
             ORDER BY id DESC LIMIT 1",
        )
        .bind(project.get())
        .bind(sha)
        .fetch_optional(db.write_pool())
        .await
        .map_err(|e| query_error(QUERY, e))
    })
    .await
}

/// A version as locked for a change: id, part, channel, state.
type Locked = (i64, i64, String, String);

async fn lock_version(
    conn: &mut PgConnection,
    slug: &str,
    version: &str,
) -> Result<Locked, ForgeError> {
    const QUERY: &str = "agent_marketplace.lock_version";
    let row: Option<Locked> = sqlx::query_as(
        "SELECT v.id, v.part_id, v.channel, v.state FROM forge.agent_part_versions AS v \
         JOIN forge.agent_parts AS p ON p.id = v.part_id \
         WHERE p.slug = $1 AND v.version = $2 FOR UPDATE OF v",
    )
    .bind(slug)
    .bind(version)
    .fetch_optional(conn)
    .await
    .map_err(|e| query_error(QUERY, e))?;
    row.ok_or_else(|| {
        ForgeError::new(ErrorCode::NotFound).with_context("resource", "agent_part_version")
    })
}

fn revoked(slug: &str, version: &str) -> ForgeError {
    ForgeError::new(ErrorCode::PartVersionRevoked)
        .with_context("reason", "revoked")
        .with_context("slug", slug.to_owned())
        .with_context("version", version.to_owned())
}

/// Move `slug` `version` from beta to stable: one
/// `agent_part.version_promoted` audit row; `false` when it was stable
/// already (nothing written). A revoked version is `FG-AGENT-0006`.
pub async fn set_channel_stable(
    db: &Db,
    ctx: &ChangeContext,
    slug: &str,
    version: &str,
) -> Result<bool, ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let (id, part_id, channel, state) = tx.step(|c| lock_version(c, slug, version)).await?;
        if state == "revoked" {
            return Err(revoked(slug, version));
        }
        if channel == "stable" {
            return Ok(Settle::Discard(false));
        }
        tx.step(|c| async move {
            sqlx::query("UPDATE forge.agent_part_versions SET channel = 'stable' WHERE id = $1")
                .bind(id)
                .execute(c)
                .await
                .map_err(|e| query_error("agent_marketplace.promote", e))
                .map(drop)
        })
        .await?;
        let details = serde_json::json!({
            "slug": slug, "version": version, "version_id": id, "from": "beta", "to": "stable",
        });
        Ok(Settle::Commit(
            change_record(PROMOTED, part_id, details),
            true,
        ))
    }
    .await;
    tx.settle(PROMOTED, outcome).await
}

/// Revoke `slug` `version` by `by` for `reason` (1..=500 characters): one
/// `agent_part.version_revoked` audit row. A revoked version is
/// `FG-AGENT-0006` (final).
pub async fn revoke_version(
    db: &Db,
    ctx: &ChangeContext,
    slug: &str,
    version: &str,
    by: UserId,
    reason: &str,
) -> Result<(), ForgeError> {
    let mut tx = ChangeTx::begin(db, ctx).await?;
    let outcome = async {
        let (id, part_id, _channel, state) = tx.step(|c| lock_version(c, slug, version)).await?;
        if state == "revoked" {
            return Err(revoked(slug, version));
        }
        tx.step(|c| async move {
            sqlx::query(
                "UPDATE forge.agent_part_versions SET state = 'revoked', revoked_reason = $2, \
                 revoked_by = $3, revoked_at = now() WHERE id = $1",
            )
            .bind(id)
            .bind(reason)
            .bind(by.get())
            .execute(c)
            .await
            .map_err(|e| query_error("agent_marketplace.revoke", e))
            .map(drop)
        })
        .await?;
        let details = serde_json::json!({
            "slug": slug, "version": version, "version_id": id, "reason": reason,
        });
        Ok(Settle::Commit(change_record(REVOKED, part_id, details), ()))
    }
    .await;
    tx.settle(REVOKED, outcome).await
}

fn change_record(action: &'static str, part_id: i64, details: serde_json::Value) -> ChangeRecord {
    ChangeRecord {
        audit: AuditEntry {
            action,
            outcome: Outcome::Succeeded,
            resource_type: PART_RESOURCE,
            resource_id: Some(part_id),
            project_id: None,
            error_code: None,
            details,
        },
        affected: Vec::new(),
        events: Vec::new(),
    }
}

#[cfg(test)]
#[path = "agent_marketplace_tests.rs"]
mod tests;
