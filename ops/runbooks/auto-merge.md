# Runbook: auto-merge（FG-REVIEW-0028、FG-REVIEW-0027、FG-REVIEW-0032。関連: FG-REVIEW-0026、FG-REVIEW-0023、FG-CI-0023、FG-CI-0025）

M4 C9（ADR-0017 Amendment 2026-11-13、計画 04 M-5 / M-6）の自動マージ（`forge serve` のタスク `auto_merge`、表
`forge.project_auto_merge` / `forge.auto_merge_instance` / `forge.auto_merge_states`）が動かない・止まった・期待と違うとき。
自動マージの主体は、マージできる MR を C8 のマージキューに入れるだけ。マージそのものはキュー（Runbook `merge-queue`）が行う。

## Trigger

- `forge serve` のログに `FG-REVIEW-0028` AutoMergeScanFailed（error、`forge.auto_merge.scan_failed`）: MR を判定できなかった。
  `cause_code` が元のコード（FG-DB-*、FG-GIT-* など）。
- `forge serve` のログに `FG-REVIEW-0027` AutoMergeEmergencyStopped（warn、`forge.auto_merge.emergency_stop`）: 誰かが緊急停止を
  入れた（`scope=project|instance`、`user_id`）。
- `forge serve` のログに `FG-SYS-0005` ServeFailed（`operation=auto_merge_tick`、`reason=auto_merge_tick_timeout` など）が
  1 分に 1 回まで続く。`woken_by`（`sweep` / `events` / `named` / `poll`）が、何で起きたときの失敗か。
- 「チェックが緑になったのに、列に入るまで 30 秒かかる」という問い合わせ（下の「起床と見回り」）。
- 利用者から「full のプロジェクトなのに自動でマージされない」「止まった理由がわからない」と問い合わせがあった。
- `FG-REVIEW-0032` AutoMergeFullNotAllowed（info）: Owner が `full` を選ぼうとしたが、インスタンスの `merge.allow_full_auto` が
  外れている（M4 の締め、ADR-0017 Amendment 2026-11-17）。`forge doctor` の `review.auto_merge` が warn（`full` のプロジェクトの数）。
- `forge serve` のログに `forge.auto_merge.full_auto_disallowed`（warn、`user_id`）: 管理者が `merge.allow_full_auto` を外した。

## Impact

- 自動マージが進まないだけ。人のマージ（`gstn mr merge`、「準備ができたらマージ」）は影響を受けない。止まって困るのは速さだけで、
  壊れたものが入ることはない（止まる方向に倒れる）。
- 1 プロジェクトなら SEV4。DB や git の障害で全体の判定が失敗しているなら、元の障害の重さ（`db-unavailable` / `git-unavailable`）。

## Diagnose（読み取りのみ）

1. MR の 1 行: `GET /api/v1/projects/{id}/merge-requests/{iid}/auto-merge`（または MR の画面の「自動マージ:」の行）。
   `state`（`queued` / `waiting` / `held` / `stopped`）、`class`、`reason`、`detail`、`request_id`。
   - `stopped` は 4 分類（`program_breakage` / `data_loss` / `leak` / `developer_pc`）、`not_assessed`、`emergency_stop` だけ。
     `detail` が forge/risk の規則か必須チェックの名前。`gstn checks list --project <id> --sha <head>` で forge/risk の根拠を読む。
   - `waiting` は止まっていない（`risk_pending`、`draft`、`approvals_missing`、`protected_path`、`conflict` など）。条件がそろえば進む。
   - `held reason=dequeued_by_person`: 人がキューから外した版。新しい push か、人の「準備ができたらマージ」で進む。
   - `stopped reason=merge_queue_doc_lint_failed`（`program_breakage`、`detail=forge/doc-lint`。計画 18 S1）: その MR だけでは
     forge/doc-lint が通ったが、列車の木（前に並ぶ MR と合わせた結果）で文書の問題が増え、キューから外れた（`FG-CI-0032`、
     `reason=doc_lint_failed`）。同じ版は自動では入れ直さない。作者が直して push する。Runbook `merge-queue` の「列車の doc-lint」。
   - `held reason=merge_queue_doc_lint_base`: 厳格なリポジトリ（`.forge/repo.yml` の `merge_queue.doc_lint_strict`）で、target に
     すでに文書の問題がある（`FG-CI-0032`、`reason=doc_lint_base_not_clean`）。この MR は原因ではない。target がきれいになった後、
     新しい push か人の「準備ができたらマージ」で進む。Runbook `merge-queue`。
   - `held reason=merge_queue_repo_config_invalid`: 上と同じだが、厳格になっているのは target の `.forge/repo.yml` が**正しくない**
     から（`FG-REPO-0024`、`reason=doc_lint_repo_config_invalid`）。この MR は原因ではない。`.forge/repo.yml` を直す MR が着地した後、
     新しい push か人の「準備ができたらマージ」で進む。Runbook `merge-queue` の「`.forge/repo.yml` が正しくない」。
   - `stopped reason=merge_queue_doc_lint_unavailable`（`not_assessed`、`detail=forge/doc-lint`）: 列車の doc-lint を 3 回終えられず
     （文書が多すぎる、時間切れ、読み取りの失敗）、キューから外れた（`FG-CI-0024`、`reason=doc_lint_unavailable`）。安全と示せないので、
     同じ版は自動では入れ直さない（以前は `stalled` として入れ直し、同じ 3 回を繰り返していた）。管理者が原因を直した後、新しい push か
     人の「準備ができたらマージ」で進む。Runbook `merge-queue` の「列車の doc-lint」。
   - `held reason=full_auto_not_allowed`: プロジェクトは `full` だが、インスタンスの `merge.allow_full_auto` が外れている
     （実効は `reserve`）。キューにあった自動マージのエントリーは `reason=full_auto_not_allowed` で外れている。人がマージするか、
     管理者が許可を戻す。
   - `stopped reason=merge_result_risk`（M4 の締め）: その MR だけでは forge/risk が通ったが、キューで前に並ぶ MR と合わせた
     **マージ結果**（バッチの土台から列車のその段までの差分。target に書かれる木そのもの）に同じ規則が `stop` を見つけた。`class` が
     分類、規則と段はキューのログ `forge.merge_queue.entry_removed`（`FG-REVIEW-0026`、`reason=merge_result_<class>`、`context` に
     `class=… rule=… step=…`）。同じ版は自動では入れ直さない。前の MR が入った後の target で人が確かめて「準備ができたらマージ」を
     押すか、新しい push で判定し直す。
   - `null`: プロジェクトの方式が `off` / `reserve`、または worker がまだ見ていない（数十秒）。
