//! The AI asset drafts (plan 17 §1.5 and P17-10, ADR-0039 Amendment
//! P17-10): every [`PERIOD`], for each open **draft** merge request from
//! `improve/<id>` in a project whose `ai_assets.enabled` is on (at most
//! `forge_db::llm::asset_draft::MAX_CANDIDATES`),
//!
//! 1. the improvement request is read from the description
//!    (`Improvement-Request:` / `Outlet:`, PFS-4); only a `skill` or
//!    `playbook` outlet gets a draft. A merge request whose draft branch
//!    `ai-draft/<id>` exists already got one: never again;
//! 2. the input is built by code: the cases the merge request adds or
//!    changes (none holding raw data, plan 17 §4) and the unprotected
//!    asset files of the outlet's kind (`forge_core::llm::asset_draft`),
//!    secrets redacted (`internal`);
//! 3. unless this very input is being asked about or was within
//!    [`RETRY_AFTER_SECS`], or was asked [`MAX_ASKS`] times (and the same
//!    bound in memory for a failure that left no row; only the draft
//!    branch ends the asking, so an answer whose commit failed or that
//!    proposed nothing acceptable is asked again), the gateway is asked
//!    (the autofix tier, the draft prompt, edits of the given assets only);
//! 4. the kept edits pass the commit gate (`refused_edit`, the push secret
//!    scan); the writer `Principal::asset_draft` is decided on fresh facts
//!    (`prove(RepoPush)` on `Resource::AssetDraft`: a creation of exactly
//!    `ai-draft/<id>`, the merge request still open and a draft, the
//!    setting still on) and ONE commit over the merge request's head
//!    creates the branch, recorded as a push (`repository.pushed`).
//!
//! The draft decides nothing: the merge request stays a draft, its branch
//! never moves, nothing is commented, approved or merged; a person applies
//! the proposal. The LLM being down, paused or refusing leaves the
//! improvement request open as it was (the gateway logs its own failures).
//! Runs only with the gateway (`llm.enabled`). One merge request's failure
//! never stops the others: the pass goes on and returns the first error,
//! logged once per streak with its code and `task=asset_draft`. A skip, a
//! refused input (`FG-LLM-0005`) and a refused draft (`FG-LLM-0007`) are
//! logged once per merge request and input.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use forge_auth::{prove, Action, AssetDraftCtx, AssetDraftTarget, Principal, ProjectView};
use forge_auth::{RefProtection, RefUpdate};
use forge_core::improve_draft::ai_draft_branch_for;
use forge_core::llm::asset_draft::{
    changed_cases, commit_message, draft_input, parse_request, policy, refused_edit, select_assets,
    without_raw_data, CommitFacts, DraftFile, DraftRequest, Listed, Skip, AUTHOR_EMAIL,
    AUTHOR_NAME,
};
use forge_core::llm::{Advice, Purpose, SubjectKind};
use forge_core::{
    telemetry, MergeRequestChange, ObjectId, ProjectAccessLoader as _, PushRecord, PushUpdateKind,
    PushedCommits, PushedRef, RefName, RepoId, RepositoryState, RequestId,
};
use forge_db::change::{Actor, ChangeContext};
use forge_db::llm::asset_draft::{asks, candidates, target_facts, Candidate, MAX_CANDIDATES};
use forge_db::Db;
use forge_errors::{ErrorCode, ForgeError};
use forge_git::browse::{Browser, ListedBlob, RepoPath};
use forge_git::commit::CommitIdentity;
use forge_git::file_commit::{BranchFiles, FilePath};
use forge_git::secret_scan::{scan, SecretScan};
use forge_llm::{Gateway, Outcome, Submission};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::provision::Storage;

/// The task name in the logs.
pub(crate) const NAME: &str = "asset_draft";
/// How often it looks for draft improvement merge requests.
pub(crate) const PERIOD: Duration = Duration::from_secs(60);
/// A failed ask is tried again after this long.
pub(crate) const RETRY_AFTER_SECS: i64 = 10 * 60;
/// The most asks of one input.
pub(crate) const MAX_ASKS: i64 = 3;
/// The bound of each git write and read of the draft.
const GIT_TIMEOUT: Duration = Duration::from_secs(20);
/// The most files one listing reads (the asset repository is small).
const MAX_LISTED: usize = 2_000;
/// The memory is cleared beyond this many entries (a restart forgets too).
const MAX_REMEMBERED: usize = 10_000;
/// The audit label of the writer's push.
const ACTOR_LABEL: &str = AUTHOR_NAME;
/// The writer's login-less username in its commit identity.
const AUTHOR_USERNAME: &str = "forge-ai-draft";

