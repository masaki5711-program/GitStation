//! The PFS portal (ADR-0036): the news outbox `forge.portal_news` and the
//! reads behind the home widgets.
//!
//! **News.** [`stage`] is called by the business changes themselves, inside
//! their own transaction (a deployment's end in `deploy::worker`, an
//! auto-merge stop in `review::auto_merge::record_state`, a spec release in
//! `spec_releases`): one row per `source_id` (`ON CONFLICT DO NOTHING`, the
//! contract's idempotency) and its `portal.news_send` job, or nothing when
//! [`Db::portal_news`] is `None` (`portal.enabled` / `portal.news_enabled`
//! off: nothing is written). The sender (forge-integ) reads the row
//! ([`news`]), the audience's staff ids ([`audience`]) and records the
//! outcome ([`record_sent`], [`record_attempt`], [`record_final`]). A row
//! whose job ended without recording one (the job dead-lettered after an
//! expired lease, or the database failed on its last attempt) is ended by
//! the hourly [`sweep_orphaned`], so no row stays `pending` for ever.
//!
//! **Widgets.** Candidates only, never a decision: the projects where the
//! person holds a membership ([`member_projects`]), and the newest pipeline
//! of a ref and the newest deployment of each of a few projects
//! ([`latest_pipelines`], [`latest_deployments`]). forge-pipelines decides
//! every project with `authorize()` before anything of it is shown.

use std::collections::BTreeSet;
use std::time::Duration;

use forge_core::portal::NewsTopic;
use forge_core::{ProjectId, RequestId, UserId, UserKind};
use forge_errors::{ErrorCode, ForgeError};
use serde_json::{json, Value};
use sqlx::PgConnection;

use crate::identities::{staff_id_ok, LinkRules};
use crate::jobs::{enqueue_on, DedupKey, JobKind, JobTimeout, MaxAttempts, NewJob};
use crate::roles::CAP;
use crate::{query_error, read_query_error, Db};

/// The job that sends one news item.
pub const NEWS_JOB: &str = "portal.news_send";
/// The most people one news item is addressed to.
pub const MAX_AUDIENCE: usize = 200;
/// The most projects a widget offers (contract §6.4).
pub const MAX_OPTIONS: i64 = 200;
/// The most projects a `status` widget shows (contract §3).
pub const MAX_STATUS_ITEMS: usize = 24;
/// Finished news rows are kept this long.
pub const RETENTION: Duration = Duration::from_secs(30 * 24 * 3600);

const TS: &str = "'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'";

/// How the news jobs run (`portal.max_attempts`, `portal.timeout_ms`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewsPolicy {
    pub max_attempts: MaxAttempts,
    /// One run: the token, the POST and the bookkeeping.
    pub timeout: JobTimeout,
}

impl NewsPolicy {
    /// `max_attempts` 1..=25 and a run of `post_timeout` plus 15 s for the
    /// token and the database (`FG-SYS-0001` otherwise: the configuration
    /// bounds them).
    pub fn new(max_attempts: u8, post_timeout: Duration) -> Result<Self, ForgeError> {
        let internal = |reason: &'static str| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", reason)
        };
        Ok(Self {
            max_attempts: MaxAttempts::new(max_attempts)
                .ok_or_else(|| internal("news_attempts_out_of_range"))?,
            timeout: JobTimeout::new(post_timeout + Duration::from_secs(15))
                .ok_or_else(|| internal("news_timeout_out_of_range"))?,
        })
    }
}

/// A news item a business change stages.
#[derive(Debug, Clone, PartialEq)]
pub struct NewsDraft {
    /// The contract's `source_id` (`forge_core::portal::*_source_id`).
    pub source_id: String,
    pub topic: NewsTopic,
    pub project: ProjectId,
    /// References only (no personal data): `iid`, `tier`, `version`,
    /// `sha`, `status`.
    pub facts: Value,
    /// Who is told (duplicates and absent ids dropped; at most
    /// [`MAX_AUDIENCE`]).
    pub audience: Vec<UserId>,
    pub request_id: RequestId,
}

fn internal(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal).with_context("reason", reason)
}

