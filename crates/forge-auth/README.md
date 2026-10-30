# forge-auth

**Responsibility**: the single authorization decision point (CLAUDE.md §3-4,
ADR-0010 and its amendment). Every HTTP route and every git operation calls
`authorize()` before any side effect. Authentication providers (local
password, LDAP) and personal access tokens join this crate when the database
lands (M1 phase 5).

**Public API**
- `authorize(&Principal, Action, &Resource) -> Result<(), ForgeError>` — pure:
  no I/O. The caller loads memberships / effective roles / token scopes / ref
  protection into the inputs first.
- `prove<R: ResourceCtx>(&Principal, Action, R) -> Result<Authorized<R>, ForgeError>`
  — calls `authorize()` and nothing else (same code, same context on denial;
  `prove_denies_exactly_like_authorize` checks every action × actor × fixture of
  the matrix). `Authorized<R>` has private fields and no `Clone` / `Default` /
  public constructor (`compile_fail` doctests in `src/prove.rs`); it exposes
  `action()`, `id() -> &R::Id` and `ctx() -> &R`. Handlers take the proof and
  read the target id from it (ADR-0015). The action is a runtime value (a
  `WebShell` proof has the same type as a `UserCreate` one): forge-web's
  registry checks `proof.action() == def.action` before every handler
  (`Dispatched<R>`, built only there), so handlers are unreachable outside
  the registry; `registry_tests::a_proof_for_another_action_is_refused` pins
  the rejection (M1 phase 8 MR4).
- `ResourceCtx { type Id; fn resource(&self) -> Resource; fn id(&self) -> Self::Id }`
  — **sealed** (only this crate implements it), so `id()` and `resource()`
  always read the same value. Contexts have private fields and one
  constructor taking one loaded row: `InstanceCtx` (`Id = ()`),
  `ProjectCtx::new(LoadedProject { id, visibility, role, pending_deletion })`
  (`Id = ProjectId`; `view()`),
  `ResolvedCtx::new(LoadedNamespace { namespace, via, view })`
  (`Id = NamespaceId`; `via()`, `view()`) and
  `UnresolvedCtx::new(UnresolvedReason)` (`Id = ()`; `reason()`; `prove`
  denies it for every non-public action). The lookup layer builds the row from
  one query result; new context types are added here.
- `Principal::authenticated(user, is_admin, active, Credential)` /
  `Principal::anonymous()` — no permissive defaults; `admin_rights()` is true
  for an administrator in a session, or with a token carrying `Scope::Admin`.
- `Credential` — `Session` (all scopes) or `Token(Vec<Scope>)`.
- `Action` — fieldless enum generated with `Action::ALL` and `name()` by one
  macro; `is_public()`, `is_git_operation()` ("git does it as well":
  `RepoRead` is also an HTTP route), `required_scope()`.
- `Resource` — `Instance`, `Group { role }`, `Namespace { owner }`,
  `Project(ProjectView)`, `Ref { project, name, update, protection }`,
  `Transfer { source, target_role }`,
  `Membership { scope, change, removes_last_owner }`,
  `RefProtectionChange { project, from, to }`, `VisibilityChange { project, to }`,
  `Token { owner }` (list / revoke), `TokenRequest { owner, scopes }` (create),
  `Unresolved(UnresolvedReason)` (`Absent` / `TombstoneDeleted` /
  `KindMismatch` — a path of another kind than the route's scope, MR4;
  `UnresolvedReason::ALL`) and
  `Resolved { via: ResolvedVia, view: NamespaceView }` (`PathResolve` only;
  `ResolvedVia::{Live, Moved}`, `NamespaceView::{User, Group { role },
  Project(ProjectView)}` — the kind is the variant, `kind()` names it).