/// What one pass did (for the tests and the log).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct PassCount {
    pub(crate) asked: usize,
    pub(crate) proposed: usize,
    pub(crate) failed: usize,
    pub(crate) skipped: usize,
}

/// What the task remembers between passes.
#[derive(Debug, Default)]
pub(crate) struct Memory {
    /// (merge request, head, reason) logged once.
    logged: HashSet<(i64, String, String)>,
    /// (merge request, input hash) → (attempts, last attempt) of failures
    /// that left no request row (the rows bound the others).
    attempts: HashMap<(i64, String), (i64, Instant)>,
}

impl Memory {
    fn trim(&mut self) {
        if self.logged.len() > MAX_REMEMBERED {
            self.logged.clear();
        }
        if self.attempts.len() > MAX_REMEMBERED {
            self.attempts.clear();
        }
    }

    fn held(&self, key: &(i64, String), now: Instant) -> bool {
        self.attempts.get(key).is_some_and(|(n, at)| {
            *n >= MAX_ASKS
                || now.duration_since(*at) < Duration::from_secs(RETRY_AFTER_SECS.unsigned_abs())
        })
    }

    /// Whether `(mr, head, reason)` is new (then it is logged).
    fn first(&mut self, mr: i64, head: &str, reason: &str) -> bool {
        self.logged.insert((mr, head.to_owned(), reason.to_owned()))
    }
}

/// The handles one pass uses.
pub(crate) struct Deps<'a> {
    pub(crate) gateway: &'a Gateway,
    pub(crate) db: &'a Db,
    pub(crate) storage: &'a Storage,
    pub(crate) browser: &'a Browser,
}

/// One pass (module docs).
pub(crate) async fn pass(
    deps: &Deps<'_>,
    request_id: &RequestId,
    memory: &mut Memory,
) -> Result<PassCount, ForgeError> {
    let mut count = PassCount::default();
    let found = candidates(deps.db, MAX_CANDIDATES).await?;
    let mut first_error = None;
    for c in found {
        if let Err(e) = one(deps, request_id, &c, memory, &mut count).await {
            count.failed += 1;
            if first_error.is_none() {
                first_error = Some(
                    e.with_context("merge_request_id", c.merge_request.get())
                        .with_context("project_id", c.project.get()),
                );
            }
        }
    }
    memory.trim();
    match first_error {
        Some(e) => Err(e),
        None => Ok(count),
    }
}

fn stored_ref(raw: &str) -> Result<RefName, ForgeError> {
    RefName::parse(raw).map_err(|_| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "stored_branch_invalid")
    })
}

fn stored_oid(raw: &str) -> Result<ObjectId, ForgeError> {
    ObjectId::parse(raw).map_err(|_| {
        ForgeError::new(ErrorCode::Internal).with_context("reason", "stored_head_invalid")
    })
}

fn log_skip(c: &Candidate, reason: Skip, request_id: &RequestId, memory: &mut Memory) {
    if memory.first(c.merge_request.get(), &c.head_sha, reason.as_str()) {
        tracing::info!(
            target: "forge_cli",
            event = "forge.llm.asset_draft_skipped",
            request_id = %request_id,
            task = NAME,
            project_id = c.project.get(),
            merge_request_id = c.merge_request.get(),
            skip = reason.as_str(),
            "no AI asset draft is asked for this improvement merge request"
        );
    }
}

/// Log `e` once per merge request, head and code (`FG-LLM-0005`,
/// `FG-LLM-0007`, a denied write).
fn log_once(e: ForgeError, c: &Candidate, request_id: &RequestId, memory: &mut Memory) {
    let key = format!("{}:{}", e.code().id(), reason_of(&e));
    if memory.first(c.merge_request.get(), &c.head_sha, &key) {
        telemetry::log_failure(
            &e.with_context("task", NAME)
                .with_context("project_id", c.project.get())
                .with_context("merge_request_id", c.merge_request.get()),
            request_id,
        );
    }
}

