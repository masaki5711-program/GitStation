# forge-review

**Responsibility**: merge requests (M3, ADR-0024). R1a: open, read, list, edit
(title, description, draft flag, target branch), close and reopen — each write
**authorized again inside its own transaction**. A feature crate (ADR-0024 §11):
`forge-web / forge-cli → forge-review → forge-auth / forge-git / forge-db →
forge-core → forge-errors`. It runs no SQL of its own (the SQL is
`forge_db::review`, the `db-write-boundary` gate) and depends on no other feature
crate (not forge-hosting, not the future forge-checks).

Later MRs add here: versions on push and the hidden refs (R1b), the diff and
mergeability job (R2), discussions (R3), the merge job and `evaluate_merge()`
(R4), approvals and CODEOWNERS (R5).

**How a command decides.** The route registry proves the action on what its
loader read (the project and the merge request row, or the merge request about
to be opened) and hands the `Authorized<MergeRequestCtx>` to the handler; the
handler passes it here. The command calls a forge-db review command
(`open_mr_decided`, `edit_mr_decided`, `close_mr_decided`, `reopen_mr_decided`),
which locks the project row, the merge request row, the caller's roles
(`FOR SHARE`) and account, and calls `decide` — here `Authorized::reconfirm`.
A role removed, a demotion or a disable committed first is seen (denied, nothing
written, no iid drawn). Only this crate calls those commands
(`tests/decided_callers.rs`: `only_forge_review_calls_the_review_commands`); their
names never match forge-hosting's needles, so forge-hosting's scan needs no
exception for this crate.

