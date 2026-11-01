# forge-git

**Responsibility**: the only place forge runs `git` (CLAUDE.md §3-5) and the
only place repository storage paths are derived (ADR-0009). It also owns the
Smart HTTP transport (ADR-0007, M1 phase 9): streaming subprocesses,
pkt-line / side-band framing, gzip decoding and the upload-pack /
receive-pack command sets. Protected-ref hooks (ADR-0008) follow in phase 10.

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
- **Streaming** (`stream.rs`, M1 phase 9): `GitCommand::spawn_stream(principal).await -> GitStream`.
  - `take_stdin() -> GitStdin` (`AsyncWrite`; `shutdown` when the body is complete),
    `take_stdout() -> GitStdout` (`AsyncRead`), `ended() -> StreamEnded` (`.wait().await`
    resolves once git ended — select on it while blocked on the client), `stats()`.
  - `finish().await -> Result<StreamStats, ForgeError>` (`bytes_in`, `bytes_out`, `duration`):
    exit 0 → stats; non-zero → FG-GIT-0003 (`exit_code`, `stderr`, `stderr_truncated`);
    killed → FG-GIT-0002 `kind=idle` (`idle_timeout_ms`) / `kind=total` (`timeout_ms`).
  - `abort_client_disconnected().await -> ForgeError` (FG-GIT-0015, `reason=client_disconnected`);
    dropping a `GitStream` does the same and logs `git.stream.aborted` (FG-GIT-0015, `reason=dropped`).
  - `.idle_timeout(d)` / `.timeout(d)` override `GitConfig::stream_idle_timeout` (60 s) /
    `stream_timeout_total` (1 h). Separate pool `max_streams` (16; `acquire_timeout` →
    FG-GIT-0004 `reason=streams`) and `max_streams_per_principal` (4, refused at once →
    FG-GIT-0004 `reason=principal_streams`); `principal` is the caller's key and is not logged.
  - A supervisor task owns the child: stderr ≤ 64 KiB kept (the rest drained), idle (no byte in
    either direction) and total timeouts, kill of the whole process group (Unix,
    `process_group(0)` + `killpg`, sent before the leader is reaped) / Job Object with
    kill-on-close (Windows, `win32job`; other platforms do not compile). Slot and claim are
    released only after git is reaped. `.stdin(bytes)` on a stream is refused (FG-SYS-0001).
    A leader that fails on its own (non-zero, signal) gets its group killed while it is
    still a zombie (`waitid(WNOWAIT)` + SIGCHLD / 100 ms tick), then reaped. Dropping the
    supervisor (runtime shutdown) kills the tree (`Drop for ProcessTree`). A killed git that
    cannot be confirmed reaped adds `reap_failed` / `reap_error` to the returned error
    (captured runs too).
- **Smart HTTP** (`smart_http`): `Git::upload_pack(&StorageRoot, RepoId, Phase, git_protocol)` /
  `Git::receive_pack(&StorageRoot, RepoId, Phase, git_protocol, max_input_size: NonZeroU64)` (git reads 0 as unlimited) →
  `GitCommand` (path only from `StorageRoot::existing`). Fixed `-c` sets
  `UPLOAD_PACK_CONFIG` / `RECEIVE_PACK_CONFIG` + `receive.maxInputSize` +
  `core.hooksPath=<home>/empty-hooks` (checked to be an empty real directory, not group- or
  world-writable, on every call; created 0700 if missing; otherwise FG-GIT-0001 `reason=hooks_dir`); `--stateless-rpc`,
  `--advertise-refs` for `Phase::Advertise`, upload-pack `--strict`, `-- <path>`.
  `advertisement_prefix(Service, ProtocolVersion)` (`# service=…` + flush, none for
  upload-pack v2); `ProtocolVersion::from_git_protocol(header)` uses the sanitised value only;
  `Service::from_name` / `.name()`.
- **pkt-line** (`pkt_line`): `decode(buf, PktMode) -> Option<(Packet, used)>`, `decode_all`,
  `encode` / `encode_data`, `PktReader` (async, one packet ≤ 65520 bytes), `encode_sideband` /
  `decode_sideband` (`Band::{Data, Progress, Error}`, ≤ 65515 data bytes per packet). Invalid
  framing → FG-GIT-0014 `reason=pkt_line` (`detail`).
- **gzip** (`gzip`): `GzipDecoder::new(cap)` → `push(chunk)` / `finish()`, `decode_capped(body, cap)`.
  Over the cap → FG-GIT-0013 `limit=gzip_decompressed` (decoding stops there); corrupt /
  truncated / trailing data → FG-GIT-0014 `reason=gzip`.
- `version::detect(&Git)` / `GitVersion::require_supported()` — minimum 2.38.
- `StorageRoot::open(path)`, `path_for(RepoId)` (pure, stable layout),
  `existing(RepoId)` (every directory from the root down is a real directory),
  `init_bare(&Git, RepoId, token: Uuid, hooks_path)` — atomic (temp dir +
  rename), refuses an existing path, no template, `receive.autogc=false`,
  `gc.auto=0`, fsck on; writes `forge.provisionToken=<token>`
  (`PROVISION_TOKEN_KEY`, the `repositories.provision_token`) last, before the
  rename, so a marked repository is a fully configured one.
