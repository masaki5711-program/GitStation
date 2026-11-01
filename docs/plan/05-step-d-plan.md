# 手順 D の計画（承認済み）

> 2026-10-28、Git3 のセッションで planner が作成し、ユーザーが承認した（Git3 からの報告「計画は承認済み」）。
> 原文は英語のまま残す。実装は D1 から、テストを先に書いて始める。D1 の前提となるゲートの規則（守る関数を呼ぶトリガー、audited の表のトリガー）は root 282eea8 で入った。

# Step D plan (planner output, 2026-10-28) — approved by the user 2026-10-28

Worktree: D:\app\git\.claude\worktrees\feat-db-domain-tables (product branch feat/db-domain-tables, base 1fcb9ac).
Recommended option A: shared vocabulary (Role, Visibility, UserKind, RoleSet, PathSegment, EventType, AuditDetails) in forge-core,
typed commands + role loader in forge-db, pure role-basis seam in forge-auth; one generic audit-presence trigger; five phases.

## Premise findings (verified against the worktree)
- ChangeTx has no production caller (change/mod.rs:11-14), writes one audit row per change.
- Only `events` has a deferred audit trigger; it checks txid only, not resource or outcome (migrations/20261027100400, 100500).
- tables.toml has only log/derived tables; doctor schema expectations hard-coded in bootstrap/schema.rs.
- event_type is &'static str, payload/details serde_json::Value — ADR-0016 §5/§9 DomainEvent/AuditDetails enums missing.
- Principal has `user: Option<UserId>`, no subject kind (forge-auth/src/model.rs).
- SQL outside forge-db forbidden (validate-db-write-boundary.js rule 5) → loader in forge-db.
- ids: only RepoId/UserId/GroupId/ProjectId. 23 Actions. No rand/subtle crates.
- Next free codes on branch: AUTH-0005, REPO-0007. `forge admin` is read-only by design.
- validate-migrations.js treats only triggers on ledger-guarded tables as guarded (721-724) → root gate change needed before D1.

## Phases
D1 audit-presence infra + users/namespaces/tombstones
  - 20261028100000_audited_change_present.sql: generic plpgsql fn, TG_ARGV (resource type literal or =column, id col,
    unaudited cols, txn-audited cols); requires audit_log row same txid + (resource_type, resource_id) + outcome='succeeded';
    compares to_jsonb(NEW) vs OLD minus unaudited cols (new columns audited by default); raises XF002 (table + resource type only).
  - 20261028100100_users_and_namespaces.sql: users (kind human/service_account/bot, lower(email) unique for humans,
    employee_number partial unique, state, is_admin, auth_source, external_id); namespaces (ADR-0015 schema B, full_path COLLATE "C",
    depth<=20, len<=255); namespace_tombstones (cause_type/cause_id, insert/delete only). Deferred triggers registered in guarded_objects.
    forge_ro: no email/employee_number/external_id.
  - tables.toml: kind="audited", audit_resource, unaudited_columns, txn_audited_columns. Doctor expectations data-driven from ledger (toml dep).
  - commands: users::create (+ personal namespace), users::disable (no last-owner yet).
  - forge-core: UserKind, PathSegment::parse, RESERVED_TOP_LEVEL (+edge, fleet, devices), NamespaceId, TombstoneId, AuditDetails.
  - codes: REPO-0007 NamespacePathInvalid, 0008 NamespacePathReserved, 0009 NamespacePathTaken, AUTH-0005 UserEmailTaken; FG-DB-0013 prose update.
  - tests: a_state_change_without_its_audit_row_cannot_commit, an_audit_row_for_another_resource_does_not_cover_a_change,
    a_denied_audit_row_does_not_cover_a_change, a_new_column_is_audited_by_default, declared_unaudited_columns_need_no_audit_row,
    a_path_rewrite_needs_an_audit_row_in_the_transaction, every_audited_table_has_its_audit_trigger,
    a_weakened_state_table_audit_trigger_is_reported, user_create_writes_user_namespace_and_one_audit_row,
    human_email_is_unique_ignoring_case, a_device_is_not_a_user_kind, a_reserved_or_taken_or_tombstoned_username_is_rejected,
    path_segment_rules_match_adr_0015 (proptest), the_db_check_rejects_what_the_parser_rejects,
    the_read_pool_cannot_read_user_email, audit_details_have_no_pii_fields.
