# forge-checks

**Responsibility**: check runs (M3 C1, ADR-0024 §10, §11; plan 02 §5.4) — a
commit's reported checks (CI, kanflow) that protected branches can require. A
feature crate beside forge-review: `forge-web / forge-cli → forge-checks →
forge-auth / forge-git / forge-db → forge-core → forge-errors`. It runs no SQL of
its own (the SQL is `forge_db::checks`) and never depends on forge-review (nor the
reverse): the merge conditions read the runs through `forge_core::CheckResultsSource`,
which forge-db implements on `Db` with the same SQL the merge gate runs inside its
transaction.

**How a report decides.** The route parses the name first (its namespace is an
input of the decision: `FG-REVIEW-0020 reason=name_invalid`), loads the project and
whether the caller holds a grant of the namespace, and proves `CheckWrite` on a
`CheckRunCtx` (project, commit and name are the proof's id). `Checks::report` then
checks the body (`validate`), the commit's reachability from a branch or tag
(`CommitProbe`; in `forge serve` the browse API's id lookup — unreachable is
answered exactly like missing, and a check that cannot decide writes nothing), and
calls `forge_db::checks::report_check_run_decided`, which locks the project, the
caller's roles, account and grant and decides again (`Authorized::reconfirm`). Only
this crate calls the check commands (`tests/decided_callers.rs`).

**Public API** (`Checks::new(Db, Arc<dyn CommitProbe>)`)
- `report(request, &Authorized<CheckRunCtx>, repo, &ReportInput) -> Reported`
  (`created`, `completed_event`: `check.completed` only when the conclusion changed).
- `list(&Authorized<ProjectCtx>, repo, &ObjectId) -> Vec<StoredCheckRun>`
  (`CheckRead`; by name, with annotations).
- `granted(user, project, namespace)` (the route loader), `grants_of(user)` (by project),
  `change_grant(request, &Authorized<CheckNamespaceGrantCtx>, grant) -> GrantOutcome`
  (`CheckNamespaceGrantManage`: administrators, people only; `ci` / `kanflow`; **a grant is
  for one project** — review fix 1).
- `unreportable(project, &[CheckName])`: the required names no active service account may
  report on that project (`forge/`, and `ci/` / `kanflow/` without a granted active
  reporter there) — the merge rule's `unreportable_checks`.
- `reporters(&Db)`: active reporters per namespace (doctor `checks.reporters`).
- `validate::{parse_name, validate}`: the rules of docs/api/checks.md.

**Owned error codes**: FG-REVIEW-0020 CheckPayloadInvalid (422), FG-REVIEW-0021
CheckRunLimitReached (409, from forge-db), FG-DB-0022 CheckRunDataInvalid (500,
alert; from forge-db). Denials are FG-AUTH-0003 with `reason`
(`not_service_account`, `namespace_not_granted`, `forge_namespace_internal`,
`ext_namespace_unavailable`, `agent_token`).

**Runbook**: `ops/runbooks/checks-reporting.md`.