- `init_bare_or_adopt_until(&Git, RepoId, token, hooks_path, deadline: Instant)` (D4 MR3b):
  the same under one deadline — each git call gets at most the time left, each
  file-system step runs under it (blocking ones on the blocking pool); an overrun is
  FG-GIT-0002 or FG-REPO-0003 `reason=deadline` (both retryable).
- `init_bare_or_adopt(&Git, RepoId, token: Uuid, hooks_path) -> Provisioned`
  (`Created(path)` | `Adopted(path)`, `.path()` / `.into_path()`) — the
  idempotent form for the provisioning job (ADR-0009 amendment, D4). Adopts an
  existing path only when it is a real directory under the root (no symlink),
  its `config` is a regular file, that file (read with `--file --no-includes`)
  has no `include.*` / `includeIf.*` key and exactly one
  `forge.provisionToken`, equal to `token`, and it is bare. Adoption re-checks
  that `config` is a regular file, then re-applies the settings and
  `core.hooksPath` with `git config --file <repo>/config` (no `GIT_DIR`). A
  concurrent attempt that loses the rename (FG-REPO-0004 from `create`) runs
  the adopt path once. Anything else stays `FG-REPO-0004` with
  `adopt_refused` = `symlink` | `not_dir` | `not_a_repository` |
  `config_not_a_file` | `includes` | `unmarked` | `duplicate_marker` |
  `token_mismatch` | `unreadable` | `not_bare` | `unwritable` (tokens never
  go into the context). Temp dirs (`.tmp-<id>-<uuid>.git`) left by a crashed
  attempt do not block a retry and are not removed by it. Assumes the storage
  root is writable only by forge (ADR-0009 amendment).
- `storage::provision_error_is_retryable(&ForgeError) -> bool` — the retry
  rule for the provisioning job:
  - **retry**: `FG-GIT-0001` unavailable, `FG-GIT-0002` timeout, `FG-GIT-0004`
    busy, `FG-REPO-0003` storage I/O (a directory that vanished during
    adoption included), and `FG-GIT-0003` — which reaches the caller only from
    *creating* (the temp dir is removed, nothing is left at the final path);
  - **do not retry** (operator needed): `FG-REPO-0004` (every `adopt_refused`,
    including git failing or printing too much while reading or writing an
    existing directory, and a permission-denied `config`), `FG-REPO-0001`,
    `FG-REPO-0002`, `FG-SYS-0002` invalid hooks path, `FG-GIT-0006`.

**Error codes owned**

| Code | When | Runbook |
|---|---|---|
| FG-GIT-0001 GitUnavailable | binary missing / unparseable / older than 2.38 | `git-unavailable` |
| FG-GIT-0002 GitTimeout | total timeout exceeded (child killed and reaped); streams also `kind=idle` | `smart-http` (streams) |
| FG-GIT-0003 GitProcessFailed | unexpected non-zero exit, or reading / waiting failed | `git-process-failed` |
| FG-GIT-0004 GitBusy | no free subprocess slot within `acquire_timeout`; streams `reason=streams` / `principal_streams` | `smart-http` (streams) |
| FG-GIT-0006 GitOutputTooLarge | captured stdout over `max_output_bytes` | — |
| FG-GIT-0013 GitRequestTooLarge | gzip body over its decompressed cap (MR3/MR4: body and command limits) | `smart-http` |
| FG-GIT-0014 GitProtocolInvalid | bad pkt-line / gzip (MR3/MR4: content_type, too_many_commands, report_status_required, push_cert) | `smart-http` |
| FG-GIT-0015 GitClientDisconnected | stream aborted or dropped (client went away); tree killed | `smart-http` |
| FG-REPO-0001 StoragePathRejected | root missing / not a dir / symlink; symlink, non-dir or outside-root entry | `repo-storage` |
| FG-REPO-0002 StorageMissing | repository directory missing | `repo-storage` |
| FG-REPO-0003 StorageIoFailed | permission / disk / I/O failure on the storage root | `repo-storage` |
| FG-REPO-0004 RepoPathOccupied | creating a repository where something already exists | `repo-storage` |

`FG-GIT-0005 InvalidRefName` is raised by `forge_core::RefName` and
`Revision::object_id`.

**Known gaps (tracked)**
- Captured runs (`output()`) still kill only the direct child on timeout
  (short commands without long-lived helpers); streams kill the whole tree.
- **protocol v2 serves unreferenced objects** (any commit / blob whose id is
  known), and `uploadpack.allowAnySHA1InWant=false` does not stop it; v0
  serves unreferenced blobs. Pinned by
  `v2_fetch_serves_unreferenced_objects_despite_allow_any_sha1_false`; the
  decision (check wants in forge / v0 only / accept) is MR3's (ADR-0007
  amendment).
- Windows: the process is assigned to its Job Object right after spawn (a
  helper started before that would escape; git starts helpers only after
  reading the request). The Windows path is compile- and clippy-checked
  (`--target x86_64-pc-windows-msvc`) but its tests have not run on Windows yet.
- On Unix a helper still running after the leader exited **successfully** is
  not killed. After a failed exit (non-zero / signal) the group is killed
  while the leader is still a zombie (`waitid(WNOWAIT)`); this needs
  `waitid` (Linux, macOS — not OpenBSD).
- File size: `command.rs` ~490 lines and `stream.rs` ~680 (target 400) — split
  the env/argv builder and the supervisor out when MR3/MR4 touch them.
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
