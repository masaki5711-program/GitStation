# forge-core

**Responsibility**: domain primitives and runtime foundations shared by every
server-side crate. Depends only on `forge-errors`.

**Public API**: `RequestId` (`generate`, `parse` — log-safe validation,
`as_str`), `telemetry::init(LogFormat, default_filter)`,
`telemetry::log_failure(&ForgeError, &RequestId)` — the single way a handled
failure is logged (once, at the boundary, level by severity).

**Configuration** (ADR-0013): `config::load(file, &Env) -> Config` — defaults ←
TOML file ← environment (`FORGE_<SECTION>_<KEY>`, derived from the key); flags
are applied by the caller. `Env::from_process()` / `Env::from_pairs` (tests).
`Config::storage_root()` and `DatabaseConfig::url()` return FG-SYS-0002
`reason=missing` for commands that need them. `Secret` and `DatabaseUrl`
never print a password. Every key is listed in `config::KEYS`.

**Domain types**: `RepoId`, `UserId`, `GroupId`, `ProjectId`, `NamespaceId`,
`TombstoneId`, `ProtectedRefId` (typed i64 ids), `RefName::parse` (git check-ref-format rules + no
component starting with `-`; `branch()`, `tag()`).

**Repositories and protected refs** (step D4 MR1; ADR-0008; the repository kind of ADR-0004 is gone, ADR-0023):
`RepositoryState` (`provisioning`, `ready`; same API) — the CHECKs of
`forge.repositories`. `RefPattern::parse` (FG-REPO-0013, rules checked in this
order: ≤ 255 bytes `too_long`; starts with `refs/heads/` or `refs/tags/`
`prefix`; at most one `*`, only in the last path component `wildcard`; with the
`*` read as a letter, the `RefName` rules `check_ref_format` + `rule`), `as_str`,
`key()` (case-folded; duplicate rules share it), `matches(&RefName)` (ignores case
on both sides; `*` matches any run, `/` and the empty run included — broader
only ever protects more). The fold is per character and context-free
(`char::to_lowercase`, then final `ς` → `σ`), so `Σ` folds alike in a pattern and
in a ref wherever it stands; no Unicode normalization (NFC vs NFD differ). `MAX_PATTERN_LEN`, `PATTERN_PREFIXES`.

**Namespaces** (ADR-0015, step D1): `PathSegment::parse` (any segment) and
`PathSegment::parse_top_level` (user names and top-level groups; also refuses
`RESERVED_TOP_LEVEL`), `as_str`. `RESERVED_TOP_LEVEL` (the one list, including
`edge`, `fleet`, `devices`), `MAX_DEPTH` (20), `MAX_FULL_PATH_LEN` (255).
`NamespacePath` (`from_segments`, `parse(full_path)` — FG-REPO-0007 `empty` /
`too_deep` / `path_too_long`; `segments`, `depth`, `top_level`, `full_path`) and
`LookupPath::fold(&[&str])` (request side: refuses empty segments, non-ASCII,
control characters and `%`, then `to_ascii_lowercase` only and `PathSegment::parse`;
FG-WEB-0001 `reason=path_rejected`, `part=namespace`, `rule`).
`UserKind` (`human`, `service_account`, `bot`; `ALL`, `as_str`, `parse`; no device kind).

