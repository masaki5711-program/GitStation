//! `forge diagnose` (plan 19 L5-7) against a migrated database and a
//! scripted model: the fact bundle is always printed, the prompt only sees
//! redacted lines, and the advice is recorded as an incident diagnosis.
//! Synthetic log lines and tokens only (forge:allow-secret: the push secret
//! check, FG-GIT-0022).

use std::sync::Mutex;

use forge_core::llm::{ChatFuture, ChatReply, ChatRequest, LlmProvider, ModelRef};
use forge_db::llm::{AdviceView, ReaderKind};
use forge_db::test_support::FreshDb;
use forge_llm::DigestFuture;

use super::*;

const RID: &str = "r-7f3a9c";

fn rid() -> RequestId {
    RequestId::parse("r-diagnose-test").expect("request id")
}

fn token() -> String {
    format!("fgp_{}", "Ab3dE5gH7jK9mN1pQ3sT5vX7zA9cE1gI")
}

fn stream() -> String {
    [
        r#"{"level":"INFO","message":"start","request_id":"r-other"}"#.to_owned(),
        format!(
            r#"{{"level":"WARN","error_code":"FG-DB-0001","request_id":"{RID}","context":"reason=acquire_timeout pool=write token={}","message":"The database is unreachable"}}"#,
            token()
        ),
    ]
    .join("\n")
}

/// A model that answers from a script and keeps the prompts it saw.
struct Scripted {
    answer: Result<String, ErrorCode>,
    seen: Mutex<Vec<String>>,
}

impl Scripted {
    fn new(answer: Result<String, ErrorCode>) -> Arc<Self> {
        Arc::new(Self {
            answer,
            seen: Mutex::new(Vec::new()),
        })
    }
}

impl LlmProvider for Scripted {
    fn chat<'a>(&'a self, req: &'a ChatRequest) -> ChatFuture<'a> {
        self.seen.lock().expect("lock").push(req.user.clone());
        let answer = self.answer.clone();
        Box::pin(async move {
            match answer {
                Ok(content) => Ok(ChatReply {
                    content,
                    input_tokens: 10,
                    output_tokens: 5,
                }),
                Err(code) => Err(ForgeError::new(code).with_context("reason", "scripted")),
            }
        })
    }
}

fn digest() -> String {
    "0".repeat(64)
}

struct Pinned;

impl ModelCatalog for Pinned {
    fn digest<'a>(&'a self, _: &'a str) -> DigestFuture<'a> {
        Box::pin(async { Ok(Some(format!("sha256:{}", digest()))) })
    }
}

fn gateway(db: &Db, provider: Arc<Scripted>) -> Gateway {
    Gateway::new(
        GatewayConfig {
            model: ModelRef::parse(&format!("qwen3:27b@sha256:{}", digest())).expect("model"),
            num_ctx: 8192,
            concurrency: 1,
            paused: false,
            call_timeout: Duration::from_secs(5),
        },
        provider,
        Arc::new(DbRecords(db.clone())),
    )
}

fn args(dir: &Path) -> Args {
    Args {
        target: Target::RequestId(RID.to_owned()),
        since: Duration::from_secs(2 * 60 * 60),
        logs: None,
        runbooks: Some(dir.join("runbooks")),
        incidents: Some(dir.join("records.jsonl")),
        doctor: false,
        ask: true,
        json: false,
    }
}

/// A runbook and an incident record for FG-DB-0001's runbook.
fn sources() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(dir.path().join("runbooks")).expect("dir");
    let runbook = ErrorCode::DbUnavailable
        .entry()
        .runbook
        .expect("FG-DB-0001 has a runbook");
    std::fs::write(
        dir.path().join("runbooks").join(format!("{runbook}.md")),
        "# DB\n\n## Pool\n\nFG-DB-0001: check the write pool first.\n\n## Other\n\nunrelated\n",
    )
    .expect("runbook");
    std::fs::write(
        dir.path().join("records.jsonl"),
        "{\"id\":\"INC-2026-001\",\"error_codes\":[\"FG-DB-0001\"],\"cause\":\"pool exhausted\"}\n",
    )
    .expect("records");
    dir
}

async fn facts_of(db: &Db, a: &Args, notes: &mut Vec<Note>) -> (Facts, Option<PreparedInput>) {
    let log = diagnosis::select_log_lines(&stream(), &a.target);
    let facts = gather(Some(db), a, log, None, None, notes).await;
    let input = prepare_facts(&facts, notes);
    (facts, input)
}

fn report(
    a: &Args,
    facts: Facts,
    input: Option<PreparedInput>,
    notes: Vec<Note>,
    ai: Ai,
) -> Report {
    Report {
        target: a.target.clone(),
        since: a.since,
        request_id: rid(),
        input,
        log: facts.log,
        notes,
        ai,
        model: Some("qwen3:27b".to_owned()),
    }
}