fn reason_of(e: &ForgeError) -> String {
    e.context()
        .iter()
        .find(|(k, _)| *k == "reason")
        .map_or_else(String::new, |(_, v)| v.clone())
}

fn listed(blob: &ListedBlob) -> Listed {
    Listed {
        path: blob.path.clone(), // clone: the pure layer owns its listing
        oid: blob.oid.clone(),   // clone: as above
        size: blob.size,
    }
}

/// The texts of `wanted` (paths of `listing`), skipping what is not UTF-8.
async fn read_texts(
    browser: &Browser,
    repo: RepoId,
    listing: &[ListedBlob],
    wanted: &[&Listed],
) -> Result<Vec<DraftFile>, ForgeError> {
    let blobs: Vec<ListedBlob> = wanted
        .iter()
        .filter_map(|w| listing.iter().find(|b| b.path == w.path).cloned()) // clone: read below
        .collect();
    let contents = browser.read_blobs(repo, &blobs).await?;
    Ok(blobs
        .iter()
        .zip(contents)
        .filter_map(|(b, bytes)| {
            String::from_utf8(bytes).ok().map(|text| DraftFile {
                path: b.path.clone(), // clone: the file owns its path
                text,
            })
        })
        .collect())
}

async fn one(
    deps: &Deps<'_>,
    request_id: &RequestId,
    c: &Candidate,
    memory: &mut Memory,
    count: &mut PassCount,
) -> Result<(), ForgeError> {
    let source = stored_ref(&c.source_branch)?;
    let Some(draft_branch) = ai_draft_branch_for(&source) else {
        return Ok(());
    };
    let req = match parse_request(&c.description) {
        Ok(r) => r,
        Err(skip) => {
            count.skipped += 1;
            log_skip(c, skip, request_id, memory);
            return Ok(());
        }
    };
    let Some(record) = deps.db.load_project(c.project, None).await? else {
        return Ok(());
    };
    if record.repository.state != RepositoryState::Ready {
        return Ok(());
    }
    let repo = record.repository.id;
    let git = &deps.storage.git;
    let root = &deps.storage.root;
    // Proposed once: an existing draft branch is never asked about again.
    if git
        .read_ref(root, repo, &draft_branch, GIT_TIMEOUT)
        .await?
        .is_some()
    {
        return Ok(());
    }
    let head = stored_oid(&c.head_sha)?;
    let dirs = |names: &[&str]| -> Result<Vec<RepoPath>, ForgeError> {
        names
            .iter()
            .map(|d| {
                RepoPath::parse(d).map_err(|_| {
                    ForgeError::new(ErrorCode::Internal).with_context("reason", "asset_dir_invalid")
                })
            })
            .collect()
    };
    let head_listing = deps
        .browser
        .blobs_under(
            repo,
            head.as_str(),
            &dirs(&["evals", "skills", "playbooks"])?,
            MAX_LISTED,
        )
        .await?
        .blobs;
    let target = stored_ref(&c.target_branch)?;
    let base_listing = match git.read_ref(root, repo, &target, GIT_TIMEOUT).await? {
        Some(base) => {
            deps.browser
                .blobs_under(repo, base.as_str(), &dirs(&["evals"])?, MAX_LISTED)
                .await?
                .blobs
        }
        None => Vec::new(),
    };
    let head_listed: Vec<Listed> = head_listing.iter().map(listed).collect();
    let base_listed: Vec<Listed> = base_listing.iter().map(listed).collect();
    let case_list = changed_cases(&head_listed, &base_listed);
    if case_list.is_empty() {
        count.skipped += 1;
        log_skip(c, Skip::NoCase, request_id, memory);
        return Ok(());
    }
    let cases = read_texts(deps.browser, repo, &head_listing, &case_list).await?;
    let (cases, _left_out) = without_raw_data(cases);
    if cases.is_empty() {
        count.skipped += 1;
        log_skip(c, Skip::CaseRawData, request_id, memory);
        return Ok(());
    }
    let asset_list = select_assets(req.outlet, &head_listed, &cases);
    let assets = read_texts(deps.browser, repo, &head_listing, &asset_list).await?;
    if assets.is_empty() {
        count.skipped += 1;
        log_skip(c, Skip::NoAsset, request_id, memory);
        return Ok(());
    }
    let input = match draft_input(&req, &c.title, &cases, &assets) {
        Ok(i) => i,
        Err(e) => {
            // The input cannot be sent (a secret that cannot be redacted):
            // no draft; a person fixes the asset. Logged once.
            log_once(e, c, request_id, memory);
            return Ok(());
        }
    };
    let key = (c.merge_request.get(), input.sha256().to_owned());
    if memory.held(&key, Instant::now()) {
        return Ok(());
    }
    // Only the draft branch ends the asking: an answer whose commit failed,
    // that was overtaken (a new head, the merge request marked ready) or
    // that proposed nothing acceptable is asked again after the interval,
    // within the bound.
    let before = asks(deps.db, c.merge_request, input.sha256(), RETRY_AFTER_SECS).await?;
    if before.live > 0 || before.recent > 0 || before.total >= MAX_ASKS {
        return Ok(());
    }
    let earlier = forge_db::llm::last_request(
        deps.db,
        Purpose::AiAssetDraft,
        SubjectKind::MergeRequest,
        c.merge_request.get(),
        &key.1,
    )
    .await?
    .map(|(id, _)| id);
    count.asked += 1;
    let prompt = forge_llm::prompts::ai_asset_draft();
    let prompt_version = prompt.version;
    let submitted = deps
        .gateway
        .submit(Submission {
            purpose: Purpose::AiAssetDraft,
            priority: None,
            subject_kind: SubjectKind::MergeRequest,
            subject_id: Some(c.merge_request.get()),
            project: Some(c.project),
            requested_by: None,
            head_sha: Some(c.head_sha.clone()), // clone: the advice binds to it
            prompt,
            input,
            policy: policy(),
            // No deep-review rule applies to an asset draft.
            deep_review: None,
            request_id: request_id.clone(), // clone: the submission owns it
        })
        .await;
    let advised = match submitted {
        Ok(Outcome::Advised(a)) => a,
        // Nothing was queued: asked again on a later pass.
        Ok(Outcome::Paused) => return Ok(()),
        Err(e) => {
            // The gateway logged it; the improvement request stays open as
            // it was. A failure that left no row is bounded in memory.
            let last = forge_db::llm::last_request(
                deps.db,
                Purpose::AiAssetDraft,
                SubjectKind::MergeRequest,
                c.merge_request.get(),
                &key.1,
            )
            .await?
            .map(|(id, _)| id);
            if last.is_none() || last == earlier {
                let entry = memory.attempts.entry(key).or_insert((0, Instant::now()));
                entry.0 += 1;
                entry.1 = Instant::now();
            }
            count.failed += 1;
            tracing::info!(
                target: "forge_cli",
                event = "forge.llm.asset_draft_unavailable",
                request_id = %request_id,
                task = NAME,
                project_id = c.project.get(),
                merge_request_id = c.merge_request.get(),
                error_code = e.code().id(),
                "no AI asset draft now; the improvement merge request is unchanged"
            );
            return Ok(());
        }
    };
    if advised.advice.dropped_edits > 0 {
        tracing::info!(
            target: "forge_cli",
            event = "forge.llm.asset_draft_edits_dropped",
            request_id = %request_id,
            task = NAME,
            merge_request_id = c.merge_request.get(),
            llm_request_id = advised.llm_request_id,
            dropped = advised.advice.dropped_edits,
            kept = advised.advice.edits.len(),
            "the model's draft named files or contents the output check refused"
        );
    }
    let files = match committable(&req, &advised.advice, &assets) {
        Ok(f) => f,
        Err(e) => {
            log_once(
                e.with_context("llm_request_id", advised.llm_request_id)
                    .with_context("advice_id", advised.advice_id),
                c,
                request_id,
                memory,
            );
            return Ok(());
        }
    };
    // Decided again with fresh facts right before the write.
    let Some(facts) = target_facts(deps.db, c.project, c.merge_request).await? else {
        return Ok(());
    };
    if facts.head_sha.as_deref() != Some(c.head_sha.as_str()) {
        // A newer head: its own input is asked about on a later pass.
        return Ok(());
    }
    let rules = forge_db::protected_refs::rules_for(deps.db, c.project).await?;
    let target_facts = AssetDraftTarget {
        project: ProjectView {
            visibility: record.access.visibility,
            role: None,
            pending_deletion: facts.pending_deletion,
        },
        protection: RefProtection::of(&rules, &draft_branch),
        branch: draft_branch.clone(), // clone: the target owns its facts
        source_branch: stored_ref(&facts.source_branch)?,
        update: RefUpdate::Create,
        mr_open: facts.mr_open,
        mr_draft: facts.mr_draft,
        ai_assets_enabled: facts.ai_assets_enabled,
    };
    if let Err(e) = prove(
        &Principal::asset_draft(),
        Action::RepoPush,
        AssetDraftCtx::new(c.project, c.iid, target_facts),
    ) {
        // The merge request became ready or closed, the setting was turned
        // off: nothing is written. Logged once.
        log_once(e, c, request_id, memory);
        return Ok(());
    }
    let author = CommitIdentity::new(AUTHOR_NAME, AUTHOR_EMAIL, AUTHOR_USERNAME)?;
    let message = commit_message(&CommitFacts {
        request: &req,
        llm_request_id: advised.llm_request_id,
        advice_id: advised.advice_id,
        model: &deps.gateway.config().model.name,
        prompt_version,
        request_id: request_id.as_str(),
    });
    let bytes: Vec<(FilePath, &[u8])> = files
        .iter()
        .map(|(p, content)| (p.clone(), content.as_bytes())) // clone: the commit borrows its own list
        .collect();
    let written = git
        .commit_branch_files(
            root,
            repo,
            &BranchFiles {
                branch: &draft_branch,
                parent: &head,
                files: &bytes,
                message: &message,
                author: &author,
            },
            GIT_TIMEOUT,
        )
        .await;
    deps.browser.invalidate(repo);
    let commit = match written {
        Ok(commit) => commit,
        Err(e) if e.code() == ErrorCode::FileCommitPathInvalid => {
            log_once(
                ForgeError::new(ErrorCode::AiAssetDraftRefused)
                    .with_context("reason", "path_refused")
                    .with_context("llm_request_id", advised.llm_request_id)
                    .with_source(e),
                c,
                request_id,
                memory,
            );
            return Ok(());
        }
        Err(e) => {
            // Someone created the branch meanwhile: theirs stands.
            if git
                .read_ref(root, repo, &draft_branch, GIT_TIMEOUT)
                .await?
                .is_some()
            {
                return Ok(());
            }
            return Err(e);
        }
    };
    record_push(deps.db, repo, &draft_branch, &head, &commit, request_id).await;
    // ADR-0039 P17-10b: the proposal's own record (the merge request
    // screen's notice and the author's notification).
    let change = MergeRequestChange::AiDraftProposed {
        merge_request: c.merge_request,
        iid: c.iid,
        branch: draft_branch.clone(), // clone: the record owns its facts
        commit: commit.clone(),       // clone: as above
        parent: head.clone(),         // clone: as above
        files: files.iter().map(|(p, _)| p.as_str().to_owned()).collect(),
        dropped_edits: u32::try_from(advised.advice.dropped_edits).unwrap_or(u32::MAX),
        advice_id: advised.advice_id,
        llm_request_id: advised.llm_request_id,
    };
    record_proposal(deps.db, c, change, request_id).await;
    count.proposed += 1;
    tracing::info!(
        target: "forge_cli",
        event = "forge.llm.asset_draft_proposed",
        request_id = %request_id,
        task = NAME,
        project_id = c.project.get(),
        merge_request_id = c.merge_request.get(),
        llm_request_id = advised.llm_request_id,
        advice_id = advised.advice_id,
        branch = draft_branch.as_str(),
        commit = commit.as_str(),
        files = files.len(),
        "the local LLM's draft of an AI asset fix was proposed on its own branch"
    );
    Ok(())
}

