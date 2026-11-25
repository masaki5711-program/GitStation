//! The gateway with a stand-in model and records (plan 19 L5-3). No real
//! Ollama: [`Fake`] answers from a script, and the HTTP provider is tested
//! against a local fake server.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use forge_core::llm::{
    prepare, ChatFuture, ChatReply, InputClass, InputLimits, InputPart, OutputPolicy,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;

use super::*;

/// The model: answers from a script; optionally waits for a release.
#[derive(Default)]
struct Fake {
    answers: Mutex<Vec<Result<String, ErrorCode>>>,
    calls: AtomicUsize,
    gate: Option<Arc<Notify>>,
}

impl Fake {
    fn answering(answers: Vec<Result<String, ErrorCode>>) -> Self {
        Self {
            answers: Mutex::new(answers.into_iter().rev().collect()),
            ..Self::default()
        }
    }
}

impl LlmProvider for Fake {
    fn chat<'a>(&'a self, _request: &'a ChatRequest) -> ChatFuture<'a> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.gate {
                gate.notified().await;
            }
            let next = self.answers.lock().expect("lock").pop();
            match next {
                Some(Ok(content)) => Ok(ChatReply {
                    content,
                    input_tokens: 50,
                    output_tokens: 5,
                }),
                Some(Err(code)) => Err(ForgeError::new(code).with_context("reason", "connect")),
                None => Err(ForgeError::new(ErrorCode::LlmUnavailable)
                    .with_context("reason", "script_empty")),
            }
        })
    }
}

/// The records, in memory.
#[derive(Default)]
struct Memory {
    requests: Mutex<Vec<(NewRequest, &'static str, Option<&'static str>)>>,
    advice: Mutex<Vec<NewAdvice>>,
}

impl Memory {
    fn states(&self) -> Vec<(&'static str, Option<&'static str>)> {
        self.requests
            .lock()
            .expect("lock")
            .iter()
            .map(|(_, s, c)| (*s, *c))
            .collect()
    }
}

impl Records for Memory {
    fn queued<'a>(&'a self, r: &'a NewRequest) -> RecordFuture<'a, i64> {
        Box::pin(async move {
            let mut rows = self.requests.lock().expect("lock");
            rows.push((r.clone(), "queued", None));
            Ok(i64::try_from(rows.len()).expect("small"))
        })
    }

    fn running(&self, id: i64) -> RecordFuture<'_, bool> {
        Box::pin(async move {
            let mut rows = self.requests.lock().expect("lock");
            let row = &mut rows[usize::try_from(id - 1).expect("id")];
            row.1 = "running";
            Ok(true)
        })
    }

    fn finished(&self, id: i64, ending: Ending, _usage: Usage) -> RecordFuture<'_, bool> {
        Box::pin(async move {
            let mut rows = self.requests.lock().expect("lock");
            let row = &mut rows[usize::try_from(id - 1).expect("id")];
            (row.1, row.2) = match ending {
                Ending::Succeeded => ("succeeded", None),
                Ending::Failed(c) => ("failed", Some(c.id())),
                Ending::Abandoned(c) => ("abandoned", Some(c.id())),
            };
            Ok(true)
        })
    }

    fn advice<'a>(&'a self, a: &'a NewAdvice) -> RecordFuture<'a, i64> {
        Box::pin(async move {
            let mut rows = self.advice.lock().expect("lock");
            rows.push(a.clone());
            Ok(i64::try_from(rows.len()).expect("small"))
        })
    }
}

struct Catalog(Mutex<Option<String>>);

impl ModelCatalog for Catalog {
    fn digest<'a>(&'a self, _model: &'a str) -> DigestFuture<'a> {
        Box::pin(async move { Ok(self.0.lock().expect("lock").clone()) })
    }
}

fn digest() -> String {
    "d".repeat(64)
}

fn config(paused: bool) -> GatewayConfig {
    GatewayConfig {
        model: ModelRef::parse(&format!("qwen3:27b@sha256:{}", digest())).expect("model"),
        num_ctx: 8192,
        concurrency: 1,
        paused,
        call_timeout: Duration::from_secs(600),
    }
}

