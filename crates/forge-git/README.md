# forge-git

**Responsibility**: the only place forge runs `git` (CLAUDE.md §3-5) and the
only place repository storage paths are derived (ADR-0009). Smart HTTP
(ADR-0007) and protected-ref hooks (ADR-0008) build on this crate in M1
phases 9–10.

**Public API**
- `Git::new(GitConfig)` / `Git::command(subcommand) -> GitCommand` — builder:
  `.arg()` (forge-controlled), `.operand()` (user-controlled; placed after a
  single `--`), `.current_dir()`, `.env()` (explicit `GIT_*` such as
  `GIT_PROTOCOL`), `.stdin()`, `.timeout()`, `.output().await`.
  Environment is cleared and rebuilt (`HOME` = forge-owned,
  `GIT_CONFIG_NOSYSTEM=1`, `GIT_TERMINAL_PROMPT=0`, `LC_ALL=C`; only `PATH`,
  `SystemRoot`, `TEMP`/`TMP`/`TMPDIR` pass through). A global semaphore limits
  concurrent subprocesses; `kill_on_drop` ends a timed-out child.
- `version::detect(&Git)` / `GitVersion::require_supported()` — minimum 2.38.
- `StorageRoot::open(path)`, `path_for(RepoId)` (pure),
  `existing(RepoId)` (verified real directory under the root, no symlink),
  `init_bare(&Git, RepoId, hooks_path)`.

**Error codes owned**

| Code | When | Runbook |
|---|---|---|
| FG-GIT-0001 GitUnavailable | binary missing / unparseable / older than 2.38 | `git-unavailable` |
| FG-GIT-0002 GitTimeout | total timeout exceeded (child killed) | — |
| FG-GIT-0003 GitProcessFailed | non-zero exit (context: `subcommand`, `exit_code`, `stderr` head) | `git-process-failed` |
| FG-GIT-0004 GitBusy | no free subprocess slot within `acquire_timeout` | — |
| FG-REPO-0001 StoragePathRejected | root unusable, symlink, not a dir, outside root, create failed | `repo-storage` |
| FG-REPO-0002 StorageMissing | repository directory missing | `repo-storage` |

`FG-GIT-0005 InvalidRefName` is raised by `forge_core::RefName`.

**Gotchas**
- `git check-ref-format` takes no `--`; the oracle test passes names that
  always start with `refs/`.
- On Windows the symlink-escape test cannot create symlinks without a
  privilege; the Windows variant checks the not-a-directory rejection and Linux
  CI runs the symlink case.
- `canonicalize` adds `\\?\` on Windows; `StorageRoot` strips it before
  comparing.

**Tests**: `cargo nextest run -p forge-git` (needs a `git` >= 2.38 on `PATH`).
