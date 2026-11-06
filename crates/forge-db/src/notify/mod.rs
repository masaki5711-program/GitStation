//! Notifications (M3 N1, ADR-0016 Amendment N1): a person's settings, the
//! delivery records the `notify` subscriber writes, and the state machine
//! the `notification.send` job walks. The SQL lives here (forge-db owns all
//! of it); the decisions — who is told, what the mail says, whether the
//! recipient may still see the project — are forge-notify's, through
//! `forge_auth::authorize`.

use forge_core::ids::{ProjectId, UserId};
use forge_core::{NotificationKind, NotificationState, RequestId, UserKind};
use forge_errors::{ErrorCode, ForgeError, Lang};

use crate::jobs::{enqueue_on, DedupKey, Enqueued, JobKind, JobTimeout, MaxAttempts, NewJob};
use crate::subscribers::{DerivedTx, SourceEvent};
use crate::{query_error, Db};

/// The job kind that delivers one notification (payload: its id).
pub const SEND_JOB: &str = "notification.send";

/// A burst of notes on one merge request is one mail per this many seconds
/// per recipient (the rest are `skipped` with reason [`COALESCED`]).
pub const COMMENT_WINDOW_SECS: i64 = 600;
/// The reason of a notification folded into an earlier mail of its window.
pub const COALESCED: &str = "coalesced";

/// A person's settings (`forge.notification_preferences`). No row is the
/// default: mail on, the instance language, nothing muted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preferences {
    pub email_enabled: bool,
    /// `None`: the instance default language.
    pub language: Option<Lang>,
    /// Sorted, distinct.
    pub muted: Vec<NotificationKind>,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            email_enabled: true,
            language: None,
            muted: Vec::new(),
        }
    }
}

/// `user`'s settings (the defaults when they never saved any).
pub async fn preferences(db: &Db, user: UserId) -> Result<Preferences, ForgeError> {
    const QUERY: &str = "notify.preferences";
    let row: Option<PreferencesRow> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT email_enabled, language, muted_kinds \
                 FROM forge.notification_preferences WHERE user_id = $1",
            )
            .bind(user.get())
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    row.map_or_else(
        || Ok(Preferences::default()),
        |r| parse_preferences(r, user),
    )
}

/// id, user, kind, project, iid, actor, request id, state.
type SendRow = (i64, i64, String, i64, i64, Option<i64>, String, String);

type PreferencesRow = (bool, Option<String>, Vec<String>);

/// A stored value this build cannot read (the CHECKs make it a bug or a
/// newer writer): `FG-SYS-0001`, never a silent default.
fn stored_invalid(reason: &'static str, user: UserId) -> ForgeError {
    ForgeError::new(ErrorCode::Internal)
        .with_context("reason", reason)
        .with_context("user_id", user.get())
}

fn parse_preferences(row: PreferencesRow, user: UserId) -> Result<Preferences, ForgeError> {
    {
        let (email_enabled, language, muted) = row;
        let language = language
            .map(|tag| {
                Lang::parse(&tag).ok_or_else(|| stored_invalid("stored_language_invalid", user))
            })
            .transpose()?;
        let mut kinds = muted
            .iter()
            .map(|k| {
                NotificationKind::parse(k)
                    .ok_or_else(|| stored_invalid("stored_kind_invalid", user))
            })
            .collect::<Result<Vec<_>, _>>()?;
        kinds.sort_unstable();
        kinds.dedup();
        Ok(Preferences {
            email_enabled,
            language,
            muted: kinds,
        })
    }
}