/// Stage `draft` on `conn` (the business change's transaction) when `db`
/// stages news: the row and its job, or nothing for a `source_id` already
/// staged (`Ok(None)`), an empty audience, or news turned off.
pub(crate) async fn stage(
    db: &Db,
    conn: &mut PgConnection,
    draft: &NewsDraft,
) -> Result<Option<i64>, ForgeError> {
    const QUERY: &str = "portal.news.stage";
    let Some(policy) = db.portal_news() else {
        return Ok(None);
    };
    let audience: Vec<i64> = draft
        .audience
        .iter()
        .map(|u| u.get())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(MAX_AUDIENCE)
        .collect();
    if audience.is_empty() {
        return Ok(None);
    }
    let id: Option<i64> = sqlx::query_scalar(
        "INSERT INTO forge.portal_news (source_id, topic, project_id, facts, audience, \
         occurred_at, request_id) VALUES ($1, $2, $3, $4, $5, now(), $6) \
         ON CONFLICT (source_id) DO NOTHING RETURNING id",
    )
    .bind(&draft.source_id)
    .bind(draft.topic.as_str())
    .bind(draft.project.get())
    .bind(&draft.facts)
    .bind(&audience)
    .bind(draft.request_id.as_str())
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| query_error(QUERY, e))?;
    let Some(id) = id else {
        return Ok(None);
    };
    let job = NewJob {
        kind: JobKind::parse(NEWS_JOB).ok_or_else(|| internal("invalid_job_kind"))?,
        payload: json!({ "news_id": id }),
        max_attempts: policy.max_attempts,
        timeout: policy.timeout,
        dedup_key: DedupKey::parse(&format!("portal-news:{id}")),
        origin_request_id: draft.request_id.clone(), // clone: the job owns its request id
        origin_event_seq: None,
    };
    enqueue_on(conn, &job).await?;
    Ok(Some(id))
}

/// Stage `draft` in its own transaction (a change that has none of its
/// own, the tests).
pub async fn stage_alone(db: &Db, draft: &NewsDraft) -> Result<Option<i64>, ForgeError> {
    const QUERY: &str = "portal.news.stage_alone";
    db.within(QUERY, async {
        let mut tx = db
            .write_pool()
            .begin()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        let id = stage(db, &mut tx, draft).await?;
        tx.commit().await.map_err(|e| query_error(QUERY, e))?;
        Ok(id)
    })
    .await
}

/// A news row as the sender reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredNews {
    pub id: i64,
    pub source_id: String,
    pub topic: NewsTopic,
    pub project: ProjectId,
    pub facts: Value,
    pub audience: Vec<UserId>,
    /// UTC, RFC 3339.
    pub occurred_at: String,
    /// `occurred_at` + the topic's days shown, UTC, RFC 3339.
    pub expires_at: String,
    /// `pending`, `sent`, `failed`, `dropped`.
    pub state: String,
    pub attempts: i32,
}

type NewsRow = (
    i64,
    String,
    String,
    i64,
    Value,
    Vec<i64>,
    String,
    String,
    String,
    i32,
);

/// News row `id` (`None`: gone — purged with its project).
pub async fn news(db: &Db, id: i64) -> Result<Option<StoredNews>, ForgeError> {
    const QUERY: &str = "portal.news.get";
    let row: Option<NewsRow> = db
        .within(QUERY, async {
            sqlx::query_as(&format!(
                "SELECT id, source_id, topic, project_id, facts, audience, \
                 to_char(occurred_at AT TIME ZONE 'UTC', {TS}), \
                 to_char((occurred_at + make_interval(days => \
                   CASE topic WHEN 'spec_released' THEN 30 ELSE 7 END)) AT TIME ZONE 'UTC', {TS}), \
                 state, attempts FROM forge.portal_news WHERE id = $1"
            ))
            .bind(id)
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    let Some((id, source_id, topic, project, facts, audience, occurred, expires, state, n)) = row
    else {
        return Ok(None);
    };
    let topic = NewsTopic::parse(&topic).ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", "stored_news_topic_invalid")
            .with_context("news_id", id)
    })?;
    Ok(Some(StoredNews {
        id,
        source_id,
        topic,
        project: ProjectId::new(project),
        facts,
        audience: audience.into_iter().map(UserId::new).collect(),
        occurred_at: occurred,
        expires_at: expires,
        state,
        attempts: n,
    }))
}

/// One person of a news item's audience, as their account reads now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudienceMember {
    pub user: UserId,
    /// `users.employee_number` (the portal's `staff_id`); `None`: the
    /// person cannot be addressed on the portal.
    pub staff_id: Option<String>,
    pub active: bool,
    pub kind: UserKind,
}

/// The accounts of `users` that exist (the write pool: employee numbers
/// are not readable by the read role).
pub async fn audience(db: &Db, users: &[UserId]) -> Result<Vec<AudienceMember>, ForgeError> {
    const QUERY: &str = "portal.news.audience";
    let ids: Vec<i64> = users.iter().take(MAX_AUDIENCE).map(|u| u.get()).collect();
    let rows: Vec<(i64, Option<String>, String, String)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT id, employee_number, state, kind FROM forge.users \
                 WHERE id = ANY($1) ORDER BY id",
            )
            .bind(&ids)
            .fetch_all(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    rows.into_iter()
        .map(|(id, staff_id, state, kind)| {
            Ok(AudienceMember {
                user: UserId::new(id),
                staff_id: staff_id
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty()),
                active: state == "active",
                kind: UserKind::parse(&kind).ok_or_else(|| {
                    ForgeError::new(ErrorCode::Internal)
                        .with_context("reason", "stored_user_kind_invalid")
                        .with_context("user", id)
                })?,
            })
        })
        .collect()
}

