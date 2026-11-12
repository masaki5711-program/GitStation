//! Smart HTTP push (M1 phase 9 MR4, ADR-0008 option B, ADR-0007):
//! `POST {path}.git/git-receive-pack`. The dispatcher has authenticated,
//! resolved and proved the project-level `RepoPush` **before** this
//! handler runs, so nothing of the body (no `100 Continue`) is read for a
//! caller who may not push at all. Then:
//!
//! 1. The command section is read (one body-read slot; `body_timeout`,
//!    idle timeout) and parsed strictly (`forge_git::receive_request`):
//!    ≤ 1000 commands, ≤ 1 MiB, `report-status` required, `push-cert`
//!    refused, every ref through `RefName::parse` (`FG-GIT-0005`) and the
//!    hex-like check (`FG-GIT-0011`). git's probe (`0000` only) is answered
//!    without git.
//! 2. The protection snapshot is loaded **once** (`ProtectionLoader`), each
//!    update is classified — create / delete from the zero sides; an
//!    existing tag that moves is a force; a protected branch whose new
//!    commit is already in the repository is checked with git
//!    (`Git::fast_forwards`); anything else is decided by git itself — and
//!    every ref is decided by `forge_auth::authorize_ref_updates`
//!    (`authorize()` per ref, on the project proof). Handlers never call
//!    `authorize()` directly.
//! 3. Any refusal: the rest of the body is drained (at most
//!    [`DRAIN_MAX`]), git is **not** run, and the whole push is refused
//!    in-band — `unpack ok`, `ng <ref> <FG code> <user message>` for each
//!    refused ref, `ng <ref> atomic push failure` for the others, and on
//!    side-band 2 `<FG code> <user message> (request_id: …)` — logged once
//!    (the first refusal, with `denied_refs` / `denied_codes`).
//! 4. Otherwise receive-pack runs (`-c receive.denyNonFastForwards=true`
//!    when a protected branch is updated, `receive.maxInputSize =
//!    git.http.push_max_bytes`) and gets the **original** decoded bytes
//!    verbatim, the pack streamed as it arrives (idle 60 s, total
//!    `git.http.timeout_ms`) — with one exception: a push of more than one
//!    command that updates a protected branch gets `atomic` added to its
//!    first command (git refuses a non-fast-forward per ref; atomic, the
//!    push is applied whole or not at all). Its answer streams back while
//!    forge reads the report (`forge_git::receive_report`): git's
//!    `non-fast-forward` refusal of a protected ref becomes `FG-GIT-0007`;
//!    texts that may carry server details (an `unpack` error, an `ng`
//!    reason outside git's short ones, band 3) become `FG-GIT-0014` with
//!    the request id, the storage root is scrubbed from band 2, and git's
//!    originals are logged once (`reason=git_refused`).
//! 5. Once the body is complete, receive-pack belongs to a task the
//!    request does not own (counted in `SmartHttp::push_tasks`, which
//!    `forge serve` waits for on shutdown): a client that goes away then
//!    cannot kill it in the middle of its ref updates. Before that (body
//!    still arriving) a failure or disconnect kills git; usually no ref has
//!    changed, but git may have read a whole pack before the bytes that
//!    failed, so the repository is asked (step 6, reconciliation).
//! 6. After git exits: the applied refs are those git reported `ok` — or,
//!    when there is no readable report (git failed or was killed, the
//!    answer was unreadable), those whose ref now holds the new object
//!    (`Git::ref_values`; gone for a delete). A minimal
//!    `forge.git.push_applied` line (`stage=minimal`: kinds from the zero
//!    sides, no commits) is logged at once; then the kind
//!    (`Git::fast_forwards`) and the new commits (`Git::new_commits`, ≤ 20
//!    listed per ref, all counted within 10 s for the push; past it
//!    `total_commits_partial`) are read, the `stage=final` line is logged
//!    (the replay input), then the push is recorded
//!    (`PushRecorder::record_push`, 3 attempts within 30 s, `Deduped` =
//!    success), else `FG-GIT-0012` (`PushRecord::failed`, runbook
//!    push-record-failed). If the repository cannot be read either, every
//!    command is logged once with `FG-GIT-0012 reason=reconcile_failed`.
//!
//! **The one-time history import** (2026-11-12, ADR-0008 amendment
//! "history import"): when the repository has no refs (the snapshot of
//! step 4a') and an administrator left an open allowance
//! (`forge_core::HistoryImports`), the push checks run in
//! `ScanMode::HistoryImport` — a secret only in history does not refuse.
//! Each such finding is logged once (`forge.git.push_history_finding`,
//! warn, rule / path / commit / blob); when git applied refs of an accepted
//! push, the allowance is used up and audited
//! (`repository.history_imported`, one `repository.history_secret_imported`
//! per finding), else `FG-GIT-0026` (alert). The push CLAIMS the allowance
//! atomically before its receive-pack runs (review 2026-11-12: of two
//! concurrent first pushes only one gets the relaxed scan); a push that
//! applied no ref gives the claim back; a claim kept after refs landed is
//! never re-armed. A failed claim is logged and the push is checked in full.
//!
//! A push through a `moved` tombstone never reaches this handler
//! (`FG-REPO-0005 reason=tombstone_push`, the dispatcher).

use std::collections::BTreeSet;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::header::USER_AGENT;
use axum::response::{IntoResponse, Response};
use forge_auth::{authorize_ref_updates, ProjectCtx, RefKind, RefUpdate, RefUpdateCtx};
use forge_core::{
    telemetry, ObjectId, ProjectId, ProjectRepository, PushApplied, PushRecord, PushRecorded,
    PushUpdateKind, PushedRef, Pusher, RefName, RequestId,
};
use forge_errors::{ErrorCode, ForgeError, Lang};
use forge_git::push_hook::{display_path, HookOutcome, PushCheckPolicy, PushCheckRun, Verdict};
use forge_git::push_introduced::RefSnapshot;
use forge_git::push_scan::ScanLimits;
use forge_git::receive_report::{refusal_report, ReportRewriter, RewriteTexts};
use forge_git::receive_request::{
    check_unambiguous, CommandParser, Parsed, ReceivePackRequest, MAX_COMMAND_SECTION,
};
use forge_git::smart_http::{Phase, Service};
use forge_git::{Ancestry, CommitQuery, GitStdout, GitStream};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use super::git::{
    check_content_type, git_protocol, git_response, gzip_encoded, principal_key, ready, SmartHttp,
    RECEIVE_RESULT,
};
use crate::app::AppState;
use crate::git_stream::PushBody;
use crate::registry::Dispatched;

/// The most body bytes read (and dropped) after a refusal, so the client
/// can read the answer; past it the connection is closed.
pub const DRAIN_MAX: u64 = 64 * 1024 * 1024;
/// The fast-forward check before git runs, and each of the two reads after
/// the push (kinds, commits).
pub const PUSH_CHECK_TIMEOUT: Duration = Duration::from_secs(10);
/// The bound of listing the commits a push introduced (M3 R5 round 4); past
/// it the record is `push_commits_incomplete`.
pub const INTRODUCED_TIMEOUT: Duration = Duration::from_secs(30);
/// Recording an applied push: attempts and their common budget.
pub const RECORD_ATTEMPTS: u32 = 3;
pub const RECORD_BUDGET: Duration = forge_core::PUSH_RECORD_BUDGET;
/// The part of [`RECORD_BUDGET`] kept for the final attempt: the attempts
/// before it end that much early (R5 review round 6).
pub const RECORD_FINAL_RESERVE: Duration = Duration::from_secs(10);
/// Waits between record attempts.
const RECORD_BACKOFF: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(2)];
/// Chunks of git's answer queued for the client.
const ANSWER_QUEUE: usize = 256;
/// The bytes read from git's stdout at once.
const CHUNK: usize = 64 * 1024;
/// The most bytes of git's withheld texts put into one log line.
const WITHHELD_LOG_MAX: usize = 16 * 1024;
/// git's `ng` reason when the pre-receive hook refused the push.
const HOOK_DECLINED: &str = "pre-receive hook declined";
/// The longest `User-Agent` kept for the record.
const MAX_USER_AGENT: usize = 512;
/// The bound of recording a history import (its change has its own 30 s).
pub const HISTORY_IMPORT_RECORD_TIMEOUT: Duration = Duration::from_secs(35);

