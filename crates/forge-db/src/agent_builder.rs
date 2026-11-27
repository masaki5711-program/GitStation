//! The agent builder's reads and records (plan 16 A7, ADR-0032 Amendment
//! A7): the published versions a build selects with their files' contents,
//! the stable catalog the recommendation picks from, and
//! `forge.agent_builds` (derived: a builder run of a project — its
//! selection, the lock's digest, a dry run or the merge request it opened;
//! deleted with the project by the purge).
//!
//! Plan 16 A9 (ADR-0032 Amendment A9): the update job
//! [`AGENT_UPDATE_JOB`] (its payload, [`agent_update_job`],
//! [`agent_update_payload`]), the projects whose finished builds selected
//! a part version (`projects_using_on`, the revocation's fan-out inside
//! its transaction), the person an update is opened for after a
//! revocation ([`builders`]) and the update merge request's event
//! ([`record_agent_update_opened`], `merge_request.agent_update_opened`:
//! its author is told).
//!
//! The parts' rows are only read here (they are frozen at publication,
//! A3 / A5). What a build reads is verified again by forge-agent's
//! `compile` (digest and signature), so a row changed in the database is
//! refused there, not trusted here.

use std::time::Duration;

use forge_core::{
    AgentUpdateOrigin, AuditDetails, DedupKey as EventDedupKey, DedupPart, EventType,
    MergeRequestChange, MergeRequestId, ProjectId, RepoId, RequestId, UserId,
};
use forge_errors::{ErrorCode, ForgeError};
use sqlx::postgres::PgConnection;

use crate::agent_marketplace::{version_sql, StoredVersion};
use crate::change::{
    AuditEntry, ChangeContext, ChangeRecord, ChangeTx, Committed, DomainEvent, Outcome,
};
use crate::jobs::{DedupKey, Enqueued, JobKind, JobTimeout, MaxAttempts, NewJob};
use crate::{query_error, read_query_error, Db};

/// The job that opens an agent configuration's update merge request
/// (`forge serve`, next to the browse cache its branch write invalidates).
pub const AGENT_UPDATE_JOB: &str = "agent.update_merge_request";
/// The bound of one update: the plan (60 s), the bot's commit (60 s), the
/// merge request and the records (a few short transactions).
pub const AGENT_UPDATE_TIMEOUT: Duration = Duration::from_secs(300);

/// The most bytes of file contents one build reads (all parts together).
pub const MAX_BUILD_BYTES: i64 = 8 * 1024 * 1024;
/// The most versions one read names.
pub const MAX_SELECTED: usize = 200;
/// The most stable versions the recommendation reads.
pub const MAX_CATALOG: i64 = 5000;

