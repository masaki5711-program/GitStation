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
- `merge_request_head_runs(&Authorized<MergeRequestCtx>, &ObjectId)` (`MergeRequestRead`,
  M3 X5): the runs on a merge request's stored head, for the context API (no branch
  reachability: the version ref keeps the head).
- `granted(user, project, namespace)` (the route loader), `grants_of(user)` (by project),
  `change_grant(request, &Authorized<CheckNamespaceGrantCtx>, grant) -> GrantOutcome`
  (`CheckNamespaceGrantManage`: administrators, people only; `ci` / `kanflow`; **a grant is
  for one project** — review fix 1).
- `unreportable(project, &[CheckName])`: the required names no active service account may
  report on that project (`forge/` names forge does not report itself — X2 reports
  `forge/spec-drift` and (X7) `forge/risk` — and `ci/` / `kanflow/` without a granted active reporter there) — the merge rule's `unreportable_checks`.
- `reporters(&Db)`: active reporters per namespace (doctor `checks.reporters`).
- `validate::{parse_name, validate}`: the rules of docs/api/checks.md.
- `spec_refs::{read_repo_config, follow, Reader, Followed}` (X1 / X2), `spec_drift::{SpecDrift,
  assess}` (X2), `risk::{Risk, assess, record_error, Limits}` (X7).
- `spec_index::{index_commit, own_dirs, IndexLimits, Indexed, Tags}` (X4): one commit's
  requirement index (the own specification directories, the `@req` tags), bounded;
  `spec_checks::{SpecChecks, assess_lint, assess_trace, record_errors}` (X4);
  `spec_index::index_head` and `doc_lint::{assess_doc_lint, DocLimits, DocTree}` (plan 13 stage 2).

