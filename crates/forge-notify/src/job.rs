//! The `notification.send` job (ADR-0016 Amendment N1): one run delivers one
//! notification by e-mail.
//!
//! 1. [`begin_send`](forge_db::notify::begin_send) takes the send. A
//!    notification already in a final state is left alone; one still
//!    `sending` means a previous run died or timed out mid-send, so its
//!    outcome is unknown and it is **never sent again** (`unknown`, logged).
//! 2. The recipient is decided again ([`crate::access::decide`]): a person
//!    who switched mail off, was disabled or can no longer see the project
//!    gets nothing — `skipped` / `suppressed`, with the reason.
//! 3. The mail is built from what the recipient may see ([`crate::templates`])
//!    in their language and handed to the [`Sender`].
//! 4. The outcome is recorded before the job ends:
//!    - delivered → `sent`;
//!    - not handed over, the relay may accept later (connect, TLS, login,
//!      timeout, 4xx) → `retrying` and the job is retried with the queue's
//!      backoff (10 s, doubling, at most `notify.max_attempts` runs); the
//!      last run is `failed` (`tries_used_up`);
//!    - refused for good (5xx) → `failed`, no retry;
//!    - handed over but the answer is missing → `unknown`, no retry.
//!
//!    Every failure is logged once with `FG-INT-0015` (warn), `request_id`
//!    of the event that caused it, the stage and the relay's reply code —
//!    never the address, the message or the relay's text. A failed send is
//!    never a dead job (that would be an alert): the notification's own
//!    state carries the result.
//!
//! The database failing is the one thing returned as a retryable job
//! failure; if it fails after a successful hand-over the next run finds the
//! notification `sending` and records it `unknown` instead of mailing twice.

use std::sync::Arc;
use std::time::SystemTime;

use forge_core::config::NotifyConfig;
use forge_core::telemetry;
use forge_db::jobs::{ClaimedJob, JobFailure, JobFuture, JobHandler, JobKind};
use forge_db::notify::{self, Begin, Finish, StoredNotification, SEND_JOB};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError, Lang};

use crate::access::{decide, Decision};
use crate::mail::Mail;
use crate::smtp::{SendFailure, SendOutcome, Sender};
use crate::templates::{render, Facts};

/// The handler of `notification.send`.
pub struct SendHandler {
    kind: JobKind,
    inner: Arc<Inner>,
}

struct Inner {
    db: Db,
    sender: Arc<dyn Sender>,
    from_address: String,
    from_name: String,
    default_language: Lang,
    /// `web.base_url`'s origin, for the link in the mail.
    origin: Option<String>,
}

impl SendHandler {
    /// `origin`: `web.base_url`'s origin (no link in the mail without it).
    pub fn new(
        db: &Db,
        sender: Arc<dyn Sender>,
        cfg: &NotifyConfig,
        origin: Option<&str>,
    ) -> Result<Self, ForgeError> {
        let smtp = cfg.smtp.as_ref().ok_or_else(|| {
            ForgeError::new(ErrorCode::ConfigInvalid)
                .with_context("config_key", "notify.smtp_host")
                .with_context("reason", "missing")
        })?;
        let kind = JobKind::parse(SEND_JOB).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "invalid_job_kind")
                .with_context("kind", SEND_JOB)
        })?;
        Ok(Self {
            kind,
            inner: Arc::new(Inner {
                db: db.clone(), // clone: a pool handle
                sender,
                from_address: smtp.from_address.clone(), // clone: the handler owns its settings
                from_name: smtp.from_name.clone(),       // clone: as above
                default_language: cfg.default_language,
                origin: origin.map(str::to_owned),
            }),
        })
    }
}

impl JobHandler for SendHandler {
    fn kind(&self) -> JobKind {
        self.kind.clone() // clone: the runner keys its map by an owned kind
    }

    fn run(&self, job: ClaimedJob) -> JobFuture {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move { inner.run(job).await })
    }
}

