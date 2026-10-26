# Runbook: db-unavailable（FG-DB-0001 / FG-DB-0002）

## Trigger

- `error_code=FG-DB-0001` DbUnavailable: DB の接続を取得できない（起動時、または稼働中）
- `error_code=FG-DB-0002` DbQueryFailed: 接続はあるが SQL 文が失敗した（タイムアウトなど）が続いている
- 利用者から「データベースに接続できません」の問い合わせ（問い合わせ番号 = `request_id`）

## Impact

- FG-DB-0001 が続く場合は、DB を使う機能がすべて使えない（SEV1〜SEV2）。
- FG-DB-0002 が 1 種類の `query` に集中している場合は、その機能だけの障害（SEV3）。

## Diagnose（読み取りのみ）

1. ログを `error_code` と時間帯で絞り、`context` を読む。`target` は `host:port/dbname`（パスワードは出ない）。文の失敗には `query`（文の名前）が付く。
   - FG-DB-0001 の `reason`（起動時でも、稼働中の文の実行時でも出る）
     - `connect_refused`: DB のサーバーが待ち受けていない（停止している、ポートが違う）
     - `connect_timeout`: 応答がない（ネットワーク、ファイアウォール、DB の過負荷）。forge 側のタイマーが切れた場合は `timeout_ms` と `timeout_key`（どの設定のタイムアウトか）が付く。OS の TCP タイムアウトの場合は付かない
     - `auth_failed`: ユーザー名かパスワードが違う。`database.password_file` の中身を確認する（値は表示しない）
     - `server_rejected`: 認証以外の理由で接続を拒否された。`sqlstate` と `db_message` を見る（`3D000` = DB がない）
     - `tls`: TLS の確立に失敗した（サーバーが TLS に対応していない、証明書の検証に失敗した）。`database.tls_mode` と `database.tls_root_cert` を確認する
     - `version`: PostgreSQL 16 未満。`server_version_num` が付く
     - `acquire_timeout`: プールに空きがない。長く接続を握っている処理か、接続数の不足
     - `pool_closed`: 停止処理の途中
     - `connection_lost`: 実行中に接続が切れた（DB の再起動、ネットワーク）
     - `server_shutdown`（SQLSTATE `57P01`〜`57P03`）: DB のサーバーが停止中、または起動中
     - `too_many_connections`（`53300`）: DB のサーバー側の接続数の上限に達した
     - `read_only`（`25006`）: DB が読み取り専用（スタンバイに接続している、リカバリ中）
     - `resource_exhausted`（`53xxx`）: DB のサーバーのディスク不足・メモリ不足など。DB の担当者へ
     - `server_fault`（`58xxx` / `XXxxx`）: DB のサーバー内部の障害（I/O エラー、データの破損）。DB の担当者へすぐにエスカレーションする
     - `ping_timeout`: 準備確認の `SELECT 1` が `database.acquire_timeout_ms` 以内に返らなかった（接続が半分切れている、DB が応答しない）
     - `driver`: `source` を読む
   - FG-DB-0002 の `reason`（接続は使える状態で、文そのものが失敗した）
     - `statement_timeout`（SQLSTATE `57014`）: 文が `database.statement_timeout_ms` を超えた
     - `lock_timeout`（`55P03`）: ロックの待ちが `database.lock_timeout_ms` を超えた
     - `serialization`（`40001`）/ `deadlock`（`40P01`）: 同時実行の競合。再実行してよい。頻発するなら呼び出し元を見直す
     - `constraint`（`23xxx`）: 制約の違反。`constraint` と `table` が付く。行の値はログに出さない
     - `row_not_found`: あるはずの行がない（呼び出し元の不具合の可能性）
     - `idle_in_transaction_timeout`（`25P03`）: 呼び出し元がトランザクションを開いたまま `database.idle_in_transaction_timeout_ms` を超えて放置し、DB がセッションを終了した。呼び出し元の不具合
     - `server_error` / `client_error`: `sqlstate`（と、値を含まない種類なら `db_message`）を読む
2. DB のサーバーの状態を確認する。まず `forge doctor`（読み取りのみ）で、`database` と `schema` の行を見る。
   - 開発機: `docker compose -f ops/dev/compose.yml ps`（`Git/` で実行）
   - 本番: `pg_isready -h <host> -p <port>`、DB のサーバーのログ（同じ時刻）
3. タイムアウトやロック待ちの場合は、DB に読み取り専用で接続して、遅い文と待たせている側を確認する。
   - `SELECT pid, state, now() - query_start AS running, wait_event_type, wait_event, application_name FROM pg_stat_activity WHERE datname = '<dbname>' ORDER BY running DESC;`（SQL の本文 `query` 列は値を含むことがあるので表示しない。必要なときも障害記録に貼らない）
   - `SELECT pid, pg_blocking_pids(pid) FROM pg_stat_activity WHERE cardinality(pg_blocking_pids(pid)) > 0;`
4. `acquire_timeout` の場合は、接続数（`SELECT count(*) FROM pg_stat_activity WHERE application_name = 'forge';`）と `database.max_connections` を比べる。

## Mitigate

1. 開発機で DB が止まっている場合: `docker compose -f ops/dev/compose.yml up -d --wait`（開発機のみ。承認は不要）。
2. 本番で DB のサーバーが止まっている場合: DB のサーバーを起動する。`[MUTATING — needs approval]`（ロールバック: なし。起動に失敗したら DB の担当者にエスカレーションする）
3. 設定の誤り（`auth_failed`、`tls`、`server_rejected` の `3D000`）: 設定ファイルか環境変数を直し、forge を再起動する。`[MUTATING — needs approval]`（ロールバック: 変更前の設定に戻して再起動する）
4. 長く続いている文がほかを待たせている場合: その文を止める（`SELECT pg_cancel_backend(<pid>);`）。`[MUTATING — needs approval]`（ロールバック: なし。止めた処理の利用者に再実行を依頼する）
5. 負荷で `acquire_timeout` が続く場合: `database.max_connections` を上げて再起動する。DB のサーバー側の `max_connections` を超えないこと。`[MUTATING — needs approval]`（ロールバック: 元の値に戻して再起動する）

## Verify

- 新しい FG-DB-0001 / FG-DB-0002 が出ていない
- 起動時のログに `forge.db.connected`（`db_target`、`tls_mode`、`server_version_num`）が出ている

## Escalate

- DB のサーバー自体の障害（ディスク、クラッシュ、レプリケーション）は DB の担当者へ。
- パスワードや接続文字列の値を、障害記録やチャットに貼らない。
