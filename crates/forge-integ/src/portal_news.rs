//! The `portal.news_send` job (ADR-0036; the PFS portal contract v1 §7.1):
//! one run sends one news item of `forge.portal_news` to the portal.
//!
//! 1. The row is read; one that is gone or no longer `pending` is left
//!    alone (a repeated run after a lost answer sends nothing twice: the
//!    portal is idempotent on `source_id` anyway).
//! 2. The audience is decided again, now: every person who is active, a
//!    person (not a service account or a bot), has an employee number and
//!    may read the project — `ProjectRead` for them as a session without
//!    administrator rights, the decision the web UI shows the project by.
//!    Nobody left → `dropped` (`no_audience`), nothing is sent.
//! 3. The payload: `source_id`, `kind`, `severity`, `title` and `body` in
//!    ja / en / id, the app-relative `href`, `audience.staff_ids`,
//!    `occurred_at`, `expires_at` — never a name, an address or a body of
//!    GitStation's own (only the project path, a number, an environment, a
//!    version, a short SHA).
//! 4. `POST {portal.registry_url}/api/v1/news` with the gitstation client's
//!    client-credentials token ([`forge_core::portal::ServiceTokenSource`]):
//!    no proxy, no redirect, only `portal.ca_file`'s roots,
//!    `portal.timeout_ms` in all.
//!    - 202 → `sent`.
//!    - 400 / 404 / 409 / 413 / 422 → `failed` (`refused`): the payload will
//!      not become acceptable by waiting; `FG-INT-0023` (alert).
//!    - anything else (401 / 403 also forget the cached token), no answer,
//!      no token → one failed attempt, `FG-INT-0022` (info), retried with the
//!      queue's backoff; the last run (`portal.max_attempts`) is `failed`
//!      (`tries_used_up`, `FG-INT-0023`).
//!
//! A failed news item is never a dead job (that would be a second alert):
//! its row carries the result. Only the database failing is returned as a
//! retryable job failure without a row update.

use std::sync::Arc;
use std::time::Duration;

use forge_auth::{prove, Action, Credential, LoadedProject, Principal, ProjectCtx};
use forge_core::portal::{
    lang_tag, news_body, news_href, news_title, NewsFacts, ServiceTokenSource, LANGS,
};
use forge_core::{telemetry, UserKind};
use forge_db::jobs::{ClaimedJob, JobFailure, JobFuture, JobHandler, JobKind};
use forge_db::portal::{self as db_portal, StoredNews, NEWS_JOB};
use forge_db::{resolve, Db};
use forge_errors::{ErrorCode, ForgeError};
use serde_json::{json, Map, Value};

/// The portal's news intake below `portal.registry_url`.
pub const NEWS_PATH: &str = "/api/v1/news";

/// How one POST ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Posted {
    /// The portal answered this status.
    Answered(u16),
    /// No answer: `connect`, `connect_timeout`, `timeout`, `tls`,
    /// `redirect`, `request`.
    Failed(&'static str),
}

/// Sends a news payload to the portal (tests use a stand-in).
pub trait NewsTransport: Send + Sync {
    fn post<'a>(
        &'a self,
        url: &'a str,
        token: &'a str,
        body: Vec<u8>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Posted> + Send + 'a>>;
}

/// The production transport: rustls with `portal.ca_file`'s roots only.
pub struct HttpNewsTransport {
    client: reqwest::Client,
}

impl std::fmt::Debug for HttpNewsTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpNewsTransport").finish_non_exhaustive()
    }
}

impl HttpNewsTransport {
    /// `ca_pem`: `portal.ca_file`'s content (`None`: no root is trusted, an
    /// https registry fails TLS; the configuration requires it for https).
    pub fn new(ca_pem: Option<&[u8]>, timeout: Duration) -> Result<Self, ForgeError> {
        let invalid = |reason: &'static str| {
            ForgeError::new(ErrorCode::ConfigInvalid)
                .with_context("config_key", "portal.ca_file")
                .with_context("reason", reason)
        };
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(timeout.min(Duration::from_secs(3)))
            .timeout(timeout)
            .tls_built_in_root_certs(false)
            .user_agent(concat!("gitstation/", env!("CARGO_PKG_VERSION")));
        if let Some(pem) = ca_pem {
            let certs = reqwest::Certificate::from_pem_bundle(pem)
                .map_err(|e| invalid("invalid_pem").with_source(e))?;
            if certs.is_empty() {
                return Err(invalid("no_certificate"));
            }
            for cert in certs {
                builder = builder.add_root_certificate(cert);
            }
        }
        let client = builder.build().map_err(|e| {
            ForgeError::new(ErrorCode::Internal)
                .with_context("reason", "http_client")
                .with_source(e)
        })?;
        Ok(Self { client })
    }
}

/// A transport error as the reason it means (never its text).
fn classify(e: &reqwest::Error) -> &'static str {
    if e.is_connect() && e.is_timeout() {
        "connect_timeout"
    } else if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connect"
    } else if e.is_redirect() {
        "redirect"
    } else {
        "request"
    }
}

