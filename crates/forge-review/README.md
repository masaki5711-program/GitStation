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

Before the transaction a create checks both branches exist, an edit a new target,
a reopen both branches, through `BranchProbe` (in `forge serve`:
`forge_git::browse::Browser::resolve_ref` on the full `refs/heads/…` name — the
browse API's ref list, no new git plumbing). A branch deleted later leaves the
merge request in place (ADR-0024 §1).

**Public API** (`Review::new(Db, Arc<dyn BranchProbe>)`)
- Reads (read pool): `merge_request(project, iid)`, `merge_requests(project,
  state, before, limit)` (newest first, at most `MAX_PAGE` = 100).
- Writes: `open(request, proof, repo, NewFields)`, `edit(request, proof, repo,
  EditFields)` (`EditOutcome::Changed(events)` / `Unchanged`), `close`,
  `reopen(…, repo)` (`StateOutcome::Changed` / `Unchanged`).
- `loaded_merge_request(&StoredMergeRequest)`: forge-auth's `LoadedMergeRequest`,
  built the same way by the route loaders and the commands.
- `check_title`, `check_description`: the field rules (title 1..=255
  characters, no control character; description ≤ 64 KiB).

**Error codes owned**: FG-REVIEW-0001 DuplicateOpenMergeRequest (409, from
forge-db under the project lock; the partial unique index
`merge_requests_one_open` is the backstop). Reused: FG-AUTH-0002 / 0003 (with
`not_a_branch`, `source_equals_target`, `foreign_author`, `foreign_draft`,
`merged`, `not_open`), FG-REPO-0005 (an invisible project and a missing iid
alike), FG-REPO-0006, FG-REPO-0016 (repository not ready), FG-REPO-0023
(`ref_not_found` with `branch=source_branch|target_branch`), FG-WEB-0006
(`param_invalid` with `param=title|description`), FG-GIT-0002 / 0004 (the branch
lookup), FG-DB-0001 / 0002, FG-SYS-0001 (`proof_action_mismatch`,
`proof_shape_mismatch`, `proof_without_user`, `reconfirm_target_mismatch`).

**Runbooks**: none yet (no alerting code; `merge-execution`, `codeowners`,
`secret-removal` and `merge-blocked` arrive with R1b–R6).

**Tests**: `tests/commands.rs` (the full flow through proofs, a role removed or a
Maintainer demoted after the proof, missing branches, bad input and mismatched
proofs), `tests/decided_callers.rs`; the SQL side in
`crates/forge-db/src/review/tests.rs` (iid allocation under concurrency, one
event per transition, the state guard, FG-REVIEW-0001).
