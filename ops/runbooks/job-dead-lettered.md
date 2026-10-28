# Runbook: job-dead-lettered（FG-DB-0006。関連: FG-DB-0007）

## Trigger

- `forge worker` のログに `FG-DB-0006` JobDeadLettered（alert）が出た（`reason` = `attempts_exhausted` / `permanent_failure` / `lease_expired`）
- `forge doctor` の `jobs.dead` が失敗した（直近 24 時間に `dead` になったジョブがある、`reason=dead_lettered`）
- ジョブがたまっている・回収されていない（`jobs.queue_depth`、`jobs.expired_leases`）のは `FG-DB-0017` → Runbook `job-queue-backlog.md`

## Impact

- `dead` のジョブの仕事（例: 封印、保持期間の削除、通知）は**行われていない**。もう一度投入されるまで再試行されない。
- 画面と git の操作は動き続ける。監査の封印のジョブが止まっている場合は ADR-0018 の封印の遅れにもつながる（その Runbook も見る）。
- 1 件なら SEV3。同じ種類で繰り返す、または封印・保持期間のジョブが 1 時間以上止まっているなら SEV2。

## Diagnose（読み取りのみ）

1. `forge doctor --json` の `jobs.dead` を読む。`context`:
   - `job_id`、`kind`、`attempts`、`last_error_code`（最後の失敗のコード。`FG-DB-0007` はリース切れ）、`origin_request_id`（ジョブを投入した要求・イベントの `request_id`）
   - `dead_recent`: 直近 24 時間の `dead` の件数（`100000+` で打ち切り）
2. ジョブの行を読む（読み取りのみ。payload は参照だけで秘密は入っていない）:
   `SELECT id, kind, state, attempts, max_attempts, dedup_key, last_error_code, last_error, origin_request_id, origin_event_seq, created_at, finished_at FROM forge.jobs WHERE id = <job_id>;`
3. worker のログを `origin_request_id` で検索する（要求 → イベント → ジョブを 1 本でたどれる）。ジョブの実行器は原因の失敗を先に
   その失敗自身の `error_code` で記録し（`context` に `job_id`、`kind`、`attempt`）、続けて `FG-DB-0006` を記録する（同じ `request_id`）。`last_error_code` の Runbook に進む:
   - `FG-DB-0001`: DB に接続できない → `db-unavailable.md`
   - `FG-DB-0007` `reason=execution_timeout`: ハンドラーがジョブの `timeout_ms` を超えた（外部の呼び出しが返らない、件数が多すぎる）。
     `reason=abort_pending` が同じ `job_id` で出ていれば、ハンドラーが await せずに止まっている（同期の待ち）
   - `FG-DB-0007` `reason=lease_expired`: worker が実行中に落ちた・強制終了された（ログの `forge.worker.stopped`、プロセスの再起動を確かめる）。
     通常の停止では実行中のジョブは `released_on_shutdown` で返され、試行回数を使わない
   - `FG-SYS-0001` `reason=job_panicked`: ハンドラーの不具合 → `internal-error.md`
   - `reason=permanent_failure`: ハンドラーが「やり直しても成功しない」と判断した（対象が消えたなど）。原因の行の `reason` を読む

## Mitigate

1. 原因（`last_error_code` の Runbook）を直す。直すまで同じ仕事を再投入しない。
2. 仕事をやり直す: その種類を持つコマンドで同じ仕事を投入し直す。`dead` のジョブは `dedup_key` を持たないので（migration `20261027120000`）、
   同じキーで投入すれば**新しいジョブ**になる（同じキーの `queued` / `running` / `succeeded` のジョブがあれば二重にはならない）。
   種類ごとのコマンドは、ハンドラーが追加されたときにここへ追記する。`[MUTATING — needs approval]`（ロールバック: なし。新しいジョブが `succeeded` になるまで見届ける）
3. **してはいけないこと**: `forge.jobs` の `state` / `attempts` / `lease_until` を手で書き換えない。`dead` の行を `queued` に戻さない（手順 2 で新しいジョブを作る）。
   リースの切れていないジョブを `queued` に戻すと、二つの worker が同じジョブを同時に実行する。

## Verify

- 手順 2 の新しいジョブが `succeeded` になった（`SELECT id, state, attempts, last_error_code FROM forge.jobs WHERE kind = '<kind>' AND dedup_key = '<key>' ORDER BY id DESC LIMIT 2;`）。
- worker のログに同じ `kind` の新しい `FG-DB-0006` がない。`jobs.dead` は元の行が 24 時間を過ぎるまで失敗のままなので、`dead_recent` が増えていないことを見る。

## Escalate

- 同じ `kind` が繰り返し `dead` になる、`FG-SYS-0001` が原因、または 1 時間で直らないときは、開発者（Claude のセッション）に
  `forge doctor --json` の出力、`job_id`、`origin_request_id`、worker のログを渡す。
