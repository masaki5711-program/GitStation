# Runbook: merge-queue（FG-CI-0024、FG-CI-0032。関連: FG-CI-0023、FG-CI-0025、FG-CI-0005、FG-CI-0002、FG-DOC-0002、FG-REVIEW-0009）

M4 C8（ADR-0030）のマージキュー（`forge serve` のタスク `merge_queue`、表 `forge.merge_queues` / `forge.merge_queue_batches` /
`forge.merge_queue_entries`、隠した ref `refs/merge-queue/<queue_id>/train`、パイプラインの `source = merge_queue`）が
進まない・止まった・期待と違うとき。

## Trigger

- `forge serve` のログに `FG-CI-0024` MergeQueueStalled（error、`forge.merge_queue.batch_stalled` / `operation=merge_queue_reap`）:
  バッチが期限（既定 2 時間）までに試し終わらず、捨てて MR を列に戻した。3 回目の MR は列から外れた（`forge.merge_queue.entry_removed`、
  `reason=stalled`）。
- `forge doctor` の `review.merge_queue` が FG-CI-0024 で失敗: 期限を過ぎた生きたバッチがある（`batch_ids`）、または貸し出しが
  10 分以上前に切れたままの列がある（`reason=worker_not_running`、`queue_ids`）。
- 利用者から「キューに入れたのにマージされない」「キューから外れた（FG-CI-0023 / FG-CI-0025 / FG-CI-0032）」と問い合わせがあった。
- `forge serve` のログに `FG-CI-0032` MergeQueueDocLintFailed（warn、`forge.merge_queue.entry_removed`、`reason=doc_lint_failed` か
  `doc_lint_base_not_clean`）: 列車の doc-lint（計画 18 S1、ADR-0037 (b)）が MR をキューから外した。**障害ではなく、判定の結果**
  （下の「列車の doc-lint」）。
- `forge serve` のログに `FG-DOC-0002`（warn、`forge.merge_queue.train_lint_unavailable`、`scope=train`）: 列車を lint できなかった
  （時間切れ `reason=train_budget`、読めなかった `reason=read_failed`）。バッチは捨てられ（`batch_abandoned` ではなくこの行が出る。
  バッチの `reason=doc_lint_unavailable`）、期限切れと同じに数える。同じ MR で 3 回目なら `FG-CI-0024`（`reason=stalled`）で外れる。
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
   - `batch` の `reason=doc_lint_failed`（`state=failed`、`FG-CI-0032`）/ `reason=doc_lint_unavailable`（`state=abandoned`、`FG-DOC-0002`）:
     下の「列車の doc-lint」。
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

## 列車の doc-lint（計画 18 S1。FG-CI-0032、FG-DOC-0002 `scope=train`）

単独では `forge/doc-lint` が通る MR どうしが、組み合わさると参照を壊すことがある（片方が文書を消し、もう片方がそれを参照する）。
列車のパイプラインが緑になった後（`.forge-ci.yml` の無いリポジトリでは列車ができてすぐ）、着地の前に、worker が**列車の最後の段の木**を
`gstn doc lint` と同じ規則で lint する。列車のどの MR も文書（`.md`）と文書型（`.forge/doctypes/**`）を変えず、厳格でもないなら lint しない。

| 判定 | 起きること | ログ / 記録 |
|---|---|---|
| 問題なし、または**土台（target の先端）に無い問題が増えていない** | 着地する。土台に元からある問題は、厳格でないリポジトリでは着地を止めない | `batch_landed` の `lint_ms`。列車の先端の sha に `forge/doc-lint` のチェック（木の全体の評価。元からの問題が残っていれば `failure` で、要約の先頭の行に「土台に無い問題は 0 件」と出る） |
| 土台に無い問題が増えた | 段を二分して**最初に壊した段の MR だけ**を外す（`FG-CI-0032`、`reason=doc_lint_failed`）。バッチは `failed`、残りは列に戻り、順番のまま次の列車になる | `entry_removed`（warn）の `step`（何段目か）、`problems`（`rule:path:参照先の ID` を 20 件まで）、`train_sha`。その `train_sha` の `forge/doc-lint` のチェックに全件 |
| 厳格なリポジトリで、土台にすでに問題がある | 列車の MR を全部外す（`FG-CI-0032`、`reason=doc_lint_base_not_clean`）。どの MR も原因ではないが、着地もしない | `entry_removed` に `step` は無い。`problems` は土台の問題 |
| lint できなかった | **着地しない**（通したことにしない）。バッチを捨て、MR は列に戻る（`reaps` + 1）。3 回目で `FG-CI-0024`（`reason=stalled`） | `train_lint_unavailable`（warn、`FG-DOC-0002`）の `reason`、`cause`（元のコード）、`bound`、`budget_ms` |

設定（2 つ）:

- `merge_queue.train_lint_budget_ms`（**サーバー**の設定。`[merge_queue] train_lint_budget_ms` か `FORGE_MERGE_QUEUE_TRAIN_LINT_BUDGET_MS`。
  既定 10000、100〜60000）: 列車の lint 1 回の上限。**0 で列車の lint を切る**（計画 18 S1 の前の動きに戻る。起動のログ
  `forge.serve.merge_queue_started` の `train_lint` が `off`）。