impl NewsTransport for HttpNewsTransport {
    fn post<'a>(
        &'a self,
        url: &'a str,
        token: &'a str,
        body: Vec<u8>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Posted> + Send + 'a>> {
        Box::pin(async move {
            let sent = self
                .client
                .post(url)
                .bearer_auth(token)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await;
            match sent {
                Ok(response) => {
                    let status = response.status().as_u16();
                    if (300..400).contains(&status) {
                        Posted::Failed("redirect")
                    } else {
                        Posted::Answered(status)
                    }
                }
                Err(e) => Posted::Failed(classify(&e)),
            }
        })
    }
}

/// The handler of `portal.news_send`.
pub struct NewsSender {
    kind: JobKind,
    inner: Arc<Inner>,
}

struct Inner {
    db: Db,
    url: String,
    tokens: Arc<dyn ServiceTokenSource>,
    transport: Arc<dyn NewsTransport>,
}

impl NewsSender {
    /// `registry_url`: `portal.registry_url` (an origin).
    pub fn new(
        db: &Db,
        registry_url: &str,
        tokens: Arc<dyn ServiceTokenSource>,
        transport: Arc<dyn NewsTransport>,
    ) -> Result<Self, ForgeError> {
        let kind = JobKind::parse(NEWS_JOB).ok_or_else(|| {
            ForgeError::new(ErrorCode::Internal).with_context("reason", "invalid_job_kind")
        })?;
        Ok(Self {
            kind,
            inner: Arc::new(Inner {
                db: db.clone(), // clone: a pool handle
                url: format!("{}{NEWS_PATH}", registry_url.trim_end_matches('/')),
                tokens,
                transport,
            }),
        })
    }
}

impl JobHandler for NewsSender {
    fn kind(&self) -> JobKind {
        self.kind.clone() // clone: the runner keys its map by an owned kind
    }

    fn run(&self, job: ClaimedJob) -> JobFuture {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move { inner.run(job).await })
    }
}

/// Statuses that will not become acceptable by waiting.
fn refused_for_good(status: u16) -> bool {
    matches!(status, 400 | 404 | 409 | 413 | 422)
}