/// The portal answered 202: `sent` (only from `pending`).
pub async fn record_sent(db: &Db, id: i64, attempt: i32) -> Result<bool, ForgeError> {
    const QUERY: &str = "portal.news.sent";
    db.within(QUERY, async {
        sqlx::query(
            "UPDATE forge.portal_news SET state = 'sent', sent_at = now(), updated_at = now(), \
             attempts = greatest(attempts, $2), last_status = 202, last_error_code = NULL \
             WHERE id = $1 AND state = 'pending'",
        )
        .bind(id)
        .bind(attempt)
        .execute(db.write_pool())
        .await
        .map(|r| r.rows_affected() == 1)
        .map_err(|e| query_error(QUERY, e))
    })
    .await
}

/// One failed attempt (it stays `pending`; the job is retried).
pub async fn record_attempt(
    db: &Db,
    id: i64,
    attempt: i32,
    status: Option<u16>,
    code: ErrorCode,
) -> Result<(), ForgeError> {
    const QUERY: &str = "portal.news.attempt";
    db.within(QUERY, async {
        sqlx::query(
            "UPDATE forge.portal_news SET attempts = greatest(attempts, $2), last_status = $3, \
             last_error_code = $4, updated_at = now() WHERE id = $1 AND state = 'pending'",
        )
        .bind(id)
        .bind(attempt)
        .bind(status.map(i32::from))
        .bind(code.id())
        .execute(db.write_pool())
        .await
        .map(drop)
        .map_err(|e| query_error(QUERY, e))
    })
    .await
}

/// The end of a news item that was not sent: `failed` (with the code) or
/// `dropped` (nobody to tell), with `reason`.
pub async fn record_final(
    db: &Db,
    id: i64,
    state: &'static str,
    reason: &'static str,
    attempt: i32,
    status: Option<u16>,
    code: Option<ErrorCode>,
) -> Result<(), ForgeError> {
    const QUERY: &str = "portal.news.final";
    if !matches!(state, "failed" | "dropped") {
        return Err(internal("news_final_state_invalid").with_context("state", state));
    }
    db.within(QUERY, async {
        sqlx::query(
            "UPDATE forge.portal_news SET state = $2, reason = $3, attempts = greatest(attempts, $4), \
             last_status = coalesce($5, last_status), last_error_code = coalesce($6, last_error_code), \
             updated_at = now() WHERE id = $1 AND state = 'pending'",
        )
        .bind(id)
        .bind(state)
        .bind(reason)
        .bind(attempt)
        .bind(status.map(i32::from))
        .bind(code.map(|c| c.id()))
        .execute(db.write_pool())
        .await
        .map(drop)
        .map_err(|e| query_error(QUERY, e))
    })
    .await
}

/// What `forge doctor` reports about the news outbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NewsHealth {
    pub pending: i64,
    /// Seconds since the oldest pending row was staged.
    pub oldest_pending_s: Option<i64>,
    /// `failed` rows updated in the last 24 hours.
    pub failed_24h: i64,
}

/// The outbox's state (the read pool).
pub async fn health(db: &Db) -> Result<NewsHealth, ForgeError> {
    const QUERY: &str = "portal.news.health";
    let (pending, oldest, failed): (i64, Option<f64>, i64) = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT (SELECT count(*) FROM forge.portal_news WHERE state = 'pending'), \
                 (SELECT extract(epoch FROM now() - min(created_at))::float8 \
                    FROM forge.portal_news WHERE state = 'pending'), \
                 (SELECT count(*) FROM forge.portal_news WHERE state = 'failed' \
                    AND updated_at > now() - interval '24 hours')",
            )
            .fetch_one(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(NewsHealth {
        pending,
        // Whole seconds, never negative (a clock step).
        oldest_pending_s: oldest.map(|s| s.max(0.0).floor() as i64),
        failed_24h: failed,
    })
}

/// Delete at most `limit` finished rows older than [`RETENTION`]; how many.
pub async fn purge_finished(db: &Db, limit: i64) -> Result<u64, ForgeError> {
    const QUERY: &str = "portal.news.purge";
    let secs = RETENTION.as_secs_f64();
    db.within(QUERY, async {
        sqlx::query(
            "DELETE FROM forge.portal_news WHERE id IN (SELECT id FROM forge.portal_news \
             WHERE state <> 'pending' AND updated_at < now() - make_interval(secs => $1) \
             ORDER BY updated_at LIMIT $2)",
        )
        .bind(secs)
        .bind(limit)
        .execute(db.write_pool())
        .await
        .map(|r| r.rows_affected())
        .map_err(|e| query_error(QUERY, e))
    })
    .await
}

