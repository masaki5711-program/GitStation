//! "Merge when ready" without needless git (load test 2026-11-19, ADR-0037
//! addendum "enqueue fast refusal"): what the database alone refuses — a
//! required check not green on the head, a draft, a closed merge request,
//! unresolved discussions, a missing work item link — is answered before
//! any git process starts, with exactly the error the full decision gives;
//! and an accepted enqueue reads the changed paths from the stored merge
//! result instead of a second trial merge.

use std::future::Future;

use forge_auth::MergeRequestEdit;
use forge_errors::ForgeError;

use super::*;

impl World {
    /// `main`'s merge rule: `checks` required, discussions resolved
    /// (`talks`), a work item link (`link`).
    async fn rule(&self, checks: &[&str], talks: bool, link: bool) {
        let checks: Vec<String> = checks.iter().map(|c| format!("'{c}'")).collect();
        self.t
            .commit_audited(
                ("project", self.project.get()),
                &[
                    format!(
                        "INSERT INTO forge.protected_refs (project_id, pattern, push_min_role) \
                         VALUES ({}, 'refs/heads/main', 'developer')",
                        self.project.get()
                    ),
                    format!(
                        "INSERT INTO forge.protected_ref_merge_rules (protected_ref_id, \
                         project_id, required_approvals, required_checks, \
                         require_resolved_discussions, require_work_item_link) \
                         SELECT id, project_id, 0, ARRAY[{}]::text[], {talks}, {link} \
                         FROM forge.protected_refs WHERE project_id = {}",
                        checks.join(","),
                        self.project.get()
                    ),
                ],
            )
            .await;
    }

    /// A run of `name` on `mr`'s head, as an external reporter writes it.
    async fn head_run(
        &self,
        mr: &StoredMergeRequest,
        name: &str,
        status: &str,
        conclusion: Option<&str>,
    ) {
        let head = mr.head_sha.as_deref().expect("head");
        let conclusion = conclusion.map_or_else(|| "NULL".to_owned(), |c| format!("'{c}'"));
        let completed = if status == "completed" {
            "now()"
        } else {
            "NULL"
        };
        self.t
            .commit_audited(
                ("project", self.project.get()),
                &[format!(
                    "INSERT INTO forge.check_runs (project_id, head_sha, name, status, \
                     conclusion, completed_at, reported_by, request_id) VALUES ({p}, '{head}', \
                     '{name}', '{status}', {conclusion}, {completed}, {u}, 'req-check') \
                     ON CONFLICT (project_id, head_sha, name) DO UPDATE SET \
                     status = EXCLUDED.status, conclusion = EXCLUDED.conclusion, \
                     completed_at = EXCLUDED.completed_at, \
                     revision = forge.check_runs.revision + 1",
                    p = self.project.get(),
                    u = self.owner.get()
                )],
            )
            .await;
    }

    /// Branch `name` one commit ahead of the base and its draft merge
    /// request (checked as the worker does), by dave.
    async fn draft_mr(&mut self, name: &str) -> StoredMergeRequest {
        let file = format!("{name}.txt");
        let mut all = self.base_files.to_vec();
        all.push((file.as_str(), "x\n"));
        let head = commit(&self.path, &all, Some(&self.base));
        plumb(
            &self.path,
            &["update-ref", &format!("refs/heads/{name}"), &head],
            None,
        );
        self.browser.invalidate(self.repo);
        let proof = open_proof(
            &self.t,
            &session(self.dev),
            self.project,
            (name, "main"),
            true,
        )
        .await
        .expect("open proof");
        let mr = self
            .review
            .open(
                &request(&format!("req-open-{name}")),
                &proof,
                ready(self.repo),
                &NewFields {
                    title: format!("Draft {name}"),
                    description: String::new(),
                },
            )
            .await
            .expect("open");
        self.checked(mr.id).await;
        self.reread(&mr).await
    }

    async fn reread(&self, mr: &StoredMergeRequest) -> StoredMergeRequest {
        self.review
            .merge_request(self.project, mr.iid)
            .await
            .expect("read")
            .expect("mr")
    }
}

/// `work`, and the git processes it started on this task.
async fn counted<T>(work: impl Future<Output = T>) -> (T, u64) {
    forge_git::counting_spawns(async {
        let out = work.await;
        (out, forge_git::scoped_git_spawns())
    })
    .await
}

