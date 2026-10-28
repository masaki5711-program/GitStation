# Runbook: job-dead-lettered（FG-DB-0006。関連: FG-DB-0007）

## Trigger

- `forge worker` のログに `FG-DB-0006` JobDeadLettered（alert）が出た（`reason` = `attempts_exhausted` / `permanent_failure` / `lease_expired`）
- `forge doctor` の `jobs.dead`（直近 24 時間に `dead` になったジョブがある、`reason=dead_lettered`）または
  `jobs.queue_depth`（`reason=queue_depth`: ある種類の実行可能なジョブが 1000 件超、`reason=queue_wait`: 最も古いものが 10 分超待っている）が失敗した
- 関連: `jobs.expired_leases` が `FG-DB-0007` `reason=reaper_behind`（warn）で失敗した — リースの切れたジョブを 60 秒以上だれも回収していない

## Impact

- `dead` のジョブの仕事（例: 封印、保持期間の削除、通知）は**行われていない**。もう一度投入されるまで再試行されない。
- `queue_wait` / `reaper_behind` は `forge worker` が動いていないか、その種類のハンドラーを持たない版が動いている。種類ごとの仕事が止まっている。
- 画面と git の操作は動き続ける。監査の封印のジョブが止まっている場合は ADR-0018 の封印の遅れにもつながる（その Runbook も見る）。
- 1 件なら SEV3。同じ種類で繰り返す、または封印・保持期間のジョブが 1 時間以上止まっているなら SEV2。

## Diagnose（読み取りのみ）

1. `forge doctor --json` の `jobs.dead`、`jobs.expired_leases`、`jobs.queue_depth` を読む。`context`:
   - `job_id`、`kind`、`attempts`、`last_error_code`（最後の失敗のコード。`FG-DB-0007` はリース切れ）、`origin_request_id`（ジョブを投入した要求・イベントの `request_id`）
   - `dead_recent`: 直近 24 時間の `dead` の件数（`100000+` で打ち切り）
   - `queue_depth` / `queue_wait`: `kind`、`ready`、`oldest_wait_ms`
   - `reaper_behind`: `expired_leases`、`worker_id`（リースを持っていた worker: `w<pid>-<乱数>`）、`expired_ms`
2. ジョブの行を読む（読み取りのみ。payload は参照だけで秘密は入っていない）:
   `SELECT id, kind, state, attempts, max_attempts, last_error_code, last_error, origin_request_id, origin_event_seq, created_at, finished_at FROM forge.jobs WHERE id = <job_id>;`
3. worker のログを `origin_request_id` で検索する（要求 → イベント → ジョブを 1 本でたどれる）。ジョブの実行器は `FG-DB-0006` の直前に、原因の失敗を
   その失敗自身の `error_code` で記録している（`context` に `job_id`、`kind`、`attempt`）。`last_error_code` の Runbook に進む:
   - `FG-DB-0001`: DB に接続できない → `db-unavailable.md`
   - `FG-DB-0007` `reason=execution_timeout`: ハンドラーがジョブの `timeout_ms` を超えた（外部の呼び出しが返らない、件数が多すぎる）
   - `FG-DB-0007` `reason=lease_expired`: worker が実行中に止まった・落ちた（ログの `forge.worker.stopped`、`forge.worker.job_abandoned`、プロセスの再起動を確かめる）
   - `FG-SYS-0001` `reason=job_panicked`: ハンドラーの不具合 → `internal-error.md`
4. `queue_wait` / `reaper_behind`: `forge worker` が動いているか（`systemctl status forge-worker`）、起動時のログ `forge.worker.jobs` の `job_kinds`（登録されたハンドラーの数）を確かめる。
5. `queue_depth`: その `kind` のジョブを 1 件ずつ投入している呼び出し元を探す。**このキューは内部の仕事用で、エッジ端末 1 台ごとにジョブを作ってはいけない**（plan 01 §3.10）。

## Mitigate

1. 原因（`last_error_code` の Runbook）を直す。直すまで同じジョブを再投入しない。
2. 仕事をやり直す: その種類を持つコマンドでジョブを投入し直す（種類ごとの手順は、ハンドラーが追加されたときにここへ追記する）。`[MUTATING — needs approval]`（ロールバック: 投入したジョブは `dedup_key` があれば二重にならない）
3. `forge worker` が止まっている場合（`queue_wait`、`reaper_behind`）: 起動する（`systemctl start forge-worker`）。`[MUTATING — needs approval]`（ロールバック: `systemctl stop forge-worker`。止めても実行中のジョブはリース切れの後に回収されて再実行される）
4. **してはいけないこと**: `forge.jobs` の `state` / `attempts` / `lease_until` を手で書き換えない。リースの切れていないジョブを `queued` に戻すと、二つの worker が同じジョブを同時に実行する。

## Verify

- `forge doctor` の `jobs.dead` が `ok`（24 時間たつまで同じ行が残る場合は、再投入したジョブが `succeeded` になったことを手順 2 の SQL で確かめる）。
- `jobs.queue_depth` と `jobs.expired_leases` が `ok`。worker のログに新しい `FG-DB-0006` がない。

## Escalate

- 同じ `kind` が繰り返し `dead` になる、`FG-SYS-0001` が原因、または 1 時間で直らないときは、開発者（Claude のセッション）に
  `forge doctor --json` の出力、`job_id`、`origin_request_id`、worker のログを渡す。