fn submission(purpose: Purpose, class: InputClass) -> Submission {
    Submission {
        purpose,
        priority: None,
        subject_kind: forge_core::llm::SubjectKind::MergeRequest,
        subject_id: Some(7),
        project: None,
        requested_by: None,
        head_sha: Some("a".repeat(40)),
        prompt: Prompt {
            id: "mr_review".to_owned(),
            version: 1,
            system: "Review the diff.".to_owned(),
        },
        input: prepare(
            vec![InputPart::new(
                "diff",
                class,
                "+    let attempts = 3;\n+    upload(&file).await.unwrap();",
            )
            .with_path("src/upload.rs")],
            &InputLimits::default(),
        )
        .expect("input"),
        policy: OutputPolicy::default(),
        request_id: RequestId::parse("req-llm-test").expect("id"),
    }
}

const GOOD: &str = r#"{"summary": "Adds retries.", "findings": [
  {"severity": "high", "path": "src/upload.rs", "line": 2, "quote": "upload(&file).await.unwrap();",
   "message": "unwrap on a network call", "confidence": 0.9},
  {"severity": "low", "path": "src/upload.rs", "line": 1, "quote": "nothing like this is there",
   "message": "invented", "confidence": 0.9}]}"#;

#[tokio::test]
async fn an_advice_is_written_and_its_class_kept() {
    let records = Arc::new(Memory::default());
    let gw = Gateway::new(
        config(false),
        Arc::new(Fake::answering(vec![Ok(GOOD.to_owned())])),
        records.clone(),
    );
    let out = gw
        .submit(submission(Purpose::MrReview, InputClass::Confidential))
        .await
        .expect("advised");
    let Outcome::Advised(advised) = out else {
        panic!("not advised: {out:?}");
    };
    assert_eq!(advised.advice.findings.len(), 1);
    assert_eq!(advised.advice.dropped_evidence, 1);
    assert_eq!(records.states(), vec![("succeeded", None)]);
    let rows = records.advice.lock().expect("lock");
    assert_eq!(rows[0].input_class, InputClass::Confidential);
    assert_eq!(rows[0].dropped_findings, 1);
    assert!(rows[0].degraded, "a finding was dropped for its evidence");
    assert_eq!(rows[0].findings[0]["severity"], "high");
    let requests = records.requests.lock().expect("lock");
    // Only the hash of the input is recorded.
    assert_eq!(requests[0].0.input_sha256.len(), 64);
    assert_eq!(
        requests[0].0.model_digest.as_deref(),
        Some(digest().as_str())
    );
    let m = gw.metrics();
    assert_eq!(
        m.counter(metrics::REQUESTS, "purpose=\"mr_review\",outcome=\"ok\""),
        1
    );
    assert_eq!(
        m.counter(metrics::EVIDENCE_DROPPED, "purpose=\"mr_review\""),
        1
    );
    assert_eq!(m.samples(metrics::QUEUE_WAIT, "priority=\"review\""), 1);
}

#[tokio::test]
async fn llm_unavailable_is_an_error_never_a_success() {
    let records = Arc::new(Memory::default());
    let gw = Gateway::new(
        config(false),
        Arc::new(Fake::answering(vec![Err(ErrorCode::LlmUnavailable)])),
        records.clone(),
    );
    let err = gw
        .submit(submission(Purpose::MrReview, InputClass::Internal))
        .await
        .expect_err("error");
    assert_eq!(err.code(), ErrorCode::LlmUnavailable);
    assert_eq!(records.states(), vec![("failed", Some("FG-LLM-0001"))]);
    assert!(records.advice.lock().expect("lock").is_empty());

    // Two invalid answers: FG-LLM-0002, counted.
    let gw = Gateway::new(
        config(false),
        Arc::new(Fake::answering(vec![
            Ok("x".to_owned()),
            Ok("y".to_owned()),
        ])),
        records.clone(),
    );
    let err = gw
        .submit(submission(Purpose::MrReview, InputClass::Internal))
        .await
        .expect_err("error");
    assert_eq!(err.code(), ErrorCode::LlmOutputInvalid);
    assert_eq!(
        gw.metrics()
            .counter(metrics::OUTPUT_INVALID, "purpose=\"mr_review\""),
        1
    );
}

