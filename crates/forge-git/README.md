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
  - `.git_protocol(value)` — the only caller-set git variable (validated
    `key=value` pairs); `.stdin(bytes)`, `.timeout(d)`, `.output().await`
  - crate-internal: `.arg_owned()` (refused at run time if it starts with `-`),
    `.config_override()` (`-c` is protected config, so values are forge-owned)
- Environment is cleared and rebuilt: `HOME` = forge-owned,
  `GIT_CONFIG_GLOBAL=<home>/.gitconfig`, `GIT_CONFIG_NOSYSTEM=1`,
  `GIT_TERMINAL_PROMPT=0`, `GIT_NO_REPLACE_OBJECTS=1`, `GIT_LITERAL_PATHSPECS=1`,
  `LC_ALL=C`, applied after caller variables so nothing overrides them; only
  `PATH`, `SystemRoot`, `TEMP`/`TMP`/`TMPDIR` pass through. Without an explicit
  directory git runs in forge's home.
- stdin / stdout / stderr are pumped concurrently (no pipe deadlock); stdout is
  capped (`max_output_bytes`; the child is killed as soon as the cap is hit);
  one deadline covers I/O and the final wait; stderr (bounded; URL userinfo,
  secret query values and `Authorization:` values redacted) goes
  to the error context for the log only — `GitOutput` exposes stdout only.
- A global semaphore limits concurrent subprocesses; on timeout the child is
  killed and reaped; each run emits a debug `git.exec` event (subcommand,
  duration_ms, exit_code).
- `version::detect(&Git)` / `GitVersion::require_supported()` — minimum 2.38.
- `StorageRoot::open(path)`, `path_for(RepoId)` (pure, stable layout),
  `existing(RepoId)` (every directory from the root down is a real directory),
  `init_bare(&Git, RepoId, token: Uuid, hooks_path)` — atomic (temp dir +
  rename), refuses an existing path, no template, `receive.autogc=false`,
  `gc.auto=0`, fsck on; writes `forge.provisionToken=<token>`
  (`PROVISION_TOKEN_KEY`, the `repositories.provision_token`) last, before the
  rename, so a marked repository is a fully configured one.
- `init_bare_or_adopt(&Git, RepoId, token: Uuid, hooks_path) -> Provisioned`
  (`Created(path)` | `Adopted(path)`, `.path()` / `.into_path()`) — the
  idempotent form for the provisioning job (ADR-0009 amendment, D4). Adopts an
  existing path only when it is a real directory under the root (no symlink),
  its `config` is a regular file, its `forge.provisionToken` equals `token`
  and it is bare; adoption re-applies the settings and `core.hooksPath`.
  Anything else stays `FG-REPO-0004` with `adopt_refused` = `symlink` |
  `not_dir` | `not_a_repository` | `config_not_a_file` | `unmarked` |
  `token_mismatch` | `unreadable` | `not_bare` (tokens never go into the
  context). Git timeout / busy / unavailable keep their own codes (retryable).
  Temp dirs (`.tmp-<id>-<uuid>.git`) left by a crashed attempt do not block a
  retry and are not removed by it.

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
- `Revision::object_id` accepts 40 or 64 hex digits regardless of the
  repository's object format; callers that compare against an approved SHA
  must pass the repository's format (before M3 merge / M4 deploy).
- `--end-of-options` is accepted differently by some subcommands
  (`update-ref`, `symbolic-ref` take plain operands): add a real-git test for
  each subcommand phases 9–10 use.
- On Windows, `git` on `PATH` may be the `cmd\git.exe` launcher; killing it may
  not kill the real git — configure an absolute path to `mingw64\bin\git.exe`.
- Temp directories left by a crashed `init_bare` (`<shard>/.tmp-<id>-*.git`)
  are never removed automatically; a doctor check / admin cleanup is open
  (candidate for D4 MR3b or later).
- `command.rs` is ~480 lines (target 400): split `GitConfig` out when the
  streaming API lands.

- **Flaky (open, 2026-10-26)**: `large_stdin_and_stdout_do_not_deadlock` hung ~590 s once
  in a full parallel run on Windows (its own 60 s timeout did not fire; passes in
  6 s alone and in later full runs). Suspect: tokio Windows pipe I/O on the
  blocking pool outliving the timeout. Investigate with the debugger agent
  before the streaming path (phase 9).

**Gotchas**
- `git check-ref-format` takes no `--`; the oracle test passes names that
  always start with `refs/`.
- On Windows the symlink tests cannot create symlinks without a privilege; the
  Windows variant checks the not-a-directory rejection and Linux CI runs the
  symlink cases.
- `canonicalize` adds `\\?\` on Windows; `StorageRoot` strips it before
  comparing.

**Tests**: `cargo nextest run -p forge-git` (needs `git` >= 2.38 on `PATH`).
