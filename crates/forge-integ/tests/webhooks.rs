#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly (rust/testing.md)
//! Webhooks end to end against a migrated database and a local receiver
//! (M3 I2, ADR-0016 §8): the dispatch subscriber, the delivery job over
//! real HTTP, the signature with the secret shown once (and both secrets
//! during a rotation's overlap), retries with backoff, the last attempt
//! (FG-INT-0004), redirects never followed, the target checked again at
//! delivery (a loopback receiver under the production policy), the
//! owner's standing, and what the log may carry of an answer.
//!
//! The receivers listen on 127.0.0.1: a loopback address is always refused
//! by the production policy, so these tests deliver through
//! `TargetPolicy::allowing_loopback_for_tests` (feature `test-support`)
//! except where the refusal itself is tested.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::sync::{Arc, Mutex};

use forge_auth::{prove, Action, Credential, LoadedProject, Principal, ProjectCtx, WebhookCtx};
use forge_core::webhook::{signature, TargetPolicy, WebhookSecretKeys};
use forge_core::{ProjectId, RequestId, Role, UserId, UserKind, WebhookId};
use forge_db::change::{Actor, ChangeContext};
use forge_db::jobs::{JobRunner, WorkerId};
use forge_db::projects::{self, CreatedProject, NewProject};
use forge_db::sequencer::{self, AuditSequencing, BatchSize};
use forge_db::subscribers::{self, Progress, Start};
use forge_db::test_support::*;
use forge_db::users::{self, NewUser};
use forge_db::Db;
use forge_integ::webhooks::{
    DeliverHandler, Transport, WebhookChanges, WebhookDispatch, WebhookInput, WebhookRequest,
    Webhooks,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const KEYS: &str = "1 0000000000000000000000000000000000000000000000000000000000000001\n";

fn system(request: &str) -> ChangeContext {
    ChangeContext::new(
        Actor::System,
        RequestId::parse(request).expect("request id"),
    )
}

fn request(id: &str) -> WebhookRequest {
    WebhookRequest {
        request_id: RequestId::parse(id).expect("request id"),
        credential_id: None,
    }
}

// ── the local receiver ──

/// What one request carried.
#[derive(Debug, Clone)]
struct Received {
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

/// One scripted answer: status, extra header lines, body.
#[derive(Debug, Clone)]
struct Answer {
    status: u16,
    headers: Vec<String>,
    body: String,
}

fn answer(status: u16) -> Answer {
    Answer {
        status,
        headers: Vec::new(),
        body: String::new(),
    }
}

#[derive(Clone)]
struct Receiver {
    port: u16,
    got: Arc<Mutex<Vec<Received>>>,
}

impl Receiver {
    /// Answers `script` in order (the last one again when it runs out).
    async fn start(script: Vec<Answer>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let got: Arc<Mutex<Vec<Received>>> = Arc::default();
        let sink = Arc::clone(&got);
        tokio::spawn(async move {
            let mut n = 0usize;
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let header_end = loop {
                    let Ok(read) = stream.read(&mut chunk).await else {
                        break None;
                    };
                    if read == 0 {
                        break None;
                    }
                    buf.extend_from_slice(&chunk[..read]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(i + 4);
                    }
                };
                let Some(end) = header_end else { continue };
                let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                let mut headers = BTreeMap::new();
                for line in head.lines().skip(1) {
                    if let Some((k, v)) = line.split_once(':') {
                        headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
                    }
                }
                let length: usize = headers
                    .get("content-length")
                    .and_then(|l| l.parse().ok())
                    .unwrap_or(0);
                while buf.len() < end + length {
                    let Ok(read) = stream.read(&mut chunk).await else {
                        break;
                    };
                    if read == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..read]);
                }
                let body = buf[end..(end + length).min(buf.len())].to_vec();
                sink.lock().expect("sink").push(Received { headers, body });
                let a = script
                    .get(n)
                    .or_else(|| script.last())
                    .cloned()
                    .unwrap_or_else(|| answer(204));
                n += 1;
                let mut out = format!(
                    "HTTP/1.1 {} X\r\ncontent-length: {}\r\nconnection: close\r\n",
                    a.status,
                    a.body.len()
                );
                for h in &a.headers {
                    out.push_str(h);
                    out.push_str("\r\n");
                }
                out.push_str("\r\n");
                out.push_str(&a.body);
                let _ = stream.write_all(out.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        Self { port, got }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/hook?token=receiver-secret", self.port)
    }

    fn received(&self) -> Vec<Received> {
        self.got.lock().expect("sink").clone()
    }
}

// ── the world ──

struct World {
    t: FreshDb,
    alice: UserId,
    web: CreatedProject,
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let alice = users::create(
        &t.db,
        &system("req-user-alice"),
        &NewUser::human("alice", "alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let web = projects::create(
        &t.db,
        &system("req-project-web"),
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("web");
    // Every event so far is numbered; the subscriber starts at the head.
    sequence(&t.db).await;
    subscribers::register(&t.db, &WebhookDispatch, Start::Head)
        .await
        .expect("register");
    World {
        t,
        alice: alice.user_id,
        web,
    }
}

fn keys() -> Arc<WebhookSecretKeys> {
    Arc::new(WebhookSecretKeys::parse(KEYS).expect("keys"))
}

/// The policy that lets the local receiver through.
fn test_policy(port: u16) -> TargetPolicy {
    TargetPolicy::parse(&format!("127.0.0.1:{port}"), true)
        .expect("policy")
        .allowing_loopback_for_tests()
}

async fn sequence(db: &Db) {
    sequencer::run_once(db, BatchSize::MAX, AuditSequencing::Skip)
        .await
        .expect("sequence");
}

impl World {
    fn project(&self) -> ProjectId {
        self.web.project_id
    }

    async fn loaded(&self, user: UserId) -> LoadedProject {
        let access = forge_db::resolve::project_access(&self.t.db, self.project(), Some(user))
            .await
            .expect("access")
            .expect("project");
        LoadedProject {
            id: self.project(),
            visibility: access.visibility,
            role: access.role,
            pending_deletion: access.pending_deletion,
        }
    }

    fn person(user: UserId) -> Principal {
        Principal::authenticated(user, false, true, Credential::Session, UserKind::Human)
    }

    async fn create_as(
        &self,
        hooks: &Webhooks,
        user: UserId,
        url: &str,
        types: Option<Vec<String>>,
    ) -> (WebhookId, String) {
        let proof = prove(
            &Self::person(user),
            Action::WebhookManage,
            ProjectCtx::new(self.loaded(user).await),
        )
        .expect("proof");
        let created = hooks
            .create(
                &request("req-hook-create"),
                &proof,
                &WebhookInput {
                    url: url.to_owned(),
                    event_types: types,
                },
            )
            .await
            .expect("create");
        (created.webhook.id, created.secret.expose().to_owned())
    }

    /// Publish one event of the project as alice, with its audit row in the
    /// same transaction (`FreshDb::commit_audited`: the fixture never calls
    /// another feature's decided command).
    async fn publish(&self, event_type: &str, payload: &str) {
        self.t
            .commit_audited(
                ("project", self.project().get()),
                &[format!(
                    "INSERT INTO forge.events (event_id, event_type, schema_version, \
                     aggregate_type, aggregate_id, project_id, actor_kind, actor_user_id, \
                     request_id, payload) VALUES (gen_random_uuid(), '{event_type}', 1, \
                     'project', {p}, {p}, 'user', {a}, 'req-event', '{payload}'::jsonb)",
                    p = self.project().get(),
                    a = self.alice.get(),
                )],
            )
            .await;
    }

    /// A `check.completed` event of the project (integration class).
    async fn check_event(&self, n: u8) {
        let head = format!("{:x}", n % 16).repeat(40);
        self.publish(
            "check.completed",
            &format!(r#"{{"project_id": {}, "head_sha": "{head}", "name": "ci/build", "conclusion": "success"}}"#, self.project().get()),
        )
        .await;
    }

    /// Number the events and let the subscriber take every one.
    async fn dispatch(&self) {
        sequence(&self.t.db).await;
        loop {
            match subscribers::run_once(&self.t.db, &WebhookDispatch)
                .await
                .expect("step")
            {
                Progress::Handled { .. } => {}
                Progress::CaughtUp => return,
                other @ (Progress::Busy
                | Progress::Disabled
                | Progress::Failed(_)
                | Progress::Stuck(_)
                | Progress::Quarantined(_)) => panic!("dispatch: {other:?}"),
            }
        }
    }

    /// Run every ready `webhook.deliver` job once.
    async fn deliver(&self, transport: Transport) -> usize {
        let mut runner = JobRunner::new(self.t.db.clone(), WorkerId::generate());
        runner
            .register(Arc::new(
                DeliverHandler::new(&self.t.db, keys(), transport).expect("handler"),
            ))
            .expect("register");
        let mut ran = 0;
        while runner.run_once().await.expect("run").is_some() {
            ran += 1;
        }
        ran
    }

    /// Make the queued attempts ready now (skip the backoff).
    async fn hurry(&self) {
        self.t
            .exec_superuser("UPDATE forge.jobs SET run_after = now() WHERE state = 'queued'")
            .await;
    }

    async fn rows(&self, sql: &str) -> Vec<serde_json::Value> {
        self.t.json_rows(sql).await
    }
}

fn verify(header: &str, secret: &str, delivery: &str, body: &[u8]) -> bool {
    let mut t = None;
    let mut sigs = Vec::new();
    for part in header.split(',') {
        match part.split_once('=') {
            Some(("t", v)) => t = v.parse::<u64>().ok(),
            Some(("v1", v)) => sigs.push(v.to_owned()),
            _ => return false,
        }
    }
    let Some(t) = t else { return false };
    let expected = signature(secret, t, delivery, body).expect("signature");
    sigs.contains(&expected)
}

// ── the tests ──

#[tokio::test]
async fn an_event_is_delivered_once_signed_with_the_secret_shown_at_creation() {
    let w = world().await;
    let rx = Receiver::start(vec![answer(204)]).await;
    let hooks = Webhooks::new(w.t.db.clone(), test_policy(rx.port), Some(keys()));
    let (hook, secret) = w.create_as(&hooks, w.alice, &rx.url(), None).await;
    w.check_event(1).await;
    w.dispatch().await;
    let transport = Transport::new(test_policy(rx.port), None).expect("transport");
    assert_eq!(w.deliver(transport.clone()).await, 1);
    let got = rx.received();
    assert_eq!(got.len(), 1);
    let r = &got[0];
    assert_eq!(
        r.headers.get("forge-event").map(String::as_str),
        Some("check.completed")
    );
    assert_eq!(
        r.headers.get("forge-webhook-id"),
        Some(&hook.get().to_string())
    );
    assert_eq!(
        r.headers.get("forge-signature-kid").map(String::as_str),
        Some("1")
    );
    let delivery = r
        .headers
        .get("forge-delivery")
        .expect("delivery id")
        .clone();
    let sig = r.headers.get("forge-signature").expect("signature");
    assert!(verify(sig, &secret, &delivery, &r.body), "{sig}");
    assert!(!verify(sig, "whsec_other", &delivery, &r.body));
    let body: serde_json::Value = serde_json::from_slice(&r.body).expect("json body");
    assert_eq!(body["schema_version"], 1);
    assert_eq!(body["delivery_id"], delivery);
    assert_eq!(body["event"]["type"], "check.completed");
    assert!(body["event"]["id"]
        .as_str()
        .is_some_and(|i| i.starts_with("evt_")));
    assert_eq!(body["event"]["project_id"], w.project().get());
    assert_eq!(body["event"]["actor"]["user"]["username"], "alice");
    assert!(
        !r.body.windows(5).any(|w| w == b"email"),
        "no e-mail in the body"
    );
    let rows = w
        .rows(&format!(
            "SELECT state, attempts, last_status, delivery_uid::text AS uid \
             FROM forge.webhook_deliveries WHERE webhook_id = {}",
            hook.get()
        ))
        .await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["state"], "succeeded");
    assert_eq!(rows[0]["attempts"], 1);
    assert_eq!(rows[0]["last_status"], 204);
    assert_eq!(rows[0]["uid"], delivery);
    // Nothing more to run: one delivery per (webhook, event).
    assert_eq!(w.deliver(transport).await, 0);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_failed_attempt_is_retried_with_backoff() {
    let w = world().await;
    let rx = Receiver::start(vec![answer(500), answer(200)]).await;
    let hooks = Webhooks::new(w.t.db.clone(), test_policy(rx.port), Some(keys()));
    let (hook, _) = w.create_as(&hooks, w.alice, &rx.url(), None).await;
    w.check_event(2).await;
    w.dispatch().await;
    let transport = Transport::new(test_policy(rx.port), None).expect("transport");
    let log = Captured::default();
    {
        let _guard = log.install();
        assert_eq!(w.deliver(transport.clone()).await, 1);
    }
    let rows = w
        .rows(&format!(
            "SELECT d.state, d.attempts, d.last_status, d.last_reason, \
             round(extract(epoch FROM j.run_after - now()))::int AS wait \
             FROM forge.webhook_deliveries d JOIN forge.jobs j \
             ON j.dedup_key = 'delivery:' || d.id || ':0:2' WHERE d.webhook_id = {}",
            hook.get()
        ))
        .await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["state"], "pending");
    assert_eq!(rows[0]["attempts"], 1);
    assert_eq!(rows[0]["last_status"], 500);
    assert_eq!(rows[0]["last_reason"], "status");
    let wait = rows[0]["wait"].as_i64().expect("wait");
    assert!((8..=10).contains(&wait), "{wait}");
    let line = log
        .lines()
        .into_iter()
        .find(|l| l["error_code"] == "FG-INT-0020")
        .expect("the attempt's log line");
    let context = line["context"].as_str().expect("context");
    assert!(context.contains("next_attempt=2"), "{context}");
    assert!(context.contains("retry_in_s=10"), "{context}");
    assert!(
        !context.contains("receiver-secret"),
        "never the query: {context}"
    );
    // Not before the backoff …
    assert_eq!(w.deliver(transport.clone()).await, 0);
    // … then it succeeds.
    w.hurry().await;
    assert_eq!(w.deliver(transport).await, 1);
    let rows = w
        .rows(&format!(
            "SELECT state, attempts FROM forge.webhook_deliveries WHERE webhook_id = {}",
            hook.get()
        ))
        .await;
    assert_eq!(rows[0]["state"], "succeeded");
    assert_eq!(rows[0]["attempts"], 2);
    assert_eq!(rx.received().len(), 2);
    // The same delivery id both times (the receiver deduplicates on it).
    let got = rx.received();
    assert_eq!(
        got[0].headers.get("forge-delivery"),
        got[1].headers.get("forge-delivery")
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_delivery_out_of_attempts_is_int_0004() {
    let w = world().await;
    let rx = Receiver::start(vec![Answer {
        status: 503,
        headers: Vec::new(),
        body: format!("upstream token ghp_{} down", "Z".repeat(40)),
    }])
    .await;
    let hooks = Webhooks::new(w.t.db.clone(), test_policy(rx.port), Some(keys()));
    let (hook, _) = w.create_as(&hooks, w.alice, &rx.url(), None).await;
    w.check_event(3).await;
    w.dispatch().await;
    let transport = Transport::new(test_policy(rx.port), None).expect("transport");
    w.deliver(transport.clone()).await;
    // Jump to the last attempt.
    w.t.exec_superuser(&format!(
        "UPDATE forge.jobs SET payload = jsonb_set(payload, '{{attempt}}', '{}'), \
         run_after = now() WHERE state = 'queued' AND kind = 'webhook.deliver'",
        forge_core::webhook::MAX_ATTEMPTS
    ))
    .await;
    let log = Captured::default();
    {
        let _guard = log.install();
        assert_eq!(w.deliver(transport.clone()).await, 1);
    }
    let rows = w
        .rows(&format!(
            "SELECT state, last_status, last_reason, last_error_code \
             FROM forge.webhook_deliveries WHERE webhook_id = {}",
            hook.get()
        ))
        .await;
    assert_eq!(rows[0]["state"], "failed");
    assert_eq!(rows[0]["last_status"], 503);
    assert_eq!(rows[0]["last_error_code"], "FG-INT-0020");
    let line = log
        .lines()
        .into_iter()
        .find(|l| l["error_code"] == "FG-INT-0004")
        .expect("FG-INT-0004");
    assert_eq!(line["level"], "WARN");
    let context = line["context"].as_str().expect("context");
    assert!(context.contains("last_status=503"), "{context}");
    // The answer's head is bounded and redacted; the body is never stored.
    let all = log.text();
    assert!(all.contains("[redacted]"), "{all}");
    assert!(!all.contains(&"Z".repeat(20)), "{all}");
    assert!(!all.contains("receiver-secret"), "{all}");
    // No further attempt.
    w.hurry().await;
    assert_eq!(w.deliver(transport).await, 0);
    w.t.drop_now().await;
}

#[tokio::test]
async fn redirects_are_never_followed() {
    let w = world().await;
    let elsewhere = Receiver::start(vec![answer(204)]).await;
    let rx = Receiver::start(vec![Answer {
        status: 302,
        headers: vec![format!("location: {}", elsewhere.url())],
        body: String::new(),
    }])
    .await;
    let policy = TargetPolicy::parse(
        &format!("127.0.0.1:{}, 127.0.0.1:{}", rx.port, elsewhere.port),
        true,
    )
    .expect("policy")
    .allowing_loopback_for_tests();
    let hooks = Webhooks::new(w.t.db.clone(), policy.clone(), Some(keys()));
    let (hook, _) = w.create_as(&hooks, w.alice, &rx.url(), None).await;
    w.check_event(4).await;
    w.dispatch().await;
    w.deliver(Transport::new(policy, None).expect("transport"))
        .await;
    assert_eq!(rx.received().len(), 1);
    assert_eq!(elsewhere.received().len(), 0, "the redirect was followed");
    let rows = w
        .rows(&format!(
            "SELECT last_status, last_reason FROM forge.webhook_deliveries WHERE webhook_id = {}",
            hook.get()
        ))
        .await;
    assert_eq!(rows[0]["last_status"], 302);
    assert_eq!(rows[0]["last_reason"], "redirect");
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_loopback_receiver_is_refused_at_delivery_by_the_production_policy() {
    let w = world().await;
    let rx = Receiver::start(vec![answer(204)]).await;
    // Stored while the test policy let it through …
    let hooks = Webhooks::new(w.t.db.clone(), test_policy(rx.port), Some(keys()));
    let (hook, _) = w.create_as(&hooks, w.alice, &rx.url(), None).await;
    w.check_event(5).await;
    w.dispatch().await;
    // … delivered under the production policy (the same allowlist).
    let production = TargetPolicy::parse(&format!("127.0.0.1:{}", rx.port), true).expect("policy");
    let log = Captured::default();
    {
        let _guard = log.install();
        w.deliver(Transport::new(production, None).expect("transport"))
            .await;
    }
    assert_eq!(
        rx.received().len(),
        0,
        "nothing reached the loopback receiver"
    );
    let rows = w
        .rows(&format!(
            "SELECT last_reason, last_error_code FROM forge.webhook_deliveries WHERE webhook_id = {}",
            hook.get()
        ))
        .await;
    assert_eq!(rows[0]["last_reason"], "target_forbidden");
    assert_eq!(rows[0]["last_error_code"], "FG-INT-0010");
    let line = log
        .lines()
        .into_iter()
        .find(|l| l["error_code"] == "FG-INT-0010")
        .expect("FG-INT-0010");
    let context = line["context"].as_str().expect("context");
    assert!(context.contains("reason=loopback"), "{context}");
    assert!(!context.contains("receiver-secret"), "{context}");
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_rotation_signs_with_both_secrets_for_the_overlap() {
    let w = world().await;
    let rx = Receiver::start(vec![answer(204)]).await;
    let hooks = Webhooks::new(w.t.db.clone(), test_policy(rx.port), Some(keys()));
    let (hook, old) = w.create_as(&hooks, w.alice, &rx.url(), None).await;
    let proof = prove(
        &World::person(w.alice),
        Action::WebhookManage,
        WebhookCtx::new(w.loaded(w.alice).await, hook),
    )
    .expect("proof");
    let rotated = hooks
        .rotate_secret(&request("req-rotate"), &proof)
        .await
        .expect("rotate");
    assert_eq!(rotated.secret_version, 2);
    let new = rotated.secret.expose().to_owned();
    assert_ne!(new, old);
    w.check_event(6).await;
    w.dispatch().await;
    w.deliver(Transport::new(test_policy(rx.port), None).expect("transport"))
        .await;
    let got = rx.received();
    let r = &got[0];
    let delivery = r.headers.get("forge-delivery").expect("delivery");
    let sig = r.headers.get("forge-signature").expect("signature");
    assert_eq!(sig.matches("v1=").count(), 2, "{sig}");
    assert_eq!(
        r.headers.get("forge-signature-kid").map(String::as_str),
        Some("2,1")
    );
    assert!(verify(sig, &new, delivery, &r.body));
    assert!(verify(sig, &old, delivery, &r.body));
    // After the overlap only the new secret signs.
    w.t.commit_audited(
        ("project", w.project().get()),
        &[format!(
            "UPDATE forge.webhooks SET secret_prev_until = now() - interval '1 second' WHERE id = {}",
            hook.get()
        )],
    )
    .await;
    w.check_event(7).await;
    w.dispatch().await;
    w.deliver(Transport::new(test_policy(rx.port), None).expect("transport"))
        .await;
    let got = rx.received();
    let r = &got[1];
    let delivery = r.headers.get("forge-delivery").expect("delivery");
    let sig = r.headers.get("forge-signature").expect("signature");
    assert_eq!(sig.matches("v1=").count(), 1, "{sig}");
    assert!(verify(sig, &new, delivery, &r.body));
    assert!(!verify(sig, &old, delivery, &r.body));
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_webhook_delivers_only_what_its_owner_may_still_see() {
    let w = world().await;
    let bob = users::create(
        &w.t.db,
        &system("req-user-bob"),
        &NewUser::human("bob", "bob", "bob@example.test"),
    )
    .await
    .expect("bob")
    .user_id;
    projects::add_member(
        &w.t.db,
        &system("req-bob-member"),
        w.project(),
        bob,
        Role::Maintainer,
    )
    .await
    .expect("bob is a Maintainer");
    let rx = Receiver::start(vec![answer(204)]).await;
    let hooks = Webhooks::new(w.t.db.clone(), test_policy(rx.port), Some(keys()));
    let (all, _) = w.create_as(&hooks, bob, &rx.url(), None).await;
    let (named, _) = w
        .create_as(
            &hooks,
            bob,
            &rx.url(),
            Some(vec!["project.visibility_changed".to_owned()]),
        )
        .await;
    // A maintainer-class event: only the webhook that names it.
    w.publish(
        "project.visibility_changed",
        &format!(
            r#"{{"project_id": {}, "from": "private", "to": "internal"}}"#,
            w.project().get()
        ),
    )
    .await;
    w.check_event(8).await;
    w.dispatch().await;
    let count = |hook: WebhookId| {
        format!(
            "SELECT event_type FROM forge.webhook_deliveries WHERE webhook_id = {} ORDER BY id",
            hook.get()
        )
    };
    let types = |rows: Vec<serde_json::Value>| -> Vec<String> {
        rows.into_iter()
            .map(|r| r["event_type"].as_str().unwrap_or_default().to_owned())
            .collect()
    };
    assert_eq!(types(w.rows(&count(all)).await), vec!["check.completed"]);
    assert_eq!(
        types(w.rows(&count(named)).await),
        vec!["project.visibility_changed"]
    );
    // Bob is demoted: nothing new is recorded, and what was pending ends
    // as skipped (owner_lost) at delivery.
    let _demoted = projects::change_member(
        &w.t.db,
        &system("req-bob-demoted"),
        w.project(),
        bob,
        Role::Maintainer,
        Role::Developer,
    )
    .await
    .expect("demote");
    w.check_event(9).await;
    let log = Captured::default();
    {
        let _guard = log.install();
        w.dispatch().await;
        w.deliver(Transport::new(test_policy(rx.port), None).expect("transport"))
            .await;
    }
    assert_eq!(types(w.rows(&count(all)).await), vec!["check.completed"]);
    assert_eq!(rx.received().len(), 0);
    let skipped = w
        .rows("SELECT DISTINCT state, last_reason FROM forge.webhook_deliveries")
        .await;
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    assert_eq!(skipped[0]["state"], "skipped");
    assert_eq!(skipped[0]["last_reason"], "owner_lost");
    assert!(log.lines().iter().any(|l| l["error_code"] == "FG-INT-0010"
        && l["context"]
            .as_str()
            .is_some_and(|c| c.contains("reason=owner_lost"))));
    w.t.drop_now().await;
}

#[tokio::test]
async fn creating_a_webhook_checks_the_target_after_the_decision() {
    let w = world().await;
    let policy = TargetPolicy::parse("kanflow.internal:443", false).expect("policy");
    let hooks = Webhooks::new(w.t.db.clone(), policy, Some(keys()));
    let proof = prove(
        &World::person(w.alice),
        Action::WebhookManage,
        ProjectCtx::new(w.loaded(w.alice).await),
    )
    .expect("proof");
    for (url, reason) in [
        ("https://user:pw@kanflow.internal/hook", "userinfo"),
        ("https://127.0.0.1/hook", "loopback"),
        ("https://169.254.169.254/latest", "link_local"),
        ("http://kanflow.internal/hook", "scheme"),
        ("https://other.internal/hook", "not_allowlisted"),
    ] {
        let e = hooks
            .create(
                &request("req-bad"),
                &proof,
                &WebhookInput {
                    url: url.to_owned(),
                    event_types: None,
                },
            )
            .await
            .expect_err(url);
        assert_reason(&e, forge_errors::ErrorCode::WebhookTargetForbidden, reason);
    }
    // Without secret keys nothing is created (FG-SYS-0002).
    let unkeyed = Webhooks::new(
        w.t.db.clone(),
        TargetPolicy::parse("kanflow.internal:443", false).expect("policy"),
        None,
    );
    let e = unkeyed
        .create(
            &request("req-unkeyed"),
            &proof,
            &WebhookInput {
                url: "https://kanflow.internal/hook".to_owned(),
                event_types: None,
            },
        )
        .await
        .expect_err("no keys");
    assert_reason(&e, forge_errors::ErrorCode::ConfigInvalid, "missing");
    // A good one; the update checks the new URL the same way.
    let created = hooks
        .create(
            &request("req-good"),
            &proof,
            &WebhookInput {
                url: "https://kanflow.internal/hook".to_owned(),
                event_types: Some(vec!["merge_request.*".to_owned()]),
            },
        )
        .await
        .expect("created");
    assert!(created.secret.expose().starts_with("whsec_"));
    let one = prove(
        &World::person(w.alice),
        Action::WebhookManage,
        WebhookCtx::new(w.loaded(w.alice).await, created.webhook.id),
    )
    .expect("proof");
    let e = hooks
        .update(
            &request("req-update-bad"),
            &one,
            &WebhookChanges {
                url: Some("https://[::1]/hook".to_owned()),
                ..WebhookChanges::default()
            },
        )
        .await
        .expect_err("loopback");
    assert_reason(
        &e,
        forge_errors::ErrorCode::WebhookTargetForbidden,
        "loopback",
    );
    w.t.drop_now().await;
}

// ── log capture (as tests/feed.rs) ──

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

thread_local! {
    static SINK: std::cell::RefCell<Option<Captured>> = const { std::cell::RefCell::new(None) };
}

struct ThreadSink;

impl std::io::Write for ThreadSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        SINK.with(|s| {
            if let Some(c) = s.borrow().as_ref() {
                c.0.lock().expect("log buffer").extend_from_slice(buf);
            }
        });
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct CaptureGuard;

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        SINK.with(|s| s.borrow_mut().take());
    }
}

impl Captured {
    fn install(&self) -> CaptureGuard {
        static GLOBAL: std::sync::Once = std::sync::Once::new();
        GLOBAL.call_once(|| {
            let subscriber = tracing_subscriber::fmt()
                .json()
                .flatten_event(true)
                .with_max_level(tracing::Level::DEBUG)
                .with_writer(|| ThreadSink)
                .finish();
            tracing::subscriber::set_global_default(subscriber).expect("one global subscriber");
        });
        SINK.with(|s| *s.borrow_mut() = Some(self.clone()));
        let _ = std::io::stderr().flush();
        CaptureGuard
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("log buffer")).into_owned()
    }

    fn lines(&self) -> Vec<serde_json::Value> {
        self.text()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
}

// ── I2 review fixes ──

#[tokio::test]
async fn an_editor_removed_later_gets_deliveries_skipped_owner_lost() {
    let w = world().await;
    let bob = users::create(
        &w.t.db,
        &system("req-user-bob"),
        &NewUser::human("bob", "bob", "bob@example.test"),
    )
    .await
    .expect("bob")
    .user_id;
    projects::add_member(
        &w.t.db,
        &system("req-bob-member"),
        w.project(),
        bob,
        Role::Maintainer,
    )
    .await
    .expect("bob is a Maintainer");
    let rx = Receiver::start(vec![answer(204)]).await;
    let hooks = Webhooks::new(w.t.db.clone(), test_policy(rx.port), Some(keys()));
    // Alice creates it; bob points it elsewhere (another event allowlist).
    let (hook, _) = w.create_as(&hooks, w.alice, &rx.url(), None).await;
    let proof = prove(
        &World::person(bob),
        Action::WebhookManage,
        WebhookCtx::new(w.loaded(bob).await, hook),
    )
    .expect("proof");
    let _edited = hooks
        .update(
            &request("req-bob-edit"),
            &proof,
            &WebhookChanges {
                event_types: Some(vec!["check.*".to_owned()]),
                ..WebhookChanges::default()
            },
        )
        .await
        .expect("edit");
    // Bob leaves; alice is still the project's Owner.
    let _demoted = projects::change_member(
        &w.t.db,
        &system("req-bob-demoted"),
        w.project(),
        bob,
        Role::Maintainer,
        Role::Developer,
    )
    .await
    .expect("demote");
    w.check_event(10).await;
    w.dispatch().await;
    w.deliver(Transport::new(test_policy(rx.port), None).expect("transport"))
        .await;
    assert_eq!(
        rx.received().len(),
        0,
        "the removed editor's webhook delivered"
    );
    let rows = w
        .rows(&format!(
            "SELECT count(*) AS n FROM forge.webhook_deliveries WHERE webhook_id = {}",
            hook.get()
        ))
        .await;
    assert_eq!(
        rows[0]["n"], 0,
        "nothing recorded for an owner who lost the project"
    );
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_job_that_cannot_run_fails_its_delivery_with_int_0004() {
    let w = world().await;
    let rx = Receiver::start(vec![answer(204)]).await;
    let hooks = Webhooks::new(w.t.db.clone(), test_policy(rx.port), Some(keys()));
    let (hook, _) = w.create_as(&hooks, w.alice, &rx.url(), None).await;
    w.check_event(11).await;
    w.dispatch().await;
    // A payload this build cannot read (the round is gone).
    w.t.exec_superuser(
        "UPDATE forge.jobs SET payload = payload - 'round' WHERE kind = 'webhook.deliver'",
    )
    .await;
    let log = Captured::default();
    {
        let _guard = log.install();
        w.deliver(Transport::new(test_policy(rx.port), None).expect("transport"))
            .await;
    }
    let rows = w
        .rows(&format!(
            "SELECT state, last_reason FROM forge.webhook_deliveries WHERE webhook_id = {}",
            hook.get()
        ))
        .await;
    assert_eq!(rows[0]["state"], "failed", "{rows:?}");
    assert_eq!(rows[0]["last_reason"], "internal");
    assert!(log.lines().iter().any(|l| l["error_code"] == "FG-INT-0004"
        && l["context"]
            .as_str()
            .is_some_and(|c| c.contains("reason=job_failed"))));
    assert_eq!(rx.received().len(), 0);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_skipped_delivery_is_logged_with_its_reason() {
    let w = world().await;
    let rx = Receiver::start(vec![answer(204)]).await;
    let hooks = Webhooks::new(w.t.db.clone(), test_policy(rx.port), Some(keys()));
    let (hook, _) = w.create_as(&hooks, w.alice, &rx.url(), None).await;
    w.check_event(12).await;
    w.dispatch().await;
    // Disabled after the event was dispatched.
    let proof = prove(
        &World::person(w.alice),
        Action::WebhookManage,
        WebhookCtx::new(w.loaded(w.alice).await, hook),
    )
    .expect("proof");
    let _disabled = hooks
        .update(
            &request("req-off"),
            &proof,
            &WebhookChanges {
                enabled: Some(false),
                ..WebhookChanges::default()
            },
        )
        .await
        .expect("disable");
    let log = Captured::default();
    {
        let _guard = log.install();
        w.deliver(Transport::new(test_policy(rx.port), None).expect("transport"))
            .await;
    }
    let line = log
        .lines()
        .into_iter()
        .find(|l| l["event"] == "forge.webhooks.skipped")
        .expect("the skip's log line");
    assert_eq!(line["level"], "INFO");
    assert_eq!(line["reason"], "webhook_disabled");
    assert!(line["delivery_id"].is_string());
    assert!(line["request_id"].is_string());
    w.t.drop_now().await;
}