#[tokio::test]
async fn paused_gateway_queues_nothing_new() {
    let records = Arc::new(Memory::default());
    let fake = Arc::new(Fake::answering(vec![Ok(GOOD.to_owned())]));
    let gw = Gateway::new(config(true), fake.clone(), records.clone());
    let out = gw
        .submit(submission(Purpose::MrReview, InputClass::Internal))
        .await
        .expect("paused");
    assert_eq!(out, Outcome::Paused);
    assert!(records.states().is_empty(), "no record");
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0, "no model call");
    gw.resume();
    assert!(matches!(
        gw.submit(submission(Purpose::MrReview, InputClass::Internal))
            .await
            .expect("resumed"),
        Outcome::Advised(_)
    ));
    gw.pause();
    assert!(gw.is_paused());
}

#[tokio::test]
async fn model_digest_mismatch_stops_gateway() {
    let records = Arc::new(Memory::default());
    let fake = Arc::new(Fake::answering(vec![Ok(GOOD.to_owned())]));
    let gw = Gateway::new(config(false), fake.clone(), records.clone());
    let catalog = Catalog(Mutex::new(Some(format!("sha256:{}", "e".repeat(64)))));
    let err = gw.verify_model(&catalog).await.expect_err("mismatch");
    assert_eq!(err.code(), ErrorCode::LlmModelDigestMismatch);
    assert!(err.to_string().contains("actual_digest="), "{err}");
    assert!(gw.is_stopped());
    let err = gw
        .submit(submission(Purpose::MrReview, InputClass::Internal))
        .await
        .expect_err("stopped");
    assert_eq!(err.code(), ErrorCode::LlmModelDigestMismatch);
    assert!(records.states().is_empty());
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    // A missing model is a mismatch too.
    *catalog.0.lock().expect("lock") = None;
    let err = gw.verify_model(&catalog).await.expect_err("missing");
    assert!(err.to_string().contains("model_missing"), "{err}");
    // The pinned digest again: inference resumes.
    *catalog.0.lock().expect("lock") = Some(digest());
    gw.verify_model(&catalog).await.expect("matches");
    assert!(!gw.is_stopped());
    assert!(matches!(
        gw.submit(submission(Purpose::MrReview, InputClass::Internal))
            .await
            .expect("runs"),
        Outcome::Advised(_)
    ));
}

#[tokio::test(start_paused = true)]
async fn a_request_waiting_past_its_deadline_is_abandoned() {
    let records = Arc::new(Memory::default());
    let gate = Arc::new(Notify::new());
    let fake = Arc::new(Fake {
        answers: Mutex::new(vec![Ok(GOOD.to_owned()), Ok(GOOD.to_owned())]),
        calls: AtomicUsize::new(0),
        gate: Some(gate.clone()),
    });
    let gw = Gateway::new(config(false), fake.clone(), records.clone());
    // A review holds the only slot (the model is slow) ...
    let first = tokio::spawn({
        let gw = gw.clone();
        async move {
            gw.submit(submission(Purpose::MrReview, InputClass::Internal))
                .await
        }
    });
    while fake.calls.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    // ... so an approval summary (interactive, 2 min) waits and gives up.
    let err = gw
        .submit(submission(Purpose::ApprovalSummary, InputClass::Internal))
        .await
        .expect_err("deadline");
    assert_eq!(err.code(), ErrorCode::LlmQueueDeadline);
    assert!(err.to_string().contains("priority=interactive"), "{err}");
    assert_eq!(gw.queue_len(), (0, 1));
    gate.notify_one();
    assert!(matches!(
        first.await.expect("join").expect("first"),
        Outcome::Advised(_)
    ));
    assert_eq!(
        records.states(),
        vec![("succeeded", None), ("abandoned", Some("FG-LLM-0004"))]
    );
    assert_eq!(gw.queue_len(), (0, 0), "the slot is free again");
}

