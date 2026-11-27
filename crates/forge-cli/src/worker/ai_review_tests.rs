//! The merge request first pass (M5 L5-5) against a migrated database, a
//! scripted model and a scripted diff: the run on the head, an LLM failure
//! as `error`, a moved head discarded, an off project untouched.

use std::sync::atomic::{AtomicUsize, Ordering};

use forge_core::llm::{ChatFuture, ChatReply, ChatRequest, LlmProvider, ModelRef, ReviewMode};
use forge_core::{MergeRequestId, ProjectId};
use forge_db::projects::{self, NewProject};
use forge_db::test_support::FreshDb;
use forge_db::users::{self, NewUser};
use forge_git::browse::{DiffFile, DiffStatus};
use forge_llm::{DbRecords, GatewayConfig};

use super::*;

fn rid(s: &str) -> RequestId {
    RequestId::parse(s).expect("request id")
}

/// A model that answers from a script, counting the calls.
struct Scripted {
    answer: Result<String, ErrorCode>,
    calls: AtomicUsize,
}

impl Scripted {
    fn new(answer: Result<String, ErrorCode>) -> Arc<Self> {
        Arc::new(Self {
            answer,
            calls: AtomicUsize::new(0),
        })
    }
}

impl LlmProvider for Scripted {
    fn chat<'a>(&'a self, _: &'a ChatRequest) -> ChatFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
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

const PATCH: &str = "@@ -40,3 +40,4 @@\n     let db = state.db();\n+    let size: u32 = q.get(\"size\").unwrap().parse().unwrap();\n     Json(rows)\n";

/// One changed source file, nothing confidential.
struct OneFile;

impl ReviewSource for OneFile {
    fn load<'a>(&'a self, _: &'a ReviewCandidate, _: &'a RequestId) -> LoadFuture<'a> {
        Box::pin(async {
            Ok(Load::Ready(Box::new(Loaded {
                title: "Read the page size".to_owned(),
                description: String::new(),
                diff: Diff {
                    files: vec![DiffFile {
                        status: DiffStatus::Modified,
                        old_path: Some("src/orders.rs".to_owned()),
                        new_path: Some("src/orders.rs".to_owned()),
                        old_mode: "100644".to_owned(),
                        new_mode: "100644".to_owned(),
                        old_oid: "1".repeat(40),
                        new_oid: "2".repeat(40),
                        similarity: None,
                        binary: false,
                        patch: Some(PATCH.to_owned()),
                        omitted: None,
                    }],
                    truncated: false,
                },
                marks: Marks::Known(Confidential::default()),
            })))
        })
    }
}

struct World {
    t: FreshDb,
    project: ProjectId,
}

/// Alice's project with `llm.review` = `mode` and merge request !1 at head
/// `a…a`.
async fn world(mode: &str) -> World {
    let t = FreshDb::migrated().await;
    let sys = |s: &str| ChangeContext::new(Actor::System, rid(s));
    let alice = users::create(
        &t.db,
        &sys("r-a"),
        &NewUser::human("alice", "alice", "alice@example.test"),
    )
    .await
    .expect("alice");
    let web = projects::create(
        &t.db,
        &sys("r-p"),
        &NewProject::new("web", alice.namespace_id, "Web", alice.user_id),
    )
    .await
    .expect("project");
    let p = web.project_id.get();
    let a = alice.user_id.get();
    t.exec_superuser(&format!(
        "INSERT INTO forge.project_llm (project_id, review, changed_by) VALUES ({p}, '{mode}', {a})"
    ))
    .await;
    t.commit_audited(
        ("project", p),
        &[
            format!(
                "INSERT INTO forge.merge_request_counters (project_id, last_iid) VALUES ({p}, 1)"
            ),
            format!(
                "INSERT INTO forge.merge_requests (project_id, iid, title, author_id, \
                 source_branch, target_branch, head_sha) VALUES ({p}, 1, 'synthetic', {a}, \
                 'refs/heads/feature', 'refs/heads/main', '{}')",
                "a".repeat(40)
            ),
            format!(
                "INSERT INTO forge.merge_request_versions (merge_request_id, project_id,                    version_no, head_sha, base_sha, merge_base_sha) SELECT id, {p}, 1, head_sha,                    '{base}', '{base}' FROM forge.merge_requests WHERE project_id = {p}",
                base = "e".repeat(40)
            ),
        ],
    )
    .await;
    World {
        t,
        project: web.project_id,
    }
}

async fn runs(w: &World) -> Vec<serde_json::Value> {
    w.t.json_rows(&format!(
        "SELECT head_sha, status, conclusion, summary FROM forge.check_runs \
         WHERE project_id = {} AND name = 'forge/ai-review' ORDER BY head_sha",
        w.project.get()
    ))
    .await
}

fn answer(severity: &str) -> String {
    serde_json::json!({
        "summary": "Reads the page size from the query.",
        "findings": [{
            "severity": severity, "path": "src/orders.rs", "line": 41,
            "quote": "q.get(\"size\").unwrap()",
            "message": "A missing size panics the handler.", "confidence": 0.9
        }]
    })
    .to_string()
}

