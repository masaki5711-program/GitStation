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
  `Review::versions(mr)` and `Review::mergeability(mr)` (never enqueue).

**Error codes owned**: FG-REVIEW-0001 DuplicateOpenMergeRequest (409, from
forge-db under the project lock; the partial unique index
`merge_requests_one_open` is the backstop); FG-REVIEW-0002
PurgePreconditionFailed (409, M3 R1b: `purge_requires_closed_or_rewritten`,
`reopen_at_purged_head`; runbook `secret-removal`). Reused: FG-AUTH-0002 / 0003 (with
`not_a_branch`, `source_equals_target`, `foreign_author`, `foreign_draft`,
`merged`, `not_open`), FG-REPO-0005 (an invisible project and a missing iid
alike), FG-REPO-0006, FG-REPO-0016 (repository not ready), FG-REPO-0023
(`ref_not_found` with `branch=source_branch|target_branch`), FG-WEB-0006
(`param_invalid` with `param=title|description`), FG-GIT-0002 / 0004 (the branch
lookup), FG-DB-0001 / 0002, FG-SYS-0001 (`proof_action_mismatch`,
`proof_shape_mismatch`, `proof_without_user`, `reconfirm_target_mismatch`),
FG-DB-0002 (`merge_request_changed_concurrently`, retryable).

**Runbooks**: `secret-removal` (R1b). `merge-execution`, `codeowners` and
`merge-blocked` arrive with R4–R6.

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