2. プロジェクトの方式と緊急停止: `GET /api/v1/projects/{id}/auto-merge`（`mode`、`allow_full_auto`、`project_stop`、`instance_stop`）。
   C9 より前からあるプロジェクトは `off` で始まる（移行の決定。Owner が `full` にする）。`allow_full_auto = false` なら `full` は
   `reserve` として扱われる（`forge doctor` の `review.auto_merge` にも出る）。
3. ログを `request_id` で読む: `forge.auto_merge.stopped`（停止の始まりを 1 回）、`forge.merge_queue.enqueued by=auto_merge`
   （キューに入れた）、その後はキューの行（`forge.merge_queue.batch_built` …、Runbook `merge-queue`）。着地の前のマージ結果の判定は
   `forge.merge_queue.merge_result_assessed`（`decision=land|stop|cut`）と `forge.risk.merge_result_assessed`（`stop_rules`）。
4. 表を読む:
   `SELECT * FROM forge.auto_merge_instance;`（`allow_full_auto`、`allow_full_auto_by`、`allow_full_auto_at` を含む）
   `SELECT project_id, mode, mode_by, mode_at, stopped_by, stopped_at FROM forge.project_auto_merge WHERE project_id = <id>;`
   `SELECT merge_request_id, state, class, reason, detail, error_code, head_sha, request_id, checked_at, changed_at FROM forge.auto_merge_states WHERE project_id = <id> ORDER BY changed_at DESC LIMIT 20;`
   `checked_at` が古いまま更新されないなら worker が動いていない（`forge.serve.auto_merge_started` のあと `auto_merge_tick` の失敗）。
5. 監査: `auto_merge.mode_changed` / `auto_merge.stopped` / `auto_merge.resumed` / `auto_merge.allow_full_auto_changed`
   （誰がいつ）、`merge_queue.auto_enqueued`（actor system）。

## 起床と見回り（計画 18 S3）

自動マージの worker は、5 秒ごとに 50 件を見るのをやめた。次の 3 つで起きる。どれも合図にすぎず、判定は必ず表と git を読み直す。

| 起きるきっかけ | 判定するもの | `woken_by` |
|---|---|---|
| 順序づけられたイベント（`NOTIFY forge_events_sequenced`） | イベントが名指しする MR だけ（MR 自身のイベント、その head かマージ結果の `check.completed`）。同時に 4 件まで | `events` |
| 「マージできる」と答えたマージ可能性の確認（`NOTIFY forge_auto_merge`、MR の id。自動マージが扱う MR — 開いていて head があり、プロジェクトの `merge.auto` が `conditional` / `full`（行が無ければ `full`）— のときだけ送る） | その MR | `named` |
| 見回り（`auto_merge.safety_poll_ms`、既定 30000、100〜300000。聞き手がつながり直したときも） | 自動マージが扱う MR の全部、古く見た順 | `sweep` |
| LISTEN の接続が無い・切れている間（5 秒ごと。計画 18 より前の動き） | 古く見た順に `batch` 件 | `poll` |

