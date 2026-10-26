# forge-git

**Responsibility**: the only place forge runs `git` (CLAUDE.md §3-5) and the
only place repository storage paths are derived (ADR-0009). Smart HTTP
(ADR-0007) and protected-ref hooks (ADR-0008) build on this crate in M1
phases 9–10.

**Public API**
- `Git::new(GitConfig)` / `Git::command(subcommand) -> GitCommand`. Argument
  layout: `git [-c k=v]* <sub> <options>* [--end-of-options <revisions>*] [-- <paths>*]`.
  - `.arg(&'static str)` — fixed options only (user input cannot reach the
    option position; forge-generated values use the crate-internal `arg_owned`)
  - `.revision(&RefName | Revision::object_id(..))` — after `--end-of-options`
  - `.path(..)` — after `--` (git reads everything after `--` as paths)
  - `.config_override(key, value)` — `-c` for this invocation
  - `.repo(git_dir)` — pins `GIT_DIR` (no discovery); `.current_dir(dir)` —
    discovery stops at `dir` (`GIT_CEILING_DIRECTORIES` = its parent)
  - `.env(GIT_PROTOCOL, ..)`, `.stdin(bytes)`, `.timeout(d)`, `.output().await`
- Environment is cleared and rebuilt: `HOME` = forge-owned,
  `GIT_CONFIG_GLOBAL=<home>/.gitconfig`, `GIT_CONFIG_NOSYSTEM=1`,
  `GIT_TERMINAL_PROMPT=0`, `LC_ALL=C`; only `PATH`, `SystemRoot`,
  `TEMP`/`TMP`/`TMPDIR` pass through.
- stdin / stdout / stderr are pumped concurrently (no pipe deadlock); stdout is
  capped (`max_output_bytes`); stderr (bounded, URL credentials redacted) goes
  to the error context for the log only — `GitOutput` exposes stdout only.
- A global semaphore limits concurrent subprocesses; on timeout the child is
  killed and reaped; each run emits a debug `git.exec` event (subcommand,
  duration_ms, exit_code).
- `version::detect(&Git)` / `GitVersion::require_supported()` — minimum 2.38.
- `StorageRoot::open(path)`, `path_for(RepoId)` (pure, stable layout),
  `existing(RepoId)` (every directory from the root down is a real directory),
  `init_bare(&Git, RepoId, hooks_path)` — atomic (temp dir + rename), refuses an
  existing path, no template, `receive.autogc=false`, `gc.auto=0`, fsck on.

**Error codes owned**

| Code | When | Runbook |
|---|---|---|
| FG-GIT-0001 GitUnavailable | binary missing / unparseable / older than 2.38 | `git-unavailable` |
| FG-GIT-0002 GitTimeout | total timeout exceeded (child killed and reaped) | — |
| FG-GIT-0003 GitProcessFailed | unexpected non-zero exit, or reading / waiting failed | `git-process-failed` |
| FG-GIT-0004 GitBusy | no free subprocess slot within `acquire_timeout` | — |
| FG-GIT-0006 GitOutputTooLarge | captured stdout over `max_output_bytes` | — |
| FG-REPO-0001 StoragePathRejected | root missing / not a dir / symlink; symlink, non-dir or outside-root entry | `repo-storage` |
| FG-REPO-0002 StorageMissing | repository directory missing | `repo-storage` |
| FG-REPO-0003 StorageIoFailed | permission / disk / I/O failure on the storage root | `repo-storage` |
| FG-REPO-0004 RepoPathOccupied | creating a repository where something already exists | `repo-storage` |

`FG-GIT-0005 InvalidRefName` is raised by `forge_core::RefName` and
`Revision::object_id`.

**Known gaps (tracked)**
- Only the direct child is killed on timeout; helpers (`pack-objects`,
  `index-pack`, hooks) need a process group (Unix) / Job Object (Windows) —
  required before the streaming Smart HTTP path (M1 phase 9).
- No streaming API yet (phase 9) and no idle timeout yet.

**Gotchas**
- `git check-ref-format` takes no `--`; the oracle test passes names that
  always start with `refs/`.
- On Windows the symlink tests cannot create symlinks without a privilege; the
  Windows variant checks the not-a-directory rejection and Linux CI runs the
  symlink cases.
- `canonicalize` adds `\\?\` on Windows; `StorageRoot` strips it before
  comparing.

**Tests**: `cargo nextest run -p forge-git` (needs `git` >= 2.38 on `PATH`).