/// The edits that may be committed, as (path, content); `FG-LLM-0007`
/// when none may.
fn committable(
    req: &DraftRequest,
    advice: &Advice,
    assets: &[DraftFile],
) -> Result<Vec<(FilePath, String)>, ForgeError> {
    let refused = |reason: &'static str| {
        ForgeError::new(ErrorCode::AiAssetDraftRefused)
            .with_context("reason", reason)
            .with_context("dropped", advice.dropped_edits)
    };
    if advice.edits.is_empty() {
        return Err(refused("no_edit"));
    }
    let mut out = Vec::with_capacity(advice.edits.len());
    for edit in &advice.edits {
        // The output check kept only edits of given assets.
        let original = assets
            .iter()
            .find(|a| a.path == edit.path)
            .map_or("", |a| a.text.as_str());
        if let Some(reason) = refused_edit(req.outlet, edit, original) {
            return Err(refused(reason).with_context("path", edit.path.clone()));
            // clone: the context owns it
        }
        if let SecretScan::Found(_) = scan(edit.content.as_bytes()) {
            return Err(refused("secret").with_context("path", edit.path.clone()));
            // clone: as above
        }
        let path = FilePath::parse(&edit.path).map_err(|_| {
            refused("path_refused").with_context("path", edit.path.clone()) // clone: as above
        })?;
        out.push((path, edit.content.clone())); // clone: the commit owns its content
    }
    Ok(out)
}