**forge's own check (M3 X2, ADR-0023 Amendment X2)**: `spec_drift::SpecDrift` evaluates
`forge/spec-drift` for one `check.spec_drift` job (a merge request version's head; the job is
enqueued by forge-db's `spec-drift` subscriber and run by `forge worker`): it reads the head's
`.forge/repo.yml`, follows each `repo:` entry for **every signed-in person**
(`forge_auth::prove_spec_ref_for_every_person`), compares `version:` pins with the referenced
repository's tags (`forge_core::spec_version`) and records the run with
`forge_db::checks::record_forge_check_run` (no reporter). `success` / `neutral` (FG-REPO-0024,
0025, 0026, 0027; no referenced tag names or counts are stored — review X2) / `error`
(`SpecDrift::fail` on the job's last failure, `record_error` for `forge worker`'s
`spec_drift_sweep` of jobs the reaper ended). The evaluation has its own 30 s deadline. `spec_refs` holds the
reading and following shared with forge-web's `GET …/repository/spec-refs` (M3 X1, moved here
by X2: `Reader::Proof` for the route's reader, `Reader::EveryPerson` for the job).

**forge/spec-lint and forge/spec-trace (M3 X4, ADR-0023 Amendment X4)**:
`spec_checks::SpecChecks` evaluates both for one `check.spec_index` job (forge-db's `spec-index`
subscriber: every merge request version, and its current head again after a work item link
changes). It indexes the head and the merge base with the target branch's tip
(`spec_index::index_commit`: `.forge/repo.yml`'s `path:` directories or `spec/` — a `@req` in
Markdown there is no test (review X4) —, at most 1000
Markdown files / 8 MiB / 256 KiB each, and `git grep` for `@req` over the tree, at most 8 MiB —
a bound hit is `incomplete`, never a silent pass), lints the head (`forge_core::spec_index::lint`)
and traces the merge request (`trace_merge_request`: changed requirements, added tags, linked
work items' `PBI-<n>/AC-<m>` tests). spec-lint is `success` / `failure` (FG-REPO-0028) /
`neutral` (FG-REPO-0031); spec-trace is advisory: `success` / `neutral` (FG-REPO-0029, 0030,
0031), never `failure`. A run shows only the repository's own text. The evaluation's deadline is
60 s and each write's 8 s (three runs and three `error` writes: 108 s, 12 s inside the job's 120 s); `SpecChecks::fail` answers `error` for all three on the
last attempt and `record_errors` for `forge worker`'s `spec_index_sweep`. forge-web's spec API
(`GET …/specs/…`) reads the same index with the reader's permissions.

**forge/doc-lint (plan 13 stage 2, ADR-0026)**: the same job writes a third run, `doc_lint::assess_doc_lint`,
from the **same read** of the head (`spec_index::index_head`: one `ls-tree` of the whole tree —
`forge_git::Browser::blobs_in_tree` — and one `cat-file --batch`; the specification directories' files are
taken from that listing, so the index equals `index_commit`'s). Documents are the `.md` files naming a
`doctype` in any folder, their types `.forge/doctypes/*.yml`; the lint is `forge_doc::lint::lint`, the same
as `gstn doc lint`. `success` / `failure` (FG-DOC-0001; FG-DOC-0003 for a type definition that cannot be
used) / `neutral` (FG-DOC-0002: `DocLimits` — 50 000 tree entries, 2000 `.md`, 100 over the parser's limit (each read only up to the limit + 1 byte, like gstn), 32 MiB in all,
64 definitions — never a silent pass). Broken references are errors (plan 13 Q13) unless the project's
**migration setting** is in force: `doc_lint::assess_doc_lint_with(tree, Some(until))` lowers only `ref_broken`
to warnings (annotations of level `warning`; only lowered problems → `success`, or `neutral` with a bound hit),
and the summary's first line is `migrating: until YYYY-MM-DD`. The job reads the date in force with
`forge_db::checks::doc_lint_migration::in_force`, compared in SQL with the database's date (UTC) — never this
host's clock; past the date it is errors again with nothing written. `doc_migration::DocLintMigrations`
(`Checks::doc_lint_migrations`) is the setting's read and its two commands for `GET|PUT|DELETE
/api/v1/projects/{id}/doc-lint/migration` (`ProjectRead` / `DocLintMigrationManage`: people only,
Maintainer): the date's shape here (`FG-WEB-0006 param=warn_until`), its bounds — after today, at most 180 days
ahead — by forge-db (`FG-DOC-0005`), and the decision again inside the transaction (`Authorized::reconfirm`;
`tests/decided_callers.rs` keeps `set_doc_lint_migration_decided` / `clear_doc_lint_migration_decided` here).

**forge's risk check (M3 X7, ADR-0023 Amendment X7)**: `risk::Risk` evaluates `forge/risk` for one
`check.risk` job (the same head; enqueued by forge-db's `risk` subscriber): the target branches of the
open merge requests that have the head as a version (`forge_db::checks::risk::risk_targets`), then per target
the diff from the merge base (`forge_git::risk_inputs`, renames paired), the pure rules of
`forge_core::risk` (paths, new migrations' SQL, deploy files, mass deletion against the merge base's tree) and
the secret scan of `forge_git::secret_scan` over added text files (60 files, 256 KiB each at most). `success` /
`neutral` (warn, or not fully assessed: FG-REVIEW-0024 / 0025) / `failure` (a stop rule: FG-REVIEW-0023) / `error`
(`Risk::fail`, and `record_error` for `forge worker`'s `risk_sweep`). A run names rule, path and line, never the
matched text. 45 s evaluation deadline, 10 s write. No HTTP route: the run is read through the Checks API.

**Owned error codes**: FG-REVIEW-0023 RiskStop, FG-REVIEW-0024 RiskWarn, FG-REVIEW-0025 RiskNotAssessed
(info notices named in a `forge/risk` run; X7), FG-REVIEW-0020 CheckPayloadInvalid (422), FG-REVIEW-0021
CheckRunLimitReached (409, from forge-db), FG-DB-0022 CheckRunDataInvalid (500,
alert; from forge-db), FG-REPO-0027 SpecVersionBehind (info, a `forge/spec-drift`
notice; X2 — FG-REPO-0024..0026 are forge-core's and the spec refs'), FG-REPO-0028
SpecLintProblem, FG-REPO-0029 RequirementUntested, FG-REPO-0030 RequirementTagUnknown,
FG-REPO-0031 SpecIndexIncomplete (info; X4). Denials are FG-AUTH-0003 with `reason`
(`not_service_account`, `namespace_not_granted`, `forge_namespace_internal`,
`ext_namespace_unavailable`, `agent_token`).

**Runbook**: `ops/runbooks/checks-reporting.md`.
