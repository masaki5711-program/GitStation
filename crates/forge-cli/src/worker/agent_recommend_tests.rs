//! The recommendation reasons pass (plan 16 A11) against a migrated
//! database, real git and a scripted model: only open specification
//! documents reach the model, and a down, paused or timed-out model leaves
//! the reader the rules-only recommendation, flagged degraded.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use forge_core::llm::agent_recommend::{
    findings_of, merge, reasons_state, CatalogEntry, ReasonsState,
};
use forge_core::llm::{ChatFuture, ChatReply, ChatRequest, LlmProvider, ModelRef};
use forge_core::ObjectId;
use forge_db::llm::agent_recommend::upsert_ask;
use forge_git::browse::BrowseLimits;
use forge_llm::{DbRecords, GatewayConfig};
use forge_marketplace::{Recommendation, Recommended};

use super::*;
use crate::worker::review::mergeability_tests::World;

fn plumb(repo: &Path, args: &[&str], input: Option<&[u8]>) -> String {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(args)
        .env("GIT_AUTHOR_NAME", "forge-test")
        .env("GIT_AUTHOR_EMAIL", "forge-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "forge-test")
        .env("GIT_COMMITTER_EMAIL", "forge-test@example.invalid")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("git");
    let mut stdin = child.stdin.take().expect("stdin");
    if let Some(input) = input {
        stdin.write_all(input).expect("write");
    }
    drop(stdin);
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

/// A commit of `files` (paths at most one directory deep).
fn commit(repo: &Path, files: &[(&str, &str)]) -> String {
    let mut top = String::new();
    let mut dirs: std::collections::BTreeMap<&str, String> = Default::default();
    for (path, content) in files {
        let blob = plumb(
            repo,
            &["hash-object", "-w", "--stdin"],
            Some(content.as_bytes()),
        );
        match path.split_once('/') {
            Some((dir, name)) => dirs
                .entry(dir)
                .or_default()
                .push_str(&format!("100644 blob {blob}\t{name}\n")),
            None => top.push_str(&format!("100644 blob {blob}\t{path}\n")),
        }
    }
    for (dir, listing) in dirs {
        let sub = plumb(repo, &["mktree"], Some(listing.as_bytes()));
        top.push_str(&format!("040000 tree {sub}\t{dir}\n"));
    }
    let tree = plumb(repo, &["mktree"], Some(top.as_bytes()));
    plumb(
        repo,
        &["commit-tree", tree.as_str(), "-m", "synthetic"],
        None,
    )
}

const OPEN_SPEC: &str = "---\nspec: quality\ntitle: Quality\nstatus: approved\n---\n\
The service is written in Rust; every module has unit tests (OPEN-MARKER-71).\n";
const SECRET_SPEC: &str = "---\nspec: pricing\ntitle: Pricing\nstatus: approved\n\
confidential: true\n---\nCustomer price list CONFIDENTIAL-MARKER-93.\n";
const BROKEN_SPEC: &str = "---\nspec: [unclosed\ntitle: Broken\n---\nUNREADABLE-MARKER-55.\n";

/// A model that answers from a script and keeps what it was sent.
struct Scripted {
    answer: Result<String, ErrorCode>,
    sent: Mutex<Vec<String>>,
}

impl Scripted {
    fn new(answer: Result<String, ErrorCode>) -> Arc<Self> {
        Arc::new(Self {
            answer,
            sent: Mutex::new(Vec::new()),
        })
    }

    fn sent(&self) -> Vec<String> {
        self.sent.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

impl LlmProvider for Scripted {
    fn chat<'a>(&'a self, req: &'a ChatRequest) -> ChatFuture<'a> {
        if let Ok(mut s) = self.sent.lock() {
            s.push(format!("{}\n{}", req.system, req.user));
        }
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

fn gateway(db: &Db, provider: Arc<Scripted>, paused: bool) -> Gateway {
    Gateway::new(
        GatewayConfig {
            model: ModelRef::parse(&format!("qwen3:27b@sha256:{}", "0".repeat(64))).expect("model"),
            num_ctx: 8192,
            concurrency: 1,
            paused,
            call_timeout: Duration::from_secs(5),
        },
        provider,
        Arc::new(DbRecords(db.clone())),
    )
}

/// The rules as the marketplace would give them, at the tip of `main`.
struct FakeRules {
    tip: String,
}

fn rec(slug: &str, kind: &str, reason: &str) -> Recommended {
    Recommended {
        slug: slug.to_owned(),
        kind: kind.to_owned(),
        version: "1.0.0".to_owned(),
        selected_by: "rule",
        reason: reason.to_owned(),
    }
}

fn entry(slug: &str, kind: &str) -> CatalogEntry {
    CatalogEntry {
        slug: slug.to_owned(),
        kind: kind.to_owned(),
        version: "1.0.0".to_owned(),
        capabilities: Vec::new(),
        requires: Vec::new(),
        files: vec![format!(".claude/{slug}.md")],
    }
}

fn found_at(tip: &str) -> RecommendedAt {
    RecommendedAt {
        tip: Some(ObjectId::parse(tip).expect("tip")),
        recommendation: Recommendation {
            parts: vec![
                rec("claude-code-v2", "adapter", "base_model"),
                rec("format-on-save", "hook", "required_by=claude-code-v2"),
            ],
            unavailable: Vec::new(),
        },
        catalog: vec![
            entry("claude-code-v2", "adapter"),
            entry("format-on-save", "hook"),
            entry("rust-testing", "skill"),
        ],
    }
}

impl RulesSource for FakeRules {
    fn recommend_at(&self, _: RepoId) -> RulesFuture<'_> {
        let found = found_at(&self.tip);
        Box::pin(async move { Ok(found) })
    }
}

struct Fixture {
    w: World,
    tip: String,
}

async fn fixture(files: &[(&str, &str)]) -> Fixture {
    let mut w = World::new().await;
    let tip = commit(&w.path, files);
    w.push("main", Some(&tip)).await;
    Fixture { w, tip }
}

async fn run_pass(f: &Fixture, gateway: &Gateway, memory: &mut Memory) -> PassCount {
    let browser = Browser::new(
        f.w.storage.git.clone(),  // clone: the browser owns its handles
        f.w.storage.root.clone(), // clone: as git
        BrowseLimits::default(),
    );
    let rules = FakeRules {
        tip: f.tip.clone(), // clone: the fake owns it
    };
    let deps = Deps {
        gateway,
        db: &f.w.t.db,
        rules: &rules,
        browser: &browser,
    };
    pass(
        &deps,
        &RequestId::parse("req-agent-recommend").expect("rid"),
        memory,
    )
    .await
    .expect("pass")
}

/// What the route answers now (the same functions it calls).
async fn reader_view(f: &Fixture) -> (ReasonsState, Option<String>, Vec<String>, bool) {
    let ask = upsert_ask(&f.w.t.db, f.w.project, Some(&f.tip), "req-reader")
        .await
        .expect("ask");
    let state = reasons_state(Some(&ask), Some(&f.tip));
    let found = found_at(&f.tip);
    let picks: Vec<_> = found
        .recommendation
        .parts
        .iter()
        .map(|p| forge_core::llm::agent_recommend::RulePick {
            slug: p.slug.clone(),
            kind: p.kind.clone(),
            version: p.version.clone(),
            reason: p.reason.clone(),
        })
        .collect();
    let findings = match (state, ask.advice_id) {
        (ReasonsState::Ready, Some(id)) => {
            let rows =
                f.w.t
                    .json_rows(&format!(
                        "SELECT findings FROM forge.llm_advice WHERE id = {id}"
                    ))
                    .await;
            findings_of(&rows[0]["findings"]).0
        }
        _ => Vec::new(),
    };
    let merged = merge(&picks, &found.catalog, &findings);
    let slugs = merged.parts.iter().map(|p| p.pick.slug.clone()).collect();
    let degraded = state != ReasonsState::Ready;
    (state, ask.error_code, slugs, degraded)
}

fn reasons_answer() -> String {
    serde_json::json!({
        "summary": "仕様は Rust のテストを求めています。",
        "findings": [
            {"severity": "low", "path": "part:format-on-save", "line": null,
             "quote": "slug: format-on-save", "message": "spec/quality.md は整形を求める。",
             "confidence": 0.7},
            {"severity": "low", "path": "part:rust-testing", "line": null,
             "quote": "slug: rust-testing", "message": "spec/quality.md は単体テストを求める。",
             "confidence": 0.9}
        ]
    })
    .to_string()
}

#[tokio::test]
async fn confidential_spec_not_sent_to_llm() {
    let f = fixture(&[
        ("spec/quality.md", OPEN_SPEC),
        ("spec/pricing.md", SECRET_SPEC),
        ("spec/broken.md", BROKEN_SPEC),
        ("README.md", "synthetic\n"),
    ])
    .await;
    upsert_ask(&f.w.t.db, f.w.project, Some(&f.tip), "req-reader")
        .await
        .expect("ask");
    let provider = Scripted::new(Ok(reasons_answer()));
    let gw = gateway(&f.w.t.db, Arc::clone(&provider), false);
    let mut memory = Memory::default();
    let count = run_pass(&f, &gw, &mut memory).await;
    assert_eq!(count.asked, 1, "{count:?}");
    assert_eq!(count.answered, 1, "{count:?}");
    let sent = provider.sent();
    assert_eq!(sent.len(), 1);
    assert!(
        sent[0].contains("OPEN-MARKER-71"),
        "the open document is given"
    );
    assert!(
        !sent[0].contains("CONFIDENTIAL-MARKER-93") && !sent[0].contains("spec/pricing.md"),
        "a confidential document never reaches the model"
    );
    assert!(
        !sent[0].contains("UNREADABLE-MARKER-55"),
        "a document whose mark cannot be read is withheld (fail closed)"
    );
    // The reader gets the reasons; the selection is the rules' one.
    let (state, _, slugs, degraded) = reader_view(&f).await;
    assert_eq!(state, ReasonsState::Ready);
    assert!(!degraded);
    assert_eq!(slugs, ["claude-code-v2", "format-on-save"]);
    // Answered for this tip: never asked again.
    let again = run_pass(&f, &gw, &mut memory).await;
    assert_eq!(again.asked, 0);
    assert_eq!(provider.sent().len(), 1);
    f.w.t.drop_now().await;
}

#[tokio::test]
async fn a_tip_with_only_confidential_specs_sends_nothing() {
    let f = fixture(&[("spec/pricing.md", SECRET_SPEC)]).await;
    upsert_ask(&f.w.t.db, f.w.project, Some(&f.tip), "req-reader")
        .await
        .expect("ask");
    let provider = Scripted::new(Ok(reasons_answer()));
    let gw = gateway(&f.w.t.db, Arc::clone(&provider), false);
    let count = run_pass(&f, &gw, &mut Memory::default()).await;
    assert_eq!(count.asked, 0);
    assert!(provider.sent().is_empty());
    let (state, _, slugs, degraded) = reader_view(&f).await;
    assert_eq!(state, ReasonsState::Refused);
    assert!(degraded);
    assert_eq!(slugs, ["claude-code-v2", "format-on-save"]);
    f.w.t.drop_now().await;
}

#[tokio::test]
async fn llm_down_returns_rules_with_degraded_flag() {
    for (answer, paused, want, cause) in [
        // Down.
        (
            Err(ErrorCode::LlmUnavailable),
            false,
            ReasonsState::Unavailable,
            Some("FG-LLM-0001"),
        ),
        // Timed out in the queue.
        (
            Err(ErrorCode::LlmQueueDeadline),
            false,
            ReasonsState::Unavailable,
            Some("FG-LLM-0004"),
        ),
        // An answer that is not the shape, twice.
        (
            Ok("I recommend everything, LGTM".to_owned()),
            false,
            ReasonsState::Unavailable,
            Some("FG-LLM-0002"),
        ),
        // Paused: nothing is queued.
        (Ok(reasons_answer()), true, ReasonsState::Paused, None),
    ] {
        let f = fixture(&[("spec/quality.md", OPEN_SPEC)]).await;
        upsert_ask(&f.w.t.db, f.w.project, Some(&f.tip), "req-reader")
            .await
            .expect("ask");
        let provider = Scripted::new(answer);
        let gw = gateway(&f.w.t.db, Arc::clone(&provider), paused);
        let count = run_pass(&f, &gw, &mut Memory::default()).await;
        assert_eq!(count.answered, 0, "{want:?}");
        assert_eq!(count.not_answered, 1, "{want:?}");
        let (state, error_code, slugs, degraded) = reader_view(&f).await;
        assert_eq!(state, want);
        assert_eq!(error_code.as_deref(), cause);
        assert!(degraded, "never shown as a success");
        // The rules' recommendation, unchanged.
        assert_eq!(slugs, ["claude-code-v2", "format-on-save"]);
        let rows =
            f.w.t
                .json_rows("SELECT count(*) AS n FROM forge.llm_advice")
                .await;
        assert_eq!(rows[0]["n"], 0, "no advice is made up");
        f.w.t.drop_now().await;
    }
}