fn system_ctx(request_id: &RequestId) -> ChangeContext {
    let mut ctx = ChangeContext::new(Actor::System, request_id.clone()); // clone: the context owns it
    ctx.actor_label = Some(ACTOR_LABEL.to_owned());
    ctx
}

/// The branch's creation as a push (`repository.pushed`). The branch
/// stands whatever happens here: a failure is logged once.
async fn record_push(
    db: &Db,
    repo: RepoId,
    branch: &RefName,
    head: &ObjectId,
    commit: &ObjectId,
    request_id: &RequestId,
) {
    let zero = "0".repeat(commit.as_str().len());
    let Ok(old) = ObjectId::parse(&zero) else {
        return;
    };
    let record = PushRecord {
        repository_id: repo,
        updates: vec![PushedRef {
            name: branch.clone(), // clone: the record owns its name
            old,
            new: commit.clone(), // clone: as above
            kind: PushUpdateKind::Create,
            commits: vec![commit.clone()], // clone: as above
            total_commits: 1,
            total_commits_partial: false,
        }],
    };
    let commits = PushedCommits {
        commits: vec![commit.clone()], // clone: as above
        complete: true,
    };
    if let Err(e) = forge_db::repositories::record_push_with_commits(
        db,
        &system_ctx(request_id),
        &record,
        &commits,
    )
    .await
    {
        telemetry::log_failure(
            &e.with_context("phase", "asset_draft_push_record")
                .with_context("task", NAME)
                .with_context("repo_id", repo.get())
                .with_context("ref", branch.as_str().to_owned())
                .with_context("parent", head.as_str().to_owned()),
            request_id,
        );
    }
}