/// `POST {path}.git/git-receive-pack`.
pub async fn receive_pack(state: AppState, request: Dispatched<ProjectCtx>) -> Response {
    let ctx = request.ctx().clone(); // clone: the error path outlives the request
    match receive_pack_inner(&state, request).await {
        Ok(res) => res,
        Err(err) => ctx
            .fail(err.with_context("route", "receive_pack"))
            .into_response(),
    }
}

/// What every log line of one push carries (also the simple editor's
/// commit, `routes::file_commit`, which records like a push).
#[derive(Debug, Clone)]
pub(crate) struct PushLog {
    pub(crate) request_id: RequestId,
    pub(crate) project_id: ProjectId,
    pub(crate) repo_id: i64,
    pub(crate) lang: Lang,
    /// `receive_pack`, or `single_file_commit`.
    pub(crate) route: &'static str,
}

impl PushLog {
    fn context(&self, err: ForgeError) -> ForgeError {
        err.with_context("route", self.route)
            .with_context("project_id", self.project_id.get())
            .with_context("repo_id", self.repo_id)
    }

    pub(crate) fn failure(&self, err: ForgeError) {
        telemetry::log_failure(&self.context(err), &self.request_id);
    }
}

/// One command as decided.
struct Decided {
    /// `None`: allowed; else the refusal (code, user message in context).
    denial: Option<ForgeError>,
    kind: Option<RefUpdate>,
}

async fn receive_pack_inner(
    state: &AppState,
    request: Dispatched<ProjectCtx>,
) -> Result<Response, ForgeError> {
    let (smart, repository) = ready(state, &request)?;
    let smart = Arc::clone(smart);
    let Some(push) = smart.push.clone() else {
        return Err(
            ForgeError::new(ErrorCode::Internal).with_context("reason", "push_unconfigured")
        );
    };
    check_content_type(request.headers(), Service::ReceivePack)?;
    let gzip = gzip_encoded(request.headers(), Service::ReceivePack)?;
    let protocol = git_protocol(request.headers());
    let principal = principal_key(&request)?;
    let ctx = request.ctx().clone(); // clone: the body is consumed below
    let Some(user) = request.principal().user() else {
        return Err(
            ForgeError::new(ErrorCode::Internal).with_context("reason", "git_route_without_user")
        );
    };
    let pusher = Pusher {
        request_id: ctx.request_id.clone(), // clone: the record keeps its own
        user,
        kind: request.principal().account_kind(),
        credential_id: request.credential_id(),
        source_ip: None,
        user_agent: user_agent(&request),
    };
    let project_id = *request.proof().id();
    let log = PushLog {
        request_id: ctx.request_id.clone(), // clone: the log outlives the request
        project_id,
        repo_id: repository.id.get(),
        lang: ctx.lang,
        route: "receive_pack",
    };
    let limits = smart.limits;
    let started = Instant::now();
    let (proof, _, body) = request.into_proof_and_body();
    let cap = limits
        .push_max_bytes
        .get()
        .saturating_add(MAX_COMMAND_SECTION as u64);
    let mut body = PushBody::new(
        body,
        gzip,
        cap,
        limits.idle_timeout,
        limits.body_timeout.min(limits.timeout),
    );

    // 1. The command section, while holding a body-read slot.
    let slot = smart.body_slots.acquire(&principal).await?;
    let mut buffered = Vec::new();
    let mut parser = CommandParser::new();
    let parsed = loop {
        let Some(chunk) = body.next().await? else {
            match parser.feed(&buffered)? {
                Parsed::Done(parsed) => break parsed,
                Parsed::NeedMore => {
                    return Err(ForgeError::new(ErrorCode::GitProtocolInvalid)
                        .with_context("reason", "pkt_line")
                        .with_context("detail", "truncated")
                        .with_context("service", Service::ReceivePack.name()))
                }
            }
        };
        buffered.extend_from_slice(&chunk);
        if let Parsed::Done(parsed) = parser.feed(&buffered)? {
            break parsed;
        }
    };
    if parsed.commands.is_empty() {
        // git's probe before a large push: nothing to decide or run.
        if buffered.len() > parsed.section_len || body.next().await?.is_some_and(|c| !c.is_empty())
        {
            return Err(ForgeError::new(ErrorCode::GitProtocolInvalid)
                .with_context("reason", "command_invalid")
                .with_context("detail", "data_after_empty_section")
                .with_context("service", Service::ReceivePack.name()));
        }
        return Ok(git_response(RECEIVE_RESULT, Body::empty()));
    }

    // 2. One protection snapshot, every ref decided.
    let rules = match tokio::time::timeout(
        state.lookup_timeout,
        push.protection.load_protection(project_id),
    )
    .await
    {
        Ok(rules) => rules?,
        Err(_) => {
            return Err(ForgeError::new(ErrorCode::RequestTimeout)
                .with_context("operation", "protection_load")
                .with_context("timeout_ms", state.lookup_timeout.as_millis()))
        }
    };
    let decided = decide(&smart, repository, &parsed, &rules, &proof).await?;
    let protected: BTreeSet<String> = parsed
        .commands
        .iter()
        .filter_map(|c| c.name.as_ref().ok())
        .filter(|n| RefKind::of(n) == RefKind::Branch && !rules.levels(n).is_empty())
        .map(|n| n.as_str().to_owned())
        .collect();

    // 4a. git refuses a non-fast-forward per ref; with more than one
    // command a push updating a protected branch becomes atomic, so it is
    // applied as a whole or not at all (as forge's own refusals are).
    let deny_nff = parsed.commands.iter().any(|c| {
        !c.is_delete()
            && c.name
                .as_ref()
                .is_ok_and(|n| protected.contains(n.as_str()))
    });
    let mut decided = decided;
    if deny_nff && parsed.commands.len() > 1 && !parsed.atomic() {
        match parsed.with_atomic(&buffered) {
            Ok(atomic) => buffered = atomic,
            // A first command too long to take ` atomic`: refused in-band
            // with the push, like the other pre-checks.
            Err(e) if e.code() == ErrorCode::GitProtocolInvalid => {
                if let Some(first) = decided.first_mut() {
                    first.denial = Some(e);
                }
            }
            Err(e) => return Err(e),
        }
    }

    // 3. Any refusal refuses the whole push, before git runs.
    if decided.iter().any(|d| d.denial.is_some()) {
        let drained = body.drain(DRAIN_MAX).await;
        drop(slot);
        return refuse(&log, &parsed, decided, drained.err());
    }

    // 4a'. The ref tips before receive-pack runs (M3 R5 review round 5,
    // ADR-0024 §8.3): the commits the push introduces are listed against
    // them, never against the live refs. A failure does not stop the push:
    // its listing is recorded incomplete (one FG-GIT-0016 warn then).
    let snapshot_started = Instant::now();
    let before = smart
        .git
        .ref_snapshot(
            &smart.storage,
            repository.id,
            &principal,
            PUSH_CHECK_TIMEOUT,
        )
        .await;
    let snapshot_ms = u64::try_from(snapshot_started.elapsed().as_millis()).unwrap_or(u64::MAX);

    // 4b'. The one-time history import: an empty repository whose open
    // allowance this push claimed (2026-11-12). Anything else is checked in
    // full. From here every way out before the task owns the push gives the
    // claim back.
    let history_import = history_import(state, &smart, repository.id, &before, &log).await;
    let launched = async {
        // 4b. The push checks (M3 X8): one run of the pre-receive hook.
        let checks = match &smart.push_checks {
            Some(checks) => Some(
                smart.git.push_check_run(
                    &smart.storage,
                    &checks.exe,
                    &PushCheckPolicy {
                        request_id: log.request_id.to_string(),
                        lang: log.lang,
                        repo_id: repository.id,
                        limits: checks
                            .limits
                            .unwrap_or_else(|| ScanLimits::new(limits.push_max_blob_bytes)),
                        history_import: history_import.is_some(),
                    },
                )?,
            ),
            None => None,
        };

        // 4. receive-pack gets the original bytes (`atomic` added above).
        let mut cmd = smart.git.receive_pack(
            &smart.storage,
            repository.id,
            Phase::Rpc,
            protocol.as_deref(),
            limits.push_max_bytes,
        )?;
        if deny_nff {
            cmd = cmd.deny_non_fast_forwards();
        }
        if let Some(run) = &checks {
            cmd = cmd.with_push_checks(run);
        }
        let mut stream = cmd
            .idle_timeout(limits.idle_timeout)
            .timeout(limits.timeout)
            .spawn_stream(&principal)
            .await?;
        let (Some(stdin), Some(stdout)) = (stream.take_stdin(), stream.take_stdout()) else {
            return Err(
                ForgeError::new(ErrorCode::Internal).with_context("reason", "stream_pipes_missing")
            );
        };
        Ok::<_, ForgeError>((checks, stream, stdin, stdout))
    }
    .await;
    drop(slot);
    let (checks, stream, mut stdin, stdout) = match launched {
        Ok(launched) => launched,
        Err(err) => {
            if let Some(imports) = &history_import {
                release_claim(imports.as_ref(), repository.id, &log).await;
            }
            return Err(err);
        }
    };
    let (answer_tx, answer_rx) = mpsc::channel(ANSWER_QUEUE);
    let (body_tx, body_rx) = oneshot::channel::<BodyEnd>();
    let texts = RewriteTexts {
        force_refused: format!(
            "{} {}",
            ErrorCode::ProtectedRefForcePush.id(),
            ErrorCode::ProtectedRefForcePush.user_message(log.lang)
        ),
        withheld: format!(
            "{} {} (request_id: {})",
            ErrorCode::GitProtocolInvalid.id(),
            ErrorCode::GitProtocolInvalid.user_message(log.lang),
            log.request_id
        ),
        scrub: vec![smart.storage.path().to_string_lossy().into_owned()],
    };
    let task = Receiver {
        smart: Arc::clone(&smart),
        recorder: Arc::clone(&push.recorder),
        repository,
        principal,
        pusher,
        log: log.clone(), // clone: the handler logs too
        commands: parsed
            .commands
            .iter()
            .map(|c| (c.raw_name.clone(), c.old.clone(), c.new.clone())) // clone: the task owns its copy
            .collect(),
        rewriter: ReportRewriter::new(parsed.sideband, protected, texts),
        answer: Some(answer_tx),
        bytes_in: 0,
        before,
        snapshot_ms,
        checks,
        hook_declined: false,
        history_import,
    };
    // Counted before the spawn: `wait_idle` never sees 0 in between.
    let guard = smart.push_tasks.start();
    tokio::spawn(task.run(stream, stdout, body_rx, guard));

    // The body, verbatim, to git (the pack streamed as it arrives). A
    // failure here ends the request; dropping `body_tx` kills git.
    body.set_deadline(
        limits.timeout.saturating_sub(started.elapsed()),
        Instant::now(),
    );
    let mut git_reading = true;
    if stdin.write_all(&buffered).await.is_err() {
        git_reading = false;
    }
    drop(buffered);
    loop {
        let chunk = match body.next().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(err) => {
                // Dropping `body_tx` unsent kills git (the pack is usually
                // incomplete; the task then reconciles from the repository).
                drop(body_tx);
                return Err(err);
            }
        };
        if git_reading && stdin.write_all(&chunk).await.is_err() {
            // git stopped reading (e.g. index-pack refused the pack): keep
            // reading the body so the client gets git's report.
            git_reading = false;
        }
    }
    if git_reading {
        // A failed close means git stopped reading; its report says why.
        let _ = stdin.shutdown().await; // ignore: see above
    }
    drop(stdin);
    // From here receive-pack is the task's: a disconnect cannot kill it.
    let _ = body_tx.send(BodyEnd {
        bytes_in: body.raw_bytes(),
    }); // ignore: Err = the task already ended (git exited), nothing to tell
    Ok(git_response(RECEIVE_RESULT, answer_body(answer_rx)))
}