/// A `pending` row [`sweep_orphaned`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweptNews {
    pub id: i64,
    pub source_id: String,
    pub attempts: i32,
    /// The request id of the change that staged it.
    pub request_id: String,
}

/// End at most `limit` `pending` rows that no job will send any more —
/// their `portal.news_send` job is neither queued nor running (it
/// dead-lettered after an expired lease, or the database failed on its
/// last attempt before the sender could record the end): `failed` with
/// `reason = tries_used_up` and `FG-INT-0023`. Returns the rows it ended,
/// oldest first, each exactly once (the caller logs them); a row whose job
/// is still alive is never touched (the row and its job are staged in one
/// transaction, so a fresh row always has one).
pub async fn sweep_orphaned(db: &Db, limit: i64) -> Result<Vec<SweptNews>, ForgeError> {
    const QUERY: &str = "portal.news.sweep";
    let rows: Vec<(i64, String, i32, String)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "UPDATE forge.portal_news AS n SET state = 'failed', reason = 'tries_used_up', \
                 last_error_code = $2, updated_at = now() \
                 FROM (SELECT p.id FROM forge.portal_news p \
                        WHERE p.state = 'pending' AND NOT EXISTS ( \
                          SELECT 1 FROM ONLY forge.jobs j \
                           WHERE j.kind = $3 AND j.dedup_key IS NOT NULL \
                             AND j.dedup_key = 'portal-news:' || p.id::text \
                             AND j.state <> 'dead' AND j.state IN ('queued', 'running')) \
                        ORDER BY p.id LIMIT $1 FOR UPDATE OF p SKIP LOCKED) AS orphan \
                 WHERE n.id = orphan.id AND n.state = 'pending' \
                 RETURNING n.id, n.source_id, n.attempts, n.request_id",
            )
            .bind(limit)
            .bind(ErrorCode::PortalNewsUndeliverable.id())
            .bind(NEWS_JOB)
            .fetch_all(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    let mut swept: Vec<SweptNews> = rows
        .into_iter()
        .map(|(id, source_id, attempts, request_id)| SweptNews {
            id,
            source_id,
            attempts,
            request_id,
        })
        .collect();
    swept.sort_by_key(|s| s.id);
    Ok(swept)
}

/// The people who hold a role on `project` — direct members, members of a
/// group above it, the owner of its personal namespace — at most
/// [`MAX_AUDIENCE`], by id: the audience of a spec release (each is decided
/// again with `authorize()` when it is sent). More than that is logged once
/// (`forge.portal.news_audience_truncated`): the people beyond the limit
/// are not told.
///
/// ADR-0038 §2.5: the hand-made memberships only — the people an org
/// binding brings in are not addressed (a bound department would flood the
/// audience cap; they still find the release on the portal, where every
/// read is decided by `authorize()` with the org-derived role). Errs on
/// the side of telling fewer people, never of telling someone without a
/// role.
pub(crate) async fn project_people(
    conn: &mut PgConnection,
    project: ProjectId,
    request_id: &RequestId,
) -> Result<Vec<UserId>, ForgeError> {
    const QUERY: &str = "portal.news.project_people";
    // One more than the limit, only to know whether it was reached.
    let limit = i64::try_from(MAX_AUDIENCE).unwrap_or(200).saturating_add(1);
    let mut rows: Vec<i64> = sqlx::query_scalar(
        "WITH RECURSIVE up (id, parent_id, step) AS ( \
           SELECT n.id, n.parent_id, 0 FROM forge.namespaces n \
            WHERE n.kind = 'project' AND n.owner_id = $1 \
           UNION ALL SELECT n.id, n.parent_id, u.step + 1 FROM forge.namespaces n \
            JOIN up u ON n.id = u.parent_id WHERE u.step < $2) \
         SELECT DISTINCT x.user_id FROM ( \
           SELECT pm.user_id FROM forge.project_members pm WHERE pm.project_id = $1 \
           UNION SELECT gm.user_id FROM up JOIN forge.namespaces n \
             ON n.id = up.id AND n.kind = 'group' \
             JOIN forge.group_members gm ON gm.group_id = n.owner_id \
           UNION SELECT n.owner_id FROM up JOIN forge.namespaces n \
             ON n.id = up.id AND n.kind = 'user') x \
         ORDER BY 1 LIMIT $3",
    )
    .bind(project.get())
    .bind(CAP)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| query_error(QUERY, e))?;
    if rows.len() > MAX_AUDIENCE {
        rows.truncate(MAX_AUDIENCE);
        tracing::warn!(
            target: "forge_db",
            event = "forge.portal.news_audience_truncated",
            request_id = %request_id,
            project_id = project.get(),
            limit = MAX_AUDIENCE,
            "a project has more people than one portal news item is addressed to"
        );
    }
    Ok(rows.into_iter().map(UserId::new).collect())
}

