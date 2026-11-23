#![allow(clippy::expect_used, clippy::panic)]
//! The `org_master_sync` task against a local stand-in of the PFS
//! registry-api (a real HTTP server on 127.0.0.1) and a migrated database
//! (ADR-0038 §1): a valid answer replaces the copy; 403, a timeout and an
//! invalid payload keep the previous copy with their codes; a copy older
//! than the maximum age is FG-INT-0028 once. Synthetic data only.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use forge_core::config::{load, Env, OrgConfig};
use forge_core::portal::{ServiceToken, ServiceTokenFuture, ServiceTokenSource};
use forge_db::org_master::ReplacePolicy;
use forge_db::test_support::FreshDb;
use forge_integ::org_master::{HttpMasterTransport, OrgMasterSync};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;

/// How the stand-in answers one path.
#[derive(Clone)]
enum Answer {
    Json(u16, String),
    /// Accept, read the request and never answer (a timeout).
    Hang,
}

/// The registry stand-in: answers by path, records the bearer tokens.
struct Registry {
    origin: String,
    answers: Arc<Mutex<Vec<(String, Answer)>>>,
    tokens: Arc<Mutex<Vec<String>>>,
}

impl Registry {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let answers: Arc<Mutex<Vec<(String, Answer)>>> = Arc::default();
        let tokens: Arc<Mutex<Vec<String>>> = Arc::default();
        let (script, seen) = (Arc::clone(&answers), Arc::clone(&tokens));
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let script = Arc::clone(&script);
                let seen = Arc::clone(&seen);
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&buf).into_owned();
                    let path = head
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_owned();
                    for line in head.lines() {
                        if let Some(v) = line
                            .strip_prefix("authorization: Bearer ")
                            .or_else(|| line.strip_prefix("Authorization: Bearer "))
                        {
                            seen.lock().expect("tokens").push(v.trim().to_owned());
                        }
                    }
                    let answer = script
                        .lock()
                        .expect("script")
                        .iter()
                        .find(|(p, _)| *p == path)
                        .map(|(_, a)| a.clone())
                        .unwrap_or(Answer::Json(404, "{}".to_owned()));
                    match answer {
                        Answer::Hang => {
                            tokio::time::sleep(Duration::from_secs(30)).await;
                        }
                        Answer::Json(status, body) => {
                            let reply = format!(
                                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n\
                                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                                body.len()
                            );
                            let _ = stream.write_all(reply.as_bytes()).await;
                        }
                    }
                });
            }
        });
        Self {
            origin: format!("http://127.0.0.1:{port}"),
            answers,
            tokens,
        }
    }

    fn answer(&self, units: Answer, positions: Answer) {
        *self.answers.lock().expect("script") = vec![
            ("/api/v1/org/units".to_owned(), units),
            ("/api/v1/org/positions".to_owned(), positions),
        ];
    }
}

#[derive(Default)]
struct Tokens {
    forgotten: AtomicUsize,
}

impl ServiceTokenSource for Tokens {
    fn service_token(&self) -> ServiceTokenFuture<'_> {
        Box::pin(async { Ok(ServiceToken::new("synthetic-client-token".to_owned())) })
    }

    fn forget(&self) {
        self.forgotten.fetch_add(1, Ordering::SeqCst);
    }
}

fn envelope(items: serde_json::Value) -> String {
    json!({"success": true, "data": {"items": items}}).to_string()
}

fn units(section_name: &str) -> Answer {
    Answer::Json(
        200,
        envelope(json!([
            {"plantCode": "P001", "code": "SYN-DEP", "level": "department", "parentCode": null,
             "name": {"ja": "合成部", "en": "Synthetic Dept", "id": null}, "costCenter": null,
             "headStaffId": "SYN0001", "validFrom": "2026-01-01", "version": 1},
            {"plantCode": "P001", "code": "SYN-SEC", "level": "section", "parentCode": "SYN-DEP",
             "name": {"ja": section_name}, "costCenter": "CC1", "headStaffId": null,
             "validFrom": null, "version": 2}
        ])),
    )
}

