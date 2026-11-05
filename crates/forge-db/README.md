# forge-db

**Responsibility**: the PostgreSQL layer (ADR-0002, ADR-0018) — connecting with TLS and
every timeout applied, the write / read pool split and the migration
connection, mapping driver errors to registered codes, migrations
(ADR-0014) and the bootstrap checks. The Outbox sequencer and the job queue.

**Public API**
- `Db::connect(&DatabaseConfig) -> Result<Db, ForgeError>` — pool from
  `forge_core::config` (ADR-0013); checks PostgreSQL ≥ 16; logs
  `forge.db.connected` with `db_target` (host:port/dbname), `tls_mode`,
  `server_version_num`.
- Two pools, both logged in as `forge_app` (`database.url`): the write pool
  stays inside this crate (`write_pool()`, typed commands from step C3);
  `Db::pool()` returns the **`ReadPool`** (sessions run as `forge_ro` with
  `default_transaction_read_only = on`, set in the startup packet and
  verified by a probe at connect). Sizes: `database.write_max_connections`,
  `database.read_max_connections` (10 each).
- `Db::acquire()` (read pool), `Db::ping()` (both pools), `Db::close()`,
  `Db::target()`.
- `Db::connect_migrator(&DatabaseConfig) -> MigratorConn` — one connection on
  `database.migrate_url` (`forge_migrator`, `application_name = forge-migrate`),
  `SET ROLE forge_owner`; refuses a login that is a superuser or cannot
  become the owner (FG-SYS-0002 `reason=wrong_role`, `role_check`).
- `query_error` (write pool / migration connection) and `read_query_error`
  (read pool) — use at every statement:
  `.map_err(|e| query_error("repo.insert", e))`. `query` is a static name,
  never the SQL text.
- `Db::within(operation, future)` — client-side bound (acquire + statement
  timeout) for short multi-statement operations; `Db::close()` is bounded too.
- The app login must not be a superuser / BYPASSRLS or reach forge_owner,
  forge_seq or forge_purge (`wrong_role` at connect). The bootstrap objects
  live in schema `forge_audit`, owned by the member-less role `forge_audit`
  (ADR-0018 amendment).
- `bootstrap::check(&Db, privileged_members)` / `bootstrap::check_connections`
  — what `forge doctor` reports as `db.bootstrap` / `db.connections`.
  `bootstrap::FILES` embeds `ops/db/bootstrap/*.sql`; `BOOTSTRAP_VERSION` must
  match the version 03 writes (tested).
- `users::create(&Db, &ChangeContext, &NewUser) -> Result<Created, ForgeError>`
  and `users::disable(&Db, &ChangeContext, UserId) -> Result<DisableOutcome, ForgeError>`
  (step D1; `NewUser::human`, `AuthSource`): one change each, with its `user.*`
  audit row and no event. `create` also writes the personal namespace
  (`kind = user`, `owner_id` = the user), under the path's advisory lock
  (`namespaces::lock_path`, crate-private; D5 rename / move / delete / release take
  it too). Disabling a disabled user writes nothing. `disable` also revokes
  the user's active tokens in the same change (`revoked_reason = 'user_disabled'`,
  one `token.revoked` event each, `revoked_tokens` in the `user.disabled` audit
  row; step D3). `users::id_by_username(&Db, &str) -> Result<UserId, ForgeError>`
  (read pool; FG-REPO-0005 `resource=user`, `reason=absent`).
- **Disable guard** (step D3 MR2, ADR-0019 §10–§11): `disable` refuses
  (FG-AUTH-0009, nothing written, no token revoked) when it would leave no
  active **human** administrator (`last_admin`) or a **top-level** group
  without a direct active Owner (`last_owner`; subgroups inherit). `disable_with(…,
  Guard::Force)` passes it (the operator CLI's `--force` only) and records
  `forced`, `ownerless_groups` (a count) and `left_without_admin` in the
  `user.disabled` details; it returns `DisableOutcome::DisabledPastGuard` when
  the guard would have refused. Lock order: advisory lock `users.admin_set`
  (`users::lock_admin_set`, crate-private) → the user row `FOR UPDATE` →
  unlocked counts; group rows are never locked (member changes lock group →
  owner rows `FOR SHARE`). `users::bootstrap_admin(&Db, &ChangeContext,
  &Bootstrap) -> Bootstrapped` (`Promote(UserId)` or `Create(NewUser)` through
  the `create` path, `is_admin` set): refused while an active human admin
  exists (`admin_exists`), for bots / service accounts (`not_human`) and
  disabled users (`user_disabled`); same lock order; one audit row
  `user.admin_bootstrapped` (`created`, `namespace_id`), no event.
  `users::administration(&Db) -> Option<Administration>` (read pool; `None`
  before the migration) is what `forge doctor` reports as
  `users.administration`: `check()` is FG-AUTH-0010 (`no_admin` /
  `ownerless_group`, counts and the first 10 group ids, read in one statement),
  `is_fresh_install()` when there is no user at all.
  **Operator-only**: `Guard::Force`, `disable_with` and `bootstrap_admin` are
  for the CLI; the test `forge_web_never_names_the_operator_only_user_commands`
  fails if a source under `crates/forge-web/src` names them. **Every future
  change of `is_admin` or of an administrator's state** (grant / revoke admin,
  LDAP sync, any HTTP route) must take `lock_admin_set` first (ADR-0019 §11).
- `tokens::{issue, revoke, list, authenticate}` (step D3, ADR-0019): personal
  access tokens `fgp_<16 hex public_id>_<64 hex secret>`; only `sha256(secret)`
  is stored and only the write pool reads it. `issue(&Db, &ChangeContext, &NewToken)
  -> Issued` (the `PlainToken` exists only there) and `revoke(&Db, &ChangeContext,
  token_id) -> RevokeOutcome` write one audit row on the **owner** (resource
  `user`) and one `token.created` / `token.revoked` event (admin class); `issue`
  locks the owner row `FOR UPDATE`, so issues for one user and a concurrent
  disable serialize and the limit holds. Limits: expiry 1–365 days (default 90,
  `DEFAULT_EXPIRY` / `MIN_EXPIRY` / `MAX_EXPIRY`), 50 active tokens per user
  (`MAX_ACTIVE_TOKENS`), name 1–100 characters. The `admin` scope only for an
  administrator — checked here because issuing is CLI-only, outside
  `authorize()` (ADR-0019 §8). `revoke` does not check the owner (operator
  only); revoking a revoked token is `AlreadyRevoked` and writes nothing.
  `list(&Db, UserId) -> Vec<TokenInfo>` (read pool, newest first, never the
  hash). `authenticate(&Db, &RequestId, &str) -> TokenIdentity` (the type lives
  in forge-core and is re-exported; `Db` implements `forge_core::Authenticator`
  with it, which `forge serve` injects into forge-web): an unknown public id still
  costs one hash and constant-time comparison; the owner must be active;
  `last_used_at` is refreshed at most every 10 minutes, unaudited, and a failed
  refresh is logged once (FG-DB-0002, warn) without failing the request.
- `groups::create(&Db, &ChangeContext, &NewGroup) -> Result<CreatedGroup, ForgeError>`
  (step D2): the group, its namespace (`kind = group`, only at the top level or
  under a group) and its first Owner, under the path lock, with one
  `group.created` audit row. `groups::{add_member, change_member, remove_member}`:
  one `member.*` audit row each on the group (audit-only); every one locks the
  group row first; a top-level group keeps at least one direct active Owner;
  bots are never members. `change_member(.., user, from, to)` / `remove_member(.., user, from)`
  take the member's direct role the caller loaded and authorized against: a different
  stored role is FG-REPO-0010 `role_changed` (D4 review). `change_member` to the same role is
  `MemberChange::Unchanged` and writes nothing.
- `roles::{load_group_roles, load_namespace_roles}(&Db, id, UserId) -> Result<MemberRoles, ForgeError>`
  (step D2, ADR-0010): the only role computation. `MemberRoles { native, direct }` —
  `native` is the highest direct role over the group and its ancestor groups
  (recursive query on the read pool, capped at 20 steps), `direct` the role in that
  group alone; active users only; a personal namespace has no group role.
  `roles::load_project_roles(&Db, ProjectId, UserId) -> Result<MemberRoles, ForgeError>`
  (step D4 MR2, ADR-0010 Amendment D4): `direct` = the project_members role,
  `native` = the highest of it, the ancestor groups' direct roles and Owner for the
  (non-bot, active) owner of the personal namespace that is the project's direct
  parent; same query, same cap.
  `load_namespace_roles` on a project namespace gives the same answer.