/// The allowance store when this push is the one-time history import: push
/// checks on, the repository without refs (a snapshot that failed is not
/// empty), and this push CLAIMED the open allowance (atomic in the store:
/// of concurrent pushes exactly one wins; review 2026-11-12). A failed
/// claim is logged and answers `None` (the push is then checked in full).
async fn history_import(
    state: &AppState,
    smart: &SmartHttp,
    repo: forge_core::RepoId,
    before: &Result<RefSnapshot, ForgeError>,
    log: &PushLog,
) -> Option<Arc<dyn forge_core::HistoryImports>> {
    let imports = smart.history_imports.as_ref()?;
    smart.push_checks.as_ref()?;
    if !before.as_ref().is_ok_and(|s| s.refs == 0) {
        return None;
    }
    let lookup = tokio::time::timeout(
        state.lookup_timeout,
        imports.claim_import(repo, &log.request_id),
    )
    .await;
    match lookup {
        Ok(Ok(true)) => {
            tracing::info!(
                target: "forge_web::git",
                request_id = %log.request_id,
                project_id = log.project_id.get(),
                repo_id = log.repo_id,
                "forge.git.history_import_armed"
            );
            Some(Arc::clone(imports))
        }
        Ok(Ok(false)) => None,
        Ok(Err(err)) => {
            log.failure(err.with_context("operation", "history_import_claim"));
            None
        }
        Err(_) => {
            log.failure(
                ForgeError::new(ErrorCode::RequestTimeout)
                    .with_context("operation", "history_import_claim")
                    .with_context("timeout_ms", state.lookup_timeout.as_millis()),
            );
            None
        }
    }
}

/// Give back this push's claim of the history import (it applied no ref):
/// the allowance is open again. A failure is logged (FG-GIT-0026
/// `reason=release_failed`): the allowance then stays claimed, never
/// re-armed (fail closed; an operator allows anew).
async fn release_claim(
    imports: &dyn forge_core::HistoryImports,
    repo: forge_core::RepoId,
    log: &PushLog,
) {
    let released = tokio::time::timeout(
        HISTORY_IMPORT_RECORD_TIMEOUT,
        imports.release_import(repo, &log.request_id),
    )
    .await;
    let err = match released {
        Ok(Ok(())) => {
            tracing::info!(
                target: "forge_web::git",
                request_id = %log.request_id,
                project_id = log.project_id.get(),
                repo_id = log.repo_id,
                "forge.git.history_import_released"
            );
            return;
        }
        Ok(Err(err)) => ForgeError::new(ErrorCode::HistoryImportRecordFailed)
            .with_context("reason", "release_failed")
            .with_context("cause_code", err.code().id())
            .with_source(err),
        Err(_) => ForgeError::new(ErrorCode::HistoryImportRecordFailed)
            .with_context("reason", "release_failed")
            .with_context("timeout_ms", HISTORY_IMPORT_RECORD_TIMEOUT.as_millis()),
    };
    log.failure(err);
}

