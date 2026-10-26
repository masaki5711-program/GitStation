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

**Domain types**: `RepoId`, `UserId`, `GroupId`, `ProjectId` (typed i64 ids),
`RefName::parse` (git check-ref-format rules + no component starting with `-`;
`branch()`, `tag()`).

**Error codes owned**: FG-GIT-0005 InvalidRefName (`reason` names the failed rule;
the rejected name is not logged).

**Gotchas**
- Adding a configuration key: add it to `KEYS` and to `build()`; an unknown
  key in the file or under `FORGE_SERVER_` / `FORGE_STORAGE_` /
  `FORGE_DATABASE_` fails with `reason=unknown_key` / `unknown_env`.
- Externally supplied request ids are accepted only if they are short ASCII
  alphanumerics / `-` / `_` — anything else is replaced, never logged raw.
- `telemetry::init` returns `false` if a subscriber is already installed (tests).