// ------------------------------------------------------------ the person

/// The user a portal widget call stands for (ADR-0036 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortalUser {
    pub user: UserId,
    pub active: bool,
    pub kind: UserKind,
    /// `users.is_admin` as read now (a linked person only: the employee
    /// number never finds an administrator, `privileged_account`). The
    /// portal's menu shows the administrator link by it, decided by
    /// `authorize()` (ADR-0021 amendment 2026-11-24); never a right of the
    /// portal's credential.
    pub is_admin: bool,
    /// The flag was granted by the identity provider's role
    /// (`admin_source = 'oidc'`): it counts for the menu only while the
    /// portal's verified token still holds the role (forge-oidc).
    pub admin_from_idp: bool,
}

/// `FG-AUTH-0027 PortalUserNotLinked` with the log `reason`.
fn not_linked(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::PortalUserNotLinked).with_context("reason", reason)
}

/// 1..=255 characters, no NUL (the identity table's CHECKs).
fn fits(value: &str) -> bool {
    !value.is_empty() && value.chars().count() <= 255 && !value.contains('\0')
}

/// The user of a portal access token (ADR-0036 §2), by the sign-in's link
/// rules (`identities::login_decided`) but **read-only** — a widget call
/// never links or creates a user:
///
/// 1. `(issuer, subject)` is linked → that user (whatever their state:
///    `authorize()` refuses a disabled one);
/// 2. otherwise, only under [`LinkRules::EmployeeNumberOnly`] (the sign-in
///    of `auth.oidc.claims_source = access_token`), the employee number,
///    trimmed and without the case, refused in the sign-in's order:
///    missing → `employee_number_missing`; not `[A-Za-z0-9_-]{1,32}` →
///    `employee_number_format`; nobody → `unlinked`; two →
///    `employee_number_ambiguous`; `auto_link` off → `auto_link_disabled`;
///    a user who has an identity of any issuer → `employee_number_linked`;
///    an administrator → `privileged_account`; a user whose `auth_source`
///    is not in `auth.oidc.link_auth_sources` → `auth_source_not_linkable`;
///    otherwise that user — the one the next sign-in would link.
///
/// Under [`LinkRules::VerifiedEmail`] (`claims_source = id_token`) the
/// sign-in links by the number **and** a verified e-mail, which a widget
/// call cannot check: only a linked person is found, anyone else is
/// `sign_in_required`.
///
/// Every refusal is `FG-AUTH-0027` with `reason`; the subject and the
/// number are never in it. The write pool (the employee number is not
/// readable by the read role).
pub async fn portal_user(
    db: &Db,
    issuer: &str,
    subject: &str,
    employee_number: Option<&str>,
    auto_link: bool,
    rules: LinkRules,
) -> Result<PortalUser, ForgeError> {
    const QUERY: &str = "portal.user";
    if !fits(issuer) || !fits(subject) {
        return Err(internal("identity_out_of_range"));
    }
    let to_user = |id: i64, kind: &str, state: &str, is_admin: bool, admin_from_idp: bool| {
        Ok::<_, ForgeError>(PortalUser {
            user: UserId::new(id),
            active: state == "active",
            kind: UserKind::parse(kind).ok_or_else(|| internal("stored_user_kind_invalid"))?,
            is_admin,
            admin_from_idp,
        })
    };
    let linked: Option<(i64, String, String, bool, bool)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT u.id, u.kind, u.state, u.is_admin, \
                 u.is_admin AND u.admin_source = 'oidc' FROM forge.user_identities i \
                 JOIN forge.users u ON u.id = i.user_id \
                 WHERE i.issuer = $1 AND i.subject = $2",
            )
            .bind(issuer)
            .bind(subject)
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    if let Some((id, kind, state, is_admin, from_idp)) = linked {
        return to_user(id, &kind, &state, is_admin, from_idp);
    }
    let sources = match rules {
        LinkRules::EmployeeNumberOnly { auth_sources } => auth_sources,
        LinkRules::VerifiedEmail => return Err(not_linked("sign_in_required")),
    };
    let Some(number) = employee_number.map(str::trim).filter(|e| !e.is_empty()) else {
        return Err(not_linked("employee_number_missing"));
    };
    if !staff_id_ok(number) {
        return Err(not_linked("employee_number_format"));
    }
    // As the sign-in under EmployeeNumberOnly: an identity of ANY issuer
    // counts as linked (the number alone never moves a bound account).
    let mut holders: Vec<(i64, String, String, bool, bool, String)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT u.id, u.kind, u.state, u.is_admin, \
                 EXISTS (SELECT 1 FROM forge.user_identities i WHERE i.user_id = u.id), \
                 u.auth_source \
                 FROM forge.users u WHERE upper(u.employee_number) = upper($1) \
                 ORDER BY u.id LIMIT 2",
            )
            .bind(number)
            .fetch_all(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    if holders.len() > 1 {
        return Err(not_linked("employee_number_ambiguous"));
    }
    let Some((id, kind, state, is_admin, has_identity, auth_source)) = holders.pop() else {
        return Err(not_linked("unlinked"));
    };
    if !auto_link {
        return Err(not_linked("auto_link_disabled"));
    }
    if has_identity {
        return Err(not_linked("employee_number_linked"));
    }
    if is_admin {
        return Err(not_linked("privileged_account"));
    }
    if !sources.allows(&auth_source) {
        return Err(not_linked("auth_source_not_linkable"));
    }
    // Not an administrator: refused above (`privileged_account`).
    to_user(id, &kind, &state, false, false)
}

