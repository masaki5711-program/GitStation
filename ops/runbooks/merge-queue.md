# Runbook: merge-queue（FG-CI-0024、FG-CI-0032。関連: FG-DB-0024 `channel=forge_merge_queue`、FG-CI-0023、FG-CI-0025、FG-CI-0005、FG-CI-0002、FG-DOC-0002、FG-REPO-0024、FG-REVIEW-0009）

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
- `forge serve` のログに `FG-CI-0032` MergeQueueDocLintFailed（**info**、`forge.merge_queue.entry_removed`、`reason=doc_lint_failed` か
  `doc_lint_base_not_clean`）: 列車の doc-lint（計画 18 S1、ADR-0037 (b)）が MR をキューから外した。**障害ではなく、判定の結果**
  （下の「列車の doc-lint」）。
- `forge serve` のログに `FG-REPO-0024`（**warn**、`forge.merge_queue.entry_removed`、`reason=doc_lint_repo_config_invalid`）: target の
  `.forge/repo.yml` が正しくなく、target に元からある文書の問題のせいで列車の MR が全部外れた。**MR の文書ではなく設定が原因**。
  `.forge/repo.yml` を直す MR が着地するまで続く（下の「`.forge/repo.yml` が正しくない」）。
- `forge serve` のログに `FG-DOC-0002`（warn、`forge.merge_queue.train_lint_unavailable`、`scope=train`）: 列車を lint できなかった
  （時間切れ `reason=train_budget`、読めなかった `reason=read_failed`）。バッチは捨てられ（`batch_abandoned` ではなくこの行が出る。
  バッチの `reason=doc_lint_unavailable`）、期限切れと同じに数える。同じ MR で 3 回目なら `FG-CI-0024` で外れるが、理由は `stalled`
  ではなく **`reason=doc_lint_unavailable`**（試験が遅れたのではなく、文書の検査を終えられなかった）。その `entry_removed`（warn）に
  lint の `lint_reason` / `cause` / `bound` / `budget_ms` が付く。
- `forge serve` のログに `FG-SYS-0005` ServeFailed（`operation=merge_queue_tick`、`reason=merge_queue_tick_timeout` など）が
  1 分に 1 回まで続く。`worker_id` が手番（`<worker>-<番号>`）、`step=idle_wait` なら待ちに入るときの読み取りの失敗。
- `forge serve` のログに `FG-DB-0024` WakeupMissed（warn）が `channel=forge_merge_queue` で繰り返し出る: 通知なしで、安全網の
  見回り（`merge_queue.safety_poll_ms`、既定 30 秒）が進められるキューを見つけた（下の「起床と安全網」）。
- 「列に入れたのに 30 秒〜1 分たってから動き出す」「CI が緑になってから着地まで 1 分かかる」という問い合わせ（同じ節）。

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
   - `batch` の `reason=doc_lint_failed`（`state=failed`、`FG-CI-0032`）/ `reason=doc_lint_repo_config_invalid`（`state=failed`、
     `FG-REPO-0024`）/ `reason=doc_lint_unavailable`（`state=abandoned`、`FG-DOC-0002`）: 下の「列車の doc-lint」。
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

## 列車の doc-lint（計画 18 S1。FG-CI-0032、FG-DOC-0002 `scope=train`、FG-REPO-0024 `doc_lint_repo_config_invalid`）

単独では `forge/doc-lint` が通る MR どうしが、組み合わさると参照を壊すことがある（片方が文書を消し、もう片方がそれを参照する）。
列車のパイプラインが緑になった後（`.forge-ci.yml` の無いリポジトリでは列車ができてすぐ）、着地の前に、worker が**列車の最後の段の木**を
`gstn doc lint` と同じ規則で lint する。列車のどの MR も文書（`.md`）と文書型（`.forge/doctypes/**`）を変えず、厳格でもないなら lint しない。