D2 groups, group_members, load_*_roles -> RoleSet{native,effective}, Role/Visibility -> forge-core, Action::role_basis()
   (Effective only for RepoRead/RepoPush/ProjectRead/GroupRead). REPO-0010 MembershipConflict. Conflicts with W1 (forge-auth model.rs).
D3 tokens (after D1): fgp_<public_id>_<secret>, 256-bit OS RNG, SHA-256, subtle CT eq, expires_at NOT NULL, last_used_at unaudited +
   throttled; EventType enum; dedup_key shape rule; AUTH-0008 TokenRequestInvalid (0006/0007 taken by D1 user validation); CLI bootstrap-admin/token issue if Q2=CLI.
D4 projects, repositories (kind fixed), project_members, protected_refs; provisioning -> init_bare -> mark_ready;
   REPO-0012 RepositoryProvisioningStuck (alert), REPO-0013 ProtectedRefRuleInvalid; doctor checks (conflict with C6 DB_CHECKS).
D5 transfer/rename/delete/restore/purge/tombstone release; REPO-0014 TransferInvalid (was 0013: D2 took REPO-0011 GroupRequestInvalid, so D4 uses 0012/0013).

## Open questions: Q2 CLI tokens (rec.), Q3 kind immutable (rec.), Q4 user/group audit-only (rec.), Q5 defer to M4E ADR (rec.),
Q6 email required+unique for humans, employee_number partial unique (rec. yes), Q7 project.*/repository.created class=integration
+ protected_ref.* in §10 (ADR-0016 amendment), Q8 group member.* audit-only, Q9 tokens max 365d/default 90/50 active/fgp_,
Q10 deletion grace 7d, Q11 no group deletion in M1, Q12 AuthorizedChange proof -> decide in W1, Q13 ro pool sees username/display_name only.

## Answers / approval
2026-10-28: user approved the plan with every recommendation ("推奨通りで"):
Q2 CLI-issued tokens (no login API / no password_hash in M1); Q3 repository kind immutable in M1; Q4 user.*/group.* audit-only;
Q5 device actor recording deferred to the M4E ADR; Q6 human email required + unique on lower(email), employee_number partial unique;
Q7 project.* / repository.created = integration class + protected_ref.* added to ADR-0016 §10 (amendment); Q8 group member.* audit-only;
Q9 token max 365d / default 90d / 50 active per user / prefix fgp_; Q10 deletion grace 7d (configurable, min 1d);
Q11 no group deletion in M1; Q12 decided in W1; Q13 forge_ro sees users.username/display_name only.

Next: (1) main session lands the root-gate change in validate-migrations.js (triggers calling a guarded function are guarded;
every kind="audited" table has an audited_change_present trigger) BEFORE D1; (2) a session started inside the worktree implements
D1 test-first, stops for review. Commit as feat(db): … .

## D2 plan (planner, 2026-10-29) — approved by the user 2026-10-29

