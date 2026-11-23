# Runbook: config-invalid（FG-SYS-0002）

## Trigger

- 起動時または設定の再読み込み時に `error_code=FG-SYS-0002` が出た
- サーバーが起動しない

## Impact

サーバーが起動できない場合は全機能が停止（SEV1 相当）。再読み込み時の失敗なら、旧設定のまま稼働を継続する。

## Diagnose（読み取りのみ）

1. ログの `context` にある `config_key` と `reason` を読む。
2. `config_key` が指す値を、優先度の低い順に確認する。あとのものが前のものを上書きする。
   - 設定ファイル（`--config` または `FORGE_CONFIG` が指す TOML）
   - 環境変数。名前はキーから決まる（`database.url` → `FORGE_DATABASE_URL`）
   - コマンドラインのフラグ（`--bind` など）

   `config_key` の値ごとの意味は次のとおり。
   - `config_file`: 設定ファイルそのものの問題。`path` を見る。
   - `FORGE_...`: 未知の環境変数（つづりの誤り）。
   - `reason` の値ごとの意味は次のとおり。
     - `missing`: 未設定
     - `invalid`: 形式の誤り（`server.bind` は `host:port`）
     - `empty`: 空の値（中身を入れ忘れた環境変数など）
     - `zero`: 0 は不可
     - `above_max`: `min_connections` が `write_max_connections` か `read_max_connections` より大きい
     - `duplicate`: `database.privileged_members` に同じ名前が 2 回ある
     - `wrong_role`: ログインのロールが違う（`role_check` を見る。ロールが未作成なら Runbook `db-bootstrap`）
       - `database.url`: `not_member`（`forge_ro` のメンバーでない）、`superuser`（superuser か BYPASSRLS のログイン。すべての保護を迂回できるので使わない）、`privileged`（`forge_owner`・`forge_seq`・`forge_purge` に届く）、`role_not_applied`・`not_read_only`（読み取りのプールの設定が効いていない）
       - `database.migrate_url`: `not_member`（`forge_owner` のメンバーでない。`forge_migrator` で接続する）、`owner_missing`（導入の手順が未実行）、`superuser`（superuser ではマイグレーションしない）、`not_owner`
     - `connection_budget`: プールの大きさの合計（`needed`）が DB のサーバーの接続数（`usable`）の 80 % を超える。`database.write_max_connections` / `read_max_connections` を下げるか、サーバーの `max_connections` を上げる
     - `not_for_this_command`: `forge serve` の設定に `database.migrate_password_file` か `database.maint_password_file` がある。サーバーのプロセスに移行・保守の資格情報を持たせない。設定をログインごとに分ける（Runbook `db-bootstrap` の手順 5）
     - `unknown_key`: 設定ファイルのつづりの誤り
     - `unknown_env`: 環境変数のつづりの誤り
     - `renamed`: 改名前の名前（`renamed_to` に新しい名前が入る。例: `FORGE_BIND` → `FORGE_SERVER_BIND`、`FORGE_DATABASE_MAX_CONNECTIONS` → `FORGE_DATABASE_WRITE_MAX_CONNECTIONS`）
     - `not_unicode`: 値が UTF-8 でない
     - `unreadable`: ファイルを読めない
     - `parse`: 設定ファイルの構文の誤り（位置は `line` / `column`。問題の行の本文はログに出さない）
     - `password_in_url`: URL にパスワードを書いている。`database.password_file`（`migrate_url` なら `migrate_password_file`、`maint_url` なら `maint_password_file`）に移す
     - `query_in_url`: URL にクエリ（`?…`）がある。URL は `postgres://user@host[:port]/dbname` だけにし、TLS は `database.tls_mode` / `database.tls_root_cert`、タイムアウトは `database.*_timeout_ms` で設定する
     - `libpq_env`: `PGPASSWORD`、`PGSSLROOTCERT` などの `PG*` 環境変数が設定されている（`config_key` が名前）。forge は使わないので、サービスの環境から外す
     - `unverified_remote`: `database.tls_mode` が `require`（サーバーの証明書を検証しない）か `disable` のまま、ループバック以外の DB に接続しようとした。本番では `verify-full` にする。どうしても必要な場合だけ `database.tls_allow_unverified = true` を明示する（起動のたびに警告が出る）
     - `not_absolute`: パスワードファイルか CA 証明書のファイルが相対パス
     - `insecure_permissions`: Unix で、パスワードファイルをグループや他のユーザーが読める。`chmod 600` にする。Windows では権限を検査しないので、ACL を運用で管理する
     - `config_key=api.cursor_keys_file`（M1 phase 8b MR7b から `forge serve` は起動時に必ず読む。一覧のカーソルの鍵）: `missing`（未設定。
       `FORGE_API_CURSOR_KEYS_FILE` か設定ファイルの `[api] cursor_keys_file` に絶対パスを書く）、`empty`、`invalid` / `invalid_key_id` /
       `invalid_key`（`line` の行が `<kid> <64 桁の 16 進数>` でない）、`duplicate`（同じ kid が 2 行）、`unreadable`、`not_absolute`、
       `insecure_permissions`。行の中身はログに出ない。新しい鍵は `openssl rand -hex 32` で作り、`1 <hex>` の 1 行を 0600 のファイルに書く
       （入れ替えは ADR-0013 の Amendment MR6 の手順）
     - `config_key=web.base_url` の `insecure_http`: loopback 以外の `http://`。本番では `https://` にする。平文 HTTP の試験サーバー
       （`ops/gitlab-trial/`）だけは `web.allow_insecure_http = true` を明示する（クッキーが `Secure` なしになる。起動のたびに
       `forge.serve.plain_http` の警告）。`config_key=web.allow_insecure_http` の `invalid` は `true` / `false` 以外の値
     - `config_key=hosting.deletion_grace_days`（D5 MR3。削除したプロジェクトを復元できる日数）: `below_min`（0。1 日以上）、`above_max`（366 以上。365 日まで）、
       `invalid`（整数でない）。未設定なら 7 日。`FORGE_HOSTING_DELETION_GRACE_DAYS` か設定ファイルの `[hosting] deletion_grace_days`
     - `config_key=hosting.purge_interval_secs`（D5 MR4。`forge worker` の完全削除のタスクの周期、秒）: `below_min`（10 未満）、`above_max`
       （86400 超）、`invalid`（整数でない）。未設定なら 300 秒。`FORGE_HOSTING_PURGE_INTERVAL_SECS` か設定ファイルの `[hosting] purge_interval_secs`
     - `config_key=review.mergeability_fanout_max`（M3 R2。target の 1 回の前進でマージできるかの確認を積む開いている MR の数の上限）: `below_min`（0）、
       `above_max`（501 以上。500 まで）、`invalid`（整数でない）。未設定なら 50。`FORGE_REVIEW_MERGEABILITY_FANOUT_MAX` か設定ファイルの `[review] mergeability_fanout_max`
     - `config_key=review.check_run_retention_days`（チェックの実行の保持の日数）: `below_min`（29 以下。30 から）、`above_max`（3651 以上。3650 まで）、
       `invalid`（整数でない）。未設定なら 180。`FORGE_REVIEW_CHECK_RUN_RETENTION_DAYS` か設定ファイルの `[review] check_run_retention_days`
     - `config_key=review.worker_job_concurrency`（`forge worker` が同時に動かすジョブの数）: `below_min`（0）、`above_max`（17 以上。16 まで）、
       `above_half_write_pool`（`database.write_max_connections` の半分より多い。プールを広げるか値を下げる）、`invalid`（整数でない）。
       未設定なら 4（書き込みのプールの半分が 4 より小さければその数）。`FORGE_REVIEW_WORKER_JOB_CONCURRENCY` か設定ファイルの `[review] worker_job_concurrency`
     - `config_key=review.path_filter`（計画 18 S4。着地のたびに「着地したパスと重なりうる MR」だけを確かめ直し、残りの結果を新しい先端へ持ち越す。
       Runbook `merge-request-mergeability`）: `invalid`（`true` / `false` 以外）。未設定なら `true`。**`false` は停止スイッチ**（S4 より前と同じく、
       着地のたびに target の開いた MR をすべて確かめ直す）。`FORGE_REVIEW_PATH_FILTER` か設定ファイルの `[review] path_filter`
     - `config_key=review.paths_max`（計画 18 S4。変更パスの集合が持てるファイル（とディレクトリ）の数の上限。超えた変更は「不完全」で、常に確かめ直す）:
       `below_min`（0）、`above_max`（20001 以上。20000 まで）、`invalid`（整数でない）。未設定なら 5000。`FORGE_REVIEW_PATHS_MAX` か設定ファイルの `[review] paths_max`
     - `config_key=ci.mr_pipeline_on_target_move`（計画 18 S5。target が進んだだけ（head は同じ）のマージ結果にも MR パイプラインを積むか。
       Runbook `merge-blocked`）: `invalid`（`true` / `false` 以外）。未設定なら `false`（新しい head の最初のマージ結果だけが積む）。
       **`true` は S5 より前の動きに戻す**（着地ごとに開いている MR の数だけパイプライン）。読むのは `forge worker`（マージ可能性のジョブ）だけ。
       `FORGE_CI_MR_PIPELINE_ON_TARGET_MOVE` か設定ファイルの `[ci] mr_pipeline_on_target_move`
     - `config_key=jobs.safety_poll_ms`（計画 18 S2。暇なジョブ実行が通知なしでも自分で見に行く間隔、ミリ秒。Runbook `job-queue-backlog`）: `below_min`
       （0〜99。100 から）、`above_max`（60001 以上。60000 まで）、`invalid`（整数でない）。未設定なら 10000。**1000 にすると計画 18 より前と同じ
       「1 秒ごとに見る」動きに戻る**（通知が来なくても遅くならない）。`FORGE_JOBS_SAFETY_POLL_MS` か設定ファイルの `[jobs] safety_poll_ms`
     - `config_key=merge_queue.doc_batch_max` / `merge_queue.ci_batch_max`（計画 18 S7。文書のキューと CI のあるキューのバッチの上限、
       どちらも 1〜64）/ `merge_queue.doc_index_bytes`（列車の lint の文書の索引の大きさ。`0` か 1048576〜1073741824）。
       `FORGE_MERGE_QUEUE_DOC_BATCH_MAX` / `FORGE_MERGE_QUEUE_CI_BATCH_MAX` / `FORGE_MERGE_QUEUE_DOC_INDEX_BYTES` か `[merge_queue]`。
       既定は 32 / 4 / 67108864。
     - `config_key=merge_queue.safety_poll_ms` / `merge_queue.waiting_safety_poll_ms` / `merge_queue.workers`（計画 18 S3。マージキューの
       手番の待ちと数。Runbook `merge-queue` の「起床と安全網」）: `below_min`、`above_max`、`invalid`（整数でない）。
       `safety_poll_ms` は 100〜300000（未設定なら 30000）、`waiting_safety_poll_ms` は 100〜600000（未設定なら 60000）、`workers` は
       1〜8（未設定なら 2）。**2000 / 10000 / 1 にすると計画 18 より前の間隔と 1 本の手番に戻る**。`FORGE_MERGE_QUEUE_SAFETY_POLL_MS` /
       `FORGE_MERGE_QUEUE_WAITING_SAFETY_POLL_MS` / `FORGE_MERGE_QUEUE_WORKERS` か設定ファイルの `[merge_queue]`
     - `config_key=auto_merge.safety_poll_ms`（計画 18 S3。自動マージが全部の MR を自分で見回る間隔、ミリ秒。Runbook `auto-merge` の
       「起床と見回り」）: `below_min`（0〜99。100 から）、`above_max`（300001 以上。300000 まで）、`invalid`（整数でない）。未設定なら
       30000。**5000 にすると計画 18 より前の「5 秒ごと」に戻る**。`FORGE_AUTO_MERGE_SAFETY_POLL_MS` か設定ファイルの
       `[auto_merge] safety_poll_ms`
     - `config_key=merge_queue.train_lint_budget_ms`（計画 18 S1。着地の前の列車の doc-lint 1 回の上限、ミリ秒。Runbook `merge-queue`）: `below_min`
       （1〜99。100 から）、`above_max`（60001 以上。60000 まで）、`invalid`（整数でない）。未設定なら 10000。**0 は「列車の lint を切る」**
       （誤りではない。起動のログ `forge.serve.merge_queue_started` に `train_lint=off`）。`FORGE_MERGE_QUEUE_TRAIN_LINT_BUDGET_MS` か設定ファイルの
       `[merge_queue] train_lint_budget_ms`。リポジトリごとの `merge_queue.doc_lint_strict` はここではなく、リポジトリの `.forge/repo.yml`
       （その**ファイルが正しくない**ときは FG-SYS-0002 ではなく `FG-REPO-0024`。サーバーは起動する。マージキューでは
       `reason=doc_lint_repo_config_invalid` で MR が外れることがあり、直す MR は着地できる。Runbook `merge-queue` の
       「`.forge/repo.yml` が正しくない」）
     - `config_key=maintenance.*`（M3 O3。リポジトリの保守、Runbook `repo-maintenance`。`FORGE_MAINTENANCE_…` か設定ファイルの `[maintenance]`）:
       `interval_days`（1〜90、既定 7）、`window_start` / `window_end`（`HH:MM`、既定 02:00 / 05:00。同じ値は `empty_window`）、
       `window_time_zone`（IANA の名前、既定 `Asia/Tokyo`。形が違えば `invalid`、DB が知らない名前は worker の起動で `unknown`）、
       `prune_grace_hours`（24〜720、既定 48。worker の起動でマージの照合の窓と再試行の全体より短ければ `below_merge_window`、`needed_secs` つき）、
       `step_timeout_secs`（60〜3000、既定 1800）、`fsck_interval_days`（1〜365、既定 30）、`batch_max`（1〜1000、既定 10）、`refs_warn`
       （1〜10 000 000、既定 100 000）。範囲の外は `below_min` / `above_max`、整数でなければ `invalid`

   環境変数の値は `printenv | grep ^FORGE_` で見る。パスワードは値を表示しない（`database.password_file` は存在と権限だけを `ls -l` で確かめる）。設定の全体は `forge doctor` で検証できる（読み取りのみ。`config` の行に `config_key` と `reason` が出る。`--config` も同じように指定する）。
3. 直近で設定ファイル・環境変数を変更したか確認する（誰が・いつ・何を）。

## Mitigate

1. `config_key` が指す値を、設定ファイルまたは環境変数で修正する。
   `[MUTATING — needs approval]`（ロールバック: 変更前の設定ファイルに戻す）
2. サーバーを再起動する。`[MUTATING — needs approval]`

## Verify

- サーバーが起動し、ログに設定の検証の成功が出る（`forge doctor` の `config` の行が `ok` になることでも確認する）
- サーバーが起動し、`FG-SYS-0002` が新たに出ない

## Escalate

- 秘密情報（パスワード・トークン）の値が原因の場合、値をログや障害記録に貼らない。
- 原因が分かりにくかった場合は、`errors.toml` の `diagnosis` を改善するフィードバックアクションを起票する。