/// Classify every command and decide it (see the module docs, step 2).
async fn decide(
    smart: &SmartHttp,
    repository: ProjectRepository,
    parsed: &ReceivePackRequest,
    rules: &forge_core::ProtectionRules,
    proof: &forge_auth::Authorized<ProjectCtx>,
) -> Result<Vec<Decided>, ForgeError> {
    // Protected branches with an existing new commit: ask git.
    let mut pairs = Vec::new();
    let mut pair_of = Vec::with_capacity(parsed.commands.len());
    for c in &parsed.commands {
        let needs_check = c
            .name
            .as_ref()
            .is_ok_and(|n| RefKind::of(n) == RefKind::Branch && !rules.levels(n).is_empty())
            && !c.is_create()
            && !c.is_delete();
        if needs_check {
            pair_of.push(Some(pairs.len()));
            pairs.push((c.old.clone(), c.new.clone())); // clone: git reads its own copy
        } else {
            pair_of.push(None);
        }
    }
    let ancestry = smart
        .git
        .fast_forwards(&smart.storage, repository.id, &pairs, PUSH_CHECK_TIMEOUT)
        .await?;
    let mut ctxs = Vec::new();
    let mut ctx_of = Vec::with_capacity(parsed.commands.len());
    let mut decided = Vec::with_capacity(parsed.commands.len());
    for (c, pair) in parsed.commands.iter().zip(&pair_of) {
        let Ok(name) = &c.name else {
            // The parser's refusal again, with its context (FG-GIT-0005 /
            // FG-GIT-0011): the parsed error stays with the request.
            let denial = RefName::parse(&c.raw_name)
                .and_then(|n| check_unambiguous(&n))
                .err()
                .unwrap_or_else(|| {
                    ForgeError::new(ErrorCode::Internal)
                        .with_context("reason", "ref_name_refusal_lost")
                });
            ctx_of.push(None);
            decided.push(Decided {
                denial: Some(denial),
                kind: None,
            });
            continue;
        };
        let kind = if c.is_create() {
            RefUpdate::Create
        } else if c.is_delete() {
            RefUpdate::Delete
        } else if RefKind::of(name) == RefKind::Tag {
            // A tag that exists already never moves forward: any update rewrites it.
            RefUpdate::Force
        } else {
            match pair.and_then(|i| ancestry.get(i).copied()) {
                Some(Ancestry::NotFastForward) => RefUpdate::Force,
                // Unknown: the commit arrives in the pack; git decides
                // (receive.denyNonFastForwards on a protected branch).
                Some(Ancestry::FastForward | Ancestry::Unknown) | None => RefUpdate::FastForward,
            }
        };
        ctx_of.push(Some(ctxs.len()));
        ctxs.push(RefUpdateCtx::new(name.clone(), kind, rules)); // clone: the context owns its name
        decided.push(Decided {
            denial: None,
            kind: Some(kind),
        });
    }
    let mut results: Vec<Option<ForgeError>> = authorize_ref_updates(proof, &ctxs)?
        .into_iter()
        .map(Result::err)
        .collect();
    for (d, i) in decided.iter_mut().zip(ctx_of) {
        if let Some(slot) = i.and_then(|i| results.get_mut(i)) {
            d.denial = slot.take();
        }
    }
    Ok(decided)
}

/// Step 3: the whole push refused in-band.
fn refuse(
    log: &PushLog,
    parsed: &ReceivePackRequest,
    decided: Vec<Decided>,
    drain_error: Option<ForgeError>,
) -> Result<Response, ForgeError> {
    let lang = log.lang;
    let mut refs = Vec::with_capacity(parsed.commands.len());
    let mut codes: Vec<ErrorCode> = Vec::new();
    let mut first: Option<(ForgeError, String)> = None;
    let mut denied = 0usize;
    for (c, d) in parsed.commands.iter().zip(decided) {
        match d.denial {
            Some(e) => {
                denied += 1;
                let code = e.code();
                if !codes.contains(&code) {
                    codes.push(code);
                }
                refs.push((
                    c.raw_name.clone(), // clone: the report owns its lines
                    format!("{} {}", code.id(), code.user_message(lang)),
                ));
                if first.is_none() {
                    first = Some((e, kind_name(d.kind)));
                }
            }
            None => refs.push((c.raw_name.clone(), "atomic push failure".to_owned())), // clone: as above
        }
    }
    let messages: Vec<String> = codes
        .iter()
        .map(|code| {
            format!(
                "{} {} (request_id: {})",
                code.id(),
                code.user_message(lang),
                log.request_id
            )
        })
        .collect();
    let answer = refusal_report(parsed.sideband, &messages, &refs)?;
    if let Some((err, kind)) = first {
        let ids: Vec<&str> = codes.iter().map(|c| c.id()).collect();
        let mut err = err
            .with_context("refs", parsed.commands.len())
            .with_context("denied_refs", denied)
            .with_context("denied_codes", ids.join(","))
            .with_context("first_denied_kind", kind);
        if let Some(drain) = drain_error {
            err = err
                .with_context("drain_error", drain.code().id())
                .with_context("drain", drain.to_string());
        }
        log.failure(err);
    }
    Ok(git_response(RECEIVE_RESULT, Body::from(answer)))
}

fn kind_name(kind: Option<RefUpdate>) -> String {
    match kind {
        Some(RefUpdate::Create) => "create",
        Some(RefUpdate::FastForward) => "fast_forward",
        Some(RefUpdate::Force) => "force",
        Some(RefUpdate::Delete) => "delete",
        None => "invalid_name",
    }
    .to_owned()
}

/// The `User-Agent` for the record (cut, at a character boundary).
pub(crate) fn user_agent<B>(request: &Dispatched<ProjectCtx, B>) -> Option<String> {
    let value = request.headers().get(USER_AGENT)?.to_str().ok()?;
    let mut cut = value.len().min(MAX_USER_AGENT);
    while !value.is_char_boundary(cut) {
        cut -= 1;
    }
    Some(value[..cut].to_owned())
}

/// The response body: git's (rewritten) answer as the task queues it. An
/// `Err` item cuts the response (git failed after the headers were sent).
fn answer_body(rx: mpsc::Receiver<Result<Bytes, io::Error>>) -> Body {
    Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }))
}

/// The record of `applied` without asking git: create / delete from the
/// zero sides, every other update `force` (not classified yet), no
/// commits, counts partial.
fn minimal_record(
    repository: forge_core::RepoId,
    applied: &[&(String, String, String)],
) -> Result<PushRecord, ForgeError> {
    let mut updates = Vec::with_capacity(applied.len());
    for (name, old, new) in applied {
        let (old, new) = (ObjectId::parse(old)?, ObjectId::parse(new)?);
        let kind = if old.is_zero() {
            PushUpdateKind::Create
        } else if new.is_zero() {
            PushUpdateKind::Delete
        } else {
            PushUpdateKind::Force
        };
        updates.push(PushedRef {
            name: RefName::parse(name)?,
            old,
            new,
            kind,
            commits: Vec::new(),
            total_commits: 0,
            total_commits_partial: kind != PushUpdateKind::Delete,
        });
    }
    Ok(PushRecord {
        repository_id: repository,
        updates,
    })
}

/// The handler finished the body.
struct BodyEnd {
    bytes_in: u64,
}

/// Drops the repository's cached browse ref list when the push task ends
/// (M2 MR7; every way it ends).
struct RefsChanged(forge_git::browse::Browser, forge_core::RepoId);

