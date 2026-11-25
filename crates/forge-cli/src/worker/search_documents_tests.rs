//! The documents indexer (plan 20 K4) against a migrated database and a
//! provisioned bare repository: a push to the default branch writes the
//! changed documents only, another branch changes nothing, the tip's
//! classification is stored, a bound is FG-KNOW-0002 and recorded, a
//! deleted document loses its row, and a block id finds its document.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use forge_checks::doc_lint::DocLimits;
use forge_checks::doc_search::{DocumentIndexer, Indexed};
use forge_core::search::{CandidateFilter, SourceKind};
use forge_core::search_text::SearchQuery;
use forge_core::{ProjectId, RequestId};
use forge_db::search::{candidates, DocumentsRequest};
use forge_db::test_support::FreshDb;
use forge_git::browse::{BrowseLimits, Browser};

use super::*;
use crate::worker::loop_tests::Captured;
use crate::worker::provision::tests::storage;
use crate::worker::purge::tests::{project, provision};

fn rid(request: &str) -> RequestId {
    RequestId::parse(request).expect("request id")
}

fn git(dir: &Path, args: &[&str], index: Option<&Path>, input: Option<&[u8]>) -> String {
    let mut cmd = Command::new("git");
    cmd.arg("--git-dir")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "forge-test")
        .env("GIT_AUTHOR_EMAIL", "forge-test@example.invalid")
        .env("GIT_COMMITTER_NAME", "forge-test")
        .env("GIT_COMMITTER_EMAIL", "forge-test@example.invalid")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(index) = index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    let mut child = cmd.spawn().expect("git");
    let mut stdin = child.stdin.take().expect("stdin");
    if let Some(input) = input {
        stdin.write_all(input).expect("write");
    }
    drop(stdin);
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

struct World {
    t: FreshDb,
    _root: tempfile::TempDir,
    scratch: tempfile::TempDir,
    storage: Storage,
    project: ProjectId,
    repo_dir: std::path::PathBuf,
}

const NOTE_TYPE: (&str, &str) = (".forge/doctypes/note.yml", "name: note\n");

fn doc(id: &str, extra: &str, body: &str) -> String {
    format!("---\nid: {id}\ndoctype: note\ntitle: Title {id}\n{extra}---\n{body}")
}

async fn world() -> World {
    let t = FreshDb::migrated().await;
    let (root, storage) = storage();
    let (_owner, p) = project(&t, "dora").await;
    provision(&t, &storage).await;
    let repo_dir = storage.root.path_for(p.repository_id);
    World {
        t,
        _root: root,
        scratch: tempfile::tempdir().expect("scratch"),
        storage,
        project: p.project_id,
        repo_dir,
    }
}

impl World {
    /// A commit holding exactly `files` (with `parent`), on `branch`.
    fn commit(&self, branch: &str, parent: Option<&str>, files: &[(&str, &str)]) -> String {
        let index = self.scratch.path().join(format!("index-{branch}"));
        let _ = std::fs::remove_file(&index);
        for (path, content) in files {
            let blob = git(
                &self.repo_dir,
                &["hash-object", "-w", "--stdin"],
                None,
                Some(content.as_bytes()),
            );
            git(
                &self.repo_dir,
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("100644,{blob},{path}"),
                ],
                Some(&index),
                None,
            );
        }
        let tree = git(&self.repo_dir, &["write-tree"], Some(&index), None);
        let mut args = vec!["commit-tree", tree.as_str(), "-m", "synthetic"];
        if let Some(p) = parent {
            args.extend(["-p", p]);
        }
        let commit = git(&self.repo_dir, &args, None, None);
        git(
            &self.repo_dir,
            &["update-ref", &format!("refs/heads/{branch}"), &commit],
            None,
            None,
        );
        commit
    }

    fn indexer(&self) -> DocumentIndexer {
        let browse = Browser::new(
            self.storage.git.clone(),  // clone: test handles
            self.storage.root.clone(), // clone: as git
            BrowseLimits::default(),
        );
        DocumentIndexer::new(self.t.db.clone(), browse) // clone: pool handles
    }

    async fn index(&self, indexer: &DocumentIndexer) -> Indexed {
        indexer
            .run(
                DocumentsRequest {
                    project: self.project,
                    full: false,
                },
                &rid("req-docs"),
            )
            .await
            .expect("indexed")
    }

    /// `(source_key, classification, indexed_at)` of the document rows.
    async fn rows(&self) -> Vec<(String, String, String)> {
        self.t
            .json_rows(&format!(
                "SELECT source_key, classification, indexed_at::text AS at \
                 FROM forge.search_entries WHERE source_kind = 'document' \
                 AND project_id = {} ORDER BY source_key",
                self.project.get()
            ))
            .await
            .iter()
            .map(|r| {
                (
                    r["source_key"].as_str().expect("key").to_owned(),
                    r["classification"].as_str().expect("class").to_owned(),
                    r["at"].as_str().expect("at").to_owned(),
                )
            })
            .collect()
    }

    async fn state(&self) -> serde_json::Value {
        let rows = self
            .t
            .json_rows(&format!(
                "SELECT indexed_sha, state, last_error_code, attempts \
                 FROM forge.search_index_state WHERE project_id = {}",
                self.project.get()
            ))
            .await;
        rows.into_iter().next().expect("a state row")
    }
}