| 判定 | 起きること | ログ / 記録 |
|---|---|---|
| 対象でない（文書も文書型も変えず、厳格でもない） | lint せずに着地する | `train_lint_skipped`（debug）。`batch_landed` の `lint_ms` は対象かどうかを調べた時間（`.forge/repo.yml` の読み取りと、変わったパスの一覧）で、0 ではない |
| 問題なし、または**土台（target の先端）に無い問題が増えていない** | 着地する。土台に元からある問題は、厳格でないリポジトリでは着地を止めない | `batch_landed` の `lint_ms`。列車の先端の sha に `forge/doc-lint` のチェック（木の全体の評価。元からの問題が残っていれば `failure` で、要約の先頭の行に「土台に無い問題は 0 件」と出る） |
| 土台に無い問題が増えた | 段を二分して**最初に壊した段の MR だけ**を外す（`FG-CI-0032`、`reason=doc_lint_failed`）。バッチは `failed`、残りは列に戻り、順番のまま次の列車になる | `entry_removed`（info）の `step`（何段目か）、`problems`（`rule:path:参照先の ID` を 20 件まで）、`base_sha`、`train_sha`、`elapsed_ms`。その `train_sha` の `forge/doc-lint` のチェックに全件 |
| 厳格なリポジトリで、土台にすでに問題がある | 列車の MR を全部外す（`FG-CI-0032`、`reason=doc_lint_base_not_clean`）。どの MR も原因ではないが、着地もしない | `entry_removed`（info）に `step` は無い。`problems` は土台の問題 |
| 土台と列車の先端の両方で `.forge/repo.yml` が正しくなく、土台にすでに問題がある | 列車の MR を全部外す（**`FG-REPO-0024`**、`reason=doc_lint_repo_config_invalid`）。バッチも `failed` で同じ理由。原因は設定で、MR の文書ではない | `entry_removed`（**warn**）の `config_line`、`config_reason`（`.forge/repo.yml` の何行目が、なぜ正しくないか）、`problems`（土台の問題）。`step` は無い |
| lint できなかった | **着地しない**（通したことにしない）。バッチを捨て、MR は列に戻る（`reaps` + 1）。3 回目で外れる（`FG-CI-0024`、**`reason=doc_lint_unavailable`**。`stalled` ではない） | `train_lint_unavailable`（warn、`FG-DOC-0002`）の `reason`、`base_sha`、`elapsed_ms`、分かるときだけ `cause`（元のコード）、`bound`、`budget_ms`（無い欄は「言うことが無い」）。3 回目の `entry_removed`（warn）に同じ `lint_reason` / `cause` / `bound` / `budget_ms` |

ログの重さ: `FG-CI-0032` の 2 つは MR の変更に対する**予想どおりの結果**なので info。設定が原因（`FG-REPO-0024`）と 3 回目の
`FG-CI-0024` は warn。関門を切っているとき（下の `train_lint=off`）は、列車ごとの行は何も出ない（`train_lint_skipped` も出ない）ので、
「対象でなかった」と「切っている」を見分けられる。他の worker が先にバッチを終わらせて判定を捨てたときは `train_lint_raced`（debug）。

設定（2 つ）:

- `merge_queue.train_lint_budget_ms`（**サーバー**の設定。`[merge_queue] train_lint_budget_ms` か `FORGE_MERGE_QUEUE_TRAIN_LINT_BUDGET_MS`。
  既定 10000、100〜60000）: 列車の lint 1 回の上限。**0 で列車の lint を切る**（計画 18 S1 の前の動きに戻る。起動のログ
  `forge.serve.merge_queue_started` の `train_lint` が `off`）。
- `merge_queue.doc_lint_strict`（**リポジトリ**ごと。`.forge/repo.yml` の `merge_queue:` の下、既定 `false`）: `true` なら「増えていない」ではなく
  「問題が 1 件も無い」ことを求める。**列車の土台（target の先端）の `.forge/repo.yml`** を読むので、MR の中で `true` にしても、その MR が
  着地するまでは効かない。土台の値が正しく読める限り、列車の MR が外したり `false` にしたりしても**緩まない**。

### `.forge/repo.yml` が正しくない（FG-REPO-0024）

土台の `.forge/repo.yml` が読めても**正しくない**ときは、土台からは厳格かどうかが分からない。そのときだけ**列車の先端**の
`.forge/repo.yml` で決める（info の `forge.train_lint.repo_config_invalid` の `decided_by` と `strict`）:

| 列車の先端の `.forge/repo.yml` | 厳格か | `decided_by` |
|---|---|---|
| 正しい（＝この列車が直している） | 先端の値のとおり | `train_repairs_the_file` |
| 無い（＝この列車が消している） | 厳格でない | `train_removes_the_file` |
| やはり正しくない | **厳格**（読めない設定で緩めない） | `invalid_on_both` |