// ------------------------------------------------------------ widgets

/// The projects (live) where `user` holds a membership — directly, through
/// an ancestor group or as the owner of the personal namespace — newest
/// first, at most `limit` (≤ [`MAX_OPTIONS`]). Candidates only: the caller
/// decides each with `authorize()`.
pub async fn member_projects(
    db: &Db,
    user: UserId,
    limit: i64,
) -> Result<Vec<ProjectId>, ForgeError> {
    const QUERY: &str = "portal.member_projects";
    let sql = format!(
        "WITH RECURSIVE {}picked AS (SELECT DISTINCT r.project_id FROM related r \
           JOIN forge.projects p ON p.id = r.project_id AND p.deleted_at IS NULL) \
         SELECT project_id FROM picked ORDER BY project_id DESC LIMIT $3",
        crate::review::user_lists::related_ctes("$1", "$2")
    );
    let rows: Vec<i64> = db
        .within(QUERY, async {
            sqlx::query_scalar(&sql)
                .bind(user.get())
                .bind(CAP)
                .bind(limit.clamp(1, MAX_OPTIONS))
                .fetch_all(db.pool().executor())
                .await
                .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(rows.into_iter().map(ProjectId::new).collect())
}

/// The newest pipeline's status on `git_ref` of each of `projects` (in
/// their order; `None`: no pipeline yet). At most [`MAX_STATUS_ITEMS`].
pub async fn latest_pipelines(
    db: &Db,
    projects: &[ProjectId],
    git_ref: &str,
) -> Result<Vec<(ProjectId, Option<String>)>, ForgeError> {
    const QUERY: &str = "portal.latest_pipelines";
    let ids: Vec<i64> = projects
        .iter()
        .take(MAX_STATUS_ITEMS)
        .map(|p| p.get())
        .collect();
    let rows: Vec<(i64, Option<String>)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT p.id, x.status FROM unnest($1::bigint[]) WITH ORDINALITY AS p(id, n) \
                 LEFT JOIN LATERAL (SELECT c.status FROM forge.ci_pipelines c \
                   WHERE c.project_id = p.id AND c.ref = $2 ORDER BY c.id DESC LIMIT 1) x ON true \
                 ORDER BY p.n",
            )
            .bind(&ids)
            .bind(git_ref)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(|(id, status)| (ProjectId::new(id), status))
        .collect())
}

/// The newest deployment's status and environment of each of `projects`
/// (in their order; `None`: never deployed). At most [`MAX_STATUS_ITEMS`].
pub async fn latest_deployments(
    db: &Db,
    projects: &[ProjectId],
) -> Result<Vec<(ProjectId, Option<(String, String)>)>, ForgeError> {
    const QUERY: &str = "portal.latest_deployments";
    let ids: Vec<i64> = projects
        .iter()
        .take(MAX_STATUS_ITEMS)
        .map(|p| p.get())
        .collect();
    let rows: Vec<(i64, Option<String>, Option<String>)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT p.id, x.status, x.tier FROM unnest($1::bigint[]) WITH ORDINALITY AS p(id, n) \
                 LEFT JOIN LATERAL (SELECT d.status, d.tier FROM forge.deployments d \
                   WHERE d.project_id = p.id ORDER BY d.id DESC LIMIT 1) x ON true \
                 ORDER BY p.n",
            )
            .bind(&ids)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(|(id, status, tier)| (ProjectId::new(id), status.zip(tier)))
        .collect())
}

