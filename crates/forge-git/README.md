# forge-git

**Responsibility**: the only place forge runs `git` (CLAUDE.md §3-5) and the
only place repository storage paths are derived (ADR-0009). It also owns the
Smart HTTP transport (ADR-0007, M1 phase 9): streaming subprocesses,
pkt-line / side-band framing, gzip decoding and the upload-pack /
receive-pack command sets, and reads / answers receive-pack requests for the push path
(M1 phase 9 MR4, ADR-0008 option B). Protected-ref hooks follow in phase 10 (M3).

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
- **Push** (M1 phase 9 MR4): `GitCommand::deny_non_fast_forwards()` (`-c
  receive.denyNonFastForwards=true`); `receive_request::CommandParser::feed(&buf) -> Parsed`
  (the command section read incrementally: ≤ 1000 commands / 1 MiB, known capabilities on the
  first line only, `report-status` required, `push-cert` refused; each `RefCommand` keeps its
  name as sent and `RefName` or its refusal — `FG-GIT-0005`, `FG-GIT-0011` via
  `check_unambiguous`); `receive_report::ReportRewriter` (forwards side-band 2 / 3, holds the
  report ≤ 4 MiB, rewrites `ng <protected ref> non-fast-forward`, returns the parsed `Report`;
  unreadable answers are forwarded unchanged) and `refusal_report(sideband, messages, refs)`;
  `Git::fast_forwards(&StorageRoot, RepoId, &[(old, new)], timeout) -> Vec<Ancestry>`
  (`cat-file --batch-check`, then `rev-list --count --max-count=1 --stdin` with `old\n^new`) and
  `Git::new_commits(…, &[CommitQuery], principal, timeout) -> Vec<CommitList>` (≤ 20 listed,
  all counted, streamed; create: `--not --exclude=<pushed refs> --glob=refs/*` + old ids — not
  `--all`, whose HEAD no `--exclude` removes; one deadline, past it `partial`). Ids on stdin only.
- **Upload-pack requests** (`upload_request`, M1 phase 9 MR3):
  `parse_upload_pack_request(body, ProtocolVersion) -> UploadPackRequest { command, wants }`
  (v0 / v1 negotiation; v2 `ls-refs` / `fetch` only — any other v2 command or a second command
  is `FG-GIT-0014 reason=command_unsupported`; a want that is not 40 / 64 lower-case hex is
  `want_invalid`); `err_packet(message)` (one `ERR` pkt-line, cut to fit).
- **Want reachability** (`Git::check_wants(&StorageRoot, RepoId, &wants, principal, timeout) ->
  WantCheck`, ADR-0007 option (a)): `cat-file --batch-check` (missing = unreachable), commits by
  `rev-list --max-count=1 --stdin --not --all` (empty = reachable), trees / blobs / tags by a
  streamed `rev-list --objects --all --no-object-names` walk; ids only on stdin; one deadline
  (`FG-GIT-0002 operation=want_check`). `WantCheck::into_result()` → `FG-GIT-0014
  reason=want_unreachable` (`wants`, `commits`, `objects`, `unreachable`, `first_unreachable`,
  `check_ms`). Cost measured by the ignored `want_check_cost_with_many_refs` (ADR-0007 amendment MR3).
- **pkt-line** (`pkt_line`): `decode(buf, PktMode) -> Option<(Packet, used)>`, `decode_all`,
  `encode` / `encode_data`, `PktReader` (async, one packet ≤ 65520 bytes), `encode_sideband` /
  `decode_sideband` (`Band::{Data, Progress, Error}`, ≤ 65515 data bytes per packet). Invalid
  framing → FG-GIT-0014 `reason=pkt_line` (`detail`).