Premise: "W1" = the forge-auth rework (ADR-0015 step 3 `prove()` / `Authorized<R>` + ADR-0017 §4.0 `native_role()` / `role_for()` /
`kanflow_grant`); the user confirmed. The D2 line above ("RoleSet{native,effective}, Action::role_basis(), Effective only for
RepoRead/RepoPush/ProjectRead/GroupRead") contradicts ADR-0017 §4.0/§4.4, so **`role_basis` is struck from D2** and moves to W1 with Q12.

Option B (approved): data + loader only; authorize() semantics unchanged. Two MRs.
- MR1 phase 1: move `Role`, `Visibility` to forge-core; forge-auth re-exports (matrix_rows.rs untouched). Tests `role_order_is_guest_to_owner`,
  `role_str_round_trips`; gen-authz --check unchanged.
- MR1 phase 2: migration `20261029100000_groups_and_members.sql` — `groups` (id, display_name, description, timestamps; no DELETE grant, Q11),
  `group_members` (PK (group_id, user_id), FKs, role CHECK, `updated_at` unaudited; DELETE allowed), audited triggers ('group','id') /
  ('group','group_id'), guarded_objects, tables.toml kind="audited". Commands `groups::create` (lock_path + tombstone check, group namespace
  only under a group parent or top level, creator = Owner, one `group.created` audit row) and `groups::{add_member, change_member,
  remove_member}` (audit-only, Q8; group row FOR UPDATE; last-owner re-check under the lock). AuditDetails GroupCreated, MemberAdded,
  MemberRoleChanged, MemberRemoved (ids only). FG-REPO-0010 MembershipConflict 409 (already_member / not_member / last_owner);
  FG-REPO-0007 prose gains too_deep / path_too_long.
- MR2 phase 3: `load_group_roles` / `load_namespace_roles` -> `MemberRoles{native, direct}` (native = max over the namespace's ancestor
  groups via a depth-capped recursive CTE; active users only; projects in D4). Cross-crate test `ancestor_owner_may_manage_subgroup_members`
  / `ancestor_reporter_may_not`. ADR amendments: ADR-0010 (loader is the one role computation), ADR-0015 Amendment D2 (group namespaces
  only under groups), ADR-0017 note (MemberRoles seeds native_role()).

Decisions (user, 2026-10-29): last owner = a top-level group needs >= 1 direct active Owner, a subgroup may rely on an inherited Owner;
bots cannot be group members (humans and service accounts only); `users::disable` leaving a group ownerless is deferred to D3 together with
"guard disabling the last active admin".

## D3 plan (planner, 2026-10-29) — approved by the user 2026-10-29

Premise findings: `forge admin` gains its first mutating commands (token issue/revoke/list, user disable, bootstrap-admin) — an ADR
decision, runbooks mark them as human-approved steps. CLI issuance is outside `authorize()` (host operator = trust root); `tokens::issue`
enforces the invariants itself (admin scope only for admins, no bots, no disabled owners). Token hashes are read on the write pool only
(ADR-0018: `forge_ro` never sees `secret_sha256`). `Scope` moves to forge-core (forge-db cannot see forge-auth). forge-web stays
independent of forge-db: an `Authenticator` trait injected by `forge-cli serve` (readyz pattern). No Smart HTTP yet → real-git e2e
tests are named only and handed to phase 9. forge-auth README hand-offs 7 (last owner on disable) and 9 (Basic password = token only)
close here.

User direction (2026-10-30, correcting a misreading of 2026-10-29): the core purpose is tight kanflow integration to speed up
vibe coding — integration is mandatory; "keep it simple" means simple **configuration**, not fewer features or events. Hence for D3:
- `token.created` / `token.revoked` are domain events (ADR-0016 §10, admin class); bulk revoke on disable emits one per token (≤ 50).
- MR1 adds `EventType` enum (ALL, as_str, visibility_class; `DomainEvent.event_type: EventType`) and `DedupKey` (built from
  server-side ids/hashes only; `^[a-z][a-z_]*(:[0-9a-z_-]{1,64}){1,4}$`, ≤ 200 chars; violation FG-DB-0010 `invalid_dedup_key`),
  closing ADR-0016 §11. Tests every_event_type_passes_the_db_check, token_events_are_admin_class,
  a_dedup_key_from_a_raw_client_value_cannot_be_built, a_malformed_dedup_key_is_change_rejected.
- Token audit rows are attached to the owner: tables.toml `audit_resource = "user"`, `audit_id_column = "user_id"`.
- `tokens.is_agent boolean NOT NULL DEFAULT false` from the start (CLI `--agent`; ADR-0017 §4.0 consumes it in W1).
- Security measures unchanged.

Shape (option A): token primitives in forge-core (`fgp_<16 hex public_id>_<64 hex secret>`, `getrandom` 256-bit, SHA-256, `subtle`
CT compare, `TokenSecret` not Serialize/Display); `forge_db::tokens::{issue, revoke, list, authenticate}`; forge-web middleware.
Unknown public_id still runs a dummy CT compare. DB outage during authentication = FG-DB-0001 (503), never 401. Expired / revoked =
FG-AUTH-0001 (`reason` in logs only); disabled owner = inactive → FG-AUTH-0004. `last_used_at` updated at most every 10 min
(`WHERE last_used_at < now() - 10 min`), unaudited; failure = FG-DB-0002 warn once, authentication proceeds.

Disable guard (user decisions 2026-10-29): refuse by default with FG-AUTH-0009 AdministrationConflict (409, `last_admin` /
`last_owner` / `admin_exists`), only CLI `forge admin user disable --force` passes (audit `forced`, `ownerless_groups`,
`left_without_admin`); doctor reports FG-AUTH-0010 AdministrationOrphaned (warn, runbook `user-offboarding`); recovery via
`bootstrap-admin`. "Last admin" counts **active human admins only**. Top-level groups need a direct active Owner; subgroups may inherit.
Locking: xact advisory lock `users.admin_set` (disable, bootstrap-admin, future is_admin changes) → target user row FOR UPDATE →
unlocked counts. Never lock group rows (member changes lock group → owner rows FOR SHARE; reverse order would deadlock).
Disable revokes every active token in the same ChangeTx (`revoked_reason='user_disabled'`, one `user.disabled` audit row with
`revoked_tokens`); a refused disable revokes nothing. `tokens::issue` locks the owner row FOR UPDATE, so issue-vs-disable serializes.

MRs (in order):
- MR1 `feat/auth-personal-access-tokens`: forge-core `scope.rs` (moved, forge-auth re-exports) + `token.rs`; migration
  `tokens` (id, user_id FK, public_id UNIQUE CHECK hex16, secret_sha256 bytea 32, name 1–100, scopes text[] ≥1 known, created_at,
  expires_at NOT NULL > created_at and ≤ 365 d, revoked_at, revoked_reason CHECK revoked|user_disabled, last_used_at; partial index
  (user_id) WHERE revoked_at IS NULL; forge_app SELECT/INSERT + UPDATE(revoked_at, revoked_reason, last_used_at) only, no DELETE;
  forge_ro no secret_sha256; audited trigger ('user', user_id), unaudited last_used_at; guarded_objects, schema expectations);
  `AuditDetails::{TokenCreated, TokenRevoked}`; CLI `forge admin token issue --user --scopes [--expires-in 90d] [--name]`
  (token printed once on stdout, never logged), `token revoke`, `token list`. Codes: FG-AUTH-0008 TokenRequestInvalid (400; reasons
  expires_out_of_range, scopes_empty, scope_unknown, name_length, nul_character, admin_scope_requires_admin, owner_bot,
  owner_disabled, active_limit), FG-DB-0019 CredentialDataInvalid (500, alert, runbook `credential-data`). Log event
  `forge.auth.token_rejected`. Matrix unchanged.
- MR2 `feat/auth-disable-guard`: disable guard + bulk revoke + `bootstrap-admin` + doctor `users.administration`; FG-AUTH-0009/0010;
  runbook `user-offboarding.md`; D2 LOW fix (`groups/members.rs` unknown_role → FG-DB-0018).
- MR3 `feat/web-token-authentication`: forge-web `auth.rs` (Bearer, or Basic whose password is a token; username ignored and not
  logged; two auth headers → 401 `ambiguous`), `AppState.authenticator`, serve wiring; `forge-web/tests/token_principal.rs`.
  Deferred: denied-request audit aggregation (ADR-0016 §6), auth rate limiting. Phase 9 e2e names:
  git_clone_with_a_token_in_basic_auth_succeeds, git_push_with_a_read_repository_token_is_refused,
  git_without_credentials_gets_www_authenticate_basic.

ADR: new ADR-0019 (personal access tokens and authentication; architect). Amendments: ADR-0016 (token.* audit-only in M1; dedup_key
open question moves to D4), ADR-0010 (hand-offs 7 and 9), ADR-0018 (token hashes excluded from forge_ro).

## D4 plan (planner, 2026-10-30, cloud session) — approved by the user 2026-10-30 ("推奨どおりで承認")

Premise: forge-git `StorageRoot::init_bare` (atomic, refuses an existing path with REPO-0004); job queue and `ChangeTx::enqueue_job`
exist but no production handler; `EventType::RepositoryCreated` has no writer; `event_scope_mismatch` only applies when the audit row
names a project; `namespaces.kind` allows `project`; forge-auth already has `ProjectView`, `RefProtection::effective` and the matrix
rows — **the matrix does not change in D4** (no routes). Missing: the four tables, a ref pattern type, repository kind/state types, a
project role loader, a provisioning handler, a safe `init_bare` retry. Conflicts settled: `project.visibility_changed` and
`protected_ref.*` stay **maintainer** class (ADR-0016 §5 wins over Q7; D4 writes only `project.created` as integration); project
members need their own `AuditDetails::ProjectMember*` variants (same action strings as groups). Event scope rule (closes the ADR-0016
D3-amendment question): a project event needs an audit row with the same `project_id`; an audit row without a project publishes only
instance-wide events; dedup keys `<aggregate>_<verb>:<ids>`, no forced project prefix. `projects.deleted_at` / `purge_after` created
now without a writer (D5).

Option A (approved): provisioning by a worker job. tx1 = project + namespace + repository (`provisioning`) + creator as direct Owner +
`project.created` + job `repository.provision` (dedup `repository_provision:<repo_id>`); the forge-cli worker handler runs
`init_bare_or_adopt` then `mark_ready` (tx2: `repository.created`, dedup `repository_created:<repo_id>`). Idempotent retry:
`init_bare` writes `forge.provisionToken=<repositories.provision_token>` into the repo config before the rename; a retry adopts an
existing path only when the token matches, else REPO-0004.

MRs, order MR1 → {MR2 ∥ MR3a} → {MR3b ∥ MR4 ∥ MR5} (parallel MRs only append to shared registries; only MR1 edits migrations /
tables.toml / bootstrap schema; only MR3b edits doctor `DB_CHECKS`):
- MR1 `feat/db-projects-schema`: migration `projects` (namespace_id UNIQUE, display_name, description, visibility, timestamps,
  deleted_at/purge_after both-or-neither), `repositories` (project_id UNIQUE = 1:1 in M1, kind spec|system immutable — forge_app
  UPDATE(state, ready_at) only, state provisioning|ready ⇔ ready_at, provision_token), `project_members`, `protected_refs` (pattern
  `^refs/(heads|tags)/`, ≤ 255, push_min_role developer|maintainer|owner, UNIQUE (project_id, lower(pattern))); all audited as
  ('project', id|project_id); forge_ro without provision_token. forge-core `RepositoryKind`, `RepositoryState`, `RefPattern`
  (case-insensitive, at most one `*`, last component only), `ProtectedRefId`. FG-REPO-0013 ProtectedRefRuleInvalid.
- MR2 `feat/db-project-create`: `projects::create` (parent = user or group namespace, never under a project; creator = direct Owner;
  no bots; kind default system; visibility default private), `roles::load_project_roles` (direct ∨ ancestor groups ∨ personal-namespace
  owner = Owner), `projects::access`; `EventType::ProjectCreated` (integration); scope rule in `change/validate.rs`; FG-REPO-0014
  ProjectRequestInvalid. ADR amendments 0015 / 0010 / 0016.
- MR3a `feat/git-init-bare-adopt` (forge-git storage only): provision token marker and adopt-on-retry.
- MR3b `feat/repo-provisioning`: `repositories::{mark_ready, provisioning_report, ready_ids}`, worker handler `repository.provision`
  (60 s), doctor `repositories.provisioning` (> 10 min or dead job) and `repositories.storage` (ready without directory → REPO-0002,
  capped 10 000), `forge admin repo reprovision <repo_id>`, FG-REPO-0012 RepositoryProvisioningStuck (alert), runbook
  `repository-provisioning.md`.
- MR4 `feat/db-project-members`: add / change / remove (project row lock, then owner rows FOR SHARE); `member.*` events maintainer
  class; no last-owner refusal while an inherited Owner exists.
- MR5 `feat/db-protected-refs`: create / change / delete, `levels_for(db, ProjectId, &RefName)` → `RefProtection::effective`;
  `protected_ref.*` events maintainer class; ≤ 100 rules per project; ADR-0008 / ADR-0016 §10 notes.

Answers (user, 2026-10-30, all as recommended): (1) projects only under a personal namespace or a group, never under a project;
(2) creator = direct Owner, personal-namespace owner and ancestor group Owners inherit, no last-owner refusal while an inherited Owner
exists; (3) project `member.*` are events (maintainer class); (4) option A, alert after 10 min; (5) one repository per project in M1,
kind default `system`, visibility default `private`; (6) ProjectRequestInvalid = FG-REPO-0014, D5 TransferInvalid moves to FG-REPO-0015.

## M1 phase 8 plan (planner, 2026-10-30, cloud session) — approved by the user 2026-10-30 ("推奨で進めてください")

Option A: 8a = ADR-0015 step 2 (path parsing in forge-core: LookupPath, TailSegments, RouteWord, ParsedPath, parse_web_path,
NamespaceResolver + access-loader traits, url-vectors.json) + step 3 (prove()/Authorized<R>, PathResolve and WebShell actions,
Unresolved/Resolved resources, the 7-step dispatcher on the raw path, scoped() routes, Verb::Head, manifest scope/word/query_key,
namespace_tombstone_routes.rs) + `GET /api/v1/namespaces/resolve` + `GET|HEAD /api/v1/projects/{id}`; phase 9 starts after MR4.
8b (alongside phase 9): request-body plumbing + keyset cursor (FG-WEB-0006 RequestInvalid), group / project / member /
protected-ref write endpoints, ProjectList / GroupList (rows filtered by authorize, never in SQL), `GET /api/v1/user` (UserSelfRead).
MRs: MR1 feat/core-web-path-parse ∥ MR2 feat/auth-prove-authorized → MR3 feat/db-namespace-resolve → MR4 feat/web-dispatcher →
MR5 feat/web-api-project-read ∥ phase 9 → MR6–MR8 (8b).
Answers (as recommended): forge-web reads through forge-core traits implemented by forge-db (8a); one feature crate `forge-hosting`
(web → hosting → auth/db) for the 8b writes (ADR amendment); D5 actions/resources (GroupTransfer, UserRename, TombstoneRelease,
TransferTarget) move to D5 (ADR-0015 amendment); utoipa for OpenAPI in MR6 after cargo deny/audit; W1 deferred to M3 with a single
ProjectView builder now; Idempotency-Key deferred to M3; resolve rate limiting and prefix=true deferred to M2; the agent-model
authz-coverage gate is already updated (v0.4.2) — the user confirms it is on the agent model's main; next free codes FG-WEB-0006,
FG-AUTH-0011, FG-GIT-0011, FG-REPO-0016 (0015 is D5's TransferInvalid).

MR4 musts carried over from the MR2 review (authz-matrix-auditor + security-reviewer, HIGH; ADR-0015 amendment 2026-10-30):
the action is a runtime field of `Authorized<R>` (a WebShell proof has the same type as a UserCreate proof), so MR4 must
(a) check `proof.action() == def.action` in the registry before calling **every** handler, (b) give handlers no way to be called
outside the registry (the proof reaches a handler only through it), and (c) add a negative test that a proof for a different
action is rejected. MR3 / MR4 lookups must issue the same query shape (count and kind) for absent, tombstone and invisible paths.
MR3 note (review MEDIUM, accepted residual, ADR-0015 Amendment MR3): the resolver's statement count and text are constant, the
work inside is not (an absent path stops after two index probes, a deep project walks its chain) — mitigations: resolve rate
limiting (deferred to M2) and, if needed, a response-time floor in forge-web for the resolve / dispatcher paths.
MR3 / MR4 build `forge_auth::LoadedProject` / `LoadedNamespace` from one query result (contexts are sealed, fields private; new
context types are added in forge-auth).

## M1 phase 9 plan (planner, 2026-11-01, cloud session) — approved under the user's standing "推奨で進める"

Smart HTTP clone / fetch / push. Option **B**: pre-check every ref command in forge (role, delete, reserved, tag move,
hex-like name → whole push rejected in-band before git runs) + git-native `-c receive.denyNonFastForwards=true` when a protected
branch is touched; no hooks in phase 9 (ADR-0008 amendment; pre-receive moves to M3 = phase 10). Option A (pre-receive now) was
rejected: a missing hook is silently skipped by git (fail-open).

MRs: **MR0** `feat/web-connection-fairness` ∥ **MR1** `feat/git-smart-http-transport` ∥ **MR2** `feat/db-push-record` →
**MR3** `feat/web-smart-http-fetch` → **MR4** `feat/web-smart-http-push`. Codes: MR0 FG-WEB-0008 ServerBusy; MR1 FG-GIT-0013
GitRequestTooLarge, 0014 GitProtocolInvalid, 0015 GitClientDisconnected; MR2 FG-GIT-0012 PushRecordFailed (alert, runbook
push-record-failed); MR3 FG-WEB-0007 SmartHttpServiceUnsupported (FG-WEB-0006 stays reserved for 8b), FG-REPO-0016
RepositoryNotReady; MR4 FG-GIT-0011 AmbiguousRefName.
- MR1 (forge-git): streaming `spawn_stream()` (stdin sink / stdout stream / stderr ≤ 64 KiB), separate `stream_slots` (16),
  idle + total timeouts, process-group kill (Windows: Job Object), pkt-line + sideband, gzip with decompressed cap, v2
  advertisement without `# service=` prefix, fixed `-c` sets (fsckObjects, maxInputSize, keepAlive, empty core.hooksPath).
- MR2 (forge-db): `record_push` (one audit row + `repository.pushed`, ≤ 100 refs / ≤ 20 commits per ref, explicit truncation),
  dedup key `repository_pushed:<repo_id>:<sha256(request_id ‖ sorted updates)>` (ADR-0016 amendment; fixes A→B→A),
  `protected_refs::rules_for` snapshot matching `levels_for` (property test), forge-core traits PushRecorder / ProtectionLoader,
  `forge admin repo record-push --from-log`.
- MR3 (forge-web): register upload-pack routes as RepoRead; resolver adds repository_id + state; git POSTs exempt from
  request_timeout with own caps (fetch ≤ 16 MiB, 1 h, 60 s idle); 302 for moved re-appends `service`; config `git.http.*`;
  tripwire becomes `forge_web_runs_git_only_through_smart_http_api`; real-git E2E (`crates/forge-cli/tests/smart_http_e2e.rs`).
- MR4 (forge-web, forge-auth, forge-git): receive-pack as RepoPush; authorize before reading the body; strict command parse
  (≤ 1000, ≤ 1 MiB, report-status required, push-cert refused); `forge_auth::authorize_ref_updates` (per-ref authorize());
  denial = in-band `ng` for every ref, git not run; forward original bytes verbatim; push ≤ 2 GiB, 1 h, 60 s idle; receive-pack
  detached from the request once the body is complete; log `forge.git.push_applied` first, then `record_push` (3 retries / 30 s)
  else FG-GIT-0012.
Answers (recommended, adopted): server is Linux, Windows dev uses a Job Object; option B; phase 10 = M3 pre-receive; commits in
`repository.pushed` in M1 (bounded rev-list, 10 s); defaults above + ≤ 4 streams per principal; dedup key with request_id;
v2 unreferenced-object fetch: test first, block with `uploadpack.allowAnySHA1InWant=false` if served, record in ADR-0007.
Reviewers: rust / security / git-protocol / silent-failure on MR1, MR3, MR4; authz-matrix on MR3, MR4; database +
error-contract on MR2; pr-test-analyzer on all.

## M1 phase 8b plan (planner, 2026-11-01) — approved under the user's standing "推奨で進める"

Premises found: write routes need the body read, bounded and typed BEFORE load (authorize inputs live in the body: role to add,
protection level, visibility, parent) → `registry::route_with_body`; forge-db lacks `projects::update`, visibility change and
`groups::update` (+ audit variants); actions ProjectList / GroupList / UserSelfRead do not exist yet; GroupCtx / UserNamespaceCtx
arrive with the first non-public group route; TOCTOU → forge-hosting loads facts INSIDE the write transaction (FOR SHARE on role
rows), calls authorize() there, then writes; a keyset cursor must not reveal scanned-but-invisible ids → opaque AEAD cursor;
`is_agent` is not an authz input yet → once ProtectedRefManage is routed an agent could drop a protection and push (CRITICAL-class)
→ MR8 makes it one; DefaultBodyLimit 413 has no FG code.
Lists: **L1** (authorize every scanned row, AEAD cursor with key file `api_cursor_keys`, scan budget 1000/page, limit ≤100);
switch to an SQL superset (L2) in M2 only if instances exceed ~10k projects per scan (ADR-0010 amendment then).
MRs: **MR6** `feat/web-api-request-body` → {**MR7a** `feat/hosting-project-group-writes` ∥ **MR7b** `feat/web-api-lists`} →
**MR8** `feat/hosting-members-protected-refs`; `gstn` is a separate series: `feat/client-gstn-core` after MR6 (auth incl.
git-credential, api, help --json, schema, OS keyring + 0600 fallback), `feat/client-gstn-hosting` after MR8. repo rename /
transfer / delete wait for D5 (planned in the C-1 gate).
- MR6: body plumbing (strict JSON, deny_unknown_fields, ≤64 KiB per route), AEAD keyset cursor in forge-core, utoipa after
  cargo deny/audit, `GET /api/v1/user` (UserSelfRead), path/id param errors → FG-WEB-0006. Codes: FG-WEB-0006 RequestInvalid,
  FG-WEB-0009 RequestTooLarge (FG-WEB-0008 = MR0 ServerBusy).
- MR7a: new crate forge-hosting (web → hosting → auth/db; ADR-0001 amendment + dependency gate), GroupCtx / UserNamespaceCtx,
  `POST /api/v1/groups` (top level) + `POST /api/v1/groups/{id}/subgroups`, `GET|PATCH /api/v1/groups/{id}`,
  `POST /api/v1/projects`, `PATCH /api/v1/projects/{id}`, `PUT /api/v1/projects/{id}/visibility` (Owner), forge-db update
  functions; events: `project.visibility_changed` (maintainer class); `project.updated` / `group.updated` audit-only until M3.
- MR7b: `GET /api/v1/projects`, `GET /api/v1/groups` through a ProjectPageLoader trait, rows filtered by authorize().
- MR8: members (from-role loaded in the transaction, last-owner rule) and protected-refs CRUD; `is_agent` denied ProtectedRefManage,
  MemberManage, ProjectVisibilityChange (FG-AUTH-0003 reason=agent_token; ADR-0017/0019 amendment); FG-REPO-0017
  ProtectedRefPatternTaken (409) if duplicates are not already a 409.
Answers (recommended, adopted): L1; is_agent becomes an authz input in MR8; project parent in the body, subgroups via their own
route; project.updated / group.updated audit-only in M1; gstn as a separate series. Opus for MR6/MR7a/MR8 core and security
reviews; Sonnet for schemas, docs regeneration, matrix boilerplate, list tests and most gstn glue.

## D5 plan (planner, 2026-11-01) — approved under the user's standing "推奨で進める"

Option **A**: all of ADR-0015 rename / transfer / delete / restore / purge / tombstone release in M1, through API + worker + operator
CLI (one subtree-move routine for project, group and user renames). Premises found: `Resource::Transfer` is the pre-ADR shape
(admins must NOT move a project to top level); GroupTransfer / UserRename / NamespaceTombstoneRelease actions missing; ChangeTx
writes one audit row (subtree moves need one per affected project — closes the ADR-0015 D1 note); purge needs DELETE grants, a
`purging` repository state, `run_after` on jobs and a forge-git remove; create can read a moved parent's new path outside its
advisory lock → re-check the locked path in MR2; storage is keyed by RepoId (renames never touch disk).
MRs: **MR1** `feat/auth-transfer-targets` (Opus) ∥ **MR2** `feat/db-namespace-move` (Opus) → **MR3**
`feat/hosting-project-delete-restore` (Sonnet, Opus review) → {**MR4** `feat/repo-purge` (Opus) ∥ **MR5**
`feat/web-api-namespace-moves` (Opus)} → `feat/client-gstn-hosting` adds repo rename/transfer/delete/restore.
Codes: FG-REPO-0015 TransferInvalid (409), FG-REPO-0018 RenameRateLimited (429), FG-REPO-0019 RestoreUnavailable (409),
FG-REPO-0020 RepositoryPurgeStuck (alert, runbook repository-purge). Runbooks: repository-purge, namespace-redirects.
Answers (recommended, adopted): one audit row per affected project; `reclaim` flag for A→B→A (release + rename in one transaction,
only when the tombstone points at the same namespace; ADR-0015 amendment); purge deletes the rows; a deleted tombstone's
owner_at_write = deleted_by; rate limit 10 moves per namespace per 24 h (admins included, operator CLI exempt); agents denied
every D5 action incl. restore; UserRename changes username and namespace path; subtree cap 1000 namespaces; `project.purged`
admin class, other project.* integration (ADR-0016 §10 amendment); operator `forge admin project purge --now`; deletion grace
`hosting.deletion_grace` 7 d (min 1 d); pending deletion: 410 for callers who can see the project, 404 for others.
