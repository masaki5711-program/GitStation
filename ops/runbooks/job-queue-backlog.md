# Runbook: job-queue-backlog（FG-DB-0017）

## Trigger

- `forge doctor` の `jobs.queue_depth` が `FG-DB-0017` JobQueueBacklog（alert）で失敗した:
  - `reason=queue_wait`: ある種類の最も古い実行可能なジョブが 10 分より長く待っている
  - `reason=queue_depth`: ある種類の実行可能なジョブが 1000 件を超えた
- `forge doctor` の `jobs.expired_leases` が `FG-DB-0017` `reason=reaper_behind` で失敗した: リースの切れたジョブが 60 秒以上 `running` のまま（回収は 30 秒ごと）

## Impact

- その種類の仕事（例: 封印、保持期間の削除、通知）が進んでいない。ジョブは失われていない（行は残っていて、worker が動けば処理される）。
- `reaper_behind`: 落ちた worker のジョブが回収されず、再実行されない。
- 画面と git の操作は動き続ける。封印・保持期間のジョブが 1 時間以上止まっているなら SEV2、それ以外は SEV3。

## Diagnose（読み取りのみ）

1. `forge doctor --json` を読む。`context`:
   - `queue_wait` / `queue_depth`: `kind`、`ready`（その種類の実行可能な件数。`100000+` で打ち切り）、`oldest_wait_ms`
   - `reaper_behind`: `expired_leases`（件数）、`job_id`、`kind`、`worker_id`（リースを持っていた worker: `w<pid>-<乱数>`）、`attempt`、`expired_ms`
2. `forge worker` が動いているか: `systemctl status forge-worker`（unit がなければ `ps -ef | grep 'forge worker'`）。
3. worker のログを読む:
   - 起動時の `forge.worker.jobs`: `job_kinds`（登録されたハンドラーの数）と `worker_id`。その種類のハンドラーを持たない版が動いていると `queue_wait` になる
   - `task=job_runner` / `task=job_reaper` の失敗（`FG-DB-0001` なら `db-unavailable.md`）
   - 1 件のジョブが長く動いていないか（1 つの worker は 1 度に 1 件だけ実行する: 長いジョブが他の種類を待たせる）
4. 種類ごとの待ち行列を読む（読み取りのみ）:
   `SELECT kind, count(*), min(run_after) FROM forge.jobs WHERE state = 'queued' AND run_after <= now() GROUP BY kind;`
   実行中のジョブ: `SELECT id, kind, worker_id, attempts, lease_until FROM forge.jobs WHERE state = 'running' ORDER BY lease_until;`
5. `queue_depth`: その `kind` のジョブを 1 件ずつ投入している呼び出し元を探す（`origin_request_id` でログをたどる）。
   **このキューは内部の仕事用で、エッジ端末 1 台ごとにジョブを作ってはいけない**（plan 01 §3.10）。艦隊全体の仕事は、対象を自分で順にたどる 1 件のジョブにする。

## Mitigate

1. `forge worker` が止まっている場合: 起動する（`systemctl start forge-worker`）。`[MUTATING — needs approval]`（ロールバック: `systemctl stop forge-worker`。止めても実行中のジョブは猶予の後に返される）
   起動直後に回収が 1 回走り、`reaper_behind` の行は `queued`（または試行回数を使い切っていれば `dead` → `job-dead-lettered.md`）になる。
2. その種類のハンドラーを持たない版が動いている場合: ハンドラーを持つ版を配備する。`[MUTATING — needs approval]`（ロールバック: 前の版に戻す。ジョブは待ち続けるだけ）
3. `queue_depth`: 1 件ずつ投入している呼び出し元を直す（開発者に渡す）。たまった行を手で消さない。
4. **してはいけないこと**: `forge.jobs` の `state` / `attempts` / `lease_until` / `run_after` を手で書き換えない。

## Verify

- `forge doctor` の `jobs.queue_depth` と `jobs.expired_leases` が `ok`、`jobs.queue_depth` の詳細でその種類の件数が減り続けている。
- worker のログに `task=job_runner` / `task=job_reaper` の新しい失敗がない。

## Escalate

- 30 分で直らない、または `queue_depth` のときは、開発者（Claude のセッション）に `forge doctor --json` の出力、手順 4 の結果、worker のログを渡す。
