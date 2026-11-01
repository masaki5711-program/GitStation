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
     - `config_key=hosting.deletion_grace_days`（D5 MR3。削除したプロジェクトを復元できる日数）: `below_min`（0。1 日以上）、`above_max`（366 以上。365 日まで）、
       `invalid`（整数でない）。未設定なら 7 日。`FORGE_HOSTING_DELETION_GRACE_DAYS` か設定ファイルの `[hosting] deletion_grace_days`

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