fn positions() -> Answer {
    Answer::Json(
        200,
        envelope(json!([
            {"plantCode": "P001", "code": "manager", "name": "マネージャー", "rank": 55,
             "sortOrder": 1, "active": true, "version": 1}
        ])),
    )
}

struct World {
    t: FreshDb,
    registry: Registry,
    tokens: Arc<Tokens>,
    sync: OrgMasterSync,
}

impl World {
    async fn new() -> Self {
        let t = FreshDb::migrated().await;
        let registry = Registry::start().await;
        let tokens = Arc::new(Tokens::default());
        let transport = Arc::new(
            HttpMasterTransport::new(None, Duration::from_millis(800)).expect("transport"),
        );
        let sync = OrgMasterSync::new(
            &t.db,
            &registry.origin,
            Arc::clone(&tokens) as Arc<dyn ServiceTokenSource>,
            transport,
            ReplacePolicy::default(),
        );
        Self {
            t,
            registry,
            tokens,
            sync,
        }
    }

    async fn section_name(&self) -> Option<String> {
        self.t
            .json_rows("SELECT name_ja FROM forge.org_units_master WHERE code = 'SYN-SEC'")
            .await
            .first()
            .and_then(|r| r["name_ja"].as_str().map(str::to_owned))
    }

    /// The first sync: a copy to keep.
    async fn seeded(&self) {
        self.registry.answer(units("合成課"), positions());
        self.sync
            .run_once(&RequestId::generate())
            .await
            .expect("first sync");
        assert_eq!(self.section_name().await.as_deref(), Some("合成課"));
    }
}

fn lane() -> Lane {
    Lane::new(&OrgConfig::default())
}

#[tokio::test]
async fn a_valid_answer_replaces_the_copy_with_the_service_token() {
    let w = World::new().await;
    w.seeded().await;
    assert!(w
        .registry
        .tokens
        .lock()
        .expect("tokens")
        .iter()
        .all(|t| t == "synthetic-client-token"));
    w.registry.answer(units("改名した課"), positions());
    let mut lane = lane();
    let stepped = lane.step(&w.t.db, &w.sync).await;
    assert!(stepped.ok);
    assert_eq!(stepped.delay, Duration::from_secs(900));
    assert_eq!(w.section_name().await.as_deref(), Some("改名した課"));
    let positions =
        w.t.json_rows("SELECT code, rank FROM forge.org_positions_master")
            .await;
    assert_eq!(positions[0]["rank"], 55);
}

