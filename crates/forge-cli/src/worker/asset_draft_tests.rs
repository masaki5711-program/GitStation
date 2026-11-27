//! The AI asset draft pass (plan 17 P17-10) against a migrated database,
//! real git and a scripted model: a draft improvement merge request gets
//! one draft branch beside it, the merge request stays an open draft, and
//! an unavailable model or a refused proposal leaves everything as it was.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use forge_core::llm::{ChatFuture, ChatReply, ChatRequest, LlmProvider, ModelRef};
use forge_git::browse::BrowseLimits;
use forge_llm::{DbRecords, GatewayConfig};
use forge_review::StoredMergeRequest;

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

const CASE: &str = r#"{
  "id": "spc-9001",
  "anomaly_class": "defect_rate_up",
  "kind": "reproduction",
  "provenance": "replica",
  "source_hash": "3333333333333333333333333333333333333333333333333333333333333333",
  "classification": "internal",
  "plant_code": "P901",
  "offset_seconds": -3600,
  "skill": "pe-quality-spc",
  "tool_responses": [{"tool": "slice_by_4axis", "rows": [{"axis": "machine", "ratio": 2.4}]}],
  "expected": {"factor": "machine", "withhold": false}
}
"#;
const SKILL: &str = "---\nname: pe-quality-spc\n---\n# SPC\nRank the material axis first.\n";
const NEW_SKILL: &str = "---\nname: pe-quality-spc\n---\n# SPC\nRank the machine axis first.\n";

/// A model that answers from a script, counting the calls.
struct Scripted {
    answer: Result<String, ErrorCode>,
    calls: Mutex<usize>,
}

