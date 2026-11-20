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
     通常の停止では実行中のジョブは `released_on_shutdown` で返され、試行回数を使わない。停止が worker を待つのは最大でおよそ 20 秒
     （猶予 10 秒 + 中断の待ち 5 秒 + 返却 5 秒）。それより早く強制終了（`systemctl kill`、`TimeoutStopSec` が短い）すると `lease_expired` になる
   - `FG-SYS-0001`: 実行器の側の失敗。`last_error` の `reason` を見る
     - `job_panicked`: ハンドラーの不具合（panic）→ `internal-error.md`
     - `job_cancelled`: ハンドラーのタスクが実行器の外で取り消された（実行時の停止など）
     - `no_handler`: 取ったジョブの種類のハンドラーがこの版にない。実行器は自分の種類しか取らないので通常は起きない。
       同じ種類がほかの worker で待っていれば `FG-DB-0017 reason=queue_wait` も出る → `job-queue-backlog.md`
   - `FG-DB-0007` `reason=left_leased`: 中断したハンドラーが 5 秒で終わらず、リースを残した（`abort_pending` が先に出る）。リース切れの後に回収された
   - `reason=permanent_failure`: ハンドラーが「やり直しても成功しない」と判断した（対象が消えたなど）。原因の行の `reason` を読む
4. `kind = merge_request.prepare`（計画 18 S6、ADR-0037 (f)）: 版とその検査を 1 つのジョブで行う。原因の行の `step` が失敗した小段:
   - `step=refresh`: 版の記録（ソース枝の先端の読み、版の行、隠し ref）。コードは `merge_request.refresh` と同じ
     （`FG-GIT-0002/0003/0004`、`FG-REPO-0016`、`FG-DB-0001/0002`）。版が記録されていないので、MR の検査も始まっていない。
   - `step=mergeability` / `step=checks`: 判定の後始末（待たせていた判定のジョブの削除・起床）か、検査の引き継ぎの投入が
     DB で失敗した。版は記録済み。再試行で `forge.review.prepared` の `risk` / `spec` が `skipped` なら、その head の結果はもうある。
   - 小段（`mergeability`、`risk`、`spec`）そのものの失敗では prepare は失敗しない: worker のログに
     `forge.review.prepare_step_failed`（`step`、その小段の `error_code`、`job_kind=merge_request.prepare`）が 1 回出て、その小段は
     自分のジョブ（`merge_request.mergeability`、`check.risk`、`check.spec_index`）で再試行される。そのジョブが `dead` になったら
     その種類として読む（最後の試行で `error` の結論を書くのは今までどおり）。
   - prepare が `dead` になった（版はあるが検査がない）とき: 待たせていた判定のジョブ（`merge_request.mergeability`、
     `run_after` が未来）は `BACKSTOP_DELAY`（120 s）の後に自分で走る。`forge/risk` と spec の検査は、その MR に新しい push か
     MR の再オープンで新しい prepare が作られるまで出ない。

## Mitigate

1. 原因（`last_error_code` の Runbook）を直す。直すまで同じ仕事を再投入しない。
2. 仕事をやり直す: その種類を持つコマンドで同じ仕事を投入し直す。`dead` のジョブは `dedup_key` を持たないので（migration `20261027120000`）、
   同じキーで投入すれば**新しいジョブ**になる（同じキーの `queued` / `running` / `succeeded` のジョブがあれば二重にはならない）。
   種類ごとのコマンドは、ハンドラーが追加されたときにここへ追記する。
   - `repository.provision`（D4 MR3b）: `forge admin repo reprovision <repo_id>`（`repo_id` は payload の `repo_id`）。
     原因の調べ方と前提は Runbook `repository-provisioning.md`。
   - `merge_request.prepare`（計画 18 S6）: 投入し直すコマンドはない。MR のソース枝への新しい push（push の
     イベントごとに新しいキー `mr_prepare:<id>:push:<seq>`）か、MR を閉じて開き直すと新しい prepare が作られる。
   `[MUTATING — needs approval]`（ロールバック: なし。新しいジョブが `succeeded` になるまで見届ける）
3. **してはいけないこと**: `forge.jobs` の `state` / `attempts` / `lease_until` を手で書き換えない。`dead` の行を `queued` に戻さない（手順 2 で新しいジョブを作る）。
   リースの切れていないジョブを `queued` に戻すと、二つの worker が同じジョブを同時に実行する。

## Verify

- 手順 2 の新しいジョブが `succeeded` になった（`SELECT id, state, attempts, last_error_code FROM forge.jobs WHERE kind = '<kind>' AND dedup_key = '<key>' ORDER BY id DESC LIMIT 2;`）。
- worker のログに同じ `kind` の新しい `FG-DB-0006` がない。`jobs.dead` は元の行が 24 時間を過ぎるまで失敗のままなので、`dead_recent` が増えていないことを見る。

## Escalate

- 同じ `kind` が繰り返し `dead` になる、`FG-SYS-0001` が原因、または 1 時間で直らないときは、開発者（Claude のセッション）に
  `forge doctor --json` の出力、`job_id`、`origin_request_id`、worker のログを渡す。