/// Replace `user`'s settings; returns what is stored.
pub async fn set_preferences(
    db: &Db,
    user: UserId,
    prefs: &Preferences,
) -> Result<Preferences, ForgeError> {
    const QUERY: &str = "notify.set_preferences";
    let mut muted: Vec<NotificationKind> = prefs.muted.clone(); // clone: normalised below
    muted.sort_unstable();
    muted.dedup();
    let names: Vec<&str> = muted.iter().map(|k| k.as_str()).collect();
    let row: PreferencesRow = db
        .within(QUERY, async {
            sqlx::query_as(
                "INSERT INTO forge.notification_preferences \
                   (user_id, email_enabled, language, muted_kinds) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (user_id) DO UPDATE SET email_enabled = EXCLUDED.email_enabled, \
                   language = EXCLUDED.language, muted_kinds = EXCLUDED.muted_kinds, \
                   updated_at = now() \
                 RETURNING email_enabled, language, muted_kinds",
            )
            .bind(user.get())
            .bind(prefs.email_enabled)
            .bind(prefs.language.map(Lang::as_str))
            .bind(&names)
            .fetch_one(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    parse_preferences(row, user)
}

/// What the subscriber decided about one recipient of one event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Initial {
    /// Send it: `pending` with a `notification.send` job.
    Queue,
    /// The recipient chose not to receive it (`skipped`, with a reason).
    Skip(&'static str),
    /// It cannot or must not be sent (`suppressed`, with a reason).
    Suppress(&'static str),
}

/// One notification to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewNotification {
    pub user: UserId,
    pub kind: NotificationKind,
    pub project: ProjectId,
    pub iid: i64,
    pub actor: Option<UserId>,
    pub initial: Initial,
}

/// The job that delivers a queued notification.
#[derive(Debug, Clone, Copy)]
pub struct Delivery {
    pub max_attempts: MaxAttempts,
    pub timeout: JobTimeout,
}

/// What [`enqueue`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    /// A new row (and, for [`Initial::Queue`], its job).
    Created(i64),
    /// This event already notified this person: nothing was written.
    Duplicate,
}

/// Record `new` for the event being handled, in the subscriber's own
/// transaction (rolled back with it; a repeated step is [`Recorded::Duplicate`]).
pub async fn enqueue(
    tx: &mut DerivedTx<'_>,
    event: &SourceEvent,
    new: &NewNotification,
    delivery: &Delivery,
) -> Result<Recorded, ForgeError> {
    // Notes come in bursts: one mail per recipient and merge request per
    // window; the others are recorded as `coalesced` (no job).
    let mut initial = new.initial;
    if initial == Initial::Queue && new.kind == NotificationKind::MergeRequestCommented {
        let recent: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM forge.notifications WHERE user_id = $1                AND project_id = $2 AND merge_request_iid = $3 AND kind = $4                AND state IN ('pending', 'sending', 'retrying', 'sent', 'unknown')                AND created_at > now() - make_interval(secs => $5))",
        )
        .bind(new.user.get())
        .bind(new.project.get())
        .bind(new.iid)
        .bind(new.kind.as_str())
        .bind(COMMENT_WINDOW_SECS as f64)
        .fetch_one(&mut *tx.conn())
        .await
        .map_err(|e| query_error("notify.coalesce", e))?;
        if recent {
            initial = Initial::Skip(COALESCED);
        }
    }
    let (state, reason) = match initial {
        Initial::Queue => (NotificationState::Pending, None),
        Initial::Skip(reason) => (NotificationState::Skipped, Some(reason)),
        Initial::Suppress(reason) => (NotificationState::Suppressed, Some(reason)),
    };
    let id: Option<i64> = sqlx::query_scalar(
        "INSERT INTO forge.notifications (user_id, kind, event_id, event_seq, project_id, \
           merge_request_iid, actor_user_id, state, reason, request_id) \
         VALUES ($1, $2, $3::uuid, $4, $5, $6, $7, $8, $9, $10) \
         ON CONFLICT (event_id, user_id) DO NOTHING RETURNING id",
    )
    .bind(new.user.get())
    .bind(new.kind.as_str())
    .bind(event.event_id.to_string())
    .bind(event.seq)
    .bind(new.project.get())
    .bind(new.iid)
    .bind(new.actor.map(UserId::get))
    .bind(state.as_str())
    .bind(reason)
    .bind(&event.request_id)
    .fetch_optional(tx.conn())
    .await
    .map_err(|e| query_error("notify.enqueue", e))?;
    let Some(id) = id else {
        return Ok(Recorded::Duplicate);
    };
    if initial == Initial::Queue {
        let job = send_job(id, delivery, event)?;
        // The row is new, so its key is new: a Deduped here would mean a
        // job for a notification that did not exist.
        match enqueue_on(tx.conn(), &job).await? {
            Enqueued::Queued(_) => {}
            Enqueued::Deduped => {
                return Err(ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "send_job_exists_for_new_notification")
                    .with_context("notification_id", id));
            }
        }
    }
    Ok(Recorded::Created(id))
}

fn send_job(id: i64, delivery: &Delivery, event: &SourceEvent) -> Result<NewJob, ForgeError> {
    let internal = |reason: &'static str| {
        ForgeError::new(ErrorCode::Internal)
            .with_context("reason", reason)
            .with_context("notification_id", id)
    };
    Ok(NewJob {
        kind: JobKind::parse(SEND_JOB).ok_or_else(|| internal("invalid_job_kind"))?,
        payload: serde_json::json!({ "notification_id": id }),
        max_attempts: delivery.max_attempts,
        timeout: delivery.timeout,
        dedup_key: Some(
            DedupKey::parse(&format!("notification:{id}"))
                .ok_or_else(|| internal("invalid_dedup_key"))?,
        ),
        origin_request_id: RequestId::parse(&event.request_id).unwrap_or_else(RequestId::generate),
        origin_event_seq: Some(event.seq),
    })
}

