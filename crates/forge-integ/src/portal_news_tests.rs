#![allow(clippy::expect_used, clippy::panic)]
//! The `portal.news_send` job (ADR-0036) against a migrated database with a
//! stand-in portal and token source: 202 is `sent`, a refusal is retried
//! (one failed attempt, `FG-INT-0022`), the last attempt and a payload the
//! portal refuses for good are `failed` (`FG-INT-0023`), 401 forgets the
//! token, nobody left to tell is `dropped`, and the payload is the
//! contract's (§7.1) with staff ids only. Synthetic data only.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use forge_core::portal::{NewsTopic, ServiceToken, ServiceTokenFuture, ServiceTokenSource};
use forge_core::{ProjectId, RequestId, UserId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::jobs::{ClaimedJob, JobId, JobKind, JobTimeout, Lease, WorkerId};
use forge_db::portal::{stage_alone, NewsDraft, NewsPolicy};
use forge_db::projects::{self, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_errors::ErrorCode;
use serde_json::{json, Value};

use super::*;

/// Answers with the statuses given, in order (the last one repeats), and
/// keeps what it was sent.
struct Portal {
    answers: Mutex<Vec<Posted>>,
    sent: Mutex<Vec<(String, String, Value)>>,
}

impl Portal {
    fn new(answers: &[Posted]) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.iter().rev().copied().collect()),
            sent: Mutex::new(Vec::new()),
        })
    }
}

impl NewsTransport for Portal {
    fn post<'a>(
        &'a self,
        url: &'a str,
        token: &'a str,
        body: Vec<u8>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Posted> + Send + 'a>> {
        let json: Value = serde_json::from_slice(&body).expect("json payload");
        self.sent
            .lock()
            .expect("sent")
            .push((url.to_owned(), token.to_owned(), json));
        let mut answers = self.answers.lock().expect("answers");
        let answer = if answers.len() > 1 {
            answers.pop().expect("answer")
        } else {
            *answers.last().expect("answer")
        };
        Box::pin(async move { answer })
    }
}

#[derive(Default)]
struct Tokens {
    forgotten: Mutex<usize>,
    broken: bool,
}

impl ServiceTokenSource for Tokens {
    fn service_token(&self) -> ServiceTokenFuture<'_> {
        let broken = self.broken;
        Box::pin(async move {
            if broken {
                Err(ForgeError::new(ErrorCode::PortalServiceTokenUnavailable)
                    .with_context("reason", "client_rejected"))
            } else {
                Ok(ServiceToken::new("synthetic-service-token".to_owned()))
            }
        })
    }

    fn forget(&self) {
        *self.forgotten.lock().expect("forgotten") += 1;
    }
}

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(Actor::System, RequestId::parse(request).expect("id"))
}

struct World {
    t: FreshDb,
    db: Db,
    olga: UserId,
    project: ProjectId,
}

impl World {
    async fn new() -> Self {
        let t = FreshDb::migrated().await;
        let mut new = NewUser::human("olga", "Olga Synthetic", "olga@example.test");
        new.employee_number = Some("SYN0001".to_owned());
        let olga = users::create(&t.db, &system("req-olga"), &new)
            .await
            .expect("olga");
        let project = projects::create(
            &t.db,
            &system("req-app"),
            &NewProject::new("app", olga.namespace_id, "App", olga.user_id),
        )
        .await
        .expect("project")
        .project_id;
        let db = t.db.clone().with_portal_news(Some(
            NewsPolicy::new(3, Duration::from_secs(5)).expect("policy"),
        ));
        Self {
            t,
            db,
            olga: olga.user_id,
            project,
        }
    }

    async fn stage(&self, audience: Vec<UserId>) -> i64 {
        stage_alone(
            &self.db,
            &NewsDraft {
                source_id: format!("gitstation-automerge-{}-abc", audience.len()),
                topic: NewsTopic::AutoMergeStopped,
                project: self.project,
                facts: json!({ "iid": 12, "class": "program_breakage" }),
                audience,
                request_id: RequestId::parse("req-news").expect("id"),
            },
        )
        .await
        .expect("stage")
        .expect("row")
    }

    fn sender(&self, portal: Arc<Portal>, tokens: Arc<Tokens>) -> NewsSender {
        NewsSender::new(&self.db, "http://127.0.0.1:4100/", tokens, portal).expect("sender")
    }

    async fn row(&self, id: i64) -> Value {
        self.t
            .json_rows(&format!(
                "SELECT state, reason, attempts, last_status, last_error_code \
                 FROM forge.portal_news WHERE id = {id}"
            ))
            .await
            .remove(0)
    }
}

fn job(news: i64, attempt: i32) -> ClaimedJob {
    ClaimedJob {
        lease: Lease {
            job_id: JobId::new(1),
            worker_id: WorkerId::generate(),
            attempt,
        },
        kind: JobKind::parse(NEWS_JOB).expect("kind"),
        payload: json!({ "news_id": news }),
        max_attempts: 3,
        timeout: JobTimeout::new(Duration::from_secs(20)).expect("timeout"),
        origin_request_id: "req-news".to_owned(),
        origin_event_seq: None,
    }
}

