# Runbook: db-unavailable（FG-DB-0001 / FG-DB-0002）

## Trigger

- `error_code=FG-DB-0001` DbUnavailable: DB の接続を取得できない（起動時、または稼働中）
- `error_code=FG-DB-0002` DbQueryFailed: 接続はあるが SQL 文が失敗した（タイムアウトなど）が続いている
- 利用者から「データベースに接続できません」の問い合わせ（問い合わせ番号 = `request_id`）

## Impact

- FG-DB-0001 が続く場合は、DB を使う機能がすべて使えない（SEV1〜SEV2）。
- FG-DB-0002 が 1 種類の `query` に集中している場合は、その機能だけの障害（SEV3）。

## Diagnose（読み取りのみ）

1. ログを `error_code` と時間帯で絞り、`context` を読む。`target` は `host:port/dbname`（パスワードは出ない）。
   - FG-DB-0001 の `reason`
     - `connect_refused`: DB のサーバーが待ち受けていない（停止している、ポートが違う）
     - `connect_timeout`: 応答がない（ネットワーク、ファイアウォール、DB の過負荷）。`timeout_ms` が付く
     - `auth_failed`: ユーザー名かパスワードが違う。`database.password_file` の中身を確認する（値は表示しない）
     - `server_rejected`: 認証以外の理由で接続を拒否された。`sqlstate` を見る（`3D000` = DB がない、`53300` = 接続数の上限）
     - `tls`: TLS の確立に失敗した（サーバーが TLS に対応していない、証明書の検証に失敗した）。`database.tls_mode` と `database.tls_root_cert` を確認する
     - `version`: PostgreSQL 16 未満。`server_version_num` が付く
     - `acquire_timeout`: プールに空きがない。長く接続を握っている処理か、接続数の不足
     - `pool_closed`: 停止処理の途中
     - `io` / `driver`: `source` を読む
   - FG-DB-0002 の `reason`
     - `statement_timeout`（SQLSTATE `57014`）: 文が `database.statement_timeout_ms` を超えた
     - `lock_timeout`（`55P03`）: ロックの待ちが `database.lock_timeout_ms` を超えた
     - `connection_lost`: 実行中に接続が切れた（DB の再起動、ネットワーク）
     - `server_error` / `client_error`: `sqlstate` と `source` を読む。`query` が呼び出し元の名前
2. DB のサーバーの状態を確認する。
   - 開発機: `docker compose -f ops/dev/compose.yml ps`（`Git/` で実行）
   - 本番: `pg_isready -h <host> -p <port>`、DB のサーバーのログ（同じ時刻）
3. タイムアウトやロック待ちの場合は、DB に読み取り専用で接続して、遅い文と待たせている側を確認する。
   - `SELECT pid, state, now() - query_start AS running, wait_event_type, left(query, 80) FROM pg_stat_activity WHERE datname = '<dbname>' ORDER BY running DESC;`
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