#[tokio::test]
async fn kanflow_busy_holds_reviews_and_counts_the_yield_once() {
    let records = Arc::new(Memory::default());
    let gw = Gateway::new(
        config(false),
        Arc::new(Fake::answering(vec![Ok(GOOD.to_owned())])),
        records.clone(),
    );
    gw.set_conditions(Some(Kanflow::Busy), None);
    let waiting = tokio::spawn({
        let gw = gw.clone();
        async move {
            gw.submit(submission(Purpose::MrReview, InputClass::Internal))
                .await
        }
    });
    while gw.queue_len().0 == 0 {
        tokio::task::yield_now().await;
    }
    gw.tick();
    gw.tick();
    assert_eq!(gw.queue_len(), (1, 0), "held while a Kanflow user waits");
    assert_eq!(
        gw.metrics()
            .counter(metrics::YIELDS, "reason=\"kanflow_busy\""),
        1
    );
    gw.set_conditions(Some(Kanflow::Idle), None);
    assert!(matches!(
        waiting.await.expect("join").expect("ran"),
        Outcome::Advised(_)
    ));
}

/// One HTTP exchange per connection: the answer for each path.
async fn fake_ollama(
    routes: Vec<(&'static str, u16, String)>,
) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = sock.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(head_end) = text.find("\r\n\r\n") {
                    let len = text[..head_end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if buf.len() >= head_end + 4 + len {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&buf).to_string();
            log.lock().expect("lock").push(text.clone());
            let path = text.split_whitespace().nth(1).unwrap_or("").to_owned();
            let (status, body) = routes
                .iter()
                .find(|(p, _, _)| *p == path)
                .map_or((404, String::new()), |(_, s, b)| (*s, b.clone()));
            let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    (format!("http://{addr}"), seen)
}

#[tokio::test]
async fn the_ollama_provider_speaks_the_chat_and_tags_api() {
    let chat = serde_json::json!({
        "message": {"role": "assistant", "content": "{\"summary\": \"ok\"}"},
        "prompt_eval_count": 321, "eval_count": 12,
    })
    .to_string();
    let tags = serde_json::json!({"models": [
        {"name": "qwen3:27b", "model": "qwen3:27b", "digest": digest()},
    ]})
    .to_string();
    let (url, seen) = fake_ollama(vec![("/api/chat", 200, chat), ("/api/tags", 200, tags)]).await;
    let provider = ollama::OllamaProvider::new(&url, None, Duration::from_secs(5)).expect("client");
    let request = ChatRequest {
        model: "qwen3:27b".to_owned(),
        system: "sys".to_owned(),
        user: "data".to_owned(),
        num_ctx: 8192,
    };
    let reply = provider.chat(&request).await.expect("reply");
    assert_eq!(reply.content, "{\"summary\": \"ok\"}");
    assert_eq!((reply.input_tokens, reply.output_tokens), (321, 12));
    let sent = seen.lock().expect("lock")[0].clone();
    assert!(sent.starts_with("POST /api/chat"), "{sent}");
    assert!(sent.contains("\"format\":\"json\""), "{sent}");
    assert!(sent.contains("\"num_ctx\":8192"), "{sent}");
    assert!(sent.contains("\"stream\":false"), "{sent}");
    let found = ModelCatalog::digest(&provider, "qwen3:27b")
        .await
        .expect("tags");
    assert_eq!(found, Some(digest()));
    assert_eq!(
        ModelCatalog::digest(&provider, "other:1b")
            .await
            .expect("tags"),
        None
    );
}

#[tokio::test]
async fn ollama_failures_are_unavailable_with_a_reason() {
    let (url, _) = fake_ollama(vec![(
        "/api/chat",
        500,
        "{\"error\": \"model requires more system memory\"}".to_owned(),
    )])
    .await;
    let provider = ollama::OllamaProvider::new(&url, None, Duration::from_secs(5)).expect("client");
    let request = ChatRequest {
        model: "qwen3:27b".to_owned(),
        system: "s".to_owned(),
        user: "u".to_owned(),
        num_ctx: 8192,
    };
    let err = provider.chat(&request).await.expect_err("500");
    assert_eq!(err.code(), ErrorCode::LlmUnavailable);
    assert!(err.to_string().contains("reason=status_500"), "{err}");
    assert!(
        !err.to_string().contains("memory"),
        "the provider's text stays out"
    );

    // Nothing listens: connect.
    let closed = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        l.local_addr().expect("addr")
    };
    let provider =
        ollama::OllamaProvider::new(&format!("http://{closed}"), None, Duration::from_secs(2))
            .expect("client");
    let err = provider.chat(&request).await.expect_err("down");
    assert_eq!(err.code(), ErrorCode::LlmUnavailable);
}
