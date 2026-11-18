# Runbook: job-queue-backlog（FG-DB-0017 / FG-DB-0024）

## Trigger

- `forge doctor` の `jobs.queue_depth` が `FG-DB-0017` JobQueueBacklog（alert）で失敗した:
  - `reason=queue_wait`: ある種類の最も古い実行可能なジョブが 10 分より長く待っている
  - `reason=queue_depth`: ある種類の実行可能なジョブが 1000 件を超えた
- `forge doctor` の `jobs.expired_leases` が `FG-DB-0017` `reason=reaper_behind` で失敗した: リースの切れたジョブが 60 秒以上 `running` のまま（回収は 30 秒ごと）
- ログに `error_code=FG-DB-0024` WakeupMissed（warn）が繰り返し出る: 通知なしで、保険の確認（`jobs.safety_poll_ms`、既定 10 秒）が仕事を見つけた
  （下の「起床の通知の取りこぼし」）

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
   - 起動時の `forge.worker.jobs` の `job_slots`（同時に動かすジョブの数、`review.worker_job_concurrency`。既定 4、書き込みのプールの半分まで）。
     種類ごとにも上限がある（`merge_request.mergeability` 3、`merge_request.refresh`・`ci.pipeline_create`・`check.*` 2、`merge_request.conflict_scan` と
     その他 1）。1 つの種類が上限まで長く動いていると、その種類の待ちだけが伸びる
   - 取り出す順は優先の段の順（マージキューの列車のパイプライン・`repository.provision` → `merge_request.refresh`・列にいる MR の mergeability →
     その他 → target の前進でまとめて積まれた mergeability → `merge_request.conflict_scan`。5 分以上待ったジョブは「その他」の段に上がる）。
     `merge_request.conflict_scan` や `merge_request.mergeability` の `queue_wait` は、上の段が混んでいる間は起きうる（負荷試験 2026-11-17 F2）
   - mergeability と conflict_scan は MR ごとに待ちが 1 件まで（新しい依頼が古い待ちのジョブを消す）。その種類の `ready` が開いている MR の数を
     大きく超えるなら、この仕組みの不具合として開発者に渡す
   - 配備のときの注意: マイグレーション 20261226100000 は `forge.jobs` に索引 `jobs_queued_merge_request` を普通の `CREATE INDEX` で作る。
     作っている間、ジョブの投入と取り出し（書き込み）は待たされる（読み取りは続く）。`forge.jobs` の行が多い（数十万行以上）ときは、
     先に `forge worker` を止めてから移行する `[MUTATING — needs approval]`
4. 種類ごとの待ち行列を読む（読み取りのみ）:
   `SELECT kind, count(*), min(run_after) FROM forge.jobs WHERE state = 'queued' AND run_after <= now() GROUP BY kind;`
   実行中のジョブ: `SELECT id, kind, worker_id, attempts, lease_until FROM forge.jobs WHERE state = 'running' ORDER BY lease_until;`
5. `queue_depth`: その `kind` のジョブを 1 件ずつ投入している呼び出し元を探す（`origin_request_id` でログをたどる）。
   **このキューは内部の仕事用で、エッジ端末 1 台ごとにジョブを作ってはいけない**（plan 01 §3.10）。艦隊全体の仕事は、対象を自分で順にたどる 1 件のジョブにする。

### 起床の通知の取りこぼし（FG-DB-0024 WakeupMissed、plan 18 S2）

ジョブを実行可能にした文（投入、返却、失敗や回収の後の再試行の予約）は、同じトランザクションで `NOTIFY forge_jobs, <種類>` を送る。
`forge serve` と `forge worker` はプロセスごとに 1 本の LISTEN の接続（wake hub）でこれを受け、待っているジョブの実行役とランナーの
ロングポーリングを起こす。通知は合図にすぎない: 起きた側は必ず DB を読み直し、通知がなくても `jobs.safety_poll_ms`（既定 10 秒）ごとに
自分で確かめる。だから取りこぼしても仕事は失われず、最長で保険の周期 1 回ぶん遅れるだけ。FG-DB-0024 はその遅れが起きた印。

1. ログの `context` を読む: `channel`（`forge_jobs`）、`kind`、`job_id`、`worker_id`（ランナーのロングポーリングなら `kind=ci.job` と `runner_id`）、
   `oldest_ready_ms`（見つけた仕事が実行可能になってからの時間）、`safety_poll_ms`。1 分に 1 回までしか出ないので、1 件が複数を表すことがある。
2. 同じ時間帯に LISTEN の接続の失敗があるか: `error_code=FG-DB-0001` / `FG-DB-0002` で `operation=wake_listen`（`query=wake.listen*`）。
   接続が切れている間、実行役は 1 秒ごとの確認に戻る（plan 18 より前と同じ速さ）。つながり直すと `forge.wake.listening`
   （`backend_pid`、`recovered=true`）が出て、待っている側はすぐに 1 回確かめる。失敗が続くなら `db-unavailable.md`。
3. `forge doctor` の `db.notify_queue` を見る。`FG-DB-0023` なら `db-unavailable.md` の「notify queue」。
4. どれでもなく、同じ `kind` で繰り返すなら、通知を送らずにその種類を実行可能にしている経路がある（手で書き換えた行、plan 18 S2 より
   前の版が並んで動いている、通知のない新しい書き込み）。1 回きり（手作業の後、配備の入れ替えの間）なら何もしなくてよい。

対処:

- 繰り返す場合は開発者（Claude のセッション）に渡す（`kind`、時刻、手順 2・3 の結果）。
- 直るまで通知に頼らずに動かすには、`jobs.safety_poll_ms = 1000`（環境変数 `FORGE_JOBS_SAFETY_POLL_MS`。plan 18 より前の確認の周期）にして
  `forge worker` と `forge serve` を再起動する。`[MUTATING — needs approval]`（ロールバック: 設定を元の値に戻して再起動する。ジョブは失われない）

## Mitigate

1. `forge worker` が止まっている場合: 起動する（`systemctl start forge-worker`）。`[MUTATING — needs approval]`（ロールバック: `systemctl stop forge-worker`。止めても実行中のジョブは猶予の後に返される）
   起動直後に回収が 1 回走り、`reaper_behind` の行は `queued`（または試行回数を使い切っていれば `dead` → `job-dead-lettered.md`）になる。
2. その種類のハンドラーを持たない版が動いている場合: ハンドラーを持つ版を配備する。`[MUTATING — needs approval]`（ロールバック: 前の版に戻す。ジョブは待ち続けるだけ）
3. `queue_depth`: 1 件ずつ投入している呼び出し元を直す（開発者に渡す）。たまった行を手で消さない。
4. **してはいけないこと**: `forge.jobs` の `state` / `attempts` / `lease_until` / `run_after` を手で書き換えない。

## Verify

- `forge doctor` の `jobs.queue_depth` と `jobs.expired_leases` が `ok`、`jobs.queue_depth` の詳細でその種類の件数が減り続けている。
- worker のログに `task=job_runner` / `task=job_reaper` の新しい失敗がない。
- FG-DB-0024 のとき: 新しい `FG-DB-0024` が出ていない。`forge.wake.listening` の後に `operation=wake_listen` の失敗が続いていない。

## Escalate

- 30 分で直らない、または `queue_depth` のときは、開発者（Claude のセッション）に `forge doctor --json` の出力、手順 4 の結果、worker のログを渡す。