だから、**`.forge/repo.yml` を直す（または消す）MR は、土台に古い文書の問題が残っていても着地できる**（直した値が `true` なら、
普通の厳格なリポジトリとして `doc_lint_base_not_clean`）。両方で正しくない間は、土台に問題があれば列車の MR は全部
`FG-REPO-0024`（`reason=doc_lint_repo_config_invalid`）で外れ、問題を残さない列車だけが着地する。

診断（読み取りのみ）:

1. `FG-CI-0032 reason=doc_lint_failed`: `entry_removed` の `problems` と、`train_sha` の `forge/doc-lint` のチェックの要約を読む。外れた MR の
   変更と、先に着地した（または同じ列車で前の段にいた）MR の変更の組み合わせが原因。MR の枝を target に追従させて直すよう作者に伝える
   （新しい版を push すれば、また列に入れられる。同じ版のままでは自動マージは入れ直さない）。
2. `FG-CI-0032 reason=doc_lint_base_not_clean`: `gstn doc lint` を target の先端で動かすと同じ問題が出る。**厳格なリポジトリは、target が
   きれいになるまで何も着地しない**。直し方は Mitigate の「厳格なリポジトリが詰まった」。
3. `FG-REPO-0024 reason=doc_lint_repo_config_invalid`: `entry_removed` の `config_line` と `config_reason` が、target の
   `.forge/repo.yml` のどこが正しくないかを言う（理由の一覧はエラーカタログの FG-REPO-0024）。土台の版を読む:
   `git --git-dir <リポジトリの保存先> show <base_sha>:.forge/repo.yml | sed -n '<config_line>p'`。直し方は Mitigate の
   「`.forge/repo.yml` が正しくない」。
4. `FG-DOC-0002 scope=train`（3 回目で外れた MR は `FG-CI-0024 reason=doc_lint_unavailable`。その `entry_removed` の `lint_reason` /
   `cause` / `bound` / `budget_ms` が下の `reason` / `cause` / `bound` / `budget_ms` と同じ意味）:
   - `reason=train_budget`: 文書が多くて上限に収まらない。`batch_landed` の `lint_ms` の普段の値と `budget_ms` を比べる。同じ時刻に git や
     DB が遅くなっていないかも見る（`FG-GIT-…`、`FG-DB-…`）。
   - `reason=read_failed` + `cause`: 元の失敗のコード（git、DB、ストレージ）の Runbook へ。`cause=FG-DOC-0002` と `bound` があるときは、
     木が列車の lint の上限（木のエントリー 50,000、`.md` 20,000、合計 64 MiB、文書型 64）を超えている。**上限を超えるリポジトリは、
     lint を切らない限り、文書を変える MR を着地できない**。
   - `cause=FG-SYS-0001` で `reason=train_lint_step_out_of_range` / `merge_queue_entry_without_train`: 実装の不整合。インシデントとして扱う。
   - **時間切れのあとも lint は走り切る**（既知の制限）: 上限は「待つのをやめる」だけで、木の lint そのもの（blocking のスレッドで動く
     1 回の呼び出し）は止められない。結果は捨てられる。時間切れが続くと、CPU を使う lint が数本残ることがある（1 本は上の読む上限まで。
     列は次の手番まで新しい列車を作らず、MR は 3 回で外れるので、積み上がる数は限られる）。`forge serve` の CPU が高いままなら、
     `train_lint_unavailable reason=train_budget` の回数と突き合わせ、上限を上げるか（60000 まで）、文書を減らすか、関門を切る。

## 起床と安全網（計画 18 S3。FG-DB-0024 `channel=forge_merge_queue`）

キューの手番（`forge serve` のタスク `merge_queue`。`merge_queue.workers` 個、既定 2）は、短い間隔で表を見に行かない。キューを
進められるようにした書き込みが、同じトランザクションで `NOTIFY forge_merge_queue, <キューの id>` を送り、手番はそれで起きる。
送るのは: MR を列に入れた・外した、止めていたキューの再開、列車のパイプラインの終わり（成功・失敗・取り消し・定義が正しくない）、
パイプラインを作るジョブがパイプラインを残さずに終わったとき（完了・失敗・貸し出しの期限切れで dead）、手番が 1 手の後にキューを
「すぐ次」で返したとき（返した手番が別のキューへ行っても、待っている手番が取る）。通知は合図にすぎない: 起きた手番は必ず表を読み直し、通知が
来なくても下の周期で自分で見る。だから取りこぼしても MR は失われず、遅れるだけ。