impl Drop for RefsChanged {
    fn drop(&mut self) {
        self.0.invalidate(self.1);
    }
}

/// receive-pack once it runs: forwards its answer, then records the push
/// (steps 4–6). Owned by its own task, not by the request.
struct Receiver {
    smart: Arc<SmartHttp>,
    recorder: Arc<dyn forge_core::PushRecorder>,
    repository: ProjectRepository,
    principal: String,
    pusher: Pusher,
    log: PushLog,
    /// `(name as sent, old, new)` in request order.
    commands: Vec<(String, String, String)>,
    rewriter: ReportRewriter,
    answer: Option<mpsc::Sender<Result<Bytes, io::Error>>>,
    /// Request body bytes (raw), once the handler finished it.
    bytes_in: u64,
    /// The ref tips read before receive-pack started (taken by the listing).
    before: Result<RefSnapshot, ForgeError>,
    /// How long the snapshot took.
    snapshot_ms: u64,
    /// This push's pre-receive check run (M3 X8), read once git exited.
    checks: Option<PushCheckRun>,
    /// git reported `pre-receive hook declined`.
    hook_declined: bool,
    /// This push is the one-time history import (its allowance store).
    history_import: Option<Arc<dyn forge_core::HistoryImports>>,
}

impl Receiver {
    async fn run(
        mut self,
        stream: GitStream,
        mut stdout: GitStdout,
        mut body: oneshot::Receiver<BodyEnd>,
        _task: super::git::PushTask,
    ) {
        // Whatever happens below, the refs may have changed: the browse
        // ref cache of this repository is dropped when the push ends.
        let _refs_changed = RefsChanged(self.smart.browse.clone(), self.repository.id); // clone: a shared handle
        let mut body_done = false;
        let mut buf = vec![0u8; CHUNK];
        let read_error = loop {
            tokio::select! {
                end = &mut body, if !body_done => {
                    let Ok(end) = end else {
                        // The request ended before the body did. Kill git.
                        // Usually no ref has changed (the pack is incomplete),
                        // but git may have read a whole pack before bytes
                        // that came after it failed: ask the repository.
                        drop(stdout);
                        let err = stream.abort_client_disconnected().await;
                        self.log.failure(err.with_context("phase", "request_body"));
                        self.answer = None;
                        self.reconcile("killed").await;
                        return;
                    };
                    self.bytes_in = end.bytes_in;
                    body_done = true;
                }
                read = stdout.read(&mut buf) => match read {
                    Ok(0) => break None,
                    Ok(n) => {
                        let out = self.rewriter.feed(buf.get(..n).unwrap_or_default());
                        self.send(out).await;
                    }
                    Err(e) => break Some(e),
                },
            }
        };
        drop(stdout);
        // receive-pack closed its output: the refs are updated (or not).
        // Drop the browse ref cache before the final answer reaches the
        // client, so a browse request right after the push sees them;
        // `_refs_changed` stays the backstop for every other way out.
        self.smart.browse.invalidate(self.repository.id);
        let rewriter = std::mem::replace(
            &mut self.rewriter,
            ReportRewriter::new(false, BTreeSet::new(), RewriteTexts::default()),
        );
        let mut withheld = rewriter.withheld().to_vec();
        let (rest, report) = rewriter.finish();
        self.send(rest).await;
        if !body_done {
            // git ended first (e.g. it refused the pack); the handler is
            // still reading the body and will close it. Wait for that, so
            // the answer is not cut before the client reads it.
            match body.await {
                Ok(end) => self.bytes_in = end.bytes_in,
                // The request went away meanwhile: nothing left to answer.
                Err(_) => self.answer = None,
            }
        }
        if let Ok(r) = &report {
            withheld.extend(r.withheld.iter().cloned()); // clone: logged, the report is used below
        }
        self.log_withheld(&withheld);
        let finished = stream.finish().await;
        let report = match (finished, read_error, report) {
            (Err(err), _, report) => {
                // The answer is cut: the client sees a failed push, never a
                // silently short one. A ref git reported `ok` before it
                // failed is still recorded below; without a report the
                // repository says what changed.
                self.cut().await;
                self.log.failure(
                    err.with_context("reported_ok", report.as_ref().map_or(0, |r| r.ok.len())),
                );
                match report {
                    Ok(r) => r,
                    Err(_) => {
                        self.reconcile("git_failed").await;
                        return;
                    }
                }
            }
            (Ok(_), Some(e), _) => {
                self.cut().await;
                self.log.failure(
                    ForgeError::new(ErrorCode::GitProcessFailed)
                        .with_context("reason", "stdout_read_failed")
                        .with_context("subcommand", "receive-pack")
                        .with_source(e),
                );
                self.reconcile("stdout_read_failed").await;
                return;
            }
            (Ok(_), None, Err(reason)) => {
                self.answer = None;
                self.log.failure(
                    ForgeError::new(ErrorCode::GitProcessFailed)
                        .with_context("reason", "report_unreadable")
                        .with_context("detail", reason)
                        .with_context("subcommand", "receive-pack")
                        .with_context("refs", self.commands.len()),
                );
                self.reconcile("report_unreadable").await;
                return;
            }
            (Ok(_), None, Ok(report)) => report,
        };
        // The answer is complete: end the response before recording.
        self.answer = None;
        if !report.rewritten.is_empty() {
            self.log.failure(
                ForgeError::new(ErrorCode::ProtectedRefForcePush)
                    .with_context("reason", "git_denied_non_fast_forward")
                    .with_context("refs", report.rewritten.len()),
            );
        }
        self.hook_declined = report.ng.iter().any(|(_, reason)| reason == HOOK_DECLINED);
        let applied: Vec<(String, String, String)> = self
            .commands
            .iter()
            .filter(|(name, _, _)| report.ok.iter().any(|ok| ok == name))
            .cloned() // clone: the task records its own copy
            .collect();
        self.served(applied.len(), report.ng.len(), report.unpack_ok, "report");
        self.after_push(applied, "report").await;
    }