- `PathResolve` (scope `read_api`) is decided by **can see**, not by a read
  role: a Guest on a private project resolves it (but cannot `ProjectRead`);
  groups and personal namespaces resolve for every signed-in user; a project
  for members, everyone on internal, administrators. `WebShell` is public (no
  scope). Both are routed by forge-web since M1 phase 8 MR4
  (`GET /api/v1/namespaces/resolve`, the dispatcher's shell words).
- `RefKind::of(&RefName)` — derives a ref kind (`authorize()` does it for pushes).
- `RefProtection::effective(levels)` — the strictest of all matching rules;
  the only way to build a push resource's protection.
- `Role` (Guest < Reporter < Developer < Maintainer < Owner), `Scope`
  (`read_repository` ⊂ `write_repository`, `read_api` ⊂ `api`, `admin`),
  `Visibility` (`Private`, `Internal`), `RefKind` (`Branch`, `Tag`,
  `Reserved`), `MembershipChange` (`Add`, `Change { from, to }`,
  `Remove { from }`).
- `matrix::summary()` / `summary_json()` — per-action allow / deny counts for
  `Git/docs/authz/matrix.generated.json` (the `authz-coverage` gate).

**Decision order**: public → credentials (`FG-AUTH-0001`) → active
(`FG-AUTH-0004`) → `PathResolve` only: token scope (`FG-AUTH-0002`, before
the path is looked at, so the same 403 for every path) → `Unresolved`, any
non-public action (`FG-REPO-0005`, `reason=absent` / `tombstone_deleted`; the
same 404 as an invisible project) → visibility
(`FG-REPO-0005`, 404; `reason=not_visible`, or `tombstone_not_visible` for a
resolved path that arrived through a `moved` tombstone) → pending deletion
(`FG-REPO-0006`, restore excepted) → token scope (`FG-AUTH-0002`) → role
(`FG-AUTH-0003`; administrator rights pass) → pushes: reserved namespace
(`FG-GIT-0010`, everyone), protected force (`FG-GIT-0007`) and delete
(`FG-GIT-0009`) for everyone including administrators, below the minimum push
role (`FG-GIT-0008`; administrator rights pass the minimum role).

**Escalation rules**: nobody acts on a role above their own — granted role or
the member's current effective role (loaded from the DB), and protection levels
(a Maintainer cannot demote an Owner or lift an Owner-level protection); the
last Owner cannot be removed; visibility changes need an Owner; tokens never
mint tokens; an administrator's token needs the `admin` scope and only users
with administrator rights may request it.

**Handover to the lookup layer and routes** (security review, must be done when they land)
1. Load `ProjectView.role` (max over ancestor groups), `is_admin`, `active`,
   `MembershipChange.from`, `RefProtectionChange.from` and `removes_last_owner`
   from the database — never from the request. `Add` on an existing member is a
   conflict, not an upsert. Expired / revoked tokens are authentication failures.
2. Protection patterns match case-insensitively; collect every matching rule and
   pass them through `RefProtection::effective` (the only constructor).
3. Receive-pack: `RepoPush` on `Resource::Project` before the pack is accepted;
   then every ref command (`RefUpdate` from old/new ids + ancestry) before any
   ref is updated, all-or-nothing (pre-receive, ADR-0008).
4. The `ProjectUpdate` request type has no visibility / protection fields; in
   M4 a deploy-branch change must not allow deploying from an unprotected ref.
5. Transfer into a personal namespace: `target_role = Some(Owner)` only for the
   caller's own namespace; never into a group pending deletion.
6. Project creation: the creator gets no direct role above their group role.
7. Last Owner: apply the same guard to user disable / delete. Disable: done
   in D3 MR2 (`forge_db::users::disable`, FG-AUTH-0009 `last_owner` for
   top-level groups, plus `last_admin`; ADR-0019 §10). Delete: still open.
8. List endpoints filter each item through `ProjectRead`.
9. ~~Decide explicitly whether password Basic auth is allowed on git routes~~
   **Closed (D3 MR3, ADR-0019 §7)**: no. The Basic password is a personal
   access token only (`Credential::Token(scopes)`, built by forge-web
   `auth.rs`); the username is ignored and never logged.
10. CSRF protection for session-backed changes; rate limits on login and tokens.
11. Before M4: default protection for tags, or deploy only from protected refs.

**Error codes owned**: FG-AUTH-0001..0004, FG-REPO-0005..0006,
FG-GIT-0007..0010 (all `info`, no runbook: expected client conditions).

**Matrix**: `src/matrix_rows.rs` (rows) and `src/matrix.rs` (types, summary).
Every action has allowed and denied rows; every row is asserted by
`denied_matrix_rows_all_deny`.

**Tests**: `cargo nextest run -p forge-auth` (pure, no DB, no git).