**Web paths** (ADR-0015 step 2, M1 phase 8 MR1; `web_path`): `parse_web_path(raw_path,
query) -> ParsedPath` sorts the raw (still encoded) `req.uri().path()` by its string
alone into `NamespaceHome(LookupPath)`, `Scoped { path, word: RouteWord, rest:
TailSegments }`, `Git { path, route: GitRoute }` (`.git` only on the last namespace
segment, removed once; `info/refs` needs exactly one `service=git-upload-pack` /
`git-receive-pack`) or `Instance { route: InstanceRoute, rest }` (`/-/projects/{id}/…`; `/-/assets/{file}` since M2 MR2, exactly one segment).
It never chooses the scope (group / project): that follows resolution. `RouteWord`,
`GitRoute` (`service`, `is_advertisement`, `is_push`) and `InstanceRoute` have `ALL`
and `as_str`; route words are case-sensitive. `TailSegments::decode` decodes each
segment once (strict escapes, valid UTF-8; refuses `%2F` `%5C` `\`, control
characters, invisible / bidi formatting characters (`is_invisible`), `.` `..`).
A tail segment may start with `-`: anything that reaches a git argument vector
goes after `--` / `--end-of-options` or is a resolved ref / oid. Refusals are
FG-WEB-0001 with `reason` `path_rejected` /
`unknown_route` / `service_unsupported` plus `part` and `rule`, never the input:
log the path through `log_safe_path` / `LogSafePath` (256 characters, controls and
bidi / zero-width characters escaped). `MAX_RAW_PATH_LEN` (8192 bytes),
`MAX_QUERY_LEN` (2048 bytes, every shape). The HTTP method is not checked (the
forge-web route table does).
Shared vectors: `tests/fixtures/url-vectors.json` at the repository root
(`form` = `both` / `browser` / `raw`; also for forge-web and the SPA).

**Dispatcher data access** (`namespace_resolver`; implemented by forge-db, injected
like `ReadinessProbe`): `NamespaceResolver::resolve(&LookupPath, viewer:
Option<UserId>) -> Resolution` — one call resolves the path **and** loads the
viewer's authorization inputs, and MUST issue the same statements whatever the
outcome and whether or not the viewer may see the target (no timing oracle).
`Resolution`: `Live(Resolved)`, `Moved { resolved, tombstone, current: NamespacePath }`,
`Deleted { tombstone }`, `Absent` (exact matches only). `Resolved { namespace_id,
target: ResolvedTarget }` with `ResolvedTarget::User(UserId)` / `Group { id,
viewer_role }` / `Project { id, access: ProjectAccess { visibility, role,
pending_deletion } }`; `owner() -> NamespaceOwner` (typed id), `kind()`.
`NamespaceKind` (`user`, `group`, `project`). `ProjectAccessLoader::load_project(ProjectId,
Option<UserId>) -> Option<ProjectRecord>` for routes that address a project by id
(same contract; MR5): `ProjectRecord { id, access: ProjectAccess, namespace_id, path,
display_name, description, created_at }` — the access facts and
what `GET /api/v1/projects/{id}` shows after `authorize()`, from one row.
`web_path::parse_positive_id` — the URL id rule (permalink and API: plain positive
decimal, no leading zero, fits `i64`). Boxed futures (`LookupFuture`); no forge-auth types (forge-web
converts).
`Role` (`Guest < … < Owner`; `ALL`, `as_str`, `parse`) and `Visibility` (`private`, `internal`), moved
from forge-auth in step D2 (forge-auth re-exports them).
`AuditDetails` (typed `audit_log.details`: `action()`, `to_value()`, `event_type()` —
`None` for the audit-only actions; no personal data).

**Events** (ADR-0016 §5 and its D3 amendment, step D3 MR1b): `EventType` (`ALL`,
`as_str`, `parse`, `visibility_class()`) is the one list of event types; a variant
joins in the commit that adds its writer (now `project.created` — integration class,
D4 MR2 —, `repository.created`, `token.created`, `token.revoked`).
`AuditDetails::ProjectCreated` (`project.created`: project, namespace, parent namespace
and repository ids, visibility, owner id) is also that event's payload. `VisibilityClass` (`integration`, `maintainer`, `deployment`,
`admin`). `DedupKey::new(prefix: &'static str, &[DedupPart])` — parts are `Id(i64)`,
`Hash(&[u8; 32])` (lowercase hex) and `Word(&'static str)`, so a client string
cannot become a key (a `compile_fail` doc test); shape
`^[a-z][a-z_]*(:[0-9a-z_-]{1,64}){1,4}$`, at most 200 characters, else FG-DB-0010
`reason=invalid_dedup_key` (`prefix`). `DedupKey::is_well_formed` is the check the
forge-db writers repeat.
**Injected services** (implemented elsewhere, injected by `forge serve` so
forge-web does not depend on forge-db): `readiness::ReadinessProbe` and
`Authenticator` (step D3 MR3, ADR-0019 §7) — `authenticate(&RequestId,
presented: &str) -> Result<TokenIdentity, ForgeError>` (`token_id`,
`user_id`, `username`, `display_name` (phase 8b MR6, for `GET /api/v1/user`),
`is_admin`, `scopes`, `is_agent`; the presented token is a secret
and is never logged). forge-db's `Db` implements both.
Feature `test-support`: `path_vectors::accepted()` / `rejected()`, the shared
segment vectors; `DedupKey::unchecked` (malformed keys for the writers' tests).

**Error codes owned**: FG-GIT-0005 InvalidRefName (`reason` names the failed rule;
the rejected name is not logged). FG-REPO-0007 NamespacePathInvalid (`reason`:
empty, too_long, charset, edge, double_dot, reserved_suffix, device_name; `len`;
the name is not logged). FG-REPO-0008 NamespacePathReserved (`reason=reserved`).
FG-REPO-0013 ProtectedRefRuleInvalid (`RefPattern::parse`: `too_long`, `prefix`,
`wildcard`, `check_ref_format` with `rule`; `len`; the pattern is not logged —
`too_many_rules` is raised by forge-db from step D4 MR5; a duplicate is FG-REPO-0017 since
M1 phase 8b MR8).
Raises FG-DB-0010 ChangeRejected `reason=invalid_dedup_key` (owned by forge-db).
Raises FG-WEB-0001 RouteNotFound (owned by forge-web) from `parse_web_path`,
`LookupPath::fold` and `TailSegments::decode` (`reason` path_rejected / unknown_route /
service_unsupported; `part`; `rule`).

**Gotchas**
- Adding a configuration key: add it to `KEYS` and to `build()`; an unknown
  key in the file or under `FORGE_SERVER_` / `FORGE_STORAGE_` /
  `FORGE_DATABASE_` fails with `reason=unknown_key` / `unknown_env`.
- Externally supplied request ids are accepted only if they are short ASCII
  alphanumerics / `-` / `_` — anything else is replaced, never logged raw.
- `telemetry::init` returns `false` if a subscriber is already installed (tests).
- The segment rules are mirrored by the CHECK on `forge.namespaces.segment`
  (migration `20261028100100`); change both together, and the test vectors with them.
- `RefPattern`'s prefix and length rules are repeated by the CHECKs of
  `forge.protected_refs.pattern` (migration `20261031100000`; forge-db test
  `the_db_check_agrees_with_the_ref_pattern_parser`); the wildcard and
  check-ref-format rules exist only in the parser. Change both together.
- `parse_web_path` and the SPA's parser (M2) must agree: change the rules, the
  vectors in `tests/fixtures/url-vectors.json` and the ADR-0015 amendment together.
  A vector's `form` is checked against a WHATWG URL parser (`new URL()`); a `raw`
  path is one a browser would rewrite before sending.
- A new route word (`pulls`, `pipelines`, …) is added to `RouteWord` only with the
  ADR that gives it an Action; until then it is `unknown_route`.
- A new top-level reserved word needs an ADR-0015 amendment. The planned
  `ReservedNameOccupied` doctor check (ADR-0015) is not built yet, so existing
  names are not checked against a new word.

**Opaque list cursors** (M1 phase 8b MR6, `cursor`): `CursorKeys::parse(text)`
(the `api.cursor_keys_file` format, `<kid> <hex>` per line, AES-256 keys),
`encode(purpose, payload) -> String` and `decode(purpose, cursor) ->
Vec<u8>`: AES-256-GCM, base64url, version 1, the newest kid encrypts and
every listed kid decrypts; `purpose` binds a cursor to one list and one
caller. Every failure is `FG-WEB-0006 reason=cursor_invalid` with a `rule`
and never the value. `Config::api.cursor_keys()` reads the file
(`ApiConfig`).

**List page scans** (M1 phase 8b MR7b, `list_page`): `ProjectPageLoader::project_page(after:
Option<ProjectId>, viewer: UserId, max_rows) -> Vec<ProjectRecord>` and
`GroupPageLoader::group_page(...) -> Vec<GroupListing>` (`GroupListing { id, viewer_role,
namespace_id, path, display_name, description, created_at }`): every row after `after` in
ascending id order, visible or not — forge-web decides each with `authorize()`. One
statement per call whatever the rows (the contract forge-db's `lists` keeps).