    /// What this push's pre-receive check decided (M3 X8), logged once,
    /// then its run directory removed. `applied`: the refs git updated, or
    /// `None` when the repository could not say (reconciliation failed).
    /// No result of this run while refs changed means git skipped the
    /// hook: `FG-GIT-0023 reason=hook_not_run` (the refs were not checked);
    /// unknown refs and no result: `reason=hook_unknown` (the same alert,
    /// never hidden as "nothing applied").
    fn push_checked(&mut self, applied: Option<usize>) -> Option<HookOutcome> {
        let run = self.checks.take()?;
        let applied_text = applied.map_or_else(|| "unknown".to_owned(), |n| n.to_string());
        let fail = |reason: &'static str| {
            ForgeError::new(ErrorCode::PushCheckFailed)
                .with_context("reason", reason)
                .with_context("event", "forge.git.push_checked")
                .with_context("applied", &applied_text)
        };
        let mut checked = None;
        match (run.outcome(), applied) {
            (Ok(Some(outcome)), _) => {
                self.log_checked(&outcome, &applied_text);
                checked = Some(outcome);
            }
            (Ok(None), None) => self.log.failure(fail("hook_unknown")),
            (Ok(None), Some(n)) if n > 0 => self.log.failure(fail("hook_not_run")),
            (Ok(None), Some(_)) if self.hook_declined => {
                self.log.failure(fail("hook_no_result"));
            }
            // git stopped before the hook (an unpack error): nothing ran.
            (Ok(None), Some(_)) => {}
            (Err(err), _) => self.log.failure(err.with_context("applied", &applied_text)),
        }
        if let Err(err) = run.remove() {
            self.log.failure(err);
        }
        checked
    }

    /// An accepted history import whose refs git applied: use the
    /// allowance up and audit its history findings (one change), else
    /// `FG-GIT-0026` (the findings are in the log lines already).
    async fn history_imported(&self, outcome: &HookOutcome) {
        let Some(imports) = &self.history_import else {
            return;
        };
        if outcome.verdict != Verdict::Accepted {
            return;
        }
        let r = &outcome.report;
        let used = forge_core::HistoryImportUse {
            findings: r
                .history
                .iter()
                .map(|f| forge_core::HistoryFinding {
                    rule: f.kind.rule().to_owned(),
                    blob: f.blob.clone(),     // clone: the record owns its copy
                    path: f.path.clone(),     // clone: as above
                    commit: f.commit.clone(), // clone: as above
                })
                .collect(),
            total: r.total_history,
        };
        let recorded = tokio::time::timeout(
            HISTORY_IMPORT_RECORD_TIMEOUT,
            imports.record_import(&self.pusher, self.repository.id, &used),
        )
        .await;
        let failed = |reason: &'static str| {
            ForgeError::new(ErrorCode::HistoryImportRecordFailed)
                .with_context("reason", reason)
                .with_context("history_findings", r.total_history)
        };
        match recorded {
            Ok(Ok(done)) => tracing::info!(
                target: "forge_web::git",
                request_id = %self.log.request_id,
                project_id = self.log.project_id.get(),
                repo_id = self.log.repo_id,
                history_findings = r.total_history,
                consumed = done == forge_core::HistoryImportRecorded::Consumed,
                "forge.git.history_imported"
            ),
            Ok(Err(err)) => self.log.failure(
                failed("record_failed")
                    .with_context("cause_code", err.code().id())
                    .with_source(err),
            ),
            Err(_) => self.log.failure(
                failed("timeout")
                    .with_context("timeout_ms", HISTORY_IMPORT_RECORD_TIMEOUT.as_millis()),
            ),
        }
    }

    fn log_checked(&self, outcome: &HookOutcome, applied: &str) {
        let r = &outcome.report;
        // The history import (2026-11-12): every secret only in history,
        // once, with its rule, path and commit (never the value).
        for f in &r.history {
            tracing::warn!(
                target: "forge_web::git",
                request_id = %self.log.request_id,
                error_code = ErrorCode::PushSecretDetected.id(),
                project_id = self.log.project_id.get(),
                repo_id = self.log.repo_id,
                rule = %f.kind.rule(),
                path = %display_path(&f.path),
                commit = %f.commit.as_deref().unwrap_or("-"),
                blob = %f.blob,
                "forge.git.push_history_finding"
            );
        }
        // Every waived finding, with its path (X8 review): a marker that
        // hides a real secret is found from the log, not only from review.
        for w in &r.waivers {
            tracing::info!(
                target: "forge_web::git",
                request_id = %self.log.request_id,
                project_id = self.log.project_id.get(),
                repo_id = self.log.repo_id,
                rule = %w.rule,
                blob = %w.blob,
                path = %display_path(&w.path),
                "forge.git.push_check_waived"
            );
        }
        let with_counts = |err: ForgeError| {
            err.with_context("event", "forge.git.push_checked")
                .with_context("verdict", format!("{:?}", outcome.verdict).to_lowercase())
                .with_context("applied", applied)
                .with_context("objects", r.objects)
                .with_context("blobs_scanned", r.blobs_scanned)
                .with_context("bytes_scanned", r.bytes_scanned)
                .with_context("allowed", r.allowed)
                .with_context("pack_objects", r.pack_objects)
                .with_context("skipped_large", r.skipped_large)
                .with_context("skipped_binary", r.skipped_binary)
        };
        match outcome.verdict {
            Verdict::Refused => {
                let Some(first) = r.findings.first() else {
                    self.log.failure(with_counts(
                        ForgeError::new(ErrorCode::PushCheckFailed)
                            .with_context("reason", "result_invalid")
                            .with_context("detail", "refused_without_findings"),
                    ));
                    return;
                };
                let rules: BTreeSet<&str> = r.findings.iter().map(|f| f.kind.rule()).collect();
                let rules: Vec<&str> = rules.into_iter().collect();
                self.log.failure(with_counts(
                    ForgeError::new(first.kind.code())
                        .with_context("findings", r.total_findings)
                        .with_context("rules", rules.join(","))
                        .with_context("first_rule", first.kind.rule())
                        .with_context("first_path", display_path(&first.path))
                        .with_context("first_blob", &first.blob)
                        .with_context("first_commit", first.commit.as_deref().unwrap_or("-")),
                ));
            }
            Verdict::Failed => {
                let mut err = ForgeError::new(ErrorCode::PushCheckFailed).with_context(
                    "reason",
                    outcome.reason.as_deref().unwrap_or("internal").to_owned(),
                );
                if let Some(detail) = &outcome.detail {
                    err = err.with_context("detail", detail);
                }
                if let Some(cause) = &outcome.cause_code {
                    err = err.with_context("cause_code", cause);
                }
                self.log.failure(with_counts(err));
            }
            Verdict::Accepted => match r.incomplete {
                Some(reason) => self.log.failure(with_counts(
                    ForgeError::new(ErrorCode::PushCheckIncomplete)
                        .with_context("incomplete", reason),
                )),
                None => tracing::info!(
                    target: "forge_web::git",
                    request_id = %self.log.request_id,
                    project_id = self.log.project_id.get(),
                    repo_id = self.log.repo_id,
                    verdict = "accepted",
                    objects = r.objects,
                    blobs_scanned = r.blobs_scanned,
                    bytes_scanned = r.bytes_scanned,
                    allowed = r.allowed,
                    pack_objects = r.pack_objects,
                    skipped_binary = r.skipped_binary,
                    "forge.git.push_checked"
                ),
            },
        }
    }

    /// git's texts forge did not pass on (`receive_report`), logged once.
    fn log_withheld(&self, withheld: &[String]) {
        if withheld.is_empty() {
            return;
        }
        let mut text = withheld.join(" | ");
        if text.len() > WITHHELD_LOG_MAX {
            let mut cut = WITHHELD_LOG_MAX;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
        }
        self.log.failure(
            ForgeError::new(ErrorCode::GitProtocolInvalid)
                .with_context("reason", "git_refused")
                .with_context("service", "git-receive-pack")
                .with_context("messages", withheld.len())
                .with_context("git_messages", text),
        );
    }

    fn served(&self, applied: usize, refused: usize, unpack_ok: bool, source: &'static str) {
        tracing::info!(
            target: "forge_web::git",
            request_id = %self.log.request_id,
            route = "receive_pack",
            project_id = self.log.project_id.get(),
            repo_id = self.log.repo_id,
            refs = self.commands.len(),
            applied,
            refused,
            unpack_ok,
            source,
            bytes_in = self.bytes_in,
            "forge.git.served"
        );
    }

    /// git's answer is not there to say which refs changed (it could not be
    /// read, git failed before it, or git was killed): read every commanded
    /// ref from the repository; an update is applied when the ref now holds
    /// its new object (or, for a delete, is gone). Those are recorded like
    /// reported ones (`source=<reason>`). When the repository cannot be read
    /// either, every command is logged once with `FG-GIT-0012`
    /// (`reason=reconcile_failed`) — the operator compares by hand.
    async fn reconcile(mut self, reason: &'static str) {
        let names: Vec<RefName> = self
            .commands
            .iter()
            .filter_map(|(name, _, _)| RefName::parse(name).ok())
            .collect();
        let values = match self
            .smart
            .git
            .ref_values(
                &self.smart.storage,
                self.repository.id,
                &names,
                PUSH_CHECK_TIMEOUT,
            )
            .await
        {
            Ok(values) => values,
            Err(err) => {
                // Unknown whether a ref changed: an absent hook result
                // counts as a skipped hook only when one did.
                self.push_checked(None);
                let commands: Vec<String> = self
                    .commands
                    .iter()
                    .map(|(name, old, new)| format!("{name} {old} {new}"))
                    .collect();
                self.log.failure(
                    ForgeError::new(ErrorCode::PushRecordFailed)
                        .with_context("reason", "reconcile_failed")
                        .with_context("source", reason)
                        .with_context("repo_id", self.repository.id.get())
                        .with_context("refs", self.commands.len())
                        .with_context("cause_code", err.code().id())
                        .with_context("commands", commands.join(";"))
                        .with_source(err),
                );
                return;
            }
        };
        let applied: Vec<(String, String, String)> = self
            .commands
            .iter()
            .filter(|(name, _, new)| match values.get(name.as_str()) {
                Some(Some(now)) => now == new,
                Some(None) => new.bytes().all(|b| b == b'0'),
                None => false,
            })
            .cloned() // clone: the task records its own copy
            .collect();
        self.served(
            applied.len(),
            self.commands.len() - applied.len(),
            false,
            reason,
        );
        self.after_push(applied, reason).await;
    }

    async fn send(&mut self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        if let Some(tx) = &self.answer {
            if tx.send(Ok(Bytes::from(bytes))).await.is_err() {
                // The client went away; git keeps running to its end.
                self.answer = None;
            }
        }
    }

    async fn cut(&mut self) {
        if let Some(tx) = self.answer.take() {
            let _ = tx.send(Err(io::Error::other("git failed"))).await; // ignore: the client may be gone
        }
    }

    /// Step 6, for the updates git applied (`source`: `report`, or the
    /// reconciliation's reason).
    async fn after_push(mut self, applied: Vec<(String, String, String)>, source: &'static str) {
        let checked = self.push_checked(Some(applied.len()));
        if applied.is_empty() {
            // Nothing landed: the history import's claim is open again.
            if let Some(imports) = &self.history_import {
                release_claim(imports.as_ref(), self.repository.id, &self.log).await;
            }
            return;
        }
        if let Some(outcome) = &checked {
            self.history_imported(outcome).await;
        }
        let applied: Vec<&(String, String, String)> = applied.iter().collect();
        // First the minimal line (kinds from the zero sides, no commits):
        // reading kinds and commits may take ~20 s, and the refs have
        // changed already.
        match minimal_record(self.repository.id, &applied) {
            Ok(minimal) => {
                let line = PushApplied::new(&self.pusher, self.log.project_id, &minimal);
                if let Err(err) = line.log_as("minimal", source) {
                    self.log.failure(err);
                }
            }
            Err(err) => {
                // A name git accepted but forge cannot read back is a forge
                // bug; the refs changed, so this is the record failure.
                self.log.failure(
                    PushRecord {
                        repository_id: self.repository.id,
                        updates: Vec::new(),
                    }
                    .failed(err, 0)
                    .with_context("applied", applied.len())
                    .with_context("source", source),
                );
                return;
            }
        }
        let record = match self.record_of(&applied).await {
            Ok(record) => record,
            Err(err) => {
                self.log.failure(
                    PushRecord {
                        repository_id: self.repository.id,
                        updates: Vec::new(),
                    }
                    .failed(err, 0)
                    .with_context("applied", applied.len())
                    .with_context("source", source),
                );
                return;
            }
        };
        let line = PushApplied::new(&self.pusher, self.log.project_id, &record);
        if let Err(err) = line.log_as("final", source) {
            self.log.failure(err);
        }
        let commits = self.introduced(&record).await;
        self.record(&record, &commits).await;
    }

    /// The commits the push introduced (M3 R5 round 4, ADR-0024 §8.3: the
    /// committer rule's `push_commits` rows): every commit no ref reached
    /// before the push (the snapshot read before receive-pack), at most
    /// [`forge_core::MAX_PUSHED_COMMITS`]. Every listing logs
    /// `forge.git.push_commits_listed` (refs, rows, durations); an
    /// incomplete one (cap, deadline, snapshot or git failure) logs one
    /// FG-GIT-0016 warn and is recorded incomplete (the merge requests whose
    /// range holds an unlisted commit have unknown committers).
    async fn introduced(&mut self, record: &PushRecord) -> forge_core::PushedCommits {
        let before = std::mem::replace(&mut self.before, Ok(RefSnapshot::default()));
        // The refs the snapshot read (a failed one says how far it got).
        let refs = match &before {
            Ok(b) => b.refs,
            Err(e) => e
                .context()
                .iter()
                .find(|(k, _)| *k == "refs")
                .and_then(|(_, v)| v.parse().ok())
                .unwrap_or(0),
        };
        let queries: Vec<CommitQuery> = record
            .updates
            .iter()
            .map(|u| CommitQuery {
                name: u.name.clone(), // clone: the query owns its copy
                old: u.old.as_str().to_owned(),
                new: u.new.as_str().to_owned(),
            })
            .collect();
        let started = Instant::now();
        let listed = match before {
            Err(err) => Err(("snapshot_failed", err)),
            Ok(snapshot) => self
                .smart
                .git
                .introduced_commits(
                    &self.smart.storage,
                    self.repository.id,
                    &snapshot,
                    &queries,
                    &self.principal,
                    forge_core::MAX_PUSHED_COMMITS,
                    INTRODUCED_TIMEOUT,
                )
                .await
                .map_err(|err| ("git_failed", err)),
        };
        let listing_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let (commits, incomplete) = match listed {
            Ok(found) => (
                forge_core::PushedCommits {
                    commits: found.commits,
                    complete: found.complete,
                },
                (!found.complete).then(|| (found.reason.unwrap_or("deadline"), None)),
            ),
            Err((reason, err)) => (
                forge_core::PushedCommits::unknown(),
                Some((reason, Some(err))),
            ),
        };
        tracing::info!(
            target: "forge_web::git",
            request_id = %self.log.request_id,
            repo_id = self.log.repo_id,
            refs,
            listed = commits.commits.len(),
            complete = commits.complete,
            snapshot_ms = self.snapshot_ms,
            listing_ms,
            "forge.git.push_commits_listed"
        );
        if let Some((reason, cause)) = incomplete {
            let mut err = ForgeError::new(ErrorCode::PushCommitsIncomplete)
                .with_context("reason", reason)
                .with_context("listed", commits.commits.len())
                .with_context("refs", refs)
                .with_context("listing_ms", listing_ms)
                .with_context("phase", "push_introduced_commits");
            if let Some(cause) = cause {
                err = err
                    .with_context("cause_code", cause.code().id())
                    .with_source(cause);
            }
            self.log.failure(err);
        }
        commits
    }

    /// The record of the applied updates: kinds and commits from git.
    async fn record_of(
        &self,
        applied: &[&(String, String, String)],
    ) -> Result<PushRecord, ForgeError> {
        let smart = &self.smart;
        let mut updates = Vec::with_capacity(applied.len());
        for (name, old, new) in applied {
            updates.push(PushedRef {
                name: RefName::parse(name)?,
                old: ObjectId::parse(old)?,
                new: ObjectId::parse(new)?,
                kind: PushUpdateKind::FastForward,
                commits: Vec::new(),
                total_commits: 0,
                total_commits_partial: false,
            });
        }
        // Kinds: zero sides, tags, ancestry for the rest.
        let mut pairs = Vec::new();
        let mut pair_of = Vec::new();
        for u in &updates {
            let kind = if u.old.is_zero() {
                Some(PushUpdateKind::Create)
            } else if u.new.is_zero() {
                Some(PushUpdateKind::Delete)
            } else if u.name.tag().is_some() {
                Some(PushUpdateKind::Force)
            } else {
                None
            };
            pair_of.push(match kind {
                Some(_) => None,
                None => {
                    pairs.push((u.old.as_str().to_owned(), u.new.as_str().to_owned()));
                    Some(pairs.len() - 1)
                }
            });
        }
        let ancestry = match smart
            .git
            .fast_forwards(
                &smart.storage,
                self.repository.id,
                &pairs,
                PUSH_CHECK_TIMEOUT,
            )
            .await
        {
            Ok(a) => a,
            Err(err) => {
                // Recorded as force (the conservative kind), and said so.
                self.log
                    .failure(err.with_context("phase", "push_record_kinds"));
                vec![Ancestry::NotFastForward; pairs.len()]
            }
        };
        for (u, pair) in updates.iter_mut().zip(&pair_of) {
            u.kind = if u.old.is_zero() {
                PushUpdateKind::Create
            } else if u.new.is_zero() {
                PushUpdateKind::Delete
            } else {
                match pair.and_then(|i| ancestry.get(i)) {
                    Some(Ancestry::FastForward) => PushUpdateKind::FastForward,
                    Some(Ancestry::NotFastForward | Ancestry::Unknown) | None => {
                        PushUpdateKind::Force
                    }
                }
            };
        }
        let queries: Vec<CommitQuery> = updates
            .iter()
            .map(|u| CommitQuery {
                name: u.name.clone(), // clone: the query owns its copy
                old: u.old.as_str().to_owned(),
                new: u.new.as_str().to_owned(),
            })
            .collect();
        match smart
            .git
            .new_commits(
                &smart.storage,
                self.repository.id,
                &queries,
                &self.principal,
                PUSH_CHECK_TIMEOUT,
            )
            .await
        {
            Ok(lists) => {
                let mut first_error = None;
                let mut failed = 0usize;
                for (u, list) in updates.iter_mut().zip(lists) {
                    if u.kind == PushUpdateKind::Delete {
                        continue;
                    }
                    let list = match list {
                        Ok(list) => list,
                        Err(err) => {
                            // This ref only: recorded without commits.
                            failed += 1;
                            first_error.get_or_insert(err);
                            u.total_commits_partial = true;
                            continue;
                        }
                    };
                    u.commits = list
                        .commits
                        .iter()
                        .map(|c| ObjectId::parse(c))
                        .collect::<Result<_, _>>()?;
                    u.total_commits = list.total.max(u.commits.len() as u64);
                    u.total_commits_partial = list.partial;
                }
                if let Some(err) = first_error {
                    self.log.failure(
                        err.with_context("phase", "push_record_commits")
                            .with_context("failed_refs", failed),
                    );
                }
            }
            Err(err) => {
                // Recorded without commits, flagged as partial counts.
                self.log
                    .failure(err.with_context("phase", "push_record_commits"));
                for u in &mut updates {
                    if u.kind != PushUpdateKind::Delete {
                        u.total_commits_partial = true;
                    }
                }
            }
        }
        Ok(PushRecord {
            repository_id: self.repository.id,
            updates,
        })
    }

    /// `record_push`, [`RECORD_ATTEMPTS`] within [`RECORD_BUDGET`];
    /// `FG-GIT-0012` when every attempt failed.
    async fn record(&self, record: &PushRecord, commits: &forge_core::PushedCommits) {
        record_applied(&*self.recorder, &self.pusher, record, commits, &self.log).await;
    }
}