/// The id in a `notification.send` job's payload.
pub fn send_payload(payload: &serde_json::Value) -> Result<i64, ForgeError> {
    payload
        .get("notification_id")
        .and_then(serde_json::Value::as_i64)
        .filter(|id| *id > 0)
        .ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "invalid_job_payload")
                .with_context("kind", SEND_JOB)
        })
}

/// The author of merge request `iid` of `project`, read in the step's
/// transaction (`None`: the merge request is gone).
pub async fn merge_request_author(
    tx: &mut DerivedTx<'_>,
    project: ProjectId,
    iid: i64,
) -> Result<Option<UserId>, ForgeError> {
    let author: Option<i64> = sqlx::query_scalar(
        "SELECT author_id FROM forge.merge_requests WHERE project_id = $1 AND iid = $2",
    )
    .bind(project.get())
    .bind(iid)
    .fetch_optional(tx.conn())
    .await
    .map_err(|e| query_error("notify.merge_request_author", e))?;
    Ok(author.map(UserId::new))
}

/// A notification as the send job sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredNotification {
    pub id: i64,
    pub user: UserId,
    pub kind: NotificationKind,
    pub project: ProjectId,
    pub iid: i64,
    pub actor: Option<UserId>,
    pub request_id: String,
}

/// What [`begin_send`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Begin {
    /// Now `sending`: this run owns the send.
    Send(StoredNotification),
    /// Nothing to do: it is already in this (final) state.
    Settled(NotificationState),
    /// A previous run died while sending: the outcome is unknown, the
    /// notification is now `unknown` and is not sent again.
    Interrupted,
    /// No such notification.
    Missing,
}

/// Take the send of notification `id` for run number `attempt`.
pub async fn begin_send(db: &Db, id: i64, attempt: i32) -> Result<Begin, ForgeError> {
    const QUERY: &str = "notify.begin_send";
    db.within(QUERY, async {
        let mut tx = db
            .write_pool()
            .begin()
            .await
            .map_err(|e| query_error(QUERY, e))?;
        let row: Option<SendRow> = sqlx::query_as(
            "SELECT id, user_id, kind, project_id, merge_request_iid, actor_user_id, \
                        request_id, state \
                 FROM forge.notifications WHERE id = $1 FOR UPDATE",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| query_error(QUERY, e))?;
        let Some((id, user, kind, project, iid, actor, request_id, state)) = row else {
            return Ok(Begin::Missing);
        };
        let unreadable = |reason: &'static str| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", reason)
                .with_context("notification_id", id)
        };
        let state =
            NotificationState::parse(&state).ok_or_else(|| unreadable("stored_state_invalid"))?;
        let begin = match state {
            NotificationState::Pending | NotificationState::Retrying => {
                let kind = NotificationKind::parse(&kind)
                    .ok_or_else(|| unreadable("stored_kind_invalid"))?;
                sqlx::query(
                    "UPDATE forge.notifications SET state = 'sending', attempts = $2, \
                       updated_at = now() WHERE id = $1",
                )
                .bind(id)
                .bind(attempt)
                .execute(&mut *tx)
                .await
                .map_err(|e| query_error(QUERY, e))?;
                Begin::Send(StoredNotification {
                    id,
                    user: UserId::new(user),
                    kind,
                    project: ProjectId::new(project),
                    iid,
                    actor: actor.map(UserId::new),
                    request_id,
                })
            }
            NotificationState::Sending => {
                sqlx::query(
                    "UPDATE forge.notifications SET state = 'unknown', reason = 'interrupted', \
                       updated_at = now() WHERE id = $1",
                )
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(|e| query_error(QUERY, e))?;
                Begin::Interrupted
            }
            NotificationState::Sent
            | NotificationState::Failed
            | NotificationState::Unknown
            | NotificationState::Skipped
            | NotificationState::Suppressed => Begin::Settled(state),
        };
        tx.commit().await.map_err(|e| query_error(QUERY, e))?;
        Ok(begin)
    })
    .await
}