// ------------------------------------------------------------ widget design P2a

/// The most a `merge-queue` / `spec-status` count reads (`n+` at it).
pub const MAX_STATUS_COUNT: i64 = 100;

/// One merge queue of the `merge-queue` widget (counts at most
/// [`MAX_STATUS_COUNT`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueStatus {
    pub project: ProjectId,
    /// `refs/heads/…`.
    pub target_branch: String,
    /// The emergency stop is on.
    pub paused: bool,
    /// Entries waiting for a batch (`queued`).
    pub queued: i64,
    /// Entries in the batch under test (`testing`).
    pub testing: i64,
    /// Open merge requests whose newest entry of this queue was removed by
    /// the queue itself (no `removed_by`) — as `my-reviews?kind=queue`
    /// shows them: until queued again, closed or merged.
    pub ejected: i64,
}

/// The merge queues of `projects` (candidates: the caller has decided
/// them), in the projects' order, `main` first then by branch, at most
/// [`MAX_STATUS_ITEMS`] (a project without a queue has no row). One
/// statement; each count reads at most [`MAX_STATUS_COUNT`] rows — the
/// live entries through `merge_queue_entries_live`, the ejected ones from
/// the project's open merge requests (`merge_requests_open_project`) and
/// each one's newest entry (`merge_queue_entries_merge_request`), never the
/// queue's whole history.
pub async fn merge_queue_status(
    db: &Db,
    projects: &[ProjectId],
) -> Result<Vec<QueueStatus>, ForgeError> {
    const QUERY: &str = "portal.merge_queue_status";
    let ids: Vec<i64> = projects
        .iter()
        .take(MAX_STATUS_ITEMS)
        .map(|p| p.get())
        .collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<(i64, String, bool, i64, i64, i64)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT p.id, q.target_branch, q.paused, \
                   (SELECT count(*) FROM (SELECT 1 FROM forge.merge_queue_entries e \
                      WHERE e.queue_id = q.id AND e.state = 'queued' LIMIT $2) x) AS queued, \
                   (SELECT count(*) FROM (SELECT 1 FROM forge.merge_queue_entries e \
                      WHERE e.queue_id = q.id AND e.state = 'testing' LIMIT $2) x) AS testing, \
                   (SELECT count(*) FROM (SELECT 1 FROM forge.merge_requests m \
                      CROSS JOIN LATERAL (SELECT e.queue_id, e.state, e.removed_by \
                        FROM forge.merge_queue_entries e WHERE e.merge_request_id = m.id \
                        ORDER BY e.id DESC LIMIT 1) l \
                      WHERE m.project_id = q.project_id AND m.state = 'open' \
                        AND l.queue_id = q.id AND l.state = 'removed' AND l.removed_by IS NULL \
                      LIMIT $2) x) AS ejected \
                 FROM unnest($1::bigint[]) WITH ORDINALITY AS p(id, n) \
                 JOIN forge.merge_queues q ON q.project_id = p.id \
                 ORDER BY p.n, (q.target_branch <> 'refs/heads/main'), q.target_branch \
                 LIMIT $3",
            )
            .bind(&ids)
            .bind(MAX_STATUS_COUNT)
            .bind(i64::try_from(MAX_STATUS_ITEMS).unwrap_or(i64::MAX))
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(
            |(id, target_branch, paused, queued, testing, ejected)| QueueStatus {
                project: ProjectId::new(id),
                target_branch,
                paused,
                queued,
                testing,
                ejected,
            },
        )
        .collect())
}