impl Scripted {
    fn new(answer: Result<String, ErrorCode>) -> Arc<Self> {
        Arc::new(Self {
            answer,
            calls: Mutex::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.lock().map(|n| *n).unwrap_or(0)
    }
}

impl LlmProvider for Scripted {
    fn chat<'a>(&'a self, _: &'a ChatRequest) -> ChatFuture<'a> {
        if let Ok(mut n) = self.calls.lock() {
            *n += 1;
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

fn gateway(db: &Db, provider: Arc<Scripted>) -> Gateway {
    Gateway::new(
        GatewayConfig {
            model: ModelRef::parse(&format!("qwen3:27b@sha256:{}", "0".repeat(64))).expect("model"),
            num_ctx: 8192,
            concurrency: 1,
            paused: false,
            call_timeout: Duration::from_secs(5),
        },
        provider,
        Arc::new(DbRecords(db.clone())),
    )
}

fn answer(edits: serde_json::Value) -> String {
    serde_json::json!({
        "summary": "spc-9001 は machine の軸を先に見るべきなので、スキルの順番を変えました。",
        "findings": [],
        "edits": edits,
    })
    .to_string()
}

/// A tree of `files` (paths at most one directory deep).
fn tree(repo: &Path, files: &[(&str, &str)]) -> String {
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
    plumb(repo, &["mktree"], Some(top.as_bytes()))
}

fn commit(repo: &Path, parent: Option<&str>, files: &[(&str, &str)]) -> String {
    let t = tree(repo, files);
    let mut args = vec!["commit-tree", t.as_str(), "-m", "synthetic"];
    if let Some(p) = parent {
        args.extend(["-p", p]);
    }
    plumb(repo, &args, None)
}

struct Fixture {
    w: World,
    mr: StoredMergeRequest,
    head: String,
}

/// `main` with the skill; `improve/ir-1` adds the case (the improvement's
/// first commit); the merge request is a draft naming the request, and
/// the project's `ai_assets.enabled` is on.
async fn fixture(description: &str, ai_assets: bool) -> Fixture {
    let mut w = World::new().await;
    let base_files = [
        ("skills/pe-quality-spc.md", SKILL),
        ("agents/4m.toml", "allow = []\n"),
    ];
    let base = commit(&w.path, None, &base_files);
    w.push("main", Some(&base)).await;
    let head = commit(
        &w.path,
        Some(&base),
        &[
            ("skills/pe-quality-spc.md", SKILL),
            ("agents/4m.toml", "allow = []\n"),
            ("evals/spc-9001.json", CASE),
        ],
    );
    w.push("improve/ir-1", Some(&head)).await;
    let mr = w.open("improve/ir-1", "main").await;
    // The refresh records the head (version 1).
    w.drain().await;
    let mut statements = vec![format!(
        "UPDATE forge.merge_requests SET revision = revision + 1, draft = true, description = '{}' WHERE id = {}",
        description.replace('\'', "''"),
        mr.id.get()
    )];
    if ai_assets {
        let owner =
            w.t.json_rows("SELECT id FROM forge.users WHERE username = 'alice'")
                .await;
        let owner = owner
            .first()
            .and_then(|r| r.get("id"))
            .and_then(serde_json::Value::as_i64)
            .expect("alice");
        statements.push(format!(
            "INSERT INTO forge.project_ai_assets (project_id, enabled, changed_by) \
             VALUES ({}, true, {owner})",
            w.project.get()
        ));
    }
    w.t.commit_audited(("project", w.project.get()), &statements)
        .await;
    let f = Fixture { w, mr, head };
    let (_, _, stored_head) = mr_state(&f).await;
    assert_eq!(stored_head, f.head, "the merge request's head is recorded");
    f
}

const DESCRIPTION: &str = "Symptom: 4M missed\nImprovement-Request: ir-1\nOutlet: skill\n";

async fn run_pass(f: &Fixture, provider: &Arc<Scripted>, memory: &mut Memory) -> PassCount {
    let gateway = gateway(&f.w.t.db, Arc::clone(provider));
    let browser = Browser::new(
        f.w.storage.git.clone(),  // clone: the browser owns its handles
        f.w.storage.root.clone(), // clone: as git
        BrowseLimits::default(),
    );
    let deps = Deps {
        gateway: &gateway,
        db: &f.w.t.db,
        storage: &f.w.storage,
        browser: &browser,
    };
    pass(
        &deps,
        &RequestId::parse("req-asset-draft").expect("rid"),
        memory,
    )
    .await
    .expect("pass")
}

fn read(repo: &Path, name: &str) -> String {
    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(["for-each-ref", "--format=%(objectname)", name])
        .output()
        .expect("git");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

async fn mr_state(f: &Fixture) -> (String, bool, String) {
    let rows =
        f.w.t
            .json_rows(&format!(
                "SELECT state, draft, head_sha FROM forge.merge_requests WHERE id = {}",
                f.mr.id.get()
            ))
            .await;
    let r = rows.first().expect("row");
    (
        r["state"].as_str().unwrap_or("").to_owned(),
        r["draft"].as_bool().unwrap_or(false),
        r["head_sha"].as_str().unwrap_or("").to_owned(),
    )
}

#[tokio::test]
async fn a_draft_is_proposed_beside_the_improvement_and_the_mr_stays_a_draft() {
    let f = fixture(DESCRIPTION, true).await;
    let provider = Scripted::new(Ok(answer(serde_json::json!([
        {"path": "skills/pe-quality-spc.md", "content": NEW_SKILL}
    ]))));
    let mut memory = Memory::default();
    let count = run_pass(&f, &provider, &mut memory).await;
    assert_eq!((count.asked, count.proposed), (1, 1), "{count:?}");
    let draft = read(&f.w.path, "refs/heads/ai-draft/ir-1");
    assert!(!draft.is_empty(), "the draft branch exists");
    let parents = plumb(
        &f.w.path,
        &["rev-list", "--parents", "-n", "1", &draft],
        None,
    );
    assert_eq!(parents, format!("{draft} {}", f.head));
    let shown = plumb(
        &f.w.path,
        &["show", &format!("{draft}:skills/pe-quality-spc.md")],
        None,
    );
    assert_eq!(format!("{shown}\n"), NEW_SKILL);
    let message = plumb(&f.w.path, &["log", "-1", "--format=%B", &draft], None);
    assert!(message.contains("Improvement-Request: ir-1"), "{message}");
    // draft_never_marks_ready: the merge request and its branch are as
    // they were.
    assert_eq!(read(&f.w.path, "refs/heads/improve/ir-1"), f.head);
    assert_eq!(
        mr_state(&f).await,
        ("open".to_owned(), true, f.head.clone())
    );
    // The advice is kept on the merge request.
    let advice =
        f.w.t
            .json_rows(&format!(
                "SELECT purpose, subject_kind FROM forge.llm_advice WHERE subject_id = {}",
                f.mr.id.get()
            ))
            .await;
    assert_eq!(advice.len(), 1);
    assert_eq!(advice[0]["purpose"], "ai_asset_draft");
    // Proposed once: the next pass asks nothing.
    let again = run_pass(&f, &provider, &mut memory).await;
    assert_eq!(again.asked, 0);
    assert_eq!(provider.calls(), 1);
}

#[tokio::test]
async fn llm_down_leaves_request_open() {
    let f = fixture(DESCRIPTION, true).await;
    let provider = Scripted::new(Err(ErrorCode::LlmUnavailable));
    let mut memory = Memory::default();
    let count = run_pass(&f, &provider, &mut memory).await;
    assert_eq!(
        (count.asked, count.proposed, count.failed),
        (1, 0, 1),
        "{count:?}"
    );
    assert!(read(&f.w.path, "refs/heads/ai-draft/ir-1").is_empty());
    assert_eq!(
        mr_state(&f).await,
        ("open".to_owned(), true, f.head.clone())
    );
    let rows =
        f.w.t
            .json_rows(&format!(
                "SELECT state, error_code FROM forge.llm_requests WHERE subject_id = {}",
                f.mr.id.get()
            ))
            .await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["state"], "failed");
    assert_eq!(rows[0]["error_code"], "FG-LLM-0001");
    // Not asked again before the retry interval.
    let again = run_pass(&f, &provider, &mut memory).await;
    assert_eq!(again.asked, 0);
}

#[tokio::test]
async fn a_refused_draft_writes_no_branch() {
    let f = fixture(DESCRIPTION, true).await;
    // Only files the input did not offer: a protected path and the case.
    let provider = Scripted::new(Ok(answer(serde_json::json!([
        {"path": "agents/4m.toml", "content": "allow = ['*']\n"},
        {"path": "evals/spc-9001.json", "content": "{}\n"}
    ]))));
    let mut memory = Memory::default();
    let count = run_pass(&f, &provider, &mut memory).await;
    assert_eq!((count.asked, count.proposed), (1, 0), "{count:?}");
    assert!(read(&f.w.path, "refs/heads/ai-draft/ir-1").is_empty());
    assert_eq!(
        mr_state(&f).await,
        ("open".to_owned(), true, f.head.clone())
    );
}

#[tokio::test]
async fn a_secret_in_the_proposal_writes_no_branch() {
    let f = fixture(DESCRIPTION, true).await;
    // A synthetic key, assembled so the source holds no key-shaped literal.
    let key_id = ["AKIA", "Q7ZR2LMX7VB4NT1K"].concat();
    let secret = ["wJalrXUtnFEMI/K7MDENG", "/bPxRfiCYSYNTHETIC"].concat();
    let leaky = format!("{NEW_SKILL}{key_id} aws_secret_access_key = {secret}\n");
    let provider = Scripted::new(Ok(answer(serde_json::json!([
        {"path": "skills/pe-quality-spc.md", "content": leaky}
    ]))));
    let mut memory = Memory::default();
    let count = run_pass(&f, &provider, &mut memory).await;
    assert_eq!((count.asked, count.proposed), (1, 0), "{count:?}");
    assert!(read(&f.w.path, "refs/heads/ai-draft/ir-1").is_empty());
}

#[tokio::test]
async fn no_draft_without_the_setting_or_the_request() {
    let provider = Scripted::new(Ok(answer(serde_json::json!([
        {"path": "skills/pe-quality-spc.md", "content": NEW_SKILL}
    ]))));
    // ai_assets.enabled off: no candidate at all.
    let off = fixture(DESCRIPTION, false).await;
    let count = run_pass(&off, &provider, &mut Memory::default()).await;
    assert_eq!(count, PassCount::default());
    // A threshold outlet is no AI asset to draft.
    let threshold = fixture("Improvement-Request: ir-1\nOutlet: threshold\n", true).await;
    let count = run_pass(&threshold, &provider, &mut Memory::default()).await;
    assert_eq!((count.asked, count.skipped), (0, 1), "{count:?}");
    assert_eq!(provider.calls(), 0);
}

#[tokio::test]
async fn a_merge_request_marked_ready_gets_no_draft() {
    let f = fixture(DESCRIPTION, true).await;
    f.w.t
        .commit_audited(
            ("project", f.w.project.get()),
            &[format!(
                "UPDATE forge.merge_requests SET revision = revision + 1, draft = false WHERE id = {}",
                f.mr.id.get()
            )],
        )
        .await;
    let provider = Scripted::new(Ok(answer(serde_json::json!([]))));
    let count = run_pass(&f, &provider, &mut Memory::default()).await;
    assert_eq!(count, PassCount::default());
    assert!(read(&f.w.path, "refs/heads/ai-draft/ir-1").is_empty());
}