/// The facts of a row as the words need them.
fn facts_of(news: &StoredNews, project_path: String) -> NewsFacts {
    let text = |key: &str| {
        news.facts
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    NewsFacts {
        project_path,
        iid: news.facts.get("iid").and_then(Value::as_i64),
        tier: text("tier"),
        version: text("version"),
        sha_short: text("sha"),
        status: text("status"),
    }
}

/// The contract's payload (§7.1) of `news` for `staff_ids`.
pub fn payload(news: &StoredNews, facts: &NewsFacts, staff_ids: &[String]) -> Value {
    let words = |f: fn(forge_core::portal::NewsTopic, &NewsFacts, forge_errors::Lang) -> String| {
        let mut map = Map::new();
        for lang in LANGS {
            map.insert(lang_tag(lang).to_owned(), json!(f(news.topic, facts, lang)));
        }
        Value::Object(map)
    };
    json!({
        "source_id": news.source_id,
        "kind": news.topic.kind(),
        "severity": news.topic.severity().as_str(),
        "title": words(news_title),
        "body": words(news_body),
        "href": news_href(news.topic, facts),
        "audience": { "staff_ids": staff_ids },
        "occurred_at": news.occurred_at,
        "expires_at": news.expires_at,
    })
}

/// One failed attempt's error (`FG-INT-0022`).
fn attempt_error(news: &StoredNews, (attempt, max): (i32, i32), why: &Why) -> ForgeError {
    let mut e = ForgeError::new(ErrorCode::PortalNewsAttemptFailed)
        .with_context("event", "forge.portal.news_attempt_failed")
        .with_context("news_id", news.id)
        .with_context("source_id", news.source_id.clone()) // clone: the context owns it
        .with_context("attempt", attempt)
        .with_context("max_attempts", max)
        .with_context("reason", why.reason);
    if let Some(status) = why.status {
        e = e.with_context("status", status);
    }
    e
}

/// A database failure of a run as the job's failure: retried — but on the
/// job's last attempt it ends the job at once (`Retry::Never`), because
/// nothing would run it again. The row may then still be `pending` (the
/// database that failed is the one that records the end); the worker's
/// hourly sweep ends it (`forge_db::portal::sweep_orphaned`, `FG-INT-0023`).
fn db_failure(last_attempt: bool) -> impl Fn(ForgeError) -> JobFailure {
    move |e| {
        if last_attempt {
            JobFailure::permanent(e)
        } else {
            JobFailure::retry(e)
        }
    }
}

/// Why an attempt did not end with 202.
struct Why {
    reason: &'static str,
    status: Option<u16>,
}

impl Inner {
    async fn run(&self, job: ClaimedJob) -> Result<(), JobFailure> {
        let request_id = job.request_id();
        let attempt = job.lease.attempt;
        let last_attempt = attempt >= job.max_attempts;
        let db_failed = db_failure(last_attempt);
        let id = job
            .payload
            .get("news_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| {
                JobFailure::permanent(
                    ForgeError::new(ErrorCode::Internal)
                        .with_context("reason", "invalid_job_payload")
                        .with_context("job", NEWS_JOB),
                )
            })?;
        let Some(news) = db_portal::news(&self.db, id).await.map_err(&db_failed)? else {
            return Ok(());
        };
        if news.state != "pending" {
            return Ok(());
        }
        let (staff_ids, path) = self.audience(&news).await.map_err(&db_failed)?;
        let Some(path) = path.filter(|_| !staff_ids.is_empty()) else {
            db_portal::record_final(&self.db, id, "dropped", "no_audience", attempt, None, None)
                .await
                .map_err(&db_failed)?;
            tracing::info!(
                target: "forge_integ",
                event = "forge.portal.news_dropped",
                request_id = %request_id,
                news_id = id,
                reason = "no_audience",
                "a portal news item had nobody left to tell"
            );
            return Ok(());
        };
        let facts = facts_of(&news, path);
        let body = payload(&news, &facts, &staff_ids).to_string().into_bytes();
        let why = match self.tokens.service_token().await {
            Err(e) => {
                telemetry::log_failure(&e, &request_id);
                Why {
                    reason: "service_token",
                    status: None,
                }
            }
            Ok(token) => match self.transport.post(&self.url, token.expose(), body).await {
                Posted::Answered(202) => {
                    db_portal::record_sent(&self.db, id, attempt)
                        .await
                        .map_err(&db_failed)?;
                    tracing::info!(
                        target: "forge_integ",
                        event = "forge.portal.news_sent",
                        request_id = %request_id,
                        news_id = id,
                        attempt,
                        "a portal news item was accepted"
                    );
                    return Ok(());
                }
                Posted::Answered(status) if refused_for_good(status) => {
                    return self
                        .give_up(&news, attempt, "refused", Some(status), &request_id)
                        .await
                        .map_err(&db_failed);
                }
                Posted::Answered(status) => {
                    if matches!(status, 401 | 403) {
                        self.tokens.forget();
                    }
                    Why {
                        reason: "status",
                        status: Some(status),
                    }
                }
                Posted::Failed(reason) => Why {
                    reason,
                    status: None,
                },
            },
        };
        if last_attempt {
            let last = attempt_error(&news, (attempt, job.max_attempts), &why);
            telemetry::log_failure(&last, &request_id);
            return self
                .give_up(&news, attempt, "tries_used_up", why.status, &request_id)
                .await
                .map_err(&db_failed);
        }
        db_portal::record_attempt(
            &self.db,
            id,
            attempt,
            why.status,
            ErrorCode::PortalNewsAttemptFailed,
        )
        .await
        .map_err(&db_failed)?;
        // The job runner logs it once, with the event's request id.
        Err(JobFailure::retry(attempt_error(
            &news,
            (attempt, job.max_attempts),
            &why,
        )))
    }

    /// `failed` with `reason`, logged once as `FG-INT-0023` (the job ends).
    /// `Err`: the database did not record it (nothing is logged here; the
    /// caller decides whether the job runs again).
    async fn give_up(
        &self,
        news: &StoredNews,
        attempt: i32,
        reason: &'static str,
        status: Option<u16>,
        request_id: &forge_core::RequestId,
    ) -> Result<(), ForgeError> {
        db_portal::record_final(
            &self.db,
            news.id,
            "failed",
            reason,
            attempt,
            status,
            Some(ErrorCode::PortalNewsUndeliverable),
        )
        .await?;
        let mut e = ForgeError::new(ErrorCode::PortalNewsUndeliverable)
            .with_context("event", "forge.portal.news_failed")
            .with_context("news_id", news.id)
            .with_context("source_id", news.source_id.clone()) // clone: the context owns it
            .with_context("reason", reason)
            .with_context("attempts", attempt);
        if let Some(status) = status {
            e = e.with_context("status", status);
        }
        telemetry::log_failure(&e, request_id);
        Ok(())
    }

    /// The staff ids of the people who may still be told, and the
    /// project's path (`None`: nobody could read it, or it is gone).
    async fn audience(
        &self,
        news: &StoredNews,
    ) -> Result<(Vec<String>, Option<String>), ForgeError> {
        let members = db_portal::audience(&self.db, &news.audience).await?;
        let mut staff_ids = Vec::new();
        let mut path = None;
        for m in members {
            if !m.active || m.kind != UserKind::Human {
                continue;
            }
            let Some(staff_id) = m.staff_id else {
                continue;
            };
            let Some(record) = resolve::load_project(&self.db, news.project, Some(m.user)).await?
            else {
                continue;
            };
            let person = Principal::authenticated(m.user, false, true, Credential::Session, m.kind);
            let ctx = ProjectCtx::new(LoadedProject {
                id: record.id,
                visibility: record.access.visibility,
                role: record.access.role,
                pending_deletion: record.access.pending_deletion,
            });
            if prove(&person, Action::ProjectRead, ctx).is_err() {
                continue;
            }
            path.get_or_insert_with(|| record.path.full_path());
            if !staff_ids.contains(&staff_id) {
                staff_ids.push(staff_id);
            }
        }
        Ok((staff_ids, path))
    }
}

#[cfg(test)]
#[path = "portal_news_tests.rs"]
mod tests;
