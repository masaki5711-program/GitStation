# Runbook: merge-queue（FG-CI-0019。関連: FG-CI-0018、FG-CI-0020、FG-CI-0005、FG-CI-0002、FG-REVIEW-0009）

M4 C8（ADR-0030）のマージキュー（`forge serve` のタスク `merge_queue`、表 `forge.merge_queues` / `forge.merge_queue_batches` /
`forge.merge_queue_entries`、隠した ref `refs/merge-queue/<queue_id>/train`、パイプラインの `source = merge_queue`）が
進まない・止まった・期待と違うとき。

## Trigger

- `forge serve` のログに `FG-CI-0019` MergeQueueStalled（error、`forge.merge_queue.batch_stalled` / `operation=merge_queue_reap`）:
  バッチが期限（既定 2 時間）までに試し終わらず、捨てて MR を列に戻した。3 回目の MR は列から外れた（`forge.merge_queue.entry_removed`、
  `reason=stalled`）。
- `forge doctor` の `review.merge_queue` が FG-CI-0019 で失敗: 期限を過ぎた生きたバッチがある（`batch_ids`）、または貸し出しが
  10 分以上前に切れたままの列がある（`reason=worker_not_running`、`queue_ids`）。
- 利用者から「キューに入れたのにマージされない」「キューから外れた（FG-CI-0018 / FG-CI-0020）」と問い合わせがあった。
- `forge serve` のログに `FG-SYS-0005` ServeFailed（`operation=merge_queue_tick`、`reason=merge_queue_tick_timeout` など）が
  1 分に 1 回まで続く。

## Impact

- 列の MR がマージされない（target は動かない。試していないコミットが入ることはない）。普通のマージ（`gstn mr merge`）は影響を受けない。
- 1 つの列なら SEV4。CI 全体が止まっていて多くの列が詰まるなら SEV3（`ci-runner` / `ci-pipelines` の Runbook も見る）。

## Diagnose（読み取りのみ）

1. 列の様子: `gstn queue list --project <id> --target <branch>`（止めているか、次のバッチの上限、試し中のバッチの列車と期限、並びと
   最近外れた MR の理由とコード）。MR ごとは `gstn queue status --project <id> --iid <iid>`。
2. ログを読む（`request_id` はバッチごと。`forge.merge_queue.batch_built` が最初の行）:
   `forge.merge_queue.batch_built` → `batch_failed`（二分）/ `entry_removed` / `batch_abandoned`（`reason`）/ `batch_landed`。
   - `batch_abandoned reason=target_moved`: 試している間に target が他から動いた（直接の push、普通のマージ）。次の手番で作り直すので
     何もしなくてよい。頻繁なら target への直接の push を止める（保護ブランチの `direct_push_forbidden`）。
   - `reason=entry_left`: 試し中の MR が外された・新しい版が push された・閉じられた。作り直すので何もしなくてよい。
   - `reason=entry_refused`: 着地の判定で 1 件が断られた（承認が取り消された、セッションが取り消された、権限が変わった等。
     `forge.merge_queue.entry_removed` の `error_code` と `context`）。その MR だけが外れ、残りで作り直す。
   - `reason=landing_interrupted`: worker が着地の途中で止まり、次の worker が target を見て作り直した（target は動いていない）。
3. 試し中のバッチのパイプライン:
   `SELECT id, status, error_code, created_at FROM forge.ci_pipelines WHERE source = 'merge_queue' ORDER BY id DESC LIMIT 10;`
   行が無く `SELECT id, state, last_error_code FROM forge.jobs WHERE kind = 'ci.pipeline_create' AND payload ->> 'source' = 'merge_queue' ORDER BY id DESC LIMIT 10;`
   が `dead` なら、パイプラインを作れていない（Runbook `ci-pipelines`）。`pending` のまま動かないなら Runner が取っていない
   （FG-CI-0005、Runbook `ci-runner`）。
4. 表を読む:
   `SELECT id, project_id, target_branch, paused, batch_limit, lease_owner, lease_until, next_run_at FROM forge.merge_queues ORDER BY id;`
   `SELECT id, queue_id, state, base_sha, train_sha, size, reason, error_code, deadline_at FROM forge.merge_queue_batches ORDER BY id DESC LIMIT 20;`
   `SELECT id, merge_request_id, state, position, isolate, batch_id, merge_id, reaps, reason, error_code FROM forge.merge_queue_entries WHERE queue_id = <q> ORDER BY position;`
   `lease_until` が過去のまま `next_run_at` も過去なら、worker が動いていない（`forge serve` が止まっている、または
   `forge.serve.merge_queue_started` のあと `merge_queue_tick` の失敗が続いている）。
5. 着地の途中で止まったバッチ（`state = 'landing'`）: 次の worker がリポジトリのマージのロックの下で git を見て決める。target が
   列車の先端なら残りを記録、土台のままなら試行を `superseded`（`queue_rebuild`）にして作り直す。照合（Runbook `merge-execution`）も
   630 秒後に `executing` の試行を git で決める。`FG-REVIEW-0009 ref_state_unknown` が列の試行に出たら、target を手で動かしていないか
   確かめる。
6. 1 本で走る理由（`isolate`）: `migration`（マイグレーションの SQL に触れる）、`risk_stop`（head の `forge/risk` が `failure`）、
   `fast_forward`。1 本ずつ試すので、続くと列が遅くなるのは意図どおり。

## Mitigate（変更は人の承認のあと）

- **緊急停止**: `gstn queue pause --project <id> --target <branch>`（Developer 以上）。新しいバッチを作らず、緑になっても着地しない。
  再開は `gstn queue resume`（Maintainer 以上）。止めている間も MR は列に入れられる。
- パイプラインが進まない: 元の原因（Runner、パイプラインの作成）をその Runbook の手順で直す。直ったら列は次の手番で進む。
  期限で外れた MR（`reason=stalled`）は、作者が `gstn queue add` で入れ直す。
- worker が動いていない: `forge serve` を再起動する（貸し出しは期限で切れるので、他の worker が引き継ぐ。手で `lease_owner` を
  消さない）。
- 詰まったバッチを手で捨てる SQL の操作はしない（エントリーと試行の状態がずれる）。止めて（pause）原因を直すのが先。

## Verify

- `forge doctor` の `review.merge_queue` が ok。
- `gstn queue list` でバッチが進み、`forge.merge_queue.batch_landed` が出る。target の先端が列車の先端（`train_sha`）になる。

## 関連

- ADR-0030（マージキュー）、ADR-0024（マージの実行、照合）、Runbook `merge-execution`、`ci-pipelines`、`ci-runner`。