#[tokio::test]
async fn a_refused_registry_keeps_the_previous_copy() {
    let w = World::new().await;
    w.seeded().await;
    w.registry.answer(
        Answer::Json(403, r#"{"success":false}"#.to_owned()),
        positions(),
    );
    let err = w
        .sync
        .run_once(&RequestId::generate())
        .await
        .expect_err("refused");
    assert_eq!(err.code(), ErrorCode::OrgMasterRefused);
    let text = err.to_string();
    assert!(
        text.contains("status=403") && text.contains("list=units"),
        "{text}"
    );
    assert!(!text.contains("synthetic-client-token"), "{text}");
    assert_eq!(w.tokens.forgotten.load(Ordering::SeqCst), 1);
    assert_eq!(w.section_name().await.as_deref(), Some("合成課"));
    // The lane backs off from 30 s and logs the streak's first failure.
    let mut lane = lane();
    let first = lane.step(&w.t.db, &w.sync).await;
    assert!(!first.ok && first.logged);
    assert_eq!(first.delay, Duration::from_secs(60));
    assert!(!first.stale, "a fresh copy is not stale");
    let second = lane.step(&w.t.db, &w.sync).await;
    assert!(
        !second.logged,
        "the same failure is not logged again at once"
    );
    assert_eq!(second.delay, Duration::from_secs(120));
}

#[tokio::test]
async fn an_unreachable_registry_keeps_the_previous_copy() {
    let w = World::new().await;
    w.seeded().await;
    w.registry.answer(units("合成課"), Answer::Hang);
    let err = w
        .sync
        .run_once(&RequestId::generate())
        .await
        .expect_err("timeout");
    assert_eq!(err.code(), ErrorCode::OrgMasterUnreachable);
    let text = err.to_string();
    assert!(
        text.contains("reason=timeout") && text.contains("list=positions"),
        "{text}"
    );
    assert_eq!(w.section_name().await.as_deref(), Some("合成課"));
    w.registry
        .answer(Answer::Json(502, "{}".to_owned()), positions());
    let err = w
        .sync
        .run_once(&RequestId::generate())
        .await
        .expect_err("502");
    assert_eq!(err.code(), ErrorCode::OrgMasterUnreachable);
    assert!(err.to_string().contains("status=502"), "{err}");
    // Nobody listening at all.
    let closed = OrgMasterSync::new(
        &w.t.db,
        "http://127.0.0.1:9",
        Arc::clone(&w.tokens) as Arc<dyn ServiceTokenSource>,
        Arc::new(HttpMasterTransport::new(None, Duration::from_millis(800)).expect("transport")),
        ReplacePolicy::default(),
    );
    let err = closed
        .run_once(&RequestId::generate())
        .await
        .expect_err("closed");
    assert_eq!(err.code(), ErrorCode::OrgMasterUnreachable);
}

#[tokio::test]
async fn an_invalid_payload_keeps_the_previous_copy() {
    let w = World::new().await;
    w.seeded().await;
    let bad = envelope(json!([
        {"plantCode": "P001", "code": "SYN-SEC", "level": "unit", "parentCode": null,
         "name": {"ja": "壊れた課"}, "version": 1}
    ]));
    w.registry.answer(Answer::Json(200, bad), positions());
    let err = w
        .sync
        .run_once(&RequestId::generate())
        .await
        .expect_err("invalid");
    assert_eq!(err.code(), ErrorCode::OrgMasterInvalid);
    let text = err.to_string();
    assert!(
        text.contains("field=level") && text.contains("index=0"),
        "{text}"
    );
    assert!(!text.contains("壊れた課"), "{text}");
    assert_eq!(w.section_name().await.as_deref(), Some("合成課"));
    // An empty list never wipes the copy.
    w.registry
        .answer(Answer::Json(200, envelope(json!([]))), positions());
    let err = w
        .sync
        .run_once(&RequestId::generate())
        .await
        .expect_err("empty");
    assert!(err.to_string().contains("reason=empty"), "{err}");
    assert_eq!(w.section_name().await.as_deref(), Some("合成課"));
}

#[tokio::test]
async fn a_stale_copy_is_logged_once_per_streak() {
    let w = World::new().await;
    w.registry
        .answer(Answer::Json(403, "{}".to_owned()), positions());
    let mut lane = lane();
    // Never synced: stale at once, logged once.
    assert!(lane.step(&w.t.db, &w.sync).await.stale);
    assert!(!lane.step(&w.t.db, &w.sync).await.stale);
    // A success clears it; an old copy and a failure log it again.
    w.registry.answer(units("合成課"), positions());
    assert!(lane.step(&w.t.db, &w.sync).await.ok);
    w.t.exec_superuser("UPDATE forge.org_master_state SET synced_at = now() - interval '25 hours'")
        .await;
    w.registry
        .answer(Answer::Json(403, "{}".to_owned()), positions());
    assert!(lane.step(&w.t.db, &w.sync).await.stale);
}

#[tokio::test]
async fn the_org_master_sync_needs_the_sign_in() {
    let t = FreshDb::migrated().await;
    let off = load(None, &Env::from_pairs([])).expect("config");
    assert!(crate::portal::org_master_sync(&off, &t.db)
        .expect("off")
        .is_none());
    let cfg = load(
        None,
        &Env::from_pairs([
            ("FORGE_ORG_MASTER_SYNC_ENABLED", "true"),
            ("FORGE_PORTAL_REGISTRY_URL", "http://127.0.0.1:4100"),
        ]),
    )
    .expect("config");
    let err = crate::portal::org_master_sync(&cfg, &t.db).expect_err("no auth.oidc");
    assert!(err.to_string().contains("needs_auth_oidc"), "{err}");
}