fn failure_log(
    failure: SendFailure,
    notification: i64,
    (attempt, max): (i32, i32),
    reason: Option<&'static str>,
) -> ForgeError {
    let error = failure.error(notification, attempt, max);
    match reason {
        // `tries_used_up`: the last run's own reason is kept as well.
        Some(reason) => ForgeError::new(ErrorCode::SmtpUnavailable)
            .with_context("reason", reason)
            .with_context("last_reason", failure.reason.as_str())
            .with_context("stage", failure.stage.as_str())
            .with_context("notification_id", notification)
            .with_context("attempt", attempt)
            .with_context("max_attempts", max)
            .with_context("smtp_code", failure.smtp_code.map_or(0, u16::from)),
        None => error,
    }
}

impl Inner {
    async fn run(&self, job: ClaimedJob) -> Result<(), JobFailure> {
        let id = notify::send_payload(&job.payload).map_err(JobFailure::permanent)?;
        let attempt = job.lease.attempt;
        let request_id = job.request_id();
        let began = notify::begin_send(&self.db, id, attempt)
            .await
            .map_err(JobFailure::retry)?;
        let stored = match began {
            Begin::Send(stored) => stored,
            Begin::Settled(state) => {
                tracing::debug!(
                    target: "forge_notify",
                    event = "forge.notify.already_settled",
                    request_id = %request_id,
                    notification_id = id,
                    state = state.as_str(),
                    "a send job found its notification settled; nothing to do"
                );
                return Ok(());
            }
            Begin::Interrupted => {
                // The previous run may or may not have handed the message
                // over: it is not sent again.
                let err = ForgeError::new(ErrorCode::SmtpUnavailable)
                    .with_context("reason", "outcome_unknown")
                    .with_context("stage", "interrupted")
                    .with_context("notification_id", id)
                    .with_context("attempt", attempt)
                    .with_context("max_attempts", job.max_attempts);
                telemetry::log_failure(&err, &request_id);
                return Ok(());
            }
            Begin::Missing => {
                return Err(JobFailure::permanent(
                    ForgeError::new(ErrorCode::Internal)
                        .with_context("reason", "notification_missing")
                        .with_context("notification_id", id),
                ));
            }
        };
        self.deliver(&stored, &job, &request_id).await
    }