/// The versions `(slug, version)` named, published or revoked (the caller
/// refuses a revoked one); missing ones are absent from the answer.
pub async fn versions(
    db: &Db,
    wanted: &[(String, String)],
) -> Result<Vec<StoredVersion>, ForgeError> {
    const QUERY: &str = "agent_builder.versions";
    if wanted.len() > MAX_SELECTED {
        return Err(ForgeError::new(ErrorCode::SelectionUnresolved)
            .with_context("reason", "too_many_parts")
            .with_context("count", wanted.len()));
    }
    let slugs: Vec<&str> = wanted.iter().map(|(s, _)| s.as_str()).collect();
    let versions: Vec<&str> = wanted.iter().map(|(_, v)| v.as_str()).collect();
    db.within(QUERY, async {
        sqlx::query_as(&version_sql(
            "(p.slug, v.version) IN (SELECT * FROM unnest($1::text[], $2::text[])) \
             ORDER BY p.slug, v.id",
        ))
        .bind(&slugs)
        .bind(&versions)
        .fetch_all(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}

/// The files `(version_id, path, content)` of `version_ids`, by version
/// and path (byte order). More than [`MAX_BUILD_BYTES`] in all is
/// `FG-AGENT-0008 reason=too_large` (nothing read).
pub async fn contents(
    db: &Db,
    version_ids: &[i64],
) -> Result<Vec<(i64, String, Vec<u8>)>, ForgeError> {
    const QUERY: &str = "agent_builder.contents";
    db.within(QUERY, async {
        let total: Option<i64> = sqlx::query_scalar(
            "SELECT sum(octet_length(content))::bigint FROM forge.agent_part_files \
             WHERE version_id = ANY($1)",
        )
        .bind(version_ids)
        .fetch_one(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))?;
        if total.unwrap_or(0) > MAX_BUILD_BYTES {
            return Err(ForgeError::new(ErrorCode::SelectionUnresolved)
                .with_context("reason", "too_large")
                .with_context("bytes", total.unwrap_or(0)));
        }
        sqlx::query_as(
            "SELECT version_id, path, content FROM forge.agent_part_files \
             WHERE version_id = ANY($1) ORDER BY version_id, path COLLATE \"C\"",
        )
        .bind(version_ids)
        .fetch_all(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}

/// Every published (not revoked) stable version, by slug then id, at most
/// [`MAX_CATALOG`] (the recommendation's catalog).
pub async fn stable_versions(db: &Db) -> Result<Vec<StoredVersion>, ForgeError> {
    const QUERY: &str = "agent_builder.stable_versions";
    db.within(QUERY, async {
        sqlx::query_as(&version_sql(
            "v.state = 'published' AND v.channel = 'stable' AND p.deprecated_at IS NULL \
             ORDER BY p.slug, v.id LIMIT $1",
        ))
        .bind(MAX_CATALOG)
        .fetch_all(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}

/// A build to record.
#[derive(Debug, Clone)]
pub struct NewBuild<'a> {
    pub project: ProjectId,
    pub requested_by: UserId,
    pub adapter_version_id: Option<i64>,
    /// `[{"slug","version","selected_by","reason"}]`.
    pub selection: &'a serde_json::Value,
    pub dry_run: bool,
    pub request_id: &'a str,
    /// `build` (A7), `update` or `revocation` (A9).
    pub origin: &'static str,
}

/// How a build ended.
#[derive(Debug, Clone)]
pub enum BuildOutcome<'a> {
    Succeeded {
        lock_digest: &'a str,
        merge_request: Option<MergeRequestId>,
    },
    Failed {
        error_code: &'a str,
    },
}

/// Record `build` as it ended (one row; the table is derived, no audit).
pub async fn record_build(
    db: &Db,
    build: &NewBuild<'_>,
    outcome: &BuildOutcome<'_>,
) -> Result<i64, ForgeError> {
    const QUERY: &str = "agent_builder.record_build";
    let (state, lock_digest, mr, error_code) = match outcome {
        BuildOutcome::Succeeded {
            lock_digest,
            merge_request,
        } => (
            "succeeded",
            Some(*lock_digest),
            merge_request.map(|m| m.get()),
            None,
        ),
        BuildOutcome::Failed { error_code } => ("failed", None, None, Some(*error_code)),
    };
    db.within(QUERY, async {
        sqlx::query_scalar(
            "INSERT INTO forge.agent_builds (project_id, requested_by, adapter_version_id, \
             selection, lock_digest, dry_run, mr_id, state, error_code, request_id, origin) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) RETURNING id",
        )
        .bind(build.project.get())
        .bind(build.requested_by.get())
        .bind(build.adapter_version_id)
        .bind(build.selection)
        .bind(lock_digest)
        .bind(build.dry_run)
        .bind(mr)
        .bind(state)
        .bind(error_code)
        .bind(build.request_id)
        .bind(build.origin)
        .fetch_one(db.write_pool())
        .await
        .map_err(|e| query_error(QUERY, e))
    })
    .await
}

/// One recorded build.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct StoredBuild {
    pub id: i64,
    pub requested_by: i64,
    pub selection: serde_json::Value,
    pub lock_digest: Option<String>,
    pub dry_run: bool,
    pub mr_iid: Option<i64>,
    pub state: String,
    pub error_code: Option<String>,
    pub request_id: String,
    /// `build`, `update` or `revocation`.
    pub origin: String,
    /// UTC, RFC 3339.
    pub created_at: String,
}

/// The project's newest builds, newest first, at most `limit` (1..=50).
pub async fn builds(
    db: &Db,
    project: ProjectId,
    limit: i64,
) -> Result<Vec<StoredBuild>, ForgeError> {
    const QUERY: &str = "agent_builder.builds";
    db.within(QUERY, async {
        sqlx::query_as(
            "SELECT b.id, b.requested_by, b.selection, b.lock_digest, b.dry_run, \
             m.iid AS mr_iid, b.state, b.error_code, b.request_id, b.origin, \
             to_char(b.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created_at \
             FROM forge.agent_builds AS b LEFT JOIN forge.merge_requests AS m ON m.id = b.mr_id \
             WHERE b.project_id = $1 ORDER BY b.id DESC LIMIT $2",
        )
        .bind(project.get())
        .bind(limit.clamp(1, 50))
        .fetch_all(db.pool().executor())
        .await
        .map_err(|e| read_query_error(QUERY, e))
    })
    .await
}

/// What one [`AGENT_UPDATE_JOB`] asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentUpdateRequest {
    pub project: ProjectId,
    pub origin: AgentUpdateOrigin,
    /// The person who asked (origin `update`); `None` for a revocation
    /// (the job opens it for the project's newest builder).
    pub requested_by: Option<UserId>,
    /// The base model to move to (the generation migration); `None`: the
    /// lock's own.
    pub adapter: Option<String>,
    /// The revoked `(slug, version)` (origin `revocation`).
    pub revoked: Option<(String, String)>,
}

fn job_internal(reason: &'static str) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("kind", AGENT_UPDATE_JOB)
}

/// A part slug or version as the job carries it (`[0-9A-Za-z.-]`, 1..=64).
fn part_word(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= 64
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
}

/// The job for `request`, deduplicated by `dedup` (the caller's key: a
/// revocation's `agent_update_revoked:<version_id>:<project_id>`, a
/// person's `agent_update:<project_id>:<user_id>:<lock digest>`).
pub fn agent_update_job(
    request: &AgentUpdateRequest,
    dedup: &str,
    origin_request_id: RequestId,
) -> Result<NewJob, ForgeError> {
    let mut payload = serde_json::json!({
        "project_id": request.project.get(),
        "origin": request.origin.as_str(),
    });
    if let Some(user) = request.requested_by {
        payload["requested_by"] = serde_json::Value::from(user.get());
    }
    if let Some(adapter) = &request.adapter {
        payload["adapter"] = serde_json::Value::from(adapter.as_str());
    }
    if let Some((slug, version)) = &request.revoked {
        payload["revoked"] = serde_json::json!({"slug": slug, "version": version});
    }
    Ok(NewJob {
        kind: JobKind::parse(AGENT_UPDATE_JOB).ok_or_else(|| job_internal("invalid_job_kind"))?,
        payload,
        max_attempts: MaxAttempts::DEFAULT,
        timeout: JobTimeout::new(AGENT_UPDATE_TIMEOUT)
            .ok_or_else(|| job_internal("job_timeout"))?,
        dedup_key: Some(DedupKey::parse(dedup).ok_or_else(|| job_internal("dedup_key"))?),
        origin_request_id,
        origin_event_seq: None,
    })
}

/// The request of a claimed job (`FG-SYS-0001 reason=invalid_job_payload`
/// for anything else: forge wrote it).
pub fn agent_update_payload(payload: &serde_json::Value) -> Result<AgentUpdateRequest, ForgeError> {
    let bad = || job_internal("invalid_job_payload");
    let project = payload
        .get("project_id")
        .and_then(serde_json::Value::as_i64)
        .filter(|id| *id > 0)
        .ok_or_else(bad)?;
    let origin = payload
        .get("origin")
        .and_then(serde_json::Value::as_str)
        .and_then(AgentUpdateOrigin::parse)
        .ok_or_else(bad)?;
    let requested_by = match payload.get("requested_by") {
        None => None,
        Some(v) => Some(UserId::new(
            v.as_i64().filter(|id| *id > 0).ok_or_else(bad)?,
        )),
    };
    let adapter = match payload.get("adapter") {
        None => None,
        Some(v) => Some(
            v.as_str()
                .filter(|a| part_word(a))
                .ok_or_else(bad)?
                .to_owned(),
        ),
    };
    let revoked = match payload.get("revoked") {
        None => None,
        Some(v) => {
            let word = |k: &str| {
                v.get(k)
                    .and_then(serde_json::Value::as_str)
                    .filter(|w| part_word(w))
                    .map(str::to_owned)
                    .ok_or_else(bad)
            };
            Some((word("slug")?, word("version")?))
        }
    };
    // Each origin carries exactly its own facts.
    let fits = match origin {
        AgentUpdateOrigin::Update => requested_by.is_some() && revoked.is_none(),
        AgentUpdateOrigin::Revocation => requested_by.is_none() && revoked.is_some(),
    };
    if !fits {
        return Err(bad());
    }
    Ok(AgentUpdateRequest {
        project: ProjectId::new(project),
        origin,
        requested_by,
        adapter,
        revoked,
    })
}

/// Enqueue a person's update (outside any change: the route's 202).
pub async fn enqueue_update(db: &Db, job: &NewJob) -> Result<Enqueued, ForgeError> {
    crate::jobs::enqueue(db, job).await
}

/// The projects whose finished (not Dry-Run) builds selected `slug`
/// `version`, by id (the revocation's fan-out, on its transaction's
/// connection; the job confirms each against the lock at the tip).
pub(crate) async fn projects_using_on(
    conn: &mut PgConnection,
    slug: &str,
    version: &str,
) -> Result<Vec<ProjectId>, ForgeError> {
    const QUERY: &str = "agent_builder.projects_using";
    let wanted = serde_json::json!([{ "slug": slug, "version": version }]);
    // A project being deleted (or deleted) gets no update.
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT DISTINCT b.project_id FROM forge.agent_builds AS b \
         JOIN forge.projects AS p ON p.id = b.project_id \
         WHERE b.state = 'succeeded' AND NOT b.dry_run AND b.selection @> $1 \
         AND p.deleted_at IS NULL ORDER BY b.project_id",
    )
    .bind(&wanted)
    .fetch_all(conn)
    .await
    .map_err(|e| query_error(QUERY, e))?;
    Ok(ids.into_iter().map(ProjectId::new).collect())
}

/// The most projects [`projects_with_revoked_builds`] answers.
pub const MAX_REVOKED_CANDIDATES: i64 = 1000;

/// The projects (with their ready repository) whose finished (not
/// Dry-Run) builds selected a version that is revoked now, by project id,
/// at most `limit` (clamped to 1..=[`MAX_REVOKED_CANDIDATES`]). The
/// candidates of `forge doctor`'s `agent.revoked_in_use` (plan 16 §8):
/// the caller confirms each against the lock at the default branch's tip.
/// A lock written by hand (no build recorded) is not among them — its
/// `forge/agent-lock` check fails instead (ADR-0032 Amendment A9 5).
pub async fn projects_with_revoked_builds(
    db: &Db,
    limit: i64,
) -> Result<Vec<(ProjectId, RepoId)>, ForgeError> {
    const QUERY: &str = "agent_builder.projects_with_revoked_builds";
    let rows: Vec<(i64, i64)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT DISTINCT b.project_id, r.id FROM forge.agent_builds AS b                  JOIN forge.projects AS p ON p.id = b.project_id                  JOIN forge.repositories AS r ON r.project_id = p.id                  WHERE b.state = 'succeeded' AND NOT b.dry_run                  AND p.deleted_at IS NULL AND r.state = 'ready'                  AND EXISTS (SELECT 1 FROM forge.agent_part_versions AS v                    JOIN forge.agent_parts AS ap ON ap.id = v.part_id                    WHERE v.state = 'revoked' AND b.selection @>                    jsonb_build_array(jsonb_build_object('slug', ap.slug, 'version', v.version)))                  ORDER BY b.project_id LIMIT $1",
            )
            .bind(limit.clamp(1, MAX_REVOKED_CANDIDATES))
            .fetch_all(db.pool().executor())
            .await
            .map_err(|e| read_query_error(QUERY, e))
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(|(p, r)| (ProjectId::new(p), RepoId::new(r)))
        .collect())
}

/// The people who asked for the project's finished builds, the newest
/// build's first, each once, at most `limit` (an update after a revocation
/// is opened for the first of them who may).
pub async fn builders(db: &Db, project: ProjectId, limit: i64) -> Result<Vec<UserId>, ForgeError> {
    const QUERY: &str = "agent_builder.builders";
    let ids: Vec<i64> = db
        .within(QUERY, async {
            sqlx::query_scalar(
                "SELECT requested_by FROM forge.agent_builds \
                 WHERE project_id = $1 AND state = 'succeeded' AND NOT dry_run \
                 GROUP BY requested_by ORDER BY max(id) DESC LIMIT $2",
            )
            .bind(project.get())
            .bind(limit.clamp(1, 50))
            .fetch_all(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    Ok(ids.into_iter().map(UserId::new).collect())
}

/// Record an update's finished build once per merge request (`false`: a
/// row of that merge request is there already — the job ran again).
pub async fn record_build_once(
    db: &Db,
    build: &NewBuild<'_>,
    lock_digest: &str,
    merge_request: MergeRequestId,
) -> Result<bool, ForgeError> {
    const QUERY: &str = "agent_builder.record_build_once";
    let id: Option<i64> = db
        .within(QUERY, async {
            sqlx::query_scalar(
                "INSERT INTO forge.agent_builds (project_id, requested_by, adapter_version_id, \
                 selection, lock_digest, dry_run, mr_id, state, error_code, request_id, origin) \
                 SELECT $1, $2, $3, $4, $5, false, $6, 'succeeded', NULL, $7, $8 \
                 WHERE NOT EXISTS (SELECT 1 FROM forge.agent_builds WHERE mr_id = $6) \
                 RETURNING id",
            )
            .bind(build.project.get())
            .bind(build.requested_by.get())
            .bind(build.adapter_version_id)
            .bind(build.selection)
            .bind(lock_digest)
            .bind(merge_request.get())
            .bind(build.request_id)
            .bind(build.origin)
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    Ok(id.is_some())
}

/// The `merge_request.agent_update_opened` audit row and event (a
/// [`MergeRequestChange::AgentUpdateOpened`]) on `project`; `false` when
/// it was recorded already (dedup by the merge request).
pub async fn record_agent_update_opened(
    db: &Db,
    ctx: &ChangeContext,
    project: ProjectId,
    change: MergeRequestChange,
) -> Result<bool, ForgeError> {
    let MergeRequestChange::AgentUpdateOpened { merge_request, .. } = &change else {
        return Err(
            ForgeError::new(ErrorCode::Internal).with_context("reason", "not_an_agent_update")
        );
    };
    let dedup = EventDedupKey::new(change.dedup_prefix(), &[DedupPart::Id(merge_request.get())])?;
    let aggregate: MergeRequestId = *merge_request;
    let details = AuditDetails::MergeRequest(change);
    let event_type: EventType = details.event_type().ok_or_else(|| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "audit_only_event")
    })?;
    let value = details.to_value();
    let record = ChangeRecord {
        audit: AuditEntry {
            action: details.action(),
            outcome: Outcome::Succeeded,
            resource_type: "project",
            resource_id: Some(project.get()),
            project_id: Some(project),
            error_code: None,
            details: value.clone(), // clone: the same JSON is the payload
        },
        affected: Vec::new(),
        events: vec![DomainEvent {
            event_type,
            schema_version: 1,
            aggregate_type: "merge_request",
            aggregate_id: aggregate.get(),
            project_id: Some(project),
            payload: value,
            dedup_key: Some(dedup),
        }],
    };
    let tx = ChangeTx::begin(db, ctx).await?;
    Ok(match tx.commit(record).await? {
        Committed::Recorded { .. } => true,
        Committed::Deduped => false,
    })
}

#[cfg(test)]
mod update_payload_tests {
    use super::*;

    #[test]
    fn the_update_payload_round_trips_and_refuses_mixed_origins() {
        let asked = AgentUpdateRequest {
            project: ProjectId::new(3),
            origin: AgentUpdateOrigin::Revocation,
            requested_by: None,
            adapter: None,
            revoked: Some(("format-on-save".to_owned(), "1.0.0".to_owned())),
        };
        let rid = RequestId::parse("req-a9").expect("rid");
        let job = agent_update_job(&asked, "agent_update_revoked:1:3", rid).expect("job");
        assert_eq!(agent_update_payload(&job.payload).expect("payload"), asked);
        let person = AgentUpdateRequest {
            project: ProjectId::new(3),
            origin: AgentUpdateOrigin::Update,
            requested_by: Some(UserId::new(9)),
            adapter: Some("claude-code-v2".to_owned()),
            revoked: None,
        };
        let rid = RequestId::parse("req-b").expect("rid");
        let job = agent_update_job(&person, "agent_update:3:x", rid).expect("job");
        assert_eq!(agent_update_payload(&job.payload).expect("payload"), person);
        for bad in [
            serde_json::json!({"project_id": 3, "origin": "update"}),
            serde_json::json!({"project_id": 3, "origin": "revocation", "requested_by": 9,
                "revoked": {"slug": "a", "version": "1"}}),
            serde_json::json!({"project_id": 3, "origin": "revocation",
                "revoked": {"slug": "../a", "version": "1"}}),
            serde_json::json!({"project_id": 0, "origin": "update", "requested_by": 9}),
            serde_json::json!({"project_id": 3, "origin": "other", "requested_by": 9}),
        ] {
            assert!(agent_update_payload(&bad).is_err(), "{bad}");
        }
    }
}