#[tokio::test]
async fn an_accepted_news_item_is_sent_with_the_contracts_payload() {
    let w = World::new().await;
    let id = w.stage(vec![w.olga]).await;
    let portal = Portal::new(&[Posted::Answered(202)]);
    let sender = w.sender(Arc::clone(&portal), Arc::default());
    sender.run(job(id, 1)).await.expect("sent");
    assert_eq!(w.row(id).await["state"], "sent");
    let sent = portal.sent.lock().expect("sent").clone(); // clone: no guard across the next await
    assert_eq!(sent.len(), 1);
    let (url, token, body) = &sent[0];
    assert_eq!(url, "http://127.0.0.1:4100/api/v1/news");
    assert_eq!(token, "synthetic-service-token");
    assert_eq!(body["source_id"], "gitstation-automerge-1-abc");
    assert_eq!(body["kind"], "alert");
    assert_eq!(body["severity"], "warn");
    assert_eq!(body["audience"], json!({ "staff_ids": ["SYN0001"] }));
    assert_eq!(body["href"], "/olga/app/-/pull/12");
    for lang in ["ja", "en", "id"] {
        assert!(body["title"][lang]
            .as_str()
            .is_some_and(|t| t.contains("olga/app")));
        assert!(body["body"][lang].as_str().is_some());
    }
    assert!(body["occurred_at"]
        .as_str()
        .is_some_and(|t| t.ends_with('Z')));
    assert!(body["expires_at"].as_str().is_some());
    // No personal data beyond the staff id.
    let text = body.to_string();
    assert!(!text.contains("olga@example.test") && !text.contains("Olga Synthetic"));
    // A second run of a sent item sends nothing.
    sender.run(job(id, 2)).await.expect("nothing to do");
    assert_eq!(portal.sent.lock().expect("sent").len(), 1);
}

#[tokio::test]
async fn a_refused_news_post_is_retried() {
    let w = World::new().await;
    let id = w.stage(vec![w.olga]).await;
    let portal = Portal::new(&[
        Posted::Answered(503),
        Posted::Failed("timeout"),
        Posted::Answered(202),
    ]);
    let tokens = Arc::new(Tokens::default());
    let sender = w.sender(Arc::clone(&portal), Arc::clone(&tokens));
    let failure = sender.run(job(id, 1)).await.expect_err("retried");
    assert_eq!(failure.error.code(), ErrorCode::PortalNewsAttemptFailed);
    assert!(matches!(failure.retry, forge_db::jobs::Retry::Backoff));
    let row = w.row(id).await;
    assert_eq!(row["state"], "pending");
    assert_eq!(row["attempts"], 1);
    assert_eq!(row["last_status"], 503);
    assert_eq!(row["last_error_code"], "FG-INT-0022");
    let again = sender.run(job(id, 2)).await.expect_err("retried");
    assert!(
        again.error.to_string().contains("reason=timeout"),
        "{}",
        again.error
    );
    sender.run(job(id, 3)).await.expect("sent at last");
    assert_eq!(w.row(id).await["state"], "sent");
    // A 401 drops the cached token before the retry.
    let other = w.stage(vec![w.olga, w.olga]).await;
    let refusing = Portal::new(&[Posted::Answered(401)]);
    let sender = w.sender(refusing, Arc::clone(&tokens));
    sender.run(job(other, 1)).await.expect_err("retried");
    assert_eq!(*tokens.forgotten.lock().expect("forgotten"), 1);
    // No token: an attempt that failed, retried too.
    let broken = Arc::new(Tokens {
        broken: true,
        ..Tokens::default()
    });
    let sender = w.sender(Portal::new(&[Posted::Answered(202)]), broken);
    let failure = sender.run(job(other, 2)).await.expect_err("retried");
    assert!(failure.error.to_string().contains("reason=service_token"));
}

#[tokio::test]
async fn news_gives_up_after_the_last_attempt() {
    let w = World::new().await;
    let id = w.stage(vec![w.olga]).await;
    let sender = w.sender(Portal::new(&[Posted::Answered(500)]), Arc::default());
    sender
        .run(job(id, 3))
        .await
        .expect("given up, not a dead job");
    let row = w.row(id).await;
    assert_eq!(row["state"], "failed");
    assert_eq!(row["reason"], "tries_used_up");
    assert_eq!(row["last_error_code"], "FG-INT-0023");
    // A payload the portal refuses for good ends at once.
    let other = w.stage(vec![w.olga, w.olga]).await;
    let sender = w.sender(Portal::new(&[Posted::Answered(422)]), Arc::default());
    sender.run(job(other, 1)).await.expect("refused for good");
    let row = w.row(other).await;
    assert_eq!(
        (row["state"].clone(), row["reason"].clone()),
        (json!("failed"), json!("refused"))
    );
    assert_eq!(row["last_status"], 422);
    let h = forge_db::portal::health(&w.db).await.expect("health");
    assert_eq!(h.failed_24h, 2);
}

#[tokio::test]
async fn nobody_left_to_tell_is_dropped_without_a_post() {
    let w = World::new().await;
    // dave has no employee number: the portal cannot address him.
    let dave = users::create(
        &w.t.db,
        &system("req-dave"),
        &NewUser::human("dave", "dave", "dave@example.test"),
    )
    .await
    .expect("dave")
    .user_id;
    let id = w.stage(vec![dave]).await;
    let portal = Portal::new(&[Posted::Answered(202)]);
    let sender = w.sender(Arc::clone(&portal), Arc::default());
    sender.run(job(id, 1)).await.expect("dropped");
    let row = w.row(id).await;
    assert_eq!(
        (row["state"].clone(), row["reason"].clone()),
        (json!("dropped"), json!("no_audience"))
    );
    assert!(portal.sent.lock().expect("sent").is_empty());
}