- **Browse** (`browse`, M2 MR7, ADR-0015 "ref とパスの境目"): `Browser::new(Git, StorageRoot,
  BrowseLimits)` (cheap clone; forge-web keeps one in `SmartHttp::browse`).
  - `refs(repo) -> Arc<RefList>` (`for-each-ref` of `refs/heads/` + `refs/tags/` only, NUL-separated
    fields, tags peeled one level, `default_branch` from `%(HEAD)`, ≤ `max_refs` 5 000 else
    `truncated`); cached per repository for `refs_ttl` (5 s), at most `max_cached_repos` (256);
    `invalidate(repo)` (every push) drops it, and a list loaded across an invalidation is not stored.
  - `resolve_ref(repo, "<oid | refs/heads/… | refs/tags/… | short>")` and
    `resolve_spec(repo, &segments) -> ResolvedSpec { rev, path }` in ADR-0015's order (40 / 64-hex
    commit id of any case first, then the longest branch, then the longest tag; candidates are
    `RefName`-valid prefixes only, so `-n`, `--output=x` never match). A truncated ref list falls back
    to `for-each-ref --end-of-options <exact candidates>`. Unknown → `FG-REPO-0023`
    (`ref_not_found`, `not_a_commit`).
  - `tree(repo, commit, &RepoPath)` (`ls-tree -z --long --full-tree --end-of-options <commit> --
    <path>` for the entry, then `ls-tree -z --long <tree id>`; ≤ `max_tree_entries` 5 000),
    `blob(…)` (`cat-file blob <blob id>`, ≤ `max_blob_bytes` 1 MiB), `raw_entry(…)`, `raw(…,
    principal)` (≤ `raw_inline_max_bytes` 4 MiB read whole, larger a `GitStream`),
    `archive(repo, commit, ArchiveFormat, prefix, principal) -> GitStream` (`-c
    tar.tar.gz.command=git archive gzip`: gzip inside git, never a configured filter; `--prefix
    <forge-made>/`). `FG-REPO-0023` `path_not_found`, `not_a_tree`, `not_a_blob`.
  - One deadline per operation (`timeout`, 10 s; `FG-GIT-0002 operation=browse`); streams idle 60 s
    / total 10 min (`stream_idle`, `stream_total`); at most `max_per_repo` (8) captured calls per
    repository at once (`FG-GIT-0004 reason=repo_browse`, refused at once; streams are bounded by the
    stream pools instead). Caps are `GitCommand::output_truncated(cap)` (crate-internal): git is killed
    and reaped at the cap, `truncated = true`, never an error.
  - `RepoPath::parse` / `from_segments`: no empty, `.`, `..` or control segment, ≤ 4096 bytes.
  - History (M2 MR8, `browse/{history,diff,blame}.rs`; the module is split into `browse/{path,refs,tree,
    content,history,diff,blame}.rs`):
    - `commits(repo, tip, &RepoPath, limit, Option<&LogCursor>) -> CommitPage` — `log -z --format=…
      -n N [--skip K] --end-of-options <tip> [-- <path>]`, fields split at `0x1f`, ≤ `max_log_page` 100.
      `LogCursor { tip, last, skip }` (`v1.<tip>.<last>.<skip>`, `parse` is strict): the next page
      continues from the cursor's own tip and reads the anchor again (the first record must be `last`,
      else `FG-WEB-0006 reason=cursor_invalid rule=anchor`: a cursor of another path is refused, never
      guessed). A cursor's tip is checked to be a commit first.
    - `commit(repo, oid) -> CommitDetail` (message + `Diff` against the first parent; a root commit
      against nothing via `--root`), `compare(repo, from, to) -> Comparison` (`log to ^from` ≤
      `max_compare_commits` 200 + the two-tree diff; `compare_timeout` 20 s).
    - `Diff` = two `diff-tree -r -M` runs: `--raw -z --no-abbrev` (the exact file list, ≤
      `max_diff_files` 500) and `-p` (≤ `max_diff_bytes` 2 MiB, git killed there), the patch split at
      its `diff --git ` lines and matched to the files in order (a type change is two chunks). A
      mismatch is `FG-GIT-0003 reason=diff_chunk_mismatch`, never a guess. A patch is whole or absent:
      `PatchOmitted::{TooLarge (> `max_file_patch_bytes` 256 KiB), TotalCap, NotUtf8}`; a git-binary
      change is `binary` without a patch. External diff drivers and textconv are off.
    - `blame(repo, commit, &RepoPath) -> Blame` — `blame --porcelain -L 1,<cap+1> <commit> -- <path>`
      (git's blame rejects `--end-of-options`; the operand is a validated full hex id), hunks of
      consecutive lines per commit, ≤ `max_blame_lines` 10 000 else `truncated`, a file with NUL or a
      non-UTF-8 line is `binary` (no hunks); `blame_timeout` 20 s.
    - Commit text (names, subjects, messages) is decoded as UTF-8 with replacement characters (display
      only); the ids and the cursor position never are.
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
| FG-GIT-0013 GitRequestTooLarge | gzip body over its decompressed cap; MR4 `limit=command_section` (here), `push_body` (forge-web) | `smart-http` |
| FG-GIT-0011 AmbiguousRefName | a pushed branch / tag name of 40 / 64 hex digits (`receive_request`) | — |
| FG-GIT-0014 GitProtocolInvalid | bad pkt-line / gzip; `want_invalid`, `command_unsupported`, `want_unreachable` (MR3, here); `content_type`, `content_encoding` (forge-web); MR4 (`receive_request`): `too_many_commands`, `report_status_required`, `push_cert`, `command_invalid`, `capability_unsupported`, `duplicate_ref`, `shallow_misplaced` | `smart-http` |
| FG-GIT-0015 GitClientDisconnected | stream aborted or dropped (client went away); tree killed | `smart-http` |
| FG-REPO-0001 StoragePathRejected | root missing / not a dir / symlink; symlink, non-dir or outside-root entry | `repo-storage` |
| FG-REPO-0002 StorageMissing | repository directory missing | `repo-storage` |
| FG-REPO-0003 StorageIoFailed | permission / disk / I/O failure on the storage root | `repo-storage` |
| FG-REPO-0004 RepoPathOccupied | creating a repository where something already exists | `repo-storage` |
| FG-REPO-0023 RepositoryPathNotResolved | browse (M2 MR7): a ref, commit id or path the repository does not have (`reason`) | — |

`FG-GIT-0005 InvalidRefName` is raised by `forge_core::RefName` and
`Revision::object_id`.

**Known gaps (tracked)**
- Captured runs (`output()`) still kill only the direct child on timeout
  (short commands without long-lived helpers); streams kill the whole tree.
- **protocol v2 serves unreferenced objects** (any commit / blob whose id is
  known), and `uploadpack.allowAnySHA1InWant=false` does not stop it; v0
  serves unreferenced blobs. Pinned by
  `v2_fetch_serves_unreferenced_objects_despite_allow_any_sha1_false`. Closed
  in forge by `check_wants` before upload-pack starts (MR3, option (a)); the
  object walk for non-commit wants costs a full `rev-list --objects --all`
  (bitmaps / a cache are open).
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
  `browse/` (split in M2 MR8) is ~2 700 lines with its tests; `browse/refs.rs` is the largest module.
- Browse: tree entry names that are not UTF-8 are shown lossily (`�`) and cannot be requested back;
  raw files over 4 MiB are always downloads (no image sniff on a stream).
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