| 待ち | 既定 | 設定 | 計画 18 より前 |
|---|---|---|---|
| することが無い手番、止めているキュー、全部が保留のキュー | 30 秒 | `merge_queue.safety_poll_ms`（100〜300000） | 2 秒 |
| 列車のパイプラインを待っているキューを、通知なしで次に見るまで | 60 秒 | `merge_queue.waiting_safety_poll_ms`（100〜600000） | 10 秒 |
| 今は進めない手（失敗した手、他の worker が着地の途中）の次 | 最長 10 秒 | （固定） | 10 秒 |
| LISTEN の接続が無い・切れている間 | 最長 2 秒 | （固定） | — |

- 1 つのキューを同時に進める手番は 1 つだけ（キューの貸し出し `lease_owner` / `lease_until`）。別のキューは別の手番で同時に進む。
  `lease_owner` は `<worker>-<手番の番号>`。同じリポジトリの 2 つのキューは、着地だけリポジトリのマージのロックに並ぶ。
- 起動のログ `forge.serve.merge_queue_started` に `workers`、`safety_poll_ms`、`waiting_safety_poll_ms`。止まるときは手番ごとに
  `forge.serve.merge_queue_stopped`（`worker`）。
- **通知が無い変化**（待ちの期限で気づく。最長 60 秒）: 列にいる MR に新しい版が push された・閉じられた、target が他から動いた。
  止めているキューと、自動マージの緊急停止で保留のエントリーは 30 秒ごとに見る。

`FG-DB-0024`（`channel=forge_merge_queue`）を読む:

1. `context`: `queue_id`、`outcome`（見つけた手が何をしたか。`built` = バッチを作った、`landed` = 着地した、など）、`worker_id`（手番）、
   `oldest_ready_ms`（そのキューが取れる状態になってからの時間。DB の時計。他の手番が貸し出し中に期限にされたキューは、返された
   時刻から数える）、`safety_poll_ms`、`suppressed`（前の 1 件の後に
   出さなかった件数）。1 分に 1 回まで。安全網の周期より短い遅れは出ない。
2. 同じ時間帯に LISTEN の接続の失敗があるか: `error_code=FG-DB-0001` / `FG-DB-0002` で `operation=wake_listen`、`FG-SYS-0001` で
   `task=wake_hub`、`FG-DB-0023`（NOTIFY の待ち行列）。読み方は Runbook `job-queue-backlog` の「起床の通知の取りこぼし」と
   `db-unavailable` の「notify queue」。接続が切れている間、手番は 2 秒ごとの確認に戻る（`FG-DB-0024` は出ない）。
3. 通知を送らない経路でキューが進められるようになっていないか: 表を手で変えた、計画 18 S3 より前のビルドの `forge serve` /
   `forge worker` が並んで動いている（入れ替えの途中）。`outcome` と時刻から、その手の `forge.merge_queue.*` の行をたどる。
4. 表: `SELECT id, paused, lease_owner, lease_until, next_run_at, now() FROM forge.merge_queues ORDER BY next_run_at;`
   `next_run_at` が過去で `lease_until` が空のキューが残り続けるなら、手番が動いていない（上の Diagnose 4）。

## Mitigate（変更は人の承認のあと）

- **通知に頼らずに動かす**（`FG-DB-0024` が続く、LISTEN の接続が直らない）: サーバーの設定を `merge_queue.safety_poll_ms = 2000`、
  `merge_queue.waiting_safety_poll_ms = 10000`（計画 18 より前の間隔。`FORGE_MERGE_QUEUE_SAFETY_POLL_MS` /
  `FORGE_MERGE_QUEUE_WAITING_SAFETY_POLL_MS`）にして `forge serve` を再起動する。手番を 1 つに戻すなら `merge_queue.workers = 1`。
  **人の承認が要る**（ロールバック: 設定を元の値に戻して再起動する。列の MR は失われない）。繰り返す `FG-DB-0024` は開発者
  （Claude のセッション）に渡す（`queue_id`、`outcome`、時刻、手順 2・3 の結果）。
- **緊急停止**: `gstn queue pause --project <id> --target <branch>`（Developer 以上）。新しいバッチを作らず、緑になっても着地しない。
  再開は `gstn queue resume`（Maintainer 以上）。止めている間も MR は列に入れられる。