Before the transaction a create checks both branches exist, an edit any target it
asks for (whatever the proof saw as the current one), a reopen both branches of the
row as read just before the transaction — and the transaction refuses to go on when the
row it locks is closed with other branches (or was not closed when read), re-reading
and checking again up to `STALE_ATTEMPTS` (3) times, then `FG-DB-0002
reason=merge_request_changed_concurrently` (retryable). All through `BranchProbe` (in `forge serve`:
`forge_git::browse::Browser::resolve_ref` on the full `refs/heads/…` name — the
browse API's ref list, no new git plumbing). A branch deleted later leaves the
merge request in place (ADR-0024 §1).

**Public API** (`Review::new(Db, Arc<dyn BranchProbe>)`)
- Reads (read pool): `merge_request(project, iid)`, `merge_requests(project,
  state, before, limit)` (newest first, at most `MAX_PAGE` = 100).
- Writes: `open(request, proof, repo, NewFields) -> StoredMergeRequest`,
  `edit(request, proof, repo, EditFields) -> (EditOutcome, StoredMergeRequest)`
  (`Changed(events)` / `Unchanged`), `close` and `reopen(…, repo) -> (StateOutcome,
  StoredMergeRequest)` (`Changed` / `Unchanged`): the row as committed, so the route
  answers without another read.
- `loaded_merge_request(&StoredMergeRequest)`: forge-auth's `LoadedMergeRequest`,
  built the same way by the route loaders and the commands.
- `check_title`, `check_description`: the field rules (title 1..=255
  characters, no control character; description ≤ 64 KiB).
- M3 R1b (versions, ADR-0024 §2): `BranchProbe::branch_head` (was `branch_exists`;
  the reopen compares the source's head with the purged versions);
  `versions::refresh(&Db, &VersionRefs, RefreshRequest, &RequestId) -> RefreshReport` (the
  body of the job `merge_request.refresh`: read the source tip, record the version, sync the
  refs; log `forge.review.version_refreshed`); `VersionRefs::{new, with_browser, sync, sync_locked,
  branch_tip}` (the ref sync under the merge request's refs lock — the job holds it for its
  whole run, so two runs never interleave —, every git call bounded by what is left of the
  hold: one `update-ref` transaction from the committed rows,
  `plan_ref_changes` is the pure planner, a refused transaction logged at warn
  (`forge.review.version_refs_retry`) and planned again once,
  `forge.review.version_ref_reclaimed` for an unclaimed `v<n>`, `Browser::invalidate` when
  it holds one); `Review::with_version_refs` and `purge_version_refs(request, proof, repo,
  versions) -> PurgeOutcome` (`src/purge.rs`) (`MergeRequestVersionRefsPurge`, a human administrator's
  session; log `forge.review.version_refs_purged`).
- M3 R2 (mergeability, ADR-0024 §3): `check(&Db, &Mergeability, MergeabilityRequest,
  &RequestId) -> CheckReport` (`src/mergeability.rs`, the body of the job
  `merge_request.mergeability`: the recorded head against the target's tip — `no_version`,
  `target_missing`, `unrelated_histories` are `check_failed`; `merge-tree --write-tree`
  decides can / cannot with at most 100 paths / 16 KiB stored; a git failure is stored as
  `check_failed reason=git_timeout|git_busy|git_failed` and returned for the retry; log
  `forge.review.mergeability_checked` with the paths). The version job now also records the
  head version's bases once and requests a check unless the stored status answers the
  current (head, target tip) pair (`RefreshReport::check_requested`). Reads
  `Review::versions(mr)` and `Review::mergeability(mr)` (never enqueue). Review fixes:
  `mergeability::fail(&Db, request, &RequestId, &cause)` ends a request whose job failed for good as
  `check_failed reason=job_failed` (FG-REVIEW-0003 logged); `after_version.rs` holds the version
  job's request (a `check_failed` answers the pair only for a deterministic `CheckReason`);
  `version_refs.rs` holds `VersionRefs` / `plan_ref_changes` (re-exported from `versions`). Tests
  `tests/mergeability_git.rs` (`check_maps_git_timeout_and_no_version_to_check_failed`).

- M4 C7 (merge results and required-check freshness, plan 04 M-1 / M-2, ADR-0024 Amendment
  2026-11-11): a stored clean check becomes the merge result — `Git::write_merge_result` (the
  trial merge's tree, parents target tip then head), once per (head, target tip) pair
  (`forge_db::review::merge_result_of` / `record_merge_result`, which also enqueues the merge
  request pipeline on the result), and `refs/merge-requests/<iid>/merge` pointed at it while the
  run's seq is the latest; a conflict / no target / unrelated / no version deletes the ref
  (`CheckReport::merge_result`, `merge_ref`; logs `forge.review.merge_result_updated` /
  `_removed`). Required checks are resolved against the live target tip
  (`forge_core::MergeCheckRuns::resolve`, `conditions((target, tip), …)`): a run on an earlier
  merge result only is `BlockReason::RequiredCheckStale` (`RequiredCheck::stale`), and `blocked()`
  answers FG-CI-0013 RequiredCheckStale when it is the first reason. `MergeStatus::merge_result_sha`.
  Tests in `tests/approvals_git.rs` (`a_required_check_of_an_older_merge_result_is_stale_and_blocks_the_merge`
  and four more).

- M3 X6 (conflict prediction, plan 04 C-2, ADR-0024 Amendment X6; `src/conflicts.rs`):
  `scan(&Db, &ConflictPredictor, ConflictScanRequest, &RequestId) -> ScanReport`, the body of the
  job `merge_request.conflict_scan`: the other open merge requests on the target (drafts
  included, at most `ScanLimits::peers_max` = 50) are narrowed by the paths both change since
  their merge base with the target (`merge-base`, `diff-tree`; no overlap → `no_overlap`, no
  trial merge), then `merge-tree --write-tree` of the two heads for at most `pairs_max` = 20
  candidates within a 120 s budget; a conflict in a path both change is `conflict` (a conflict
  only in one side's paths is that side's C-1 conflict → `clean`). Caps and the budget make the
  scan `incomplete` (FG-REVIEW-0022 at warn); a git failure is stored on the pair (or the scan)
  and returned for the retry. `conflicts::fail` ends a job failed for good as `job_failed`. The
  version job requests a scan unless the stored one answers the head
  (`RefreshReport::scan_requested`). Read `Review::conflicts(mr)` (never enqueues). Tests
  `tests/conflicts_git.rs` (real git).

- M3 R3 (discussions, ADR-0024 §6, §12; `src/discussions.rs`, `src/position.rs`):
  `Review::start_discussion(request, proof: &Authorized<MergeRequestCtx>, repo, body,
  position)`, `reply(request, proof, discussion, body)` (`MergeRequestComment`),
  `resolve(request, proof: &Authorized<DiscussionCtx>, resolved)` (`DiscussionResolve`),
  reads `discussions(mr, repo, key) -> DiscussionList` / `view(…)` / `discussion(mr, id)`.
  The diff source is `DiffProbe` (`Review::with_diffs`; the browse API's
  `merge_request_compare`, implemented for `Browser`). An inline position must be in a hunk
  of a file the version's diff (`merge_base..head`) lists on that side
  (`position::check_position`, FG-REVIEW-0004); `outdated` is computed when read
  (`src/outdated.rs`: `Outdated::{No, Yes, Unknown}` from `position::line_changed` on the
  diff from the thread's side commit to the current version's — a `-` line at its number,
  or its file deleted / renamed / unreadable). Unknown (no current version, a purged
  version, bases pending, more than `MAX_DIFFS_PER_READ` = 20 diffs, a failed diff) is shown
  as outdated and logged once (`forge.review.discussion_outdated_unknown`; routine states at
  debug), within a time budget per read (`OUTDATED_BUDGET` = 8 s, `Review::with_outdated_budget`;
  past it `reason=deadline` without running git); neither the list
  nor `view` (a committed write's answer) ever fails on git. Field checks `check_note_body`
  (at most 60 KiB) / `check_position_shape` (FG-WEB-0006). Tests
  `tests/discussions_git.rs` (`an_inline_discussion_outside_the_diff_is_refused`).

- M3 R4 (merge execution, ADR-0024 §4, §7; `src/merges/`): `evaluate_merge(&MergeFacts) ->
  Result<MergeClearance, Vec<BlockReason>>` (pure; every reason in the fixed order;
  `MergeClearance` has no public constructor) and `stored_mergeable` (a stored decision counts
  only for the live (head, target tip) pair). `Review::request_merge(request, CredentialRef,
  &Authorized<MergeRequestCtx>, repo, &MergeAsk) -> StoredMerge` (`MergeRequestMerge`: the
  method allowed by `MergeSettings` (defaults merge_commit + squash, default merge_commit), the
  `sha` the requester saw, the decision again under the review locks with the target's
  protection as stored, a live attempt refused, `evaluate_merge()` once; `mergeability_unknown`
  requests a check) and `Review::latest_merge(mr)`. `Merger` (`new(db, git, storage)`,
  `with_browser`, `with_settings`, `with_timeouts`) runs the job `merge_request.merge`:
  `execute(merge_id) -> MergeReport` — the repository's merge lock, tx1a (the requester's
  principal rebuilt from the database: kind, token scopes / agent flag / revocation, session
  revocation; `authorize()` and `evaluate_merge()`; the recorded head and the source tip against
  the requested head), `merge-tree` / `commit-tree` (author and committer from the database;
  squash: author = the merge request's author), the descendant invariant, tx1b (`prepared`),
  the fence (lock alive, `prepared → executing`, the update-ref window), `update-ref` CAS (a
  lost CAS re-reads the ref: `target_moved` / `ref_locked` retry / FG-REVIEW-0009), tx2; then
  the optional source branch deletion (a `RepoPush` delete through `authorize_ref_updates`, CAS
  on the merged head). Every ref write calls `Browser::invalidate`. `give_up(merge_id, cause)`
  ends an attempt that never passed the fence (`job_failed`); `reconcile(request_id) ->
  ReconcileReport` settles stuck attempts by asking git. Tests `tests/merges_git.rs` (real git).
  Review round 1: the default message is frozen at the request; every git call under the lock
  is cut to the hold's rest; `update_ref` takes both proofs; the reconciliation skips only a busy
  lock (`merge_lock_busy`), rotates what it cannot settle, has a 45 s pass budget; FF onto the
  head is `nothing_to_merge`; an identity refusal (FG-REVIEW-0012) supersedes.
- M3 R5 (approvals, rules, CODEOWNERS, ADR-0024 §8, §9; `src/approvals/`): `codeowners::parse`
  (the Q5 grammar, strict: line + `reason`; limits 64 KiB / 1 000 lines / 4 KiB / 50 owners),
  `evaluate(&ApprovalInputs) -> ApprovalReport` (pure: how each approval counts — `stale`,
  `inactive`, `not_human`, `role_insufficient`, `author`, `committer`, `committers_unknown`,
  `not_in_rule` — and the block reasons `approvals_missing`, `codeowner_approval_missing`,
  `codeowners_invalid`, `config_change_needs_maintainer_approval`, `changed_paths_unknown`;
  `is_config_path`), `committers::committers_of` (R5 round 4: the pushers of the range's
  commits as recorded at receive time in `push_commits`; a range commit without a row is
  unknown). `ReviewGit::new(git, storage)` reads the changed paths (the target tip against the
  trial merge's tree, `diff-tree --no-renames`, 10 000 paths), the range (`head ^<tip>`, 1 000
  commits) and its pushers (one indexed statement), and the **target tip's** CODEOWNERS (owners resolved, matched in a blocking task
  within 5 s), all under one `Budget` (20 s / 64 calls on a request, 10 s / 32 on the state
  route, the lock's hold in the job); a git failure is an unknown fact (warn
  `forge.review.approval_input_unread`; CODEOWNERS: one `forge.review.codeowners_invalid`). `Review::with_review_git`,
  `approve(request, proof, repo, &sha)`, `unapprove`, `approval_state`, `approval_rules`,
  `put_approval_rule(request, &Authorized<ProjectCtx>, &RuleAsk)`, `remove_approval_rule`.
  `evaluate_merge()` takes `MergeFacts::approvals`; the request and every gate of the merge job
  read them (the job under its lock). Tests `tests/approvals_git.rs` (real git).

- M3 R6 (merge conditions, ADR-0024 §7, §8.2, §8.3, §8.6, §9, §10): `evaluate_merge()` adds
  `required_check_missing` (no run reported is never success), `required_check_not_successful`
  and `unresolved_discussions` (`MergeFacts::checks` / `discussions` from `merges::conditions`:
  the target's strictest merge rule; M3 C1: the head's stored check runs — the request and the
  job read them in the gate's transaction, the status through `CheckResultsSource`,
  `Review::with_check_results`); the
  protected branch's minimum is `approvals_missing` with the rule `@protected_branch`; every
  list is ranked by `BlockReason::rank` (ADR-0024 §7). The approval half honours the project's
  `committer_approvals_excluded` and a live CODEOWNERS suspension (only the CODEOWNERS
  requirements drop). The request and every job gate read the project's stored settings
  (`method_allowed`: FG-REVIEW-0010 again in the job). `Review::merge_status` (the stable
  `GET …/status` contract), `merge_settings` / `put_merge_settings`, `toggle_codeowners`
  (`Authorized<CodeownersToggleCtx>`), `highest_protection`. `approvals::carry::
  may_carry_approvals` (pure; R6 review: never `.gitmodules` / `.gitattributes` / `.lfsconfig`, a mode change or
  new executable, a symlink, a submodule, a binary file, more than 50 files or 1 000 lines — counted
  from the head each approval was given on, `forge_db::review::approved_heads`) and the
  version job's carry reads (`versions_carry.rs`, forge-git `changed_entries`; logs
  `forge.review.approvals_carry_decided`, warn `forge.review.approvals_carry_unread`). A git
  failure of the status's fast-forward check is `ff_possible: None` (warn
  `forge.review.status_ff_unknown`, `error_code`), never a failed status read. Tests:
  the proptest combination table `merges/evaluate_tests.rs`, `approvals/evaluate_tests.rs`,
  `tests/approvals_git.rs` (real git: the rule's reasons and the status, the job deciding a new
  rule and a disallowed method again, the ban not stopping a merge, carry / no carry, a
  CODEOWNERS suspension).

**Error codes owned**: FG-REVIEW-0004 DiscussionPositionInvalid (422, M3 R3); FG-REVIEW-0005
DiscussionLimitReached (409, M3 R3 review, from forge-db under the merge request's lock). FG-REVIEW-0001 DuplicateOpenMergeRequest (409, from
forge-db under the project lock; the partial unique index
`merge_requests_one_open` is the backstop); FG-REVIEW-0002
PurgePreconditionFailed (409, M3 R1b: `purge_requires_closed_or_rewritten`,
`reopen_at_purged_head`; runbook `secret-removal`); FG-REVIEW-0003 MergeabilityNotChecked
(warn, M3 R2: `job_failed` here, `job_stale` / `fanout_deferred` / `undecided` from forge-db;
runbook `merge-request-mergeability`); M3 R4: FG-REVIEW-0006 MergeRequestConflict, 0007
MergeBlocked (`reasons`), 0008 HeadChanged (`stale_view` / `source_moved` / `target_moved`),
0009 MergeExecutionFailed (alert: `update_ref_failed` / `record_failed` / `ref_state_unknown`),
0010 MergeMethodNotAllowed, 0011 MergeInProgress (from forge-db); M3 R5: FG-REVIEW-0013
SelfApprovalRefused (from `authorize()`), 0014 CodeownersInvalid (warn, logged once per
evaluation; runbook `codeowners`), 0015 ApprovalLimitReached (from forge-db); M3 R6:
FG-REVIEW-0016 RequiredCheckNamespaceNotAllowed (from forge-hosting / forge-db); M4 C7:
FG-CI-0013 RequiredCheckStale (info, `blocked()` when the first reason is `required_check_stale`). Reused: FG-GIT-0016 (warn, round 5: a merge
commit whose range pushers are unknown gets no `push_commits` row, `merges/attribution.rs`); FG-AUTH-0002 / 0003 (with
`not_a_branch`, `source_equals_target`, `foreign_author`, `foreign_draft`,
`merged`, `not_open`), FG-REPO-0005 (an invisible project and a missing iid
alike), FG-REPO-0006, FG-REPO-0016 (repository not ready), FG-REPO-0023
(`ref_not_found` with `branch=source_branch|target_branch`), FG-WEB-0006
(`param_invalid` with `param=title|description`), FG-GIT-0002 / 0004 (the branch
lookup), FG-DB-0001 / 0002, FG-SYS-0001 (`proof_action_mismatch`,
`proof_shape_mismatch`, `proof_without_user`, `reconfirm_target_mismatch`),
FG-DB-0002 (`merge_request_changed_concurrently`, retryable).

**Runbooks**: `secret-removal` (R1b), `merge-execution` (R4), `codeowners` and
`merge-blocked` (R5).

**Tests**: `tests/commands.rs` (the full flow through proofs, a role removed or a
Maintainer demoted after the proof, missing branches, bad input and mismatched
proofs), `tests/stale_proofs.rs` (a reopen proof taken while open, a retarget the
proof saw as current: both still checked), `tests/real_browser.rs`
(`browser_probe_accepts_only_an_existing_branch`: the real `Browser` against a real
repository), `tests/decided_callers.rs`, `tests/versions_git.rs` (R1b, real git:
`a_push_adds_a_version_and_its_hidden_refs_which_survive_a_force_push`,
`purged_version_refs_are_never_recreated`), `src/versions_tests.rs` (the ref
planner); the SQL side in
`crates/forge-db/src/review/tests.rs` (iid allocation under concurrency, one
event per transition, the state guard, FG-REVIEW-0001).