- **見回りでしか気づかない変化**（最長 30 秒）: プロジェクトの `merge.auto` の変更、緊急停止とその解除、インスタンスの
  `merge.allow_full_auto`、`forge/doc-lint` の待ちの時間切れ。チェックの完了や承認はイベントで起きるので、ふつうは 1 秒以内。
- 1 回の起床で読むイベントは 200 件 × 10 回まで。残りがあれば待たずに続けて読む（`woken_by=events` が続く）。起動の後、最初の
  見回りが成功するまではイベントを読まない（それより古いものは見回りが見る）。つながり直し 1 回で見回りは 1 回。
- 起動のログ `forge.serve.auto_merge_started` に `safety_poll_ms`。
- 見回りが遅れても `FG-DB-0024` は出ない（キューの手番とジョブだけが出す）。通知が届いているかは、LISTEN の接続の失敗
  （`FG-DB-0001` / `FG-DB-0002` の `operation=wake_listen`、`FG-SYS-0001` の `task=wake_hub`）と、順序づけの失敗で見る
  （Runbook `job-queue-backlog` の「起床の通知の取りこぼし」、`db-unavailable`）。
- 「緑になってから列に入るまでが毎回 30 秒前後」なら、イベントの起床が届いていない。上の接続の失敗を探し、無ければ開発者
  （Claude のセッション）に渡す（MR の id、チェックが終わった時刻、`forge.merge_queue.enqueued by=auto_merge` の時刻）。

## Mitigate（変更は人の承認のあと）

- **通知に頼らずに動かす**: サーバーの設定を `auto_merge.safety_poll_ms = 5000`（`FORGE_AUTO_MERGE_SAFETY_POLL_MS`。計画 18 より前の
  間隔）にして `forge serve` を再起動する。**人の承認が要る**（ロールバック: 設定を元の値に戻して再起動する）。

- **すぐ止める**: プロジェクトは `POST /api/v1/projects/{id}/auto-merge/stop`（Maintainer 以上、プロジェクトの設定の画面の
  「自動マージを緊急停止」）、全体は `POST /api/v1/admin/auto-merge/stop`（管理者）。キューにある自動マージの MR は待ち（バッチに
  入らない。試し中のバッチは着地の前に捨てて列に戻す）、人の MR は進む。再開は `…/resume`。
- `FG-REVIEW-0028`: `cause_code` の Runbook（`db-unavailable`、`git-unavailable`、`repo-storage`）で元の障害を直す。worker は
  バックオフして自分で続ける。直るまで自動で何かが入るのを避けたいなら、全体の緊急停止を入れる。
- 誤って止まっている（forge/risk の誤検知）: 規則は下げられない。人が根拠を読み、MR の画面で「準備ができたらマージ」を押す
  （人のマージは自動マージの停止に縛られない）。誤検知は評価のケースとして足す（計画 04 M-6「測る」）。
- 方式を変える（`full` → `reserve` など）のは Owner が Web のプロジェクトの設定で行う。DB を直接書き換えない。
- **全プロジェクトの `full` を一度に止める**（緊急停止より弱い、方針としての変更）: 管理者が Web のセッションで
  `PUT /api/v1/admin/auto-merge/allow-full-auto` に `{"allowed": false}`（トークンは断られる。監査
  `auto_merge.allow_full_auto_changed`）。`full` のプロジェクトは人のマージだけになり、`conditional` は動き続ける。戻すのは
  `{"allowed": true}`。`full` のまま残ったプロジェクトはすぐに動き出す。
- `FG-REVIEW-0032`: 問い合わせなら、`conditional` / `reserve` / `off` を選ぶよう案内するか、管理者に許可を相談する。
- `merge_result_risk`: 規則は下げられない。合わせた結果が本当に危険か（大量の削除が 2 つの MR に分かれていないか、など）を人が
  読み、問題なければ人がマージする。

## Verify

- `GET …/auto-merge` で `project_stop` / `instance_stop` が意図どおり。
- 対象の MR の 1 行が `queued` になり、キューの `forge.merge_queue.batch_landed` が出る。
- `forge.auto_merge.scan_failed` が止まる。
- `forge doctor` の `review.auto_merge` が ok（許可が意図どおり）。

## 関連

- ADR-0017（Amendment 2026-10-30・2026-11-03・2026-11-12・2026-11-13・2026-11-17）、ADR-0030（マージキュー、Amendment
  2026-11-17）、ADR-0037（計画 18 S3 の追記: 起床と見回り）、Runbook `merge-queue`、`config-invalid`（`auto_merge.safety_poll_ms`）、
  `merge-blocked`、計画 04 M-5 / M-6。