#[tokio::test]
async fn an_advisory_head_gets_one_neutral_run_with_its_findings() {
    let w = world("advisory").await;
    let model = Scripted::new(Ok(answer("critical")));
    let g = gateway(&w.t.db, Arc::clone(&model));
    let in_flight = InFlight::default();
    let found = candidates(&g, &w.t.db, &in_flight)
        .await
        .expect("candidates");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].mode, ReviewMode::Advisory);
    // Already running: a second pass does not start it again.
    assert!(candidates(&g, &w.t.db, &in_flight)
        .await
        .expect("candidates")
        .is_empty());
    let done = review_one(&g, &w.t.db, &OneFile, &found[0], &rid("r-1"))
        .await
        .expect("reviewed");
    // A critical finding in an advisory project: advice, never `failure`.
    assert_eq!(done, Reviewed::Recorded(CheckConclusion::Neutral));
    let rows = runs(&w).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["status"], "completed");
    assert_eq!(rows[0]["conclusion"], "neutral");
    let summary = rows[0]["summary"].as_str().expect("summary");
    assert!(summary.contains("AI advice (suggestion)"));
    assert!(summary.contains("A missing size panics the handler."));
    // The advice is bound to the head, the code's deep-review reasons kept.
    let advice =
        w.t.json_rows("SELECT head_sha, purpose, subject_kind, input_class FROM forge.llm_advice")
            .await;
    assert_eq!(advice.len(), 1);
    assert_eq!(advice[0]["head_sha"], "a".repeat(40));
    assert_eq!(advice[0]["purpose"], "mr_review");
    assert_eq!(advice[0]["input_class"], "internal");
    // A completed head is never asked again.
    in_flight.clear();
    assert!(candidates(&g, &w.t.db, &in_flight)
        .await
        .expect("candidates")
        .is_empty());
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_required_project_turns_a_critical_finding_red() {
    let w = world("required").await;
    let g = gateway(&w.t.db, Scripted::new(Ok(answer("critical"))));
    let in_flight = InFlight::default();
    let found = candidates(&g, &w.t.db, &in_flight)
        .await
        .expect("candidates");
    let done = review_one(&g, &w.t.db, &OneFile, &found[0], &rid("r-1"))
        .await
        .expect("reviewed");
    assert_eq!(done, Reviewed::Recorded(CheckConclusion::Failure));
    w.t.drop_now().await;
}

#[tokio::test]
async fn llm_down_sets_error_not_success() {
    let w = world("advisory").await;
    let g = gateway(&w.t.db, Scripted::new(Err(ErrorCode::LlmUnavailable)));
    let in_flight = InFlight::default();
    let found = candidates(&g, &w.t.db, &in_flight)
        .await
        .expect("candidates");
    let done = review_one(&g, &w.t.db, &OneFile, &found[0], &rid("r-down"))
        .await
        .expect("reviewed");
    assert_eq!(done, Reviewed::Failed(ErrorCode::LlmUnavailable));
    let rows = runs(&w).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["conclusion"], "error");
    let summary = rows[0]["summary"].as_str().expect("summary");
    assert!(summary.contains("FG-LLM-0001"));
    assert!(summary.contains("r-down"));
    let advice = w.t.json_rows("SELECT id FROM forge.llm_advice").await;
    assert!(advice.is_empty(), "a failure is never an advice");
    w.t.drop_now().await;
}

#[tokio::test]
async fn stale_head_result_discarded() {
    let w = world("advisory").await;
    let model = Scripted::new(Ok(answer("high")));
    let g = gateway(&w.t.db, Arc::clone(&model));
    // The candidate was read at head b…b; the merge request is at a…a now
    // (it moved while the model answered).
    let mr =
        w.t.json_rows("SELECT id FROM forge.merge_requests")
            .await
            .first()
            .and_then(|r| r["id"].as_i64())
            .expect("mr");
    let old = ReviewCandidate {
        mr: MergeRequestId::new(mr),
        project: w.project,
        iid: 1,
        head_sha: "b".repeat(40),
        mode: ReviewMode::Advisory,
    };
    let done = review_one(&g, &w.t.db, &OneFile, &old, &rid("r-old"))
        .await
        .expect("reviewed");
    assert_eq!(done, Reviewed::Discarded);
    let rows = runs(&w).await;
    // Nothing on the current head; the old head says it moved on.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["head_sha"], "b".repeat(40));
    assert_eq!(rows[0]["conclusion"], "neutral");
    let summary = rows[0]["summary"].as_str().expect("summary");
    assert!(!summary.contains("panics"), "the old answer is not shown");
    // The current head is still a candidate of its own.
    let in_flight = InFlight::default();
    let found = candidates(&g, &w.t.db, &in_flight)
        .await
        .expect("candidates");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].head_sha, "a".repeat(40));
    w.t.drop_now().await;
}

#[tokio::test]
async fn off_project_gets_no_check_row() {
    let w = world("off").await;
    let model = Scripted::new(Ok(answer("high")));
    let g = gateway(&w.t.db, Arc::clone(&model));
    let in_flight = InFlight::default();
    assert!(candidates(&g, &w.t.db, &in_flight)
        .await
        .expect("candidates")
        .is_empty());
    assert!(runs(&w).await.is_empty());
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_paused_gateway_queues_nothing() {
    let w = world("advisory").await;
    let model = Scripted::new(Ok(answer("high")));
    let g = gateway(&w.t.db, Arc::clone(&model));
    g.pause();
    let in_flight = InFlight::default();
    assert!(candidates(&g, &w.t.db, &in_flight)
        .await
        .expect("candidates")
        .is_empty());
    assert!(runs(&w).await.is_empty());
    w.t.drop_now().await;
}