fn keys(rows: &[(String, String, String)]) -> Vec<&str> {
    rows.iter().map(|r| r.0.as_str()).collect()
}

#[tokio::test]
async fn push_to_default_branch_reindexes_changed_docs_only() {
    let w = world().await;
    let ix = w.indexer();
    let a1 = doc("A", "", "first\n");
    let b = doc("B", "", "stays\n");
    let c1 = w.commit("main", None, &[NOTE_TYPE, ("A.md", &a1), ("docs/B.md", &b)]);
    let first = w.index(&ix).await;
    assert!(
        matches!(
            first,
            Indexed::Written {
                entries: 2,
                full: true,
                ..
            }
        ),
        "the first index reads the whole tip: {first:?}"
    );
    let before = w.rows().await;
    assert_eq!(keys(&before), vec!["A.md", "docs/B.md"]);
    assert_eq!(w.state().await["indexed_sha"], c1.as_str());
    let a2 = doc("A", "", "second\n");
    let c2 = w.commit(
        "main",
        Some(&c1),
        &[NOTE_TYPE, ("A.md", &a2), ("docs/B.md", &b)],
    );
    let second = w.index(&ix).await;
    assert!(
        matches!(
            second,
            Indexed::Written {
                entries: 1,
                removed: 0,
                full: false,
                incomplete: false
            }
        ),
        "only the changed document: {second:?}"
    );
    let after = w.rows().await;
    assert_ne!(after[0].2, before[0].2, "A rewritten");
    assert_eq!(after[1].2, before[1].2, "B untouched");
    assert_eq!(w.state().await["indexed_sha"], c2.as_str());
    assert_eq!(w.state().await["state"], "ok");
    // The same tip again: nothing to do.
    assert_eq!(w.index(&ix).await, Indexed::UpToDate);
    w.t.drop_now().await;
}

#[tokio::test]
async fn other_branch_push_ignored() {
    let w = world().await;
    let ix = w.indexer();
    let a = doc("A", "", "main\n");
    let c1 = w.commit("main", None, &[NOTE_TYPE, ("A.md", &a)]);
    w.index(&ix).await;
    let c = doc("C", "", "feature only\n");
    w.commit(
        "feature",
        Some(&c1),
        &[NOTE_TYPE, ("A.md", &a), ("C.md", &c)],
    );
    assert_eq!(w.index(&ix).await, Indexed::UpToDate);
    assert_eq!(keys(&w.rows().await), vec!["A.md"]);
    assert_eq!(w.state().await["indexed_sha"], c1.as_str());
    w.t.drop_now().await;
}

#[tokio::test]
async fn confidential_at_tip_marked() {
    let w = world().await;
    let ix = w.indexer();
    let open = doc("A", "", "x\n");
    let c1 = w.commit("main", None, &[NOTE_TYPE, ("A.md", &open)]);
    w.index(&ix).await;
    assert_eq!(w.rows().await[0].1, "internal");
    // Marked at the tip: the row follows (an AI audience never sees it).
    let marked = doc("A", "classification: confidential\n", "x\n");
    w.commit("main", Some(&c1), &[NOTE_TYPE, ("A.md", &marked)]);
    w.index(&ix).await;
    assert_eq!(w.rows().await[0].1, "confidential");
    let q = SearchQuery::parse("title")
        .expect("query")
        .tsquery_literal();
    let agent = CandidateFilter {
        readable_documents: vec![w.project],
        exclude_confidential: true,
        ..CandidateFilter::default()
    };
    let hits = candidates(&w.t.db, &q, &agent, 0, 50).await.expect("read");
    assert!(hits.is_empty(), "{hits:?}");
    w.t.drop_now().await;
}

#[tokio::test]
async fn bound_hit_logged_as_know_c() {
    let w = world().await;
    let ix = w.indexer().with_limits(DocLimits {
        max_files: 1,
        ..DocLimits::default()
    });
    let a = doc("A", "", "x\n");
    let b = doc("B", "", "y\n");
    w.commit("main", None, &[NOTE_TYPE, ("A.md", &a), ("B.md", &b)]);
    let logs = Captured::default();
    let guard = logs.install();
    let ran = w.index(&ix).await;
    drop(guard);
    assert!(
        matches!(
            ran,
            Indexed::Written {
                incomplete: true,
                full: true,
                ..
            }
        ),
        "{ran:?}"
    );
    let hit: Vec<_> = logs
        .events()
        .into_iter()
        .filter(|e| e["fields"]["event"] == "forge.search.index_item")
        .collect();
    assert_eq!(hit.len(), 1, "{hit:?}");
    assert_eq!(hit[0]["level"], "WARN");
    assert_eq!(hit[0]["fields"]["error_code"], "FG-KNOW-0002");
    assert_eq!(hit[0]["fields"]["reason"], "read_bound");
    assert_eq!(hit[0]["fields"]["bound"], "doc_files");
    let state = w.state().await;
    assert_eq!(state["state"], "failed");
    assert_eq!(state["last_error_code"], "FG-KNOW-0002");
    assert!(state["indexed_sha"].is_null(), "the next push reads it all");
    assert_eq!(keys(&w.rows().await), vec!["A.md"], "what was read");
    w.t.drop_now().await;
}

