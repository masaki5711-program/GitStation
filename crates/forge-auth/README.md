# forge-auth

**Responsibility**: the single authorization decision point (CLAUDE.md §3-4,
ADR-0010). Every HTTP route and every git operation calls `authorize()` before
any side effect. Authentication providers (local password, LDAP) and personal
access tokens join this crate when the database lands (M1 phase 5).

**Public API**
- `authorize(&Principal, Action, &Resource) -> Result<(), ForgeError>` — pure:
  no I/O. The caller loads memberships / effective roles / token scopes / ref
  protection into the inputs first.
- `Principal { user, is_admin, active, token_scopes }` — `token_scopes: None`
  means an interactive session.
- `Action` — fieldless enum, `Action::ALL`, `name()`, `is_public()`,
  `is_git_operation()`, `required_scope()`. Adding a variant breaks the build
  of `authorize()` (exhaustive match) until it is decided.
- `Resource` — `Instance`, `Group { role }`, `Namespace { owner }`,
  `Project(ProjectView)`, `Ref { project, update, protection }`,
  `Transfer { source, target_role }`, `Membership { role, granting }`,
  `Token { owner }`.
- `Role` (Guest < Reporter < Developer < Maintainer < Owner), `Scope`
  (`read_repository` ⊂ `write_repository`, `read_api` ⊂ `api`), `Visibility`
  (`Private`, `Internal`).
- `matrix::ROWS` — the permission matrix as data; `matrix::summary()` /
  `summary_json()` feed `Git/docs/authz/matrix.generated.json` for the
  `authz-coverage` gate.

**Decision order**: public → credentials (`FG-AUTH-0001`) → active
(`FG-AUTH-0004`) → visibility (`FG-REPO-0005`, 404 for members-only resources
the caller cannot see) → pending deletion (`FG-REPO-0006`, except restore) →
token scope (`FG-AUTH-0002`) → role (`FG-AUTH-0003`; administrators pass) →
ref protection (`FG-GIT-0007` force, `FG-GIT-0008` below min role,
`FG-GIT-0009` delete; administrators included).

**Error codes owned**: FG-AUTH-0001..0004, FG-REPO-0005..0006,
FG-GIT-0007..0009 (all `info`, no runbook: expected client conditions).

**Rules of the matrix** (`.claude/rules/forge-authz.md`)
- Every action has at least one allowed and one denied row (public actions:
  an allowed anonymous row); `every_action_has_matrix_rows` enforces it.
- Every row is asserted by `denied_matrix_rows_all_deny`.
- Denials carry only `action`, role / scope names and a `reason` — never
  resource names, paths or tokens.

**Tests**: `cargo nextest run -p forge-auth` (pure, no DB, no git).