#[tokio::test]
async fn llm_down_prints_fact_bundle() {
    let t = FreshDb::migrated().await;
    let dir = sources();
    let a = args(dir.path());
    let mut notes = Vec::new();
    let (facts, input) = facts_of(&t.db, &a, &mut notes).await;
    assert!(notes.is_empty(), "{notes:?}");
    let input = input.expect("input");
    let provider = Scripted::new(Err(ErrorCode::LlmUnavailable));
    let ai = ask(&gateway(&t.db, provider), &Pinned, input.clone(), &rid()).await;
    let Ai::Failed(e) = &ai else {
        panic!("expected a failure, got {ai:?}");
    };
    assert_eq!(e.code(), ErrorCode::LlmUnavailable);
    let text = render_text(&report(&a, facts, Some(input), notes, ai));
    assert!(text.contains("■ AI の助言: ありません"), "{text}");
    assert!(text.contains("FG-LLM-0001"), "{text}");
    // The bundle: the log line, the catalog, the runbook section, the
    // incident and the recent changes.
    assert!(text.contains("L2: "), "{text}");
    assert!(text.contains("catalog:FG-DB-0001"), "{text}");
    assert!(text.contains("check the write pool first"), "{text}");
    assert!(!text.contains("unrelated"), "{text}");
    assert!(text.contains("incident:INC-2026-001"), "{text}");
    assert!(text.contains("migrations ("), "{text}");
    // Never the secret, even in the bundle printed to the operator.
    assert!(!text.contains(&token()), "{text}");
}

#[tokio::test]
async fn log_lines_redacted_before_prompt() {
    let t = FreshDb::migrated().await;
    let dir = sources();
    let a = args(dir.path());
    let mut notes = Vec::new();
    let (facts, input) = facts_of(&t.db, &a, &mut notes).await;
    let input = input.expect("input");
    let answer = serde_json::json!({
        "summary": "書き込みのプールが時間切れになった。",
        "findings": [
            {"severity": "high", "path": "log", "line": 2,
             "quote": "reason=acquire_timeout pool=write",
             "message": "書き込みのプールが尽きた。次に見るもの: SELECT count(*) FROM pg_stat_activity",
             "confidence": 0.8},
            {"severity": "high", "path": "log", "line": 2,
             "quote": "reason=acquire_timeout pool=write",
             "message": "接続が詰まった。systemctl restart forge-serve",
             "confidence": 0.7}
        ]
    })
    .to_string();
    let provider = Scripted::new(Ok(answer));
    let ai = ask(
        &gateway(&t.db, provider.clone()),
        &Pinned,
        input.clone(),
        &rid(),
    )
    .await;
    let seen = provider.seen.lock().expect("lock").clone();
    assert_eq!(seen.len(), 1);
    assert!(!seen[0].contains(&token()), "{}", seen[0]);
    assert!(seen[0].contains("[REDACTED"), "{}", seen[0]);
    let Ai::Advised(advised) = &ai else {
        panic!("expected an advice, got {ai:?}");
    };
    assert_eq!(advised.advice.findings.len(), 1);
    assert_eq!(advised.advice.dropped_mutating, 1);
    // Recorded as an incident diagnosis, the mutating candidate not in it.
    let stored = forge_db::llm::advice_decided(&t.db, advised.advice_id, |s| {
        assert_eq!(s.kind, SubjectKind::Incident);
        assert_eq!(s.id, None);
        Ok(ReaderKind::Person)
    })
    .await
    .expect("read")
    .expect("advice");
    let AdviceView::Shown(stored) = stored else {
        panic!("shown to a person");
    };
    assert_eq!(stored.purpose, Purpose::IncidentDiagnosis);
    assert!(!stored.findings.to_string().contains("restart"));
    let text = render_text(&report(&a, facts, Some(input), notes, ai));
    assert!(text.contains("AI の助言（提案"), "{text}");
    assert!(text.contains("log L2"), "{text}");
    assert!(
        text.contains("変更の手順を含む候補を 1 件捨てました"),
        "{text}"
    );
}

#[tokio::test]
async fn unreadable_logs_are_noted_and_the_diagnosis_goes_on() {
    let t = FreshDb::migrated().await;
    let dir = sources();
    let mut a = args(dir.path());
    a.target = Target::ErrorCode("FG-DB-0001".to_owned());
    let missing = dir.path().join("no-such.log");
    let note = read_logs(&missing).await.expect_err("missing");
    assert_eq!(note.source, "logs");
    assert!(note.reason.starts_with("unreadable"), "{note:?}");
    assert_eq!(note.error().code(), ErrorCode::DiagnoseSourceUnreadable);
    let mut notes = vec![note];
    let facts = gather(
        Some(&t.db),
        &a,
        LogSelection::default(),
        None,
        None,
        &mut notes,
    )
    .await;
    let input = prepare_facts(&facts, &mut notes);
    // The catalog and the runbook still make a diagnosis input.
    assert!(input.is_some());
    let text = render_json(&report(&a, facts, input, notes, Ai::Skipped));
    let v: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert_eq!(v["ai"]["status"], "skipped");
    assert_eq!(v["notes"][0]["source"], "logs");
    assert!(v["facts"]
        .as_array()
        .expect("facts")
        .iter()
        .any(|f| f["path"] == "catalog:FG-DB-0001"));
}

#[test]
fn since_and_target_are_checked() {
    assert_eq!(parse_since("2h"), Ok(Duration::from_secs(7200)));
    assert_eq!(parse_since("30m"), Ok(Duration::from_secs(1800)));
    assert_eq!(parse_since("1d"), Ok(Duration::from_secs(86_400)));
    assert!(parse_since("0h").is_err());
    assert!(parse_since("8d").is_err());
    assert!(parse_since("2w").is_err());
    assert!(parse_since("h").is_err());
    assert_eq!(
        target(None, Some("fg-db-0001")).expect("code"),
        Target::ErrorCode("FG-DB-0001".to_owned())
    );
    assert_eq!(
        target(None, Some("FG-DB-9999"))
            .expect_err("unknown")
            .code(),
        ErrorCode::UnknownErrorCode
    );
    assert_eq!(
        target(Some("bad id!"), None).expect_err("invalid").code(),
        ErrorCode::CliUsageInvalid
    );
}