- `projects::create(&Db, &ChangeContext, &NewProject) -> Result<CreatedProject, ForgeError>`
  (step D4 MR2, ADR-0015 Amendment D4): `NewProject::new(segment, parent, display_name,
  creator)` defaults to visibility `private`, kind `system`, an empty description. One
  `ChangeTx` writes the project, its namespace (`kind = project`, under a user or group
  namespace only — never under a project, nor in a bot's or an inactive user's
  personal namespace), its repository (`provisioning`), the creator
  as direct Owner, the `repository.provision` job (`repositories::PROVISION_JOB`, 60 s,
  dedup `repository_provision:<repo_id>`, payload `{"repo_id"}`, origin = the request
  id), one `project.created` audit row (resource `project`, `project_id` set) and one
  `project.created` event (integration class, `project_id` = the project, payload = the
  audit details). Path lock and tombstone checks as for groups. `creator` must be the
  acting user (`Actor::System`, the operator / worker, may name anyone). `CreatedProject {
  project_id, namespace_id, repository_id, provision_job }`.
- `lists::{project_page, group_page}` and `impl forge_core::ProjectPageLoader /
  GroupPageLoader for Db` (M1 phase 8b MR7b): one statement per page
  (`PROJECT_PAGE_SQL` / `GROUP_PAGE_SQL`): the ids above `after`, in order, at most
  `max_rows` (1..=`MAX_PAGE_ROWS` = 10000, else `FG-SYS-0001
  reason=page_size_out_of_range`), with the viewer's chain roles anchored at every
  scanned namespace (the role loaders' columns). **No visibility filter**: a project row
  is `resolve::interpret_project`'s (identical to `load_project`), a group row has
  `FG-DB-0021 group_without_namespace | group_path_unconfirmed | stored_path_invalid`.
- `resolve::{resolve, load_project, project_access}` and `impl forge_core::NamespaceResolver /
  ProjectAccessLoader for Db` (M1 phase 8 MR3, ADR-0015 Amendment MR3; `forge serve`
  injects the `Db` into forge-web like the authenticator):
  `resolve(&Db, &LookupPath, viewer: Option<UserId>) -> Result<Resolution, ForgeError>` —
  the exact live path first, else the exact tombstone (`Live` / `Moved { resolved,
  tombstone, current }` / `Deleted { tombstone }` / `Absent`); `project_access(&Db,
  ProjectId, viewer) -> Result<Option<forge_core::ProjectAccess>, ForgeError>`
  (`None` = no such project; it replaces D4's `projects::access` and its
  `ProjectAccess { visibility, pending_deletion }` — one type, forge-core's, now with
  the viewer's `role`); `load_project(&Db, ProjectId, viewer) ->
  Result<Option<forge_core::ProjectRecord>, ForgeError>` (MR5: the same one statement
  also returns the project's namespace — `full_path` confirmed against its segments —
  names and `created_at` in UTC; `project_access` is its `access`;
  FG-DB-0021 `project_path_unconfirmed` / `stored_path_invalid` /
  `project_without_repository` for rows it cannot read). **One statement per call, whatever the answer**
  (`RESOLVE_SQL`, `PROJECT_ACCESS_SQL`): it always returns exactly one row (an
  aggregate without `GROUP BY` drives `LEFT JOIN`s), so live / moved / deleted /
  absent, visible / invisible and signed-in / anonymous (`$2` NULL) issue the same
  one statement on the read pool under `Db::within` (ADR-0015 step 6). Only the
  statement count and text are constant, not the work inside (an absent path is two
  index probes, a deep project walks its chain): an accepted residual, mitigated by
  resolve rate limiting (M2) and, if needed, a response-time floor in forge-web. The viewer's
  role is the role loaders' own chain CTE and fold (`roles::chain_cte!`,
  `chain_role_columns!`, `fold_chain`): the same `native` as `load_project_roles`
  / `load_group_roles` (tested). A `moved` tombstone's `current` path is rebuilt
  from the target's segments, confirmed by an exact match on the live paths with
  the target's id, and read through `NamespacePath::parse`. Only a purged target
  (`namespace_id` NULL) is `Deleted`; a target that exists but whose rebuilt path is
  not live is FG-DB-0021 `moved_path_unconfirmed`, a broken chain FG-DB-0018.
  Nothing here decides visibility (`authorize()` does). The test
  `every_outcome_runs_the_same_single_statement` counts sqlx's per-statement
  events for every outcome.
- `repositories` (step D4 MR3b, ADR-0009 / ADR-0016 Amendments D4 MR3b):
  `provision_target(&Db, RepoId) -> ProvisionTarget { repo_id, project_id, state, token }`
  (write pool — `forge_ro` cannot read `provision_token`; `Debug` redacts the token);
  `provision_payload(&Value) -> RepoId` (the job's `{"repo_id"}`);
  `mark_ready(&Db, &ChangeContext, RepoId) -> MarkReady::{Marked, AlreadyReady}` —
  row `FOR UPDATE`, state `ready` + `ready_at`, one `repository.created` audit row on
  resource (`project`, project id) with `project_id` set and one `repository.created`
  event (integration, aggregate `repository`, payload `{project_id, repository_id,
  kind}`, dedup `repository_created:<repo_id>`); an already ready repository writes
  nothing (no second event); the UPDATE must touch exactly one row.
  `mark_ready_within(.., timeout)` takes the change bound from the caller (the
  provisioning job passes what is left of its own timeout).
  **Key chain**: `jobs_dedup` keeps the key of queued, running and succeeded jobs, so the
  current job of a repository is the end of `repository_provision:<id>` →
  `repository_provision:<id>:after:<succeeded job>` → … (`CHAIN_CAP` 16 links, equality
  lookups in a recursive CTE). `provisioning_report(&Db, stuck_after) ->
  Option<ProvisioningReport>` (read pool; `None` before the D4 schema): repositories in
  `provisioning` (count capped); the 10 000 oldest classified in SQL by their current job —
  `job_not_live` (none, dead or succeeded) or `too_old` (live, older than `stuck_after`,
  `PROVISIONING_STUCK_AFTER` = 10 min) — with per-reason counts and the first 100 stuck
  (`job_not_live` first, each oldest first); `ProvisioningReport::check()` → FG-REPO-0012
  (`reason`, `job_not_live`, `too_old`, `job_state`; `detail` for a succeeded job).
  `ready_ids(&Db, after, limit)` — ready repository ids in id order, keyset pages of at
  most `READY_PAGE_MAX` (1000). `reprovision(&Db, &ChangeContext, RepoId) ->
  Reprovisioned::{Queued { project_id, job, after }, AlreadyLive { project_id, job, state }}`
  — row locked, a ready repository is FG-REPO-0014 `repository_ready`; by the current job:
  queued / running → `AlreadyLive` (nothing written), dead / none → a new job with that
  link's key, succeeded → a new job keyed `…:after:<that job>`; the job plus an audit-only
  `repository.reprovision_requested` row (on the project, `holder_job_id` /
  `holder_state`) in one change. Concurrent calls serialize on the row lock.
- `projects::{add_member, change_member, remove_member}(&Db, &ChangeContext, ProjectId,
  UserId, ..)` (`add_member(.., role)`, `change_member(.., from, to)`, `remove_member(.., from)`;
  `from` is the authorized direct role, else `role_changed`) (step D4 MR4, ADR-0019 Amendment D4 MR4): one `member.*` audit row on
  (`project`, id) with `project_id` set **and** one `member.*` event of the project
  (`EventType::Member{Added,RoleChanged,Removed}`, maintainer class, payload = the audit
  details `AuditDetails::ProjectMember*`, the group JSON shapes; no dedup key). Lock
  order: the project row `FOR UPDATE`, then user rows `FOR SHARE`. Bots are never
  members; `change_member` to the same role is `MemberChange::Unchanged` (nothing
  written). The last direct Owner may be removed or demoted while an inherited active
  Owner exists (an ancestor group's direct Owner, the personal namespace's owner — the
  `load_project_roles` sources), else FG-REPO-0010 `last_owner`. No authorization here:
  a route decides with `authorize()` first.
- `projects::ownerless(&Db) -> Result<Option<Ownerless>, ForgeError>` (D4 MR4): the
  projects (not pending deletion) without any active Owner, direct or inherited —
  `Ownerless { count, project_ids (first 10) }`, `check()` = FG-AUTH-0010
  `ownerless_project`; `None` before the projects migration. For `forge doctor`
  `users.administration`. `users::disable` refuses (FG-AUTH-0009 `last_owner`,
  `project_id`) a disable that would leave a project without an active Owner; `--force`
  records `ownerless_projects` in `DisableOutcome::DisabledPastGuard` and the
  `user.disabled` details.
- `protected_refs::{create, change, delete}` (step D4 MR5, ADR-0008 Amendment D4 MR5):
  `create(&Db, &ChangeContext, ProjectId, &RefPattern, Role) -> ProtectedRefId`,
  `change(.., ProjectId, ProtectedRefId, Role) -> RuleChange { Changed { from } | Unchanged }`
  (the minimum push role only), `delete(.., ProjectId, ProtectedRefId)`. The project row is
  locked first; at most `MAX_RULES` = 100 rules per project (`too_many_rules`), no two
  patterns equal by `RefPattern::key()` (FG-REPO-0017 `duplicate_pattern` since M1 phase 8b
  MR8, non-ASCII case included),
  `push_min_role` Developer / Maintainer / Owner. One `protected_ref.*` audit row on
  (`project`, id) and one `protected_ref.*` event (maintainer class, payload = the details,
  no dedup key) each. `delete` does not parse the row, so a rule this build cannot read
  (FG-DB-0020) can be deleted; its details carry `stored_invalid` and the stored text,
  bounded. No authorization here (`Action::ProtectedRefManage` is the route's).
- M1 phase 8b MR8 — the API's commands, decided inside their transaction (only
  forge-hosting calls them, `decided_callers` test): `projects::{add,change,remove}_member_decided`
  (`ProjectMemberFacts { account, project, member_role, removes_last_owner }`),
  `groups::{add,change,remove}_member_decided` (`GroupMemberFacts`) and
  `protected_refs::{create,change,delete}_decided` (`ProtectionFacts { account, project, rule:
  RuleState::{New, Absent, Level, Unreadable} }`). Lock order: target row `FOR UPDATE` (project /
  group; for a rule then the rule row `FOR UPDATE`), the caller's membership rows `FOR SHARE`, the
  caller's account row, then the member's direct membership row `FOR UPDATE` and — when the
  change takes an Owner away — the remaining Owners' user rows `FOR SHARE`. `decide` sees the
  member's direct role (the `from`) and the last-owner fact; conflicts (`already_member`,
  `not_member`, `bot_member`, 404 rule, FG-DB-0020 on a change of an unreadable rule) and the
  last-owner backstop come after it; every non-commit exit rolls back (`ChangeTx::settle`).
  Reads for the route loaders: `members::{page, one, direct_role}` (`MemberScope`, keyset by
  user id, `MAX_PAGE` = 100) and `protected_refs::{list, rule_state}`.
- `protected_refs::levels_for(&Db, ProjectId, &RefName) -> Result<Vec<Role>, ForgeError>`:
  the `push_min_role` of every rule whose pattern matches the ref ignoring case, in rule
  id order — feed it to `forge_auth::RefProtection::effective` (strictest wins); the
  push policy snapshot is built from it. One bounded read (≤ 101 rows) on the read pool.
- `review` (M3 R1a, ADR-0024; the SQL of forge-review, plan 11 §11.2): reads
  `get(&Db, ProjectId, iid) -> Option<StoredMergeRequest>` and `list(&Db, ProjectId,
  Option<MergeRequestState>, before, limit)` (newest first, `MAX_PAGE` = 100, read pool);
  commands `open_mr_decided` / `edit_mr_decided` / `close_mr_decided` / `reopen_mr_decided`
  (named with `_mr_` so forge-hosting's `decided_callers.rs` needles never match; only
  forge-review calls them, `only_forge_review_calls_the_review_commands`). Each locks the
  project row, the merge request row, then the caller's roles and account, and calls
  `decide` (`MergeRequestFacts { account, project, merge_request }`) before writing. Open
  refuses a second open merge request for the same (project, source, target) under the
  project lock (FG-REVIEW-0001 `open_exists`; the partial unique index
  `merge_requests_one_open` answers `unique_index`), draws the iid from
  `merge_request_counters` (`INSERT … ON CONFLICT DO UPDATE`, never `max(iid)+1`: the purge
  deletes rows), then inserts. Edit (`MergeRequestPatch`: title, description, draft, target)
  returns `EditOutcome::Changed(events)` or `Unchanged`; close / reopen `StateOutcome`; every
  command also returns the row as it stands after the transaction. A CHECK on a request
  value is `FG-WEB-0006 param_invalid` (with `constraint`), not a query error.
  Every change writes one `merge_request.*` audit row (resource `project`) and one event
  (aggregate `merge_request`, integration class) per transition, deduplicated by
  `<prefix>:<merge_request_id>:<revision>` (`revision` counts the row's changes).
  M3 R1b (versions, ADR-0024 §1, §2; table `merge_request_versions`, audited on the project,
  guard `merge_request_versions_guard`): `record_mr_version(&Db, ctx, mr, head, pushed_by) ->
  VersionOutcome` (the version job's write: row FIRST under the merge request's lock, `max +
  1`, `merge_request.pushed` deduplicated by `merge_request_pushed:<id>:<head_sha>`; an older
  head again is the audit-only `merge_request.head_restored`; `AlreadyHead` / `PurgedHead` /
  `NotOpen` / `Gone` write nothing; `PurgedHead` writes only the audit-only
  `merge_request.purged_head_pushed`), `list_versions`, `refresh_target`, `refresh_job` /
  `refresh_payload` (job `merge_request.refresh`, dedup `mr_refresh:<id>:opened` /
  `reopened:<rev>` / `push:<seq>`), `lock_version_refs` (the per merge request refs lock:
  `pg_advisory_xact_lock` on `merge_request_refs:<id>`, waited ≤ 10 s, held ≤ 45 s, proven by
  `RefsLock::alive` before a ref write; `versions_snapshot` reads under it), `purge_mr_version_refs_decided` (marks `refs_purged_at`, audit-only
  `merge_request.version_refs_purged`; FG-REVIEW-0002 `purge_requires_closed_or_rewritten`).
  `open_mr_decided` / `reopen_mr_decided` enqueue the version job; `reopen_mr_decided` takes the
  source's head and refuses a purged one (FG-REVIEW-0002 `reopen_at_purged_head`). The
  subscriber `MergeRequestRefresh` (`merge-request-refresh`, independent) enqueues the job on
  `repository.pushed` for the open merge requests from the moved branches, or every open one
  when `refs_truncated`. The project purge deletes the versions. Tests
  `review/version_tests.rs`.
- **Merges** (`review/merges/`, M3 R4, ADR-0024 §4; migration 20261110100000, which also
  widens `tokens_scopes_known` with `merge_execute`): `merge_request_merges` (derived: one row
  per attempt — method, state queued / prepared / executing / applied / superseded / failed,
  the requester and the credential's kind and id only, the head asked for, `expected_old` /
  `new_sha` / `executing_at` set once, `reason` / `error_code`, `source_outcome`; guard
  `merge_request_merges_guard`; one live attempt per merge request, partial unique index
  `merge_request_merges_one_live`). `request_merge_mr_decided` (decide on the locked facts
  plus the protection rules, the stored mergeability, the live attempt and the credential —
  `MergeGate` —, then the `queued` row and the job `merge_request.merge`, dedup
  `mr_merge:<merge_id>`, FG-REVIEW-0011 for a live one), `merge_gate_decided` (tx1a / tx1b; a
  refusal supersedes in the same transaction), `fence_merge`, `executing_elapsed`,
  `finish_merge` (tx2: applied, the merge request merged, `merge_request.merged` +
  `repository.pushed`, dedup by the merge id), `record_source_deleted`, `end_merge`,
  `set_source_outcome`, `stuck_merges`, `lock_merges` (`merge:<repo_id>`, alive by `pg_locks`),
  `account_identity`; close / retarget / draft toggle refuse a live merge. Tests
  `review/merge_tests.rs`.
  Review round 1 (migration 20261110100100): the guard also covers INSERT (attempts start
  `queued`) and freezes `applied` rows but `source_outcome`; `reconcile_seen_at` +
  `mark_reconcile_seen` rotate the reconciliation's scan; `lock_merges` reports a busy lock as
  `merge_lock_busy` (`is_lock_busy`) under its own bound; `finish_merge` sets `failed`
  (`merge_request_not_open`) when the merge request is no longer open; the version-ref purge
  refuses a live merge; `merges::health` feeds doctor `review.merges`.
- **Approvals** (`review/approvals/`, M3 R5, ADR-0024 §8; migration 20261111100000, which also
  widens `tokens_scopes_known` with `merge_approve`): `merge_request_approvals` (audited; bound
  to `head_sha` / `target_branch`; ends once with `revoked_reason` unapproved / new_commits /
  target_changed; guard `merge_request_approvals_end_once`; one live row per approver),
  `approval_rules` + `approval_rule_approvers` (audited). `approve_mr_decided` /
  `unapprove_mr_decided` (FG-REVIEW-0015 past 50 live approvals; `merge_request.approved`
  carries the classification `classify` returns), the version job and the edit command revoke
  every live approval in their transaction (one `merge_request.unapproved` each),
  `put_approval_rule_mr_decided` / `remove_approval_rule_mr_decided` (audit only),
  `approval_facts` (approvers' active / kind / role / direct groups now; also in `MergeGate`),
  `range_pushers` (M3 R5 round 4: the pushers of a range's commits from `push_commits`,
  which the push record fills with every commit a push introduced — `record_push_with_commits`;
  `listed_commits` for callers without git; a merge records its own commit),
  `owners` (CODEOWNERS names → users / groups). Tests `review/approval_tests.rs`.
- **Merge conditions** (M3 R6, ADR-0024 §4.1, §8.2, §9, §12; migration 20261112100000):
  `merge_settings` (audited; one row per project that changed them — none = the defaults; the
  CODEOWNERS switch with a CHECK that a suspension ends at most 24 h after it began) via
  `review::settings` (`merge_settings`, `put_merge_settings_mr_decided` — audit only —,
  `toggle_codeowners_mr_decided` — event `merge_settings.codeowners_enforcement_changed` —,
  `restore_expired_codeowners` for the worker — event `…_restored`, dedup on project and expiry;
  an expired suspension reads as enforced by itself); `protected_ref_merge_rules` (audited; the
  minimum approvals, required checks `ci/` / `forge/` / `kanflow/`, resolved discussions, the
  direct-push ban) joined into the push / merge snapshot (`rules_for`, `rules_on`: an unreadable
  one is FG-DB-0020), `protected_refs::put_merge_rule_decided` / `remove_merge_rule_decided`
  (audit only, deleted with their rule and in the purge); `merge_request_approvals.carried_from`
  and `record_mr_version_carrying` (the carry, decided again under the lock: the setting, an
  unprotected target, the head it was decided for). `MergeGate` and `ApprovalFacts` carry the
  settings, the protection rules and the unresolved discussion count. Tests
  `review/settings_tests.rs`, `review/approval_tests.rs`.
- **Discussions** (`review/discussions/`, M3 R3, ADR-0024 §6, §12; migration
  20261109100000): `merge_request_discussions` (kind general / inline; an inline one's
  version — a composite foreign key to the versions —, path, side old / new and line;
  `started_by`, `resolved` / `resolved_by` / `resolved_at`, `revision` +1 per resolve /
  unresolve) and `merge_request_notes` (author, body 1 character..64 KiB, never updated).
  Audited on the project; guards `merge_request_discussions_guard` (identity and position
  fixed, revision +1 per change) and `merge_request_notes_guard` (no UPDATE). Commands
  `start_discussion_mr_decided`, `add_note_mr_decided`, `resolve_discussion_mr_decided`
  (decide on the locked facts; one `merge_request.commented` audit row and event each,
  `data.action` = `note_added` / `thread_resolved` / `thread_unresolved`, dedup
  `merge_request_note_added:<note_id>` or `merge_request_thread_(un)resolved:<id>:<revision>`;
  an unknown version is FG-REVIEW-0004 `version_unknown` by the foreign key). Reads
  `discussions` (at most 500 discussions / 5 000 notes, `truncated`) and `discussion`. Caps
  checked under the merge request's row lock (review round 1): 500 discussions and 5 000 notes
  per merge request, 200 notes per discussion (`MAX_DISCUSSIONS`, `MAX_NOTES`,
  `MAX_NOTES_PER_DISCUSSION`; FG-REVIEW-0005 DiscussionLimitReached). Migration 20261109100100
  adds composite foreign keys (a thread's project is its merge request's; a note's discussion,
  merge request and project agree). The steps the commands share are `discussions/steps.rs`. The
  project purge deletes notes, then discussions, before the versions. The token and
  session authenticators now read `users.kind` (`TokenIdentity.kind`,
  `SessionIdentity.kind`; `users::parse_kind`, never a default). Tests
  `review/discussion_tests.rs`.
- **Repository maintenance** (`maintenance/`, M3 O3; migration 20261117100000, tables.toml
  `derived`): the ledger `forge.repository_maintenance` (outcome, code, skip reason, consecutive
  `failures`, counts, fsck, `next_due_at`). `schedule_due(db, limit, rid)` enqueues the due ready
  repositories oldest first (never maintained: by `ready_at`), keyed `repository_maintain:<repo>:<due
  epoch>`, at most `limit` live (queued / running) maintenance jobs; `enqueue_now` (the admin
  command, key `…:admin:<request_id>`, NotFound with `state` unless ready); `start` (`None` unless
  ready; the repository row `FOR SHARE`; a first row's `next_due_at` is `ready_at`, so the
  scheduler's key stays stable; a previous run still `running` past `STALE_RUNNING_AFTER` is counted:
  `lost_previous`) / `finish` (failures count up, a success resets and sets `last_success_at`,
  migration 20261117100100); `reap_stale_running(db, retry_after)` (rows `running` past the lease
  with no live job → `failed` FG-DB-0007); `local_minute(db, tz)` (the window's
  clock; an unknown zone is FG-SYS-0002); `report` (doctor: overdue, failing ≥ 2, corrupt, refs over
  the warn, stale running, no success within twice the interval; `None` before the migration). The project purge deletes the rows. `review::merges::
  try_lock_merges` takes the merge lock without waiting (`None` while a merge holds it: the
  maintenance's pack-refs); `REPO_MAINT_LOCK_PREFIX` (`repo_maint:`) is reserved for O1.
- **Conflict predictions** (`review/conflicts/`, M3 X6, plan 04 C-2, ADR-0024 Amendment X6): the
  derived tables `merge_request_conflict_scans` (one row per merge request: `status` pending /
  scanning / scanned / incomplete / failed, `reason`, `scan_seq`, what the last scan looked at and
  its counts) and `merge_request_conflict_pairs` (one row per unordered pair `low_mr_id <
  high_mr_id`: the two heads, the target, `outcome` no_overlap / clean / conflict / failed, the
  conflicted paths, `prediction_seq`; migration 20261123100000). `request_scan_on` (the retarget
  and the reopen, in their transaction), `request_scan` / `request_scan_for_head` (the version
  job: unless a `pending` scan or a finished one of this head and target with a non-retryable
  reason answers), the job `merge_request.conflict_scan` (dedup `mr_conflict_scan:<id>:<seq>`),
  `begin_scan`, `scan_peers`, `record_scan` (under the scan row's lock; each pair row created,
  locked and updated in (low, high) order; a pair that enters `conflict` writes
  `merge_request.conflict_predicted` for both sides, one audit row each, actor `system`),
  `fail_scan` / `reap_stale_scans` (`job_failed` / `job_stale`, FG-REVIEW-0022), reads
  `conflict_scan` and `conflict_predictions` (only pairs whose two heads are current, both open,
  same target). Purged with the project. The notification kind `mr_conflict_predicted`
  (migration 20261123100100).
- **Mergeability** (`review/mergeability.rs`, M3 R2, ADR-0024 §3): the derived table
  `merge_request_mergeability` (one row per merge request: `status` unchecked / checking /
  can_be_merged / cannot_be_merged / check_failed, `reason`, `check_seq`, the checked
  head / target pair, capped `conflicts`, `last_decided`; migration 20261108100000).
  `request_check_on` (in the caller's transaction: `unchecked`, `check_seq + 1`, job
  `merge_request.mergeability` deduplicated by `mr_mergeability:<id>:<seq>`) — called by the
  edit command on a retarget, by the subscriber for the open merge requests targeting a
  pushed or deleted branch (at most `MergeRequestRefresh::new(fanout)`, the rest
  `defer_checks_on` + warn `forge.review.mergeability_deferred`), and by `request_check`
  (the version job). `begin_check` (a stale seq is `Superseded`; else `checking`),
  `record_check` (stored only while the seq is the latest; a can ⇄ cannot difference from
  `last_decided` writes `merge_request.conflict_detected` / `conflict_resolved` with an
  audit row, dedup `<prefix>:<id>:<seq>`; anything else commits through
  `Settle::CommitDerived`, a derived-only commit), `mergeability` (read pool) /
  `mergeability_written`. `record_version_bases` sets a version's `base_sha` /
  `merge_base_sha` once (audit-only `merge_request.version_bases_recorded`);
  `read_versions` (read pool). The project purge deletes the rows. Review fixes: the module is
  `review/mergeability/{mod,request,run,stale}.rs`; `record_check` also refuses a closed merge
  request or a head other than the one checked; a new version downgrades a decided status in its
  own transaction (`downgrade_on`); a purge of the checked version resets the row; `fail_check`
  (`job_failed`), `reap_stale_checks` (`checking` past `STALE_AFTER` with no live job →
  `job_stale`), `sweep_unchecked` (open `unchecked` rows with no live job, oldest first, 50 per
  run), `defer_checks_on` bounded by `DEFER_MAX` (5 000, id order), `decided_candidates` /
  `confirm_live` (decided rows verified against the live tips by the sweep, least recently
  verified first; column `confirmed_at`, migration 20261108100100), `health` (doctor
  `review.mergeability`, also `decided_stale_head` / `decided_unverified`); a run refused because
  the merge request closed resets the row to `unchecked`; all log FG-REVIEW-0003 MergeabilityNotChecked (warn, runbook
  `merge-request-mergeability`). Reasons are `forge_core::CheckReason`. Tests
  `review/mergeability/tests.rs`.
- **Event scope rule** (`ChangeTx`, ADR-0016 Amendment D4): every event's
  `project_id` equals the audit row's — a project event needs an audit row naming
  that project, an audit row without a project publishes only instance-wide events
  (FG-DB-0010 `event_scope_mismatch`); an audit row on resource `project` must have
  `project_id = resource_id` (`audit_scope_mismatch`).
- Feature `test-support` (dev-dependency of other crates only):
  `test_support::FreshDb` — a database bootstrapped with the real files,
  logins with synthetic passwords.

**Bounded waits**: `connect_timeout` (whole connect), `acquire_timeout`
(pool), and per session `statement_timeout`, `lock_timeout`,
`idle_in_transaction_session_timeout`. `application_name = forge`.

**Error codes owned**: FG-DB-0001 DbUnavailable (`reason`, `target`),
FG-DB-0002 DbQueryFailed (`query`, `sqlstate`, `reason`), FG-DB-0003
MigrationFailed, FG-DB-0004 SchemaIncompatible, FG-DB-0009
AppendOnlyViolation (XF001 and the bootstrap guards, `reason` / `guard`), FG-DB-0005
OutboxSequencerLag (`unsequenced_count`, `heartbeat_age_ms`, `lock_holder_pid`), FG-DB-0006
JobDeadLettered (`job_id`, `kind`, `attempts`, `last_error_code`, `origin_request_id`;
`reason`), FG-DB-0007 JobLeaseExpired (`job_id`, `kind`, `worker_id`, `attempt`; `reason`), FG-DB-0017 JobQueueBacklog (`kind`, `ready`,
`oldest_wait_ms`; `reason`), FG-DB-0011 AuditSealMismatch (`reason` recompute / chain /
hmac / gap, `detail`, `seal_id`), FG-DB-0015 AuditSealBacklog (`oldest_age_s`,
`audit_sequencing`), FG-DB-0008 SubscriberLag (`reason` stuck / behind / quarantined /
fanout_limit, `subscriber`, `seq`, `event_type`, `lag_events`), FG-DB-0013 AuditMissing
(`reason` no_audit_row / derived_lineage / derived_depth / derived_fanout /
derived_progress / audited_change with `table`), FG-DB-0018 MembershipDataInvalid (`roles`), FG-DB-0020 ProtectedRefDataInvalid
(`protected_refs`), FG-DB-0021 NamespaceDataInvalid (`resolve`).
Raised by `users::create` / `users::disable`: FG-REPO-0007 / 0008 (`PathSegment`),
FG-REPO-0009 (`reason=sibling` or `tombstoned`), FG-AUTH-0005 (`reason=email_taken`,
ignoring case), FG-AUTH-0006 (field rules, `reason` / `len`), FG-AUTH-0007
(`reason=employee_number_taken` / `external_id_taken`), FG-REPO-0005 (`resource=user`,
`reason=absent` on disable), FG-SYS-0001
(`reason=unexpected_dedup`, `unknown_user_kind`), FG-AUTH-0009 AdministrationConflict
(409: `last_admin` / `last_owner` from `disable`, `admin_exists` / `not_human` /
`user_disabled` from `bootstrap_admin`; ids only). `Administration::check`:
FG-AUTH-0010 AdministrationOrphaned (warn; `no_admin` / `ownerless_group`).
Raised by `groups`: FG-REPO-0007 (also `too_deep`, `path_too_long`) / 0008 / 0009,
FG-REPO-0010 MembershipConflict (`already_member`, `not_member`, `last_owner`,
`bot_member`), FG-REPO-0011 GroupRequestInvalid (`display_name_length`,
`description_length`, `nul_character`, `parent_not_group`, `owner_inactive`),
FG-REPO-0005 (`resource=group` / `namespace` / `user`, `reason=absent`), FG-DB-0018
(`unknown_role` on a stored role outside the five; `resource=group`, `id`, `user`, `raw` —
the role loader's code since D3 MR2, it was FG-SYS-0001), FG-SYS-0001 (`unexpected_dedup`).
Raised by `tokens`: FG-AUTH-0008 TokenRequestInvalid (`expires_out_of_range`,
`scopes_empty`, `name_length`, `nul_character`, `control_character`, `owner_bot`,
`owner_disabled`, `admin_scope_requires_admin`, `active_limit`; nothing is written),
FG-AUTH-0001 from `authenticate` (`reason` in the log only: `malformed`, `unknown`,
`mismatch`, `revoked`, `expired`, checked in that order), FG-AUTH-0004
(`reason=owner_disabled`), FG-REPO-0005 (`resource=user` / `token`, `reason=absent`),
FG-DB-0019 CredentialDataInvalid (a stored row this build cannot read: `unknown_scope`,
`bad_hash`; alert), FG-SYS-0001 (`reason=rng_unavailable`), FG-DB-0001 when the database
is down (never a 401), FG-DB-0002.
Raised by `projects`: FG-REPO-0007 (also `too_deep`, `path_too_long`) / 0009,
FG-REPO-0014 ProjectRequestInvalid (`display_name_length`, `description_length`,
`nul_character`, `parent_not_owner_namespace`, `parent_owner_inactive`,
`creator_inactive`, `creator_not_actor`), FG-REPO-0010 (`bot_member`: a bot as creator
or a bot's personal namespace as parent), FG-REPO-0005 (`resource=namespace` / `user` /
`project`, `reason=absent`), FG-SYS-0001 (`unexpected_dedup`, `unexpected_job_dedup`,
`namespace_owner_missing`), FG-DB-0010 from the change checks.
Raised by the project member commands (D4 MR4): FG-REPO-0010 (`already_member`,
`not_member`, `last_owner` with `project_id`, `bot_member`), FG-REPO-0005
(`resource=project` / `user`, `reason=absent`), FG-DB-0018 (`unknown_role`,
`resource=project`), FG-SYS-0001 (`project_without_namespace`). `Ownerless::check`:
FG-AUTH-0010 (`ownerless_project`, `ownerless_projects`, `project_ids`).
Raised by `protected_refs` (D4 MR5): FG-REPO-0013 (`too_many_rules`
with `rules`, `push_min_role`), FG-REPO-0017 ProtectedRefPatternTaken (409, MR8:
`duplicate_pattern` / `unique_index`, `project_id`), FG-REPO-0005 (`resource=project` / `protected_ref`,
`reason=absent`), FG-DB-0020 ProtectedRefDataInvalid (alert: `too_many_stored_rules`,
`stored_pattern_invalid`, `stored_push_min_role_invalid`; ids only, never the pattern).
Raised by `resolve`: FG-DB-0021 NamespaceDataInvalid (alert: `stored_path_invalid` with
`rule`, `moved_path_unconfirmed`, `unknown_namespace_kind`, `unknown_tombstone_kind`, `unknown_visibility` with `raw`,
`namespace_without_project`, `project_without_namespace`; ids only, never the path),
FG-DB-0018 (`namespace_chain_too_deep`, `unknown_role`; `resource=namespace` / `project`,
`user` only for a signed-in viewer), FG-DB-0001 / 0002. Absence is `Resolution::Absent` /
`None`, never an error.
Raised by `roles`: FG-REPO-0005 (`resource=group` / `namespace` / `project`, `reason=absent`),
FG-DB-0018 MembershipDataInvalid (`namespace_chain_too_deep`, `unknown_role`; `resource`,
`id`, `user`, `raw`), FG-DB-0001 / 0002.
Raised by `repositories` (D4 MR3b): FG-REPO-0012 RepositoryProvisioningStuck (alert, from
`ProvisioningReport::check`: `reason` job_not_live / too_old, `stuck`, `repo_id`,
`project_id`, `age_ms`, `job_state`, `threshold_ms`), FG-REPO-0014 (`repository_ready`
from `reprovision`), FG-REPO-0005 (`resource=repository`, `reason=absent`), FG-SYS-0001
(`invalid_job_payload`, `unexpected_dedup`, `unknown_repository_state` /
`invalid_provision_token` for a row outside its CHECKs).

**Runbooks**: `ops/runbooks/db-unavailable.md`, `db-bootstrap.md`,
`append-only-violation.md`, `outbox-sequencer-lag.md`, `job-dead-lettered.md`,
`job-queue-backlog.md`, `audit-seal-mismatch.md`, `audit-seal-backlog.md`, `membership-data.md`,
`subscriber-lag.md`, `credential-data.md`, `user-offboarding.md`,
`repository-provisioning.md`, `namespace-data.md`.

**Tests**: need the development database (ADR-0011) —
`docker compose -f ops/dev/compose.yml up -d --wait` in `Git/`, then
`cargo test -p forge-db`. Without it the DB tests **fail** with that command
(they never skip). Override the target with `FORGE_TEST_DATABASE_URL`
(a superuser, password `forge_dev`, TLS disabled). The tests run
`ops/db/bootstrap/01` once per process (cluster roles — shared by every
test process, serialized by an advisory lock) and 02 + 03 for every
`FreshDb`.

**Gotchas**
- TLS: `verify-full` (default), `require`, `disable` — no `prefer`, no
  `verify-ca` (the rustls trust store also holds the public web roots;
  ADR-0013 records the remaining risk). `require` / `disable` to a
  non-loopback host also needs `database.tls_allow_unverified = true`
  (FG-SYS-0002 `unverified_remote` otherwise). The compose database
  has no TLS, so development uses `database.tls_mode = "disable"` (a warning is
  logged for a non-loopback host).
- No `query!` macros yet, so there is no `.sqlx/` offline data; it arrives
  with the first table, together with `cargo sqlx prepare --check` in
  `/verify`.
- sqlx is pinned to 0.8.6 (0.9 needs Rust 1.94 and is not yet evaluated).
- Options are built explicitly (host / port / user / dbname from the URL,
  `new_without_pgpass`); a set `PG*` variable (PGPASSWORD, PGSSLROOTCERT,
  PGOPTIONS, …) is FG-SYS-0002 `reason=libpq_env`.
- An outage is always FG-DB-0001, also for a later statement (lost
  connection, 08xxx, 57P01–57P03, 53300, 28xxx); FG-DB-0002 means the
  statement failed on a working connection. Use `fetch_optional` when "no
  row" is a normal outcome (`RowNotFound` is `reason=row_not_found`).
- Server messages are logged only for SQLSTATE classes without row data;
  constraint errors log `constraint` / `table`, never the values.
- `Cargo.lock` lists `rsa`, `sqlx-mysql` and `sqlx-sqlite` (via the sqlx
  `macros` feature); `cargo tree -i rsa` / `-i libsqlite3-sys` show they are
  not built. An audit tool reading the lockfile will flag RUSTSEC-2023-0071 —
  waived with this reason in `.cargo/audit.toml` (user, 2026-10-26).
- `Db::connect` makes one direct probe connection (real failure reason,
  version check) and then a lazy pool: `min_connections` are opened in the
  background, so a failure there surfaces later on `acquire` (FG-DB-0001).

**Migrations** (ADR-0014): `migrate(&mut MigratorConn, &MIGRATOR, timeout)`,
`status(&db, &MIGRATOR)` (read pool, never creates `_sqlx_migrations`),
`check_schema(&db, &MIGRATOR)` (what `forge serve` runs before binding:
FG-DB-0004 for pending / db_ahead / checksum_mismatch / dirty). Product
migrations live in `migrations/` (`YYYYMMDDHHMMSS_desc.sql`, LF, forward
only); test migrators in `tests/fixtures/migrator/`. Migration tests each
create a fresh database `forge_t_<uuid>` and drop it at the end.
`forge db status --check` exits 1 with FG-DB-0004 when serve would refuse.
Runbook: `ops/runbooks/db-migration.md`.

**Changes** (ADR-0016 §6, step C3): `change::ChangeTx` (crate-private) runs a
change on the write pool — state writes via `conn()` / `within()`, then
`commit(ChangeRecord)` writes the audit row and up to 100 events
(`DomainEvent`, UUIDv7 ids) within 30 s, then commits (own 10 s bound; a
timeout there is FG-DB-0002 `commit_outcome_unknown`). A repeated
`dedup_key` rolls the whole change back (`Committed::Deduped`). Invalid
changes are FG-DB-0010 before anything is written (rules in
`change/validate.rs`; client-supplied values are cut, never refused);
a plain event committed without an audit row in its transaction is
FG-DB-0013 (deferred trigger `events_audit_present`, `WHEN caused_by_seq IS NULL`).
The typed commands (step D) are its only callers; their state writes go through
`ChangeTx::step` (one statement at a time, each inside the deadline).

**State tables** (step D1, ADR-0016 §1): a `tables.toml` entry with
`kind = "audited"` declares `audit_resource` (literal or `=column`),
`audit_id_column`, `unaudited_columns` and `txn_audited_columns`. The table
carries the deferred constraint trigger `forge.audited_change_present` with
those four arguments and `UPDATE OF` every other column, `ENABLE ALWAYS`: a
change commits only with a succeeded audit row for the same resource in its
transaction (txn-audited columns alone: a succeeded audit row of the same
resource type; a changed
id or `=column` value: audit rows for the old and the new resource), else
FG-DB-0013 `reason=audited_change` (`audited_change_config`: the trigger
does not match the ledger). The id column is an integer column.
`bootstrap::check` compares each table and trigger with the ledger
(`bootstrap/audited.rs`; FG-DB-0009 `guard=audited_trigger`, `table`,
`detail`). The root gate validate-migrations requires the trigger at the end
of the migration sequence. TRUNCATE bypasses row triggers: an audited table
must grant it to no role — the table goes into `LATER_TABLES` and its exact
ACL into `expectations()` in `src/bootstrap/schema.rs` in the migration's
commit; doctor reports an audited table missing there (FG-DB-0009
`guard=privileges`, `detail=audited_table_unlisted`).
The ledger refuses resource columns that are unaudited or txn-audited, and a
column that is both.

**Schema** (ADR-0016 §1, step C2): `forge.events` (outbox), `forge.audit_log`,
`forge.event_feed_state`, `forge.event_subscribers`,
`forge.subscriber_dead_letters`, `forge.jobs`, and (step D1, migration `20261028100100`)
`forge.users`, `forge.namespaces`, `forge.namespace_tombstones` (all `kind = "audited"`;
`forge_ro` cannot read email, employee_number or external_id; tombstone targets are
`ON DELETE SET NULL` foreign keys and unaudited), (step D2, `20261029100000`)
`forge.groups`, `forge.group_members` (audited, resource `group`; no DELETE on groups),
(step D3, `20261030100000`) `forge.tokens` (audited on the owner, resource `user` /
`user_id`; `last_used_at` unaudited; forge_app may INSERT and UPDATE only `revoked_at`,
`revoked_reason`, `last_used_at`, never DELETE; `forge_ro` cannot read `secret_sha256`),
(step D4 MR1, `20261031100000`) `forge.projects`, `forge.repositories`,
`forge.project_members`, `forge.protected_refs` (all audited on the project, resource
`project` / `id` or `project_id`; `updated_at` unaudited). A project's path is its
`namespaces` row (`kind = 'project'`, `owner_id = projects.id`; no `namespace_id`
column); `deleted_at` / `purge_after` both or neither, no DELETE yet (D5).
`repositories`: one per project (`project_id` UNIQUE), `kind` (ADR-0023: no longer a concept; the column stays because migrations are append-only, and `projects::create` always writes the constant `system`; nothing reads it) —
forge_app may UPDATE only `state` (provisioning | ready) and `ready_at` (set exactly
when ready; the guard trigger `repositories_state_forward` — XF001, FG-DB-0009
`rejected`, enabled ALWAYS, compared by doctor — allows only provisioning → ready and
never changes `ready_at` once set), no DELETE; `provision_token` (uuid, default `gen_random_uuid()`) is not
readable by `forge_ro`. `project_members`: PK (project_id, user_id), role CHECK, no
bots (trigger `project_members_no_bot`), DELETE allowed. `protected_refs`: CHECKs
`protected_refs_pattern_prefix` (`^refs/(heads|tags)/`) and `_length` (≤ 255 bytes) —
the rest of `RefPattern::parse` is not in the database — `push_min_role` developer |
maintainer | owner, unique index `protected_refs_project_pattern` on
(project_id, lower(pattern)) (`pattern` is `COLLATE "C"`: ASCII case folding);
tests `src/projects_schema_tests.rs`; (M3 R1a, `20261106100000`) `forge.merge_requests`
and `forge.merge_request_counters` (audited on the project, resource `project` /
`project_id`; `updated_at` unaudited; forge_app may DELETE for the purge): `iid` unique per
project, both branches `refs/heads/*` and different (CHECKs), `state` open | merged |
closed, `closed_at` / `closed_by` exactly while closed, `head_sha` NULL until R1b, the
partial unique index `merge_requests_one_open`, and the guard trigger
`merge_requests_state_forward` (`merge_requests_state_guard`, XF001, enabled ALWAYS,
compared by doctor; replaced by `20261106100100`): `merged` is final, `closed` is reopened
before it is merged, the project, iid, author and source branch never change, and every
change adds exactly one to `revision` (the events' dedup keys depend on it). `merge_request_versions` arrives with
R1b. The purge deletes both tables' rows (`project.purged` counts `merge_requests_removed`);
the DDL-status decision is
`forge_audit.ddl_audit_status()` (bootstrap, owned by forge_audit). The ledger `tables.toml` lists every table
(`guarded`, `kind`, `ro_columns`); rules for new migrations are in
`migrations/README.md`. events / audit_log are append-only (per-table guard
triggers, XF001 → FG-DB-0009): only forge_seq numbers rows, only forge_purge
deletes (published rows past 7 / 365 days; never db.migrate rows) or
redacts; event_feed_state only moves forward; forge_app cannot write the
reserved audit actions (db.migrate, audit.*, feed.*, events.*). `bootstrap::check` also verifies their triggers, the
exact privileges of every migrated table and unacknowledged unexpected DDL.
Schema tests (`src/schema_tests.rs`) migrate a fresh database each.

**Sequencer** (ADR-0016 §7, ADR-0018 §3, step C4): `forge.assign_seq(max_batch,
include_audit)` (migration `20261027110000`, owned by forge_seq, SECURITY
DEFINER, EXECUTE for forge_app only) numbers committed events gap-free in id
order — and audit rows only when `include_audit` AND
`audit_sequencing_enabled` — under an advisory lock, creating the
`event_feed_state` row on its first call. `sequencer::run_once(&db,
BatchSize, AuditSequencing)` calls it on the write pool and, in the same
transaction, `NOTIFY forge_events_sequenced` (payload: the last event seq)
when it numbered events. `sequencer::backlog` / `Backlog::check_lag` feed
`forge doctor` (`outbox.unsequenced_count`, `outbox.sequencer_lag`,
FG-DB-0005 when rows wait and the heartbeat is older than 30 s).
`bootstrap::check` verifies the function exactly (`bootstrap/definer.rs`).

**Audit seal** (ADR-0018 §4, step C8): `forge.audit_seal` (migration
`20261027130000`; its guard lets only `shipped_at` go NULL → value once,
forces `sealed_at`, requires each insert to continue the chain and end at or
below `audit_high_seq`; no DELETE / TRUNCATE) holds one row per sealed range.
`seal::v1` is the single definition of hash version 1 (columns, SELECT,
RFC 8785 row bytes; details numbers other than safe integers as
`{"$n":"<text>"}`, keys starting with `$` get one more `$` — injective;
golden vector `tests/fixtures/audit_seal_v1_golden.py`, run with
`py -X utf8 -I`). `seal_sha256` covers `kind` and `key_id` (`seal::chain`).
`seal::seal_due(&db, &SealKeys, SealPolicy::WORKER)` is the body of the
`audit_seal` task of `forge worker` (a minute after the last seal, or when
more than 1000 rows wait; `seal_pending` seals without the policy — the
drain after `more`): try-lock `(1718579815, 8)` (`Busy` for the loser),
≤ 5000 rows per seal, hashing on the blocking pool, cancel-safe and
idempotent (a commit timeout's outcome is unknown; calling again is safe).
Errors: gap → FG-DB-0011 reason=gap; the feed-state row gone while rows
are numbered, or `audit_high_seq` below the last seal → reason=chain
(`feed_state_missing` / `audit_high_below_seal`); a row v1 cannot read
(details nested past 128) → FG-SYS-0001 `operation=seal.v1`.
`seal::verify_range` recomputes seals on the write pool (the sealed act-as
columns are not readable by forge_ro), each in its own SELECT-only
transaction that always rolls back under `search_path = pg_catalog,
pg_temp` — **not** READ ONLY / REPEATABLE READ: the root gate
db-write-boundary keeps `SET TRANSACTION` inside ChangeTx (limitation; C9's
full reverify may need the gate to allow it). Lists are capped at
`MAX_VERIFY_SEALS` (`Verified::truncated`). `forge doctor
audit.seal_verify` checks the last 24 h by `sealed_at`, which is not
authenticated (the full walk is C9's reverify); `warn` when seals exist
but no keys are configured. `seal::backlog` feeds `audit.seal_backlog`
(FG-DB-0015 past 5 minutes). The HMAC keys come from
`audit.seal_keys_file` (`forge_core::config::SealKeys`; not zeroized on
drop — deferred, new crate).

**Job queue** (ADR-0016 §7, step C6; `src/jobs/`): `forge.jobs` with fenced
leases, for internal work only — never one job per edge device (plan 01
§3.10: fleet-wide work is one job that walks its targets). `jobs::enqueue`
(write pool) or `ChangeTx::enqueue_job` (inside a change, rolled back with
it); a `dedup_key` is held per kind by queued, running and succeeded jobs —
a dead job frees it, so its work can be enqueued again (partial index
`jobs_dedup`, migration `20261027120000`). `jobs::claim` is ONE autocommit
`FOR UPDATE SKIP LOCKED` statement taking the oldest ready job (`running`,
`attempts + 1`, `worker_id`, `lease_until = now() + timeout + 30 s`) of the
given kinds only. `complete` / `fail` / `release` are fenced by `(id,
running, worker_id, attempts)` — a lost lease returns `Fenced(FG-DB-0007
reason=fenced)` and changes nothing. `fail` requeues after 10 s ×
2^(attempt − 1) (≤ 1 h) or deads (attempts used up, `Retry::Never`);
`release` hands a job back at shutdown (ready now, attempt not counted).
`jobs::reap` is ONE statement for up to 1000 expired leases (dead with
`last_error_code = FG-DB-0007` when the attempts are used up, else requeued
with the backoff). `JobRunner` (`JobHandler` per kind, one job at a time —
a head-of-line limit to revisit before C8 / retention jobs; handler in its
own task under `timeout_ms`: `execution_timeout` → FG-DB-0007 and retry,
a panic → FG-SYS-0001 and retry; after an abort it waits up to 5 s for the
handler to end before settling, else `abort_pending`; `run_once_until`
gives a stopped job a grace, then releases it — `released_on_shutdown`; a
dropped run aborts the task, the lease expires, reap requeues it) logs the
handler's failure, dead jobs (FG-DB-0006) and lost leases (FG-DB-0007) with
the job's origin request id. Delivery is at-least-once: handler side
effects must be idempotent or fenced by `lease.attempt`. `jobs::health`
feeds `forge doctor` (`jobs.dead` → FG-DB-0006; `jobs.expired_leases`,
`jobs.queue_depth` → FG-DB-0017 JobQueueBacklog, counted per kind via
`jobs_ready_kind`). `forge worker` runs the runner (1 s idle poll, 10 s
shutdown grace) and the reaper (30 s, also at startup).
**Subscribers** (ADR-0016 §6, §7, step C5): `subscribers::Subscriber` (name,
`Ordering::Ordered` / `Independent`, `handle(&SourceEvent, &mut DerivedTx)`),
`Registry`, `register(&db, sub, Start::Head)`, `run_once(&db, sub)` — one event
(`last_seq + 1`) per write-pool transaction: `SELECT … FOR UPDATE NOWAIT` (55P03 =
`Progress::Busy`, debug log only), the handler inside a savepoint (20 s), then
`last_seq` / `last_txid` advance monotonically. Five failures in a row stop an
ordered subscriber (FG-DB-0008 `stuck`, retried, never skipped) or quarantine
the event of an independent one (`subscriber_dead_letters`, `quarantined`);
the derivation limits (depth 8, 100 per root) are `fanout_limit` at once.
Counted as failures too: an undecodable event (FG-SYS-0001 `decode`), a
failed savepoint RELEASE, and derived writes refused at COMMIT (FG-DB-0013
`derived_*`, counted in a fresh transaction); the stored code is the
handler's own (`src/subscribers/count.rs`). Not counted (step errors,
retried with backoff): DB failures, `subscriber_not_registered`,
`ordering_mismatch`, `event_missing`, `cursor_moved` (`cursor.rs`).
`DerivedTx` writes derived tables (`conn()` — a trust boundary, see the
`Subscriber` docs) and derived events (`emit`: actor
`system`, the source request id, lineage set; no audit row). Migration
`20261027140000` checks derived events (`events_lineage` BEFORE INSERT,
`events_derived_progress` deferred; XF002 → FG-DB-0013; an unknown
CONSTRAINT name is `xf002_unknown`). `Dispatcher` runs
every registered subscriber for `forge worker` (a round starts no step
after `ROUND_BUDGET` = 30 s, then `more`) and waits on the crate-private
`Wake` (LISTEN
`forge_events_sequenced` on the dedicated connection `Db::listen_options` — no
statement / idle timeout, server-side TCP keepalive, a bounded `SELECT 1` at
every 5 s poll). `states` / `check_lag` / `dead_letters` feed `forge doctor`
(`subscribers.lag`, `subscribers.dead_letters`).

**Service accounts (M3 S0, migration `20261113100000`, ADR-0019 Addendum S0)**:
`service_accounts::create` / `update` (one `service_account.created` /
`.updated` audit row on the account; the owner, an active person, is locked
`FOR SHARE`; FG-AUTH-0006 `purpose_*` / `owner_*`), `target` / `get` / `list`
(read pool) and `health` (doctor: active tokens all expiring within 14 days →
FG-AUTH-0019; no active human owner → FG-AUTH-0010 `ownerless_service_account`).
`tokens::issue_for_service_account` records `tokens.issued_by_user_id` (and
`issued_by_user_id` in `token.created`); a person is refused
(`owner_not_service_account`). The schema backstops: the
`service_account_profiles_kind_guard` trigger (`service_account_profiles_kind`,
`service_account_profiles_owner_human`), `users_service_account_no_personal_data`,
`users_service_account_not_admin`, `user_identities_human_guard` (constraint `user_identities_human_only`), `users_kind_immutable`; `users::create`
and `identities::link_by_operator` refuse first (FG-AUTH-0006, FG-AUTH-0014
`not_human`). Tests `src/service_accounts/tests.rs`.


**Integrations (M3 I1a, migration `20261114100000`, ADR-0016 Amendment I1a)**:
`integrations::create` / `update` (one `integration.created` / `.updated`
audit row on (`integration`, id); the service account is checked under its row
lock: FG-WEB-0006 `not_service_account` / `service_account_in_use`,
`unknown_event_type`), `bind_decided` / `unbind_decided` (called by
forge-hosting only, with `Authorized::reconfirm`: the project locked, the
caller's facts re-read, then the integration row `FOR UPDATE`; at most one
binding per project → FG-INT-0016, 200 per integration → FG-INT-0012, pending
deletion → FG-REPO-0006, a disabled integration or service account →
FG-INT-0017; binding makes the service account a Reporter when it
is no member and unbinding removes that membership while it is still Reporter,
with the `member.added` / `.removed` audit row and event in the same change),
`get` / `list` / `bindings` / `project_bindings` (read pool). The purge deletes
a project's binding; the member removals and role changes clear `granted_membership`
(`integrations::clear_grant`). Schema backstops: `integrations_service_account_guard`
(`integrations_service_account_kind`, `integrations_service_account_fixed`),
`integrations_kind_known`, `integrations_event_types_count`,
`integrations_event_types_entries` (20261114100100),
`integrations_one_per_service_account`, `integration_bindings_one_per_project`,
`integration_bindings_external_project_id_format`. Tests
`src/integrations/tests.rs`.

**Work items (M3 I1b, migration `20261116100000`, ADR-0016 Amendment I1b)**:
`forge.work_items` (one per integration, external project and `PBI-<n>`),
`forge.merge_request_work_items` (link intervals, `auto` / `manual`, ended
once and never deleted but by the purge) and `forge.event_work_items` (the work
items of each event), all `derived`. The ordered subscriber
`work_items::WorkItemSync` (`work-item-sync`, registered by `forge worker`)
links each merge request to the references in its current title and source
branch (`forge_core::find_refs`, at most `MAX_AUTO_LINKS` = 20; derived
`work_item.linked` / `.unlinked`), never ends a person's link nor relinks what a
person unlinked, and records every merge request event, push and link event
with its work items (at most 100); a failure stops it (FG-DB-0008 `stuck`).
A person's link: `review::link_work_item_mr_decided` /
`unlink_work_item_mr_decided` (forge-review only; FG-INT-0018 for an unbound
project, FG-INT-0019 past 50 open manual links, FG-REPO-0005
`resource=work_item_link` for nothing to unlink), `work_items::links` (read
pool). Schema backstops: `work_items_ref_format`, `work_items_identity`,
`work_items_external_project_id_format`, `merge_request_work_items_source`,
`merge_request_work_items_source_fields`, `merge_request_work_items_linked_seq`,
`merge_request_work_items_unlink_source`,
`merge_request_work_items_unlink_source_fields`,
`merge_request_work_items_one_active`, `event_work_items_seq_positive`, the
guard `merge_request_work_items_guard`. I1b review (20261116100100,
20261116100200): the deferred trigger `merge_request_work_items_audited`
(constraints `merge_request_work_items_audited` / `_derived`, FG-DB-0013)
needs a person's link and its end to carry their `work_item.*` audit row
(`converted` / `unbound` ends: `work_item.linked` / `integration.unbound`) and a
detected one the subscriber's progress; DELETE only while the repository is
`purging`; the composite FK `merge_request_work_items_merge_request_project`;
`linked_by_kind` / `unlinked_by_kind` (only a `human`'s manual unlink stops
detection); unbinding ends the binding's open links (`unbound`). Tests
`src/work_items/tests.rs`.

**Webhooks (M3 I2, migration `20261120200000`, ADR-0016 Amendment I2)**:
`webhooks::{create_webhook,update_webhook,delete_webhook,rotate_webhook_secret,redeliver_webhook}_decided` (decided in
the transaction: project `FOR SHARE`, the caller's roles and account, then the
per-project advisory lock or the webhook row `FOR UPDATE`; one `webhook.*`
audit row on the project; 20 per project, `FG-INT-0012 reason=webhook_limit`),
the subscriber's `enabled_on` / `material` / `record_delivery` (one
`webhook_deliveries` row per (webhook, event) and its first `webhook.deliver`
job), the job's `delivery_task` / `record_attempt` (one transaction: the
delivery's state, the webhook's failure count, the next attempt through
`jobs::enqueue_after_on`, and at 50 failures in a row or 24 h of failures the
system's `webhook.disabled` audit row and event), the reads `list` / `get` /
`deliveries` / `health` (the read pool cannot read the sealed secrets) and the
retention `purge_deliveries`. The project purge deletes a project's webhooks
and deliveries.

**The integration feed (M3 I1c, migration `20261118100000`, ADR-0016 Amendment
I1c)**: `feed::open` starts a `REPEATABLE READ READ ONLY` snapshot on the read
pool (`FeedSnapshot`): `state` (epoch, `high_seq`, `retained_from`, the
`work-item-sync` position, `retention_policy.events_days`; the read's bound is
`min(high_seq, work-item-sync)`), `material` (the integration, its bindings with
`generation` and the service account's access facts per project through
`resolve::PROJECT_ACCESS_SQL` — forge-auth's `prove_visible` decides),
`read_feed_page(&VisibleSet, after, bound, limit, max_bytes)` (only the set's
project ids and event types reach SQL; 100 rows per statement, ≤ 500 events and
≈ 2 MiB; each event with this integration's work items and its actor; called by
forge-integ only) and `oldest_unread_at`. `personal_facts` reads e-mail and
employee numbers on the write pool (the read pool cannot). `record_read` writes
`forge.integration_feed_reads` (derived) at most once a minute per integration;
`lag` (doctor `integrations.feed_lag`, FG-INT-0009), `watermark`
(`subscribers.work-item-sync`), `retention` (`events.retention`).
`run_retention` runs one batch of `forge.events_retention` (forge_purge,
EXECUTE forge_app; `forge.retention_policy`, default 90 days, forge_app cannot
write it); `Db::connect_maint` + `new_epoch` run `forge.feed_new_epoch`
(forge_seq, EXECUTE forge_maint only; `feed.epoch_rotated` audit row;
`epoch_changed` is FG-DB-0014). `integration_bindings.generation` (identity,
never chosen nor updated by forge_app) makes a rebind a new visible set.
`tokens_scopes_known` gains `user_employee_number`. Tests `src/feed/tests.rs`
(the visible set decided with forge-auth, a dev-dependency).

## notify (M3 N1)

`forge_db::notify` (ADR-0016 Amendment N1, migration `20261120100000`): a person's settings (`forge.notification_preferences`: `preferences` / `set_preferences`, no row = the defaults) and the delivery records (`forge.notifications`, derived, one row per source event and recipient). `enqueue` (called by the `notify` subscriber with its `DerivedTx`) writes the row and, for `Initial::Queue`, the `notification.send` job (dedup key `notification:<id>`); the send job walks the state machine: `begin_send` (row lock, `pending|retrying` → `sending`, run number recorded; a row still `sending` means the previous run died: `unknown` / `interrupted`, never sent again), `finish` (fenced by the run number: `sent`, `retrying`, `failed`, `unknown`, `skipped`, `suppressed`, each with a short reason code). `recipient` and `backlog` feed the mail and `forge doctor` (`notify.queue`). No foreign key to projects or events (the purge and the retention delete them).