#[tokio::test]
async fn an_unread_row_is_withheld_after_a_bound() {
    let w = world().await;
    let a = doc("A", "", "x\n");
    let b = doc("B", "", "y\n");
    let c1 = w.commit("main", None, &[NOTE_TYPE, ("A.md", &a), ("B.md", &b)]);
    w.index(&w.indexer()).await;
    let b2 = doc("B", "", "changed\n");
    w.commit(
        "main",
        Some(&c1),
        &[
            NOTE_TYPE,
            ("A.md", &a),
            ("B.md", &b2),
            ("gone-later.txt", "z"),
        ],
    );
    let bounded = w.indexer().with_limits(DocLimits {
        max_files: 1,
        ..DocLimits::default()
    });
    w.index(&bounded).await;
    let rows = w.rows().await;
    assert_eq!(keys(&rows), vec!["A.md", "B.md"]);
    assert_eq!(rows[0].1, "internal", "read");
    assert_eq!(rows[1].1, "confidential", "unread: fail closed");
    w.t.drop_now().await;
}

#[tokio::test]
async fn deleted_doc_removed() {
    let w = world().await;
    let ix = w.indexer();
    let a = doc("A", "", "x\n");
    let b = doc("B", "", "y\n");
    let c1 = w.commit("main", None, &[NOTE_TYPE, ("A.md", &a), ("B.md", &b)]);
    w.index(&ix).await;
    w.commit("main", Some(&c1), &[NOTE_TYPE, ("A.md", &a)]);
    let ran = w.index(&ix).await;
    assert!(
        matches!(
            ran,
            Indexed::Written {
                entries: 0,
                removed: 1,
                full: false,
                ..
            }
        ),
        "{ran:?}"
    );
    assert_eq!(keys(&w.rows().await), vec!["A.md"]);
    w.t.drop_now().await;
}

#[tokio::test]
async fn block_id_exact_query_hits() {
    let w = world().await;
    let ix = w.indexer();
    let spec = doc(
        "SPEC-7",
        "",
        "# 設備\n\n::: req REQ-0012\nポンプは止まる。\n:::\n",
    );
    let other = doc("SPEC-8", "", "nothing here\n");
    w.commit(
        "main",
        None,
        &[NOTE_TYPE, ("specs/SPEC-7.md", &spec), ("SPEC-8.md", &other)],
    );
    w.index(&ix).await;
    let q = SearchQuery::parse("REQ-0012")
        .expect("query")
        .tsquery_literal();
    let filter = CandidateFilter {
        readable_documents: vec![w.project],
        ..CandidateFilter::default()
    };
    let hits = candidates(&w.t.db, &q, &filter, 0, 50).await.expect("read");
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].kind, SourceKind::Document);
    assert_eq!(hits[0].key, "specs/SPEC-7.md");
    assert_eq!(hits[0].title, "Title SPEC-7");
    // Not readable as documents: nothing.
    let none = CandidateFilter {
        readable_projects: vec![w.project],
        ..CandidateFilter::default()
    };
    assert!(candidates(&w.t.db, &q, &none, 0, 50)
        .await
        .expect("read")
        .is_empty());
    w.t.drop_now().await;
}

#[tokio::test]
async fn a_failed_run_is_recorded_as_know_c() {
    let w = world().await;
    let a = doc(
        "A", "", "x
",
    );
    w.commit("main", None, &[NOTE_TYPE, ("A.md", &a)]);
    w.index(&w.indexer()).await;
    assert_eq!(w.rows().await[0].1, "internal");
    let handler = SearchDocumentsHandler::new(&w.t.db, &w.storage).expect("handler");
    let logs = Captured::default();
    let guard = logs.install();
    let cause = forge_errors::ForgeError::new(forge_errors::ErrorCode::GitTimeout)
        .with_context("operation", "ls_tree");
    handler
        .indexer
        .fail(w.project, &rid("req-docs-failed"), &cause)
        .await;
    drop(guard);
    let state = w.state().await;
    assert_eq!(state["state"], "failed");
    assert_eq!(state["last_error_code"], "FG-KNOW-0002");
    assert_eq!(
        w.rows().await[0].1,
        "confidential",
        "withheld until re-read"
    );
    let warned: Vec<_> = logs
        .events()
        .into_iter()
        .filter(|e| e["fields"]["event"] == "forge.search.index_item")
        .collect();
    assert_eq!(warned.len(), 1, "{warned:?}");
    assert_eq!(warned[0]["fields"]["reason"], "read_failed");
    assert_eq!(warned[0]["fields"]["cause_code"], "FG-GIT-0002");
    w.t.drop_now().await;
}
