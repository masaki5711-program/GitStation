# forge-core

**Responsibility**: domain primitives and runtime foundations shared by every
server-side crate. Depends only on `forge-errors`.

**Public API**: `RequestId` (`generate`, `parse` — log-safe validation,
`as_str`), `telemetry::init(LogFormat, default_filter)`,
`telemetry::log_failure(&ForgeError, &RequestId)` — the single way a handled
failure is logged (once, at the boundary, level by severity).

**Domain types**: `RepoId`, `UserId`, `GroupId`, `ProjectId` (typed i64 ids),
`RefName::parse` (git check-ref-format rules + no component starting with `-`;
`branch()`, `tag()`).

**Error codes owned**: FG-GIT-0005 InvalidRefName (`reason` names the failed rule;
the rejected name is not logged).

**Gotchas**
- Externally supplied request ids are accepted only if they are short ASCII
  alphanumerics / `-` / `_` — anything else is replaced, never logged raw.
- `telemetry::init` returns `false` if a subscriber is already installed (tests).