/// Those of `projects` that released a specification (a `spec.released`
/// in their audit trail, `audit_log_spec_releases`; ADR-0023: a project
/// has a specification by its content, not by a kind), in their order.
pub async fn spec_projects(db: &Db, projects: &[ProjectId]) -> Result<Vec<ProjectId>, ForgeError> {
    const QUERY: &str = "portal.spec_projects";
    let ids: Vec<i64> = projects.iter().map(|p| p.get()).collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<i64> = db
        .within(QUERY, async {
            sqlx::query_scalar(
                "SELECT p.id FROM unnest($1::bigint[]) WITH ORDINALITY AS p(id, n) \
                 WHERE EXISTS (SELECT 1 FROM forge.audit_log al \
                   WHERE al.project_id = p.id AND al.action = 'spec.released') \
                 ORDER BY p.n",
            )
            .bind(&ids)
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(rows.into_iter().map(ProjectId::new).collect())
}

/// One specification project of the `spec-status` widget: facts of its
/// open, non-draft merge requests (the changes awaiting approval) at their
/// current heads. Counts at most [`MAX_STATUS_COUNT`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecStatus {
    pub project: ProjectId,
    /// Open, non-draft merge requests with a head.
    pub awaiting: i64,
    /// Distinct places (path, line) of `forge/doc-lint` annotations of the
    /// undecided rule ([`forge_core::portal::UNDECIDED_RULE`]) on those
    /// heads.
    pub undecided: i64,
    /// A `forge/doc-lint` run on those heads holds the most annotations a
    /// run keeps: more undecided items may exist than were counted.
    pub undecided_partial: bool,
    /// Of those changes, the ones whose `forge/spec-lint` or
    /// `forge/doc-lint` run on the head concluded `failure`.
    pub failing: i64,
    /// Of those changes, the ones whose head lacks a completed run of
    /// either check, or whose run answered `error` (never "no finding").
    pub unchecked: i64,
}

/// The facts of `projects` (candidates: the caller has decided them), in
/// their order, at most [`MAX_STATUS_ITEMS`]. One statement; the open
/// merge requests through `merge_requests_open_project`, their runs by
/// `check_runs_one_per_name` (project, head, name) and the annotations by
/// their primary key.
pub async fn spec_status(db: &Db, projects: &[ProjectId]) -> Result<Vec<SpecStatus>, ForgeError> {
    use forge_core::checks::{DOC_LINT_CHECK, MAX_CHECK_ANNOTATIONS, SPEC_LINT_CHECK};
    const QUERY: &str = "portal.spec_status";
    let ids: Vec<i64> = projects
        .iter()
        .take(MAX_STATUS_ITEMS)
        .map(|p| p.get())
        .collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let checks = [SPEC_LINT_CHECK, DOC_LINT_CHECK];
    let rows: Vec<(i64, i64, i64, bool, i64, i64)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "WITH opened AS ( \
                   SELECT p.id AS project_id, p.n, m.head_sha FROM \
                     unnest($1::bigint[]) WITH ORDINALITY AS p(id, n) \
                   CROSS JOIN LATERAL (SELECT m.head_sha FROM forge.merge_requests m \
                     WHERE m.project_id = p.id AND m.state = 'open' AND NOT m.draft \
                       AND m.head_sha IS NOT NULL ORDER BY m.id DESC LIMIT $2) m) \
                 SELECT p.id, \
                   (SELECT count(*) FROM opened o WHERE o.project_id = p.id) AS awaiting, \
                   (SELECT count(*) FROM (SELECT DISTINCT a.path, a.start_line FROM opened o \
                      JOIN forge.check_runs r ON r.project_id = o.project_id \
                        AND r.head_sha = o.head_sha AND r.name = $4 \
                      JOIN forge.check_annotations a ON a.check_run_id = r.id \
                      WHERE o.project_id = p.id \
                        AND a.message LIKE a.path || ':' || a.start_line || ' ' || $5 || '%' \
                      LIMIT $2) u) AS undecided, \
                   EXISTS (SELECT 1 FROM opened o JOIN forge.check_runs r \
                       ON r.project_id = o.project_id AND r.head_sha = o.head_sha AND r.name = $4 \
                     WHERE o.project_id = p.id AND (SELECT count(*) FROM forge.check_annotations a \
                       WHERE a.check_run_id = r.id) >= $6) AS undecided_partial, \
                   (SELECT count(*) FROM opened o WHERE o.project_id = p.id AND EXISTS ( \
                      SELECT 1 FROM forge.check_runs r WHERE r.project_id = o.project_id \
                        AND r.head_sha = o.head_sha AND r.name = ANY($3) \
                        AND r.conclusion = 'failure')) AS failing, \
                   (SELECT count(*) FROM opened o WHERE o.project_id = p.id AND ( \
                      SELECT count(*) FROM forge.check_runs r WHERE r.project_id = o.project_id \
                        AND r.head_sha = o.head_sha AND r.name = ANY($3) \
                        AND r.status = 'completed' AND r.conclusion <> 'error') \
                      < cardinality($3)) AS unchecked \
                 FROM unnest($1::bigint[]) WITH ORDINALITY AS p(id, n) \
                 ORDER BY p.n",
            )
            .bind(&ids)
            .bind(MAX_STATUS_COUNT)
            .bind(&checks[..])
            .bind(DOC_LINT_CHECK)
            .bind(forge_core::portal::UNDECIDED_RULE)
            .bind(i64::try_from(MAX_CHECK_ANNOTATIONS).unwrap_or(i64::MAX))
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(
            |(id, awaiting, undecided, undecided_partial, failing, unchecked)| SpecStatus {
                project: ProjectId::new(id),
                awaiting,
                undecided,
                undecided_partial,
                failing,
                unchecked,
            },
        )
        .collect())
}

#[cfg(test)]
#[path = "portal_tests.rs"]
mod tests;
