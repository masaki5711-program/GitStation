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
`TombstoneId` (typed i64 ids), `RefName::parse` (git check-ref-format rules + no
component starting with `-`; `branch()`, `tag()`).

**Namespaces** (ADR-0015, step D1): `PathSegment::parse` (any segment) and
`PathSegment::parse_top_level` (user names and top-level groups; also refuses
`RESERVED_TOP_LEVEL`), `as_str`. `RESERVED_TOP_LEVEL` (the one list, including
`edge`, `fleet`, `devices`), `MAX_DEPTH` (20), `MAX_FULL_PATH_LEN` (255).
`UserKind` (`human`, `service_account`, `bot`; `ALL`, `as_str`, `parse`; no device kind).
`Role` (`Guest < … < Owner`; `ALL`, `as_str`, `parse`) and `Visibility` (`private`, `internal`), moved
from forge-auth in step D2 (forge-auth re-exports them).
`AuditDetails` (typed `audit_log.details`: `action()`, `to_value()`; no personal data).
Feature `test-support`: `path_vectors::accepted()` / `rejected()`, the shared
segment vectors.

**Error codes owned**: FG-GIT-0005 InvalidRefName (`reason` names the failed rule;
the rejected name is not logged). FG-REPO-0007 NamespacePathInvalid (`reason`:
empty, too_long, charset, edge, double_dot, reserved_suffix, device_name; `len`;
the name is not logged). FG-REPO-0008 NamespacePathReserved (`reason=reserved`).

**Gotchas**
- Adding a configuration key: add it to `KEYS` and to `build()`; an unknown
  key in the file or under `FORGE_SERVER_` / `FORGE_STORAGE_` /
  `FORGE_DATABASE_` fails with `reason=unknown_key` / `unknown_env`.
- Externally supplied request ids are accepted only if they are short ASCII
  alphanumerics / `-` / `_` — anything else is replaced, never logged raw.
- `telemetry::init` returns `false` if a subscriber is already installed (tests).
- The segment rules are mirrored by the CHECK on `forge.namespaces.segment`
  (migration `20261028100100`); change both together, and the test vectors with them.
- A new top-level reserved word needs an ADR-0015 amendment. The planned
  `ReservedNameOccupied` doctor check (ADR-0015) is not built yet, so existing
  names are not checked against a new word.