/// The proposal's audit row and event (`merge_request.ai_draft_proposed`).
/// The branch stands whatever happens here (it ends the asking), so a
/// failure is logged once with its code: the draft is then on its branch
/// without the screen's notice and the author's notification (ADR-0039
/// Amendment P17-10b, accepted).
async fn record_proposal(
    db: &Db,
    c: &Candidate,
    change: MergeRequestChange,
    request_id: &RequestId,
) {
    if let Err(e) =
        forge_db::llm::asset_draft::record_proposed(db, &system_ctx(request_id), c.project, change)
            .await
    {
        telemetry::log_failure(
            &e.with_context("phase", "asset_draft_proposal_record")
                .with_context("task", NAME)
                .with_context("project_id", c.project.get())
                .with_context("merge_request_id", c.merge_request.get()),
            request_id,
        );
    }
}

/// Run [`pass`] every [`PERIOD`] until `stop`.
pub(crate) fn spawn(
    gateway: Gateway,
    db: Db,
    storage: Storage,
    browser: Browser,
    mut stop: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut failing: Option<ErrorCode> = None;
        let mut memory = Memory::default();
        loop {
            if *stop.borrow() {
                return;
            }
            let request_id = RequestId::generate();
            let deps = Deps {
                gateway: &gateway,
                db: &db,
                storage: &storage,
                browser: &browser,
            };
            match pass(&deps, &request_id, &mut memory).await {
                Ok(_) => failing = None,
                Err(e) => {
                    if failing != Some(e.code()) {
                        failing = Some(e.code());
                        telemetry::log_failure(&e.with_context("task", NAME), &request_id);
                    }
                }
            }
            tokio::select! {
                () = tokio::time::sleep(PERIOD) => {}
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        return;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
#[path = "asset_draft_tests.rs"]
mod tests;