/// How a send attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finish {
    Sent,
    /// Not handed over; another try follows.
    Retry(&'static str),
    Failed(&'static str),
    Unknown(&'static str),
    Skipped(&'static str),
    Suppressed(&'static str),
}

/// Settle the run `attempt` of notification `id`. `false`: it no longer
/// owns the send (another run took over), nothing was changed.
pub async fn finish(db: &Db, id: i64, attempt: i32, how: Finish) -> Result<bool, ForgeError> {
    const QUERY: &str = "notify.finish";
    let (state, reason) = match how {
        Finish::Sent => (NotificationState::Sent, None),
        Finish::Retry(reason) => (NotificationState::Retrying, Some(reason)),
        Finish::Failed(reason) => (NotificationState::Failed, Some(reason)),
        Finish::Unknown(reason) => (NotificationState::Unknown, Some(reason)),
        Finish::Skipped(reason) => (NotificationState::Skipped, Some(reason)),
        Finish::Suppressed(reason) => (NotificationState::Suppressed, Some(reason)),
    };
    let done = db
        .within(QUERY, async {
            sqlx::query(
                "UPDATE forge.notifications SET state = $3, reason = $4, updated_at = now(), \
                   sent_at = CASE WHEN $3 = 'sent' THEN now() END \
                 WHERE id = $1 AND attempts = $2 AND state = 'sending'",
            )
            .bind(id)
            .bind(attempt)
            .bind(state.as_str())
            .bind(reason)
            .execute(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    Ok(done.rows_affected() == 1)
}

/// What the mail needs to know about a recipient.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipient {
    pub email: Option<String>,
    pub display_name: String,
    pub active: bool,
    pub kind: UserKind,
    pub is_admin: bool,
}

/// `user`'s account facts (`None`: no such user).
pub async fn recipient(db: &Db, user: UserId) -> Result<Option<Recipient>, ForgeError> {
    const QUERY: &str = "notify.recipient";
    let row: Option<(Option<String>, String, String, String, bool)> = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT email, display_name, state, kind, is_admin FROM forge.users WHERE id = $1",
            )
            .bind(user.get())
            .fetch_optional(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    row.map(|(email, display_name, state, kind, is_admin)| {
        Ok(Recipient {
            email,
            display_name,
            active: state == "active",
            kind: UserKind::parse(&kind)
                .ok_or_else(|| stored_invalid("stored_user_kind_invalid", user))?,
            is_admin,
        })
    })
    .transpose()
}

/// What `forge doctor` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backlog {
    /// `pending` + `retrying` + `sending`.
    pub waiting: i64,
    /// Age in seconds of the oldest waiting one.
    pub oldest_waiting_secs: Option<i64>,
    /// `failed` in the last 24 hours.
    pub failed_24h: i64,
    /// `unknown` in the last 30 days (there is no acknowledgement: after
    /// that it is history).
    pub unknown_30d: i64,
    /// Age in seconds of the oldest of those.
    pub oldest_unknown_secs: Option<i64>,
    /// `suppressed` in the last 24 hours because the address is missing or
    /// unusable (`no_address`, `address_invalid`): data to fix, not a choice.
    pub address_suppressed_24h: i64,
}

/// waiting, oldest waiting, failed 24 h, unknown 30 d, oldest unknown,
/// address suppressions 24 h.
type BacklogRow = (i64, Option<i64>, i64, i64, Option<i64>, i64);

pub async fn backlog(db: &Db) -> Result<Backlog, ForgeError> {
    const QUERY: &str = "notify.backlog";
    let (waiting, oldest, failed, unknown, oldest_unknown, address): BacklogRow = db
        .within(QUERY, async {
            sqlx::query_as(
                "SELECT count(*) FILTER (WHERE state IN ('pending', 'sending', 'retrying')), \
                        floor(extract(epoch FROM now() - min(created_at) \
                          FILTER (WHERE state IN ('pending', 'sending', 'retrying'))))::bigint, \
                        count(*) FILTER (WHERE state = 'failed' \
                          AND updated_at > now() - interval '24 hours'), \
                        count(*) FILTER (WHERE state = 'unknown' \
                          AND updated_at > now() - interval '30 days'), \
                        floor(extract(epoch FROM now() - min(updated_at) \
                          FILTER (WHERE state = 'unknown' \
                            AND updated_at > now() - interval '30 days')))::bigint, \
                        count(*) FILTER (WHERE state = 'suppressed' \
                          AND reason IN ('no_address', 'address_invalid') \
                          AND updated_at > now() - interval '24 hours') \
                 FROM forge.notifications",
            )
            .fetch_one(db.write_pool())
            .await
            .map_err(|e| query_error(QUERY, e))
        })
        .await?;
    Ok(Backlog {
        waiting,
        oldest_waiting_secs: oldest,
        failed_24h: failed,
        unknown_30d: unknown,
        oldest_unknown_secs: oldest_unknown,
        address_suppressed_24h: address,
    })
}

#[cfg(test)]
mod tests;