fn context(e: &ForgeError) -> Vec<(String, String)> {
    e.context()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn pairs(raw: &[(&str, String)]) -> Vec<(String, String)> {
    raw.iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

#[tokio::test]
async fn a_running_required_check_refuses_enqueue_without_git() {
    let mut w = world_with(DOC_BASE).await;
    w.rule(&["ci/gates"], false, false).await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    w.head_run(&a, "ci/gates", "in_progress", None).await;

    let (refused, spawns) = counted(w.enqueue(&a)).await;
    let refused = refused.expect_err("a running required check refuses");
    assert_eq!(refused.code(), ErrorCode::MergeBlocked);
    assert!(refused
        .context()
        .iter()
        .any(|(k, v)| *k == "reason" && v == "required_check_not_successful"));
    assert_eq!(spawns, 0, "no git process for what the database refuses");
    assert!(w.live_entry(&a).await.is_none());

    // Green now: the full decision accepts the same head (and reads git).
    w.head_run(&a, "ci/gates", "completed", Some("success"))
        .await;
    let (entry, spawns) = counted(w.enqueue(&a)).await;
    entry.expect("enqueued");
    assert!(spawns > 0, "the accepting path still reads git");
}

#[tokio::test]
async fn early_refusal_matches_the_full_decision() {
    let mut w = world_with(DOC_BASE).await;
    let mut cases: Vec<(&str, u64)> = Vec::new();

    // A draft.
    let d = w.draft_mr("fd").await;
    assert!(d.draft);
    let (refused, spawns) = counted(w.enqueue(&d)).await;
    let refused = refused.expect_err("draft");
    assert_eq!(refused.code(), ErrorCode::MergeBlocked);
    assert_eq!(
        context(&refused),
        pairs(&[
            ("reason", "draft".to_owned()),
            ("reasons", "draft".to_owned()),
            ("merge_request_id", d.id.get().to_string()),
        ])
    );
    cases.push(("draft", spawns));

    // A closed merge request.
    let c = w.mr("fc", &[("fc.txt", "x\n")]).await;
    let close = stored_proof(
        &w.t,
        &w.review,
        &session(w.dev),
        Action::MergeRequestClose,
        w.project,
        c.iid,
        MergeRequestEdit::default(),
    )
    .await
    .expect("close proof");
    w.review
        .close(&request("req-close-fc"), &close)
        .await
        .expect("close");
    let c = w.reread(&c).await;
    let (refused, spawns) = counted(w.enqueue(&c)).await;
    let refused = refused.expect_err("closed");
    assert_eq!(refused.code(), ErrorCode::MergeBlocked);
    assert_eq!(
        context(&refused),
        pairs(&[
            ("reason", "not_open".to_owned()),
            ("reasons", "not_open".to_owned()),
            ("merge_request_id", c.id.get().to_string()),
        ])
    );
    cases.push(("not_open", spawns));

    // A merge rule with a required check, resolved discussions and a work
    // item link, none of them met (one unresolved discussion).
    w.rule(&["ci/gates"], true, true).await;
    let b = w.mr("fb", &[("fb.txt", "x\n")]).await;
    let talk = w
        .mr_proof(&session(w.dev), Action::MergeRequestComment, &b)
        .await
        .expect("comment proof");
    w.review
        .start_discussion(
            &request("req-talk-fb"),
            &talk,
            ready(w.repo),
            "synthetic comment",
            None,
        )
        .await
        .expect("discussion");
    for (run, reason, detail) in [
        (None, "required_check_missing", "name=ci/gates".to_owned()),
        (
            Some(("in_progress", None)),
            "required_check_not_successful",
            "name=ci/gates status=in_progress conclusion=none".to_owned(),
        ),
        (
            Some(("completed", Some("failure"))),
            "required_check_not_successful",
            "name=ci/gates status=completed conclusion=failure".to_owned(),
        ),
    ] {
        if let Some((status, conclusion)) = run {
            w.head_run(&b, "ci/gates", status, conclusion).await;
        }
        let (refused, spawns) = counted(w.enqueue(&b)).await;
        let refused = refused.expect_err(reason);
        assert_eq!(refused.code(), ErrorCode::MergeBlocked, "{reason}");
        assert_eq!(
            context(&refused),
            pairs(&[
                ("reason", reason.to_owned()),
                (
                    "reasons",
                    format!("{reason},unresolved_discussions,work_item_link_missing")
                ),
                (
                    "details",
                    format!("{reason}:{detail};unresolved_discussions:count=1")
                ),
                ("merge_request_id", b.id.get().to_string()),
            ]),
            "{reason} {detail}"
        );
        cases.push((reason, spawns));
    }

    // Every case above was answered before any git process.
    for (case, spawns) in &cases {
        assert_eq!(*spawns, 0, "{case}: git processes started");
    }
}

/// The accepting path (the 201): the changed paths of the approval
/// evaluation come from the merge result the mergeability check stored
/// for (head, target tip) — no second `merge-tree`. 4 git processes: the
/// ref list (cold), `diff-tree` against the stored result, `rev-list` of
/// the range, `ls-tree` for CODEOWNERS (5 before, with `merge-tree`).
#[tokio::test]
async fn an_accepted_enqueue_reads_the_changed_paths_from_the_stored_merge_result() {
    let mut w = world_with(DOC_BASE).await;
    let a = w.mr("fa", &[("fa.txt", "x\n")]).await;
    let head = oid(a.head_sha.as_deref().expect("head"));
    let stored = forge_db::review::merge_result_of_read(&w.t.db, a.id, &head, &oid(&w.base))
        .await
        .expect("read");
    assert!(stored.is_some(), "the check stored the merge result");
    let (entry, spawns) = counted(w.enqueue(&a)).await;
    entry.expect("enqueued");
    assert_eq!(spawns, 4, "git processes of an accepted enqueue");
}