- パイプラインが進まない: 元の原因（Runner、パイプラインの作成）をその Runbook の手順で直す。直ったら列は次の手番で進む。
  期限で外れた MR（`reason=stalled`）は、作者が `gstn queue add` で入れ直す。
- lint できずに 3 回目で外れた MR（`FG-CI-0024`、`reason=doc_lint_unavailable`）: パイプラインは原因ではない。`entry_removed` の
  `bound`（文書が多すぎる → 文書を減らす、または下の「列車の lint を切る」）、`budget_ms`（時間切れ → 上限を上げる）、`cause`
  （元のコードの Runbook）で原因を直してから、作者が `gstn queue add` で入れ直す。自動マージの MR は
  `stopped reason=merge_queue_doc_lint_unavailable` で止まっている（Runbook `auto-merge`）。
- worker が動いていない: `forge serve` を再起動する（貸し出しは期限で切れるので、他の worker が引き継ぐ。手で `lease_owner` を
  消さない）。
- **列車の lint を切る**（lint できない状態が続いて列が進まない、上限を超えるリポジトリがある、など）: サーバーの設定を
  `merge_queue.train_lint_budget_ms = 0`（`FORGE_MERGE_QUEUE_TRAIN_LINT_BUDGET_MS=0`）にして `forge serve` を再起動する。**人の承認が要る**
  （切っている間は、組み合わせで壊れる参照が `main` に入りうる）。起動のログで `train_lint=off` を確かめ、原因を直したら戻す。
  時間切れだけなら、切る前に上限を上げる（60000 まで）。
- **厳格なリポジトリが詰まった**（土台に問題があり、`doc_lint_base_not_clean` で全部外れる）: 問題を**全部**直す MR を 1 本作って列に入れる
  （その MR だけの列車、または同じ列車の全体で木がきれいになれば着地する）。または、`.forge/repo.yml` の `doc_lint_strict` を外す MR を
  同じ修正と一緒に出す（設定は土台から読むので、設定だけを変える MR は着地できない）。どうしても通せないときだけ、上の「列車の lint を切る」。
- **`.forge/repo.yml` が正しくない**（`FG-REPO-0024`、`doc_lint_repo_config_invalid` で全部外れる）: `.forge/repo.yml` を直す（または消す）
  MR を 1 本作って列に入れる（`.forge/**` の変更なので Maintainer 以上の承認が要る: `config_change_needs_maintainer_approval`）。土台が正しくないので、この MR は
  **自分の `.forge/repo.yml` で**判定され、土台に古い文書の問題が残っていても着地する（`doc_lint_strict: true` と書いて問題を残すと
  `doc_lint_base_not_clean`）。着地したら、外れた MR を作者が入れ直す。サーバーの操作は要らない。
- `FG-CI-0032 reason=doc_lint_failed` で外れた MR は、作者が直して新しい版を push し、`gstn queue add`（または自動マージ）で入れ直す。
  運用の操作は要らない。
- 詰まったバッチを手で捨てる SQL の操作はしない（エントリーと試行の状態がずれる）。止めて（pause）原因を直すのが先。

## Verify

- `forge doctor` の `review.merge_queue` が ok。
- `gstn queue list` でバッチが進み、`forge.merge_queue.batch_landed` が出る。target の先端が列車の先端（`train_sha`）になる。
- 列車の lint: `batch_landed` の `lint_ms` が 0 より大きい（文書を変える列車）。文書を変えた列車の先端に `forge/doc-lint` のチェックがある。
  `train_lint_unavailable` が止まっている。

## 関連

- ADR-0030（マージキュー）、ADR-0037（列車の doc-lint）、ADR-0024（マージの実行、照合）、Runbook `merge-execution`、`ci-pipelines`、`ci-runner`、
  `checks-reporting`（`forge/doc-lint` のチェック）、`config-invalid`（`merge_queue.train_lint_budget_ms`、
  `merge_queue.safety_poll_ms` / `waiting_safety_poll_ms` / `workers`。リポジトリの `.forge/repo.yml` は
  ここ）、`job-queue-backlog`（`FG-DB-0024` の読み方）、`auto-merge`（`merge_queue_doc_lint_*` / `merge_queue_repo_config_invalid` の止まり方）。負荷試験: `ops/loadtest/README.md` の `a-xref`。