- `merge_queue.doc_lint_strict`（**リポジトリ**ごと。`.forge/repo.yml` の `merge_queue:` の下、既定 `false`）: `true` なら「増えていない」ではなく
  「問題が 1 件も無い」ことを求める。**列車の土台（target の先端）の `.forge/repo.yml`** を読むので、MR の中で `true` にしても、その MR が
  着地するまでは効かない。土台の `.forge/repo.yml` が読めても**正しくない**（`FG-REPO-0024`。info の `forge.train_lint.repo_config_invalid`）ときは、
  安全な側（厳格）として扱う。

診断（読み取りのみ）:

1. `FG-CI-0032 reason=doc_lint_failed`: `entry_removed` の `problems` と、`train_sha` の `forge/doc-lint` のチェックの要約を読む。外れた MR の
   変更と、先に着地した（または同じ列車で前の段にいた）MR の変更の組み合わせが原因。MR の枝を target に追従させて直すよう作者に伝える
   （新しい版を push すれば、また列に入れられる。同じ版のままでは自動マージは入れ直さない）。
2. `FG-CI-0032 reason=doc_lint_base_not_clean`: `gstn doc lint` を target の先端で動かすと同じ問題が出る。**厳格なリポジトリは、target が
   きれいになるまで何も着地しない**。直し方は Mitigate の「厳格なリポジトリが詰まった」。
3. `FG-DOC-0002 scope=train`:
   - `reason=train_budget`: 文書が多くて上限に収まらない。`batch_landed` の `lint_ms` の普段の値と `budget_ms` を比べる。同じ時刻に git や
     DB が遅くなっていないかも見る（`FG-GIT-…`、`FG-DB-…`）。
   - `reason=read_failed` + `cause`: 元の失敗のコード（git、DB、ストレージ）の Runbook へ。`cause=FG-DOC-0002` と `bound` があるときは、
     木が列車の lint の上限（木のエントリー 50,000、`.md` 20,000、合計 64 MiB、文書型 64）を超えている。**上限を超えるリポジトリは、
     lint を切らない限り、文書を変える MR を着地できない**。
   - `cause=FG-SYS-0001` で `reason=train_lint_step_out_of_range` / `merge_queue_entry_without_train`: 実装の不整合。インシデントとして扱う。

## Mitigate（変更は人の承認のあと）

- **緊急停止**: `gstn queue pause --project <id> --target <branch>`（Developer 以上）。新しいバッチを作らず、緑になっても着地しない。
  再開は `gstn queue resume`（Maintainer 以上）。止めている間も MR は列に入れられる。
- パイプラインが進まない: 元の原因（Runner、パイプラインの作成）をその Runbook の手順で直す。直ったら列は次の手番で進む。
  期限で外れた MR（`reason=stalled`）は、作者が `gstn queue add` で入れ直す。
- worker が動いていない: `forge serve` を再起動する（貸し出しは期限で切れるので、他の worker が引き継ぐ。手で `lease_owner` を
  消さない）。
- **列車の lint を切る**（lint できない状態が続いて列が進まない、上限を超えるリポジトリがある、など）: サーバーの設定を
  `merge_queue.train_lint_budget_ms = 0`（`FORGE_MERGE_QUEUE_TRAIN_LINT_BUDGET_MS=0`）にして `forge serve` を再起動する。**人の承認が要る**
  （切っている間は、組み合わせで壊れる参照が `main` に入りうる）。起動のログで `train_lint=off` を確かめ、原因を直したら戻す。
  時間切れだけなら、切る前に上限を上げる（60000 まで）。
- **厳格なリポジトリが詰まった**（土台に問題があり、`doc_lint_base_not_clean` で全部外れる）: 問題を**全部**直す MR を 1 本作って列に入れる
  （その MR だけの列車、または同じ列車の全体で木がきれいになれば着地する）。または、`.forge/repo.yml` の `doc_lint_strict` を外す MR を
  同じ修正と一緒に出す（設定は土台から読むので、設定だけを変える MR は着地できない）。どうしても通せないときだけ、上の「列車の lint を切る」。
- `FG-CI-0032` で外れた MR は、作者が直して新しい版を push し、`gstn queue add`（または自動マージ）で入れ直す。運用の操作は要らない。
- 詰まったバッチを手で捨てる SQL の操作はしない（エントリーと試行の状態がずれる）。止めて（pause）原因を直すのが先。

## Verify

- `forge doctor` の `review.merge_queue` が ok。
- `gstn queue list` でバッチが進み、`forge.merge_queue.batch_landed` が出る。target の先端が列車の先端（`train_sha`）になる。
- 列車の lint: `batch_landed` の `lint_ms` が 0 より大きい（文書を変える列車）。文書を変えた列車の先端に `forge/doc-lint` のチェックがある。
  `train_lint_unavailable` が止まっている。

## 関連

- ADR-0030（マージキュー）、ADR-0037（列車の doc-lint）、ADR-0024（マージの実行、照合）、Runbook `merge-execution`、`ci-pipelines`、`ci-runner`、
  `checks-reporting`（`forge/doc-lint` のチェック）、`config-invalid`（`merge_queue.train_lint_budget_ms`）。負荷試験: `ops/loadtest/README.md` の `a-xref`。