    async fn deliver(
        &self,
        stored: &StoredNotification,
        job: &ClaimedJob,
        request_id: &forge_core::RequestId,
    ) -> Result<(), JobFailure> {
        let attempt = job.lease.attempt;
        let id = stored.id;
        let decision = decide(
            &self.db,
            stored.user,
            stored.kind,
            stored.project,
            stored.iid,
        )
        .await;
        let (recipient, preferences, view) = match decision {
            Ok(Decision::Send {
                recipient,
                preferences,
                view,
            }) => (recipient, preferences, view),
            Ok(Decision::Skip(reason)) => {
                return self
                    .settle(id, attempt, Finish::Skipped(reason), request_id, reason)
                    .await;
            }
            Ok(Decision::Suppress(reason)) => {
                return self
                    .settle(id, attempt, Finish::Suppressed(reason), request_id, reason)
                    .await;
            }
            Err(e) => {
                // The database failed before anything was sent: try again.
                self.release_for_retry(id, attempt, "database_unavailable")
                    .await;
                return Err(JobFailure::retry(e));
            }
        };
        let Some(to) = recipient.email.as_deref() else {
            // `decide` has just seen an address.
            return Err(JobFailure::permanent(
                ForgeError::new(ErrorCode::Internal)
                    .with_context("reason", "recipient_without_address")
                    .with_context("notification_id", id),
            ));
        };
        let actor_name = match stored.actor {
            Some(actor) => match notify::recipient(&self.db, actor).await {
                Ok(found) => found.map(|r| r.display_name),
                Err(e) => {
                    self.release_for_retry(id, attempt, "database_unavailable")
                        .await;
                    return Err(JobFailure::retry(e));
                }
            },
            None => None,
        };
        let link = self
            .origin
            .as_ref()
            .map(|origin| format!("{origin}/{}/-/pull/{}", view.project_path, stored.iid));
        let (subject, body) = render(&Facts {
            kind: stored.kind,
            lang: preferences.language.unwrap_or(self.default_language),
            project_path: &view.project_path,
            iid: stored.iid,
            title: &view.title,
            actor: actor_name.as_deref(),
            link: link.as_deref(),
        });
        let mail = match Mail::new(
            (&self.from_address, &self.from_name),
            to,
            (&subject, &body),
            id,
            SystemTime::now(),
        ) {
            Ok(mail) => mail,
            Err(e) => {
                // A recipient address `decide` accepted but the mail
                // refuses cannot be fixed by trying again.
                let _ = self
                    .settle(
                        id,
                        attempt,
                        Finish::Suppressed(crate::access::reason::ADDRESS_INVALID),
                        request_id,
                        crate::access::reason::ADDRESS_INVALID,
                    )
                    .await;
                return Err(JobFailure::permanent(e));
            }
        };
        let counts = (attempt, job.max_attempts);
        match self.sender.send(&mail).await {
            SendOutcome::Delivered => {
                tracing::info!(
                    target: "forge_notify",
                    event = "forge.notify.sent",
                    request_id = %request_id,
                    notification_id = id,
                    kind = stored.kind.as_str(),
                    attempt,
                    "a notification was handed to the mail relay"
                );
                self.settle(id, attempt, Finish::Sent, request_id, "sent")
                    .await
            }
            SendOutcome::NotSent(failure) if failure.is_permanent() => {
                telemetry::log_failure(&failure_log(failure, id, counts, None), request_id);
                self.settle(
                    id,
                    attempt,
                    Finish::Failed("rejected_permanent"),
                    request_id,
                    "rejected_permanent",
                )
                .await
            }
            SendOutcome::NotSent(failure) if attempt >= job.max_attempts => {
                telemetry::log_failure(
                    &failure_log(failure, id, counts, Some("tries_used_up")),
                    request_id,
                );
                self.settle(
                    id,
                    attempt,
                    Finish::Failed("tries_used_up"),
                    request_id,
                    "tries_used_up",
                )
                .await
            }
            SendOutcome::NotSent(failure) => {
                // Recorded first; the job runner logs the returned error
                // once, with the same request id.
                let kept = notify::finish(
                    &self.db,
                    id,
                    attempt,
                    Finish::Retry(failure.reason.as_str()),
                )
                .await
                .map_err(JobFailure::retry)?;
                if !kept {
                    log_lost(id, attempt, request_id);
                    return Ok(());
                }
                Err(JobFailure::retry(failure.error(
                    id,
                    attempt,
                    job.max_attempts,
                )))
            }
            SendOutcome::Unknown(failure) => {
                telemetry::log_failure(&failure_log(failure, id, counts, None), request_id);
                self.settle(
                    id,
                    attempt,
                    Finish::Unknown("outcome_unknown"),
                    request_id,
                    "outcome_unknown",
                )
                .await
            }
        }
    }

    /// Record the ending; a failure to record is retried by the queue (the
    /// next run finds `sending` and records `unknown`, never a second mail).
    async fn settle(
        &self,
        id: i64,
        attempt: i32,
        how: Finish,
        request_id: &forge_core::RequestId,
        reason: &'static str,
    ) -> Result<(), JobFailure> {
        let applied = notify::finish(&self.db, id, attempt, how)
            .await
            .map_err(JobFailure::retry)?;
        if !applied {
            log_lost(id, attempt, request_id);
        } else if !matches!(how, Finish::Sent | Finish::Failed(_) | Finish::Unknown(_)) {
            tracing::info!(
                target: "forge_notify",
                event = "forge.notify.not_sent",
                request_id = %request_id,
                notification_id = id,
                reason,
                "a notification was not sent"
            );
        }
        Ok(())
    }

    /// Put a notification that was taken for sending back to `retrying`
    /// when nothing was sent (the database failed in between). A failure
    /// here is only logged: the next run finds `sending` and records it
    /// `unknown` rather than risk a repeat.
    async fn release_for_retry(&self, id: i64, attempt: i32, reason: &'static str) {
        if let Err(e) = notify::finish(&self.db, id, attempt, Finish::Retry(reason)).await {
            tracing::warn!(
                target: "forge_notify",
                event = "forge.notify.release_failed",
                error_code = e.code().id(),
                notification_id = id,
                "a notification could not be put back for another try"
            );
        }
    }
}

fn log_lost(id: i64, attempt: i32, request_id: &forge_core::RequestId) {
    tracing::warn!(
        target: "forge_notify",
        event = "forge.notify.send_lost_ownership",
        request_id = %request_id,
        notification_id = id,
        attempt,
        "a send run found its notification taken over by another run; nothing was changed"
    );
}