/// `record_push` of an applied push, [`RECORD_ATTEMPTS`] within
/// [`RECORD_BUDGET`] (the final attempt without the commits); `FG-GIT-0012`
/// (logged once) when every attempt failed. receive-pack's task and the
/// simple editor's commit (`routes::file_commit`) record through it.
pub(crate) async fn record_applied(
    recorder: &dyn forge_core::PushRecorder,
    pusher: &Pusher,
    record: &PushRecord,
    commits: &forge_core::PushedCommits,
    log: &PushLog,
) {
    let deadline = Instant::now() + RECORD_BUDGET;
    let unknown = forge_core::PushedCommits::unknown();
    let mut attempts = 0;
    let mut last = None;
    while attempts < RECORD_ATTEMPTS {
        if attempts > 0 {
            let wait = RECORD_BACKOFF
                .get(attempts as usize - 1)
                .copied()
                .unwrap_or(Duration::from_secs(2));
            // Before the final attempt the wait never eats its reserve.
            let wait = if attempts + 1 >= RECORD_ATTEMPTS {
                let left = deadline.saturating_duration_since(Instant::now());
                wait.min(left.saturating_sub(RECORD_FINAL_RESERVE))
            } else {
                wait
            };
            if Instant::now() + wait >= deadline {
                break;
            }
            tokio::time::sleep(wait).await;
        }
        attempts += 1;
        let plan = attempt_plan(attempts, deadline.saturating_duration_since(Instant::now()));
        if plan.timeout.is_zero() {
            // No time left for a non-final attempt: straight to the final one.
            continue;
        }
        let sent = if plan.with_commits { commits } else { &unknown };
        let outcome = match tokio::time::timeout(
            plan.timeout,
            recorder.record_push_with_commits(pusher, record, sent),
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(_) => Err(ForgeError::new(ErrorCode::DbQueryFailed)
                .with_context("reason", "record_budget_exhausted")
                .with_context("timeout_ms", plan.timeout.as_millis())),
        };
        match outcome {
            Ok(recorded) => {
                // Deduped: an earlier attempt committed after all (its
                // rows with it), nothing was dropped.
                let dropped = !plan.with_commits
                    && !commits.commits.is_empty()
                    && recorded == PushRecorded::Recorded;
                if dropped {
                    // The audit row is kept; the listed commits are not.
                    log.failure(
                        ForgeError::new(ErrorCode::PushCommitsIncomplete)
                            .with_context("reason", "record_fallback")
                            .with_context("listed", 0)
                            .with_context("dropped", commits.commits.len())
                            .with_context("attempts", attempts)
                            .with_context("phase", "push_record"),
                    );
                }
                tracing::info!(
                    target: "forge_web::git",
                    request_id = %log.request_id,
                    repo_id = log.repo_id,
                    refs = record.updates.len(),
                    attempts,
                    deduped = recorded == PushRecorded::Deduped,
                    push_commits_dropped = dropped,
                    "forge.git.push_recorded"
                );
                return;
            }
            // A record the store refuses as invalid will not pass on a retry.
            Err(err) if err.code() == ErrorCode::ChangeRejected => {
                last = Some(err);
                break;
            }
            Err(err) => last = Some(err),
        }
    }
    let cause = last.unwrap_or_else(|| {
        ForgeError::new(ErrorCode::DbQueryFailed).with_context("reason", "record_budget_exhausted")
    });
    log.failure(record.failed(cause, attempts));
}

/// One record attempt's plan (R5 review round 6): the final attempt sends
/// no commits (`PushedCommits::unknown`: no `push_commits` rows, the push
/// marked incomplete) so the audit row is written even when the rows are
/// what made the store too slow; the attempts before it end
/// [`RECORD_FINAL_RESERVE`] early so the final one keeps that time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AttemptPlan {
    with_commits: bool,
    timeout: Duration,
}

fn attempt_plan(attempt: u32, left: Duration) -> AttemptPlan {
    if attempt >= RECORD_ATTEMPTS {
        AttemptPlan {
            with_commits: false,
            timeout: left,
        }
    } else {
        AttemptPlan {
            with_commits: true,
            timeout: left.saturating_sub(RECORD_FINAL_RESERVE),
        }
    }
}

#[cfg(test)]
#[path = "git_push_tests.rs"]
mod tests;
