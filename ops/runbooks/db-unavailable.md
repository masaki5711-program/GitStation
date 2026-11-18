# Runbook: db-unavailable（FG-DB-0001 / FG-DB-0002 / FG-DB-0023）

## Trigger

- `error_code=FG-DB-0001` DbUnavailable: DB の接続を取得できない（起動時、または稼働中）
- `error_code=FG-DB-0002` DbQueryFailed: 接続はあるが SQL 文が失敗した（タイムアウトなど）が続いている
- 利用者から「データベースに接続できません」の問い合わせ（問い合わせ番号 = `request_id`）
- ログに `error_code=FG-DB-0023` NotifyQueueBacklog（alert、`operation=wake_listen`）が出た、または `forge doctor` の `db.notify_queue` が同じコードで失敗した → 下の「notify queue」

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
     - `write_on_read_pool`（読み取りのプールの上の `25006`）: 書き込みが読み取りのプール（`forge_ro`、読み取り専用）に送られた。呼び出し元の不具合。再実行しない
     - `permission_denied`（`42501`）: ロールに権限がない。呼び出し元の不具合か、マイグレーションの GRANT の漏れ。再実行しない
     - `idle_in_transaction_timeout`（`25P03`）: 呼び出し元がトランザクションを開いたまま `database.idle_in_transaction_timeout_ms` を超えて放置し、DB がセッションを終了した。呼び出し元の不具合
     - `server_error` / `client_error`: `sqlstate`（と、値を含まない種類なら `db_message`）を読む
2. DB のサーバーの状態を確認する。まず `forge doctor`（読み取りのみ）で、`database` と `schema` の行を見る。
   - 開発機: `docker compose -f ops/dev/compose.yml ps`（`Git/` で実行）
   - 本番: `pg_isready -h <host> -p <port>`、DB のサーバーのログ（同じ時刻）
3. タイムアウトやロック待ちの場合は、DB に読み取り専用で接続して、遅い文と待たせている側を確認する。
   - `SELECT pid, state, now() - query_start AS running, wait_event_type, wait_event, application_name FROM pg_stat_activity WHERE datname = '<dbname>' ORDER BY running DESC;`（SQL の本文 `query` 列は値を含むことがあるので表示しない。必要なときも障害記録に貼らない）
   - `SELECT pid, pg_blocking_pids(pid) FROM pg_stat_activity WHERE cardinality(pg_blocking_pids(pid)) > 0;`
4. `acquire_timeout` の場合は、接続数（`SELECT count(*) FROM pg_stat_activity WHERE application_name = 'forge';`）と `database.write_max_connections`・`database.read_max_connections` を比べる（`forge doctor` の `db.connections` が予算を確かめる）。ログの `pool`（`write` / `read`）でどちらのプールかが分かる。

## Mitigate

1. 開発機で DB が止まっている場合: `docker compose -f ops/dev/compose.yml up -d --wait`（開発機のみ。承認は不要）。
2. 本番で DB のサーバーが止まっている場合: DB のサーバーを起動する。`[MUTATING — needs approval]`（ロールバック: なし。起動に失敗したら DB の担当者にエスカレーションする）
3. 設定の誤り（`auth_failed`、`tls`、`server_rejected` の `3D000`）: 設定ファイルか環境変数を直し、forge を再起動する。`[MUTATING — needs approval]`（ロールバック: 変更前の設定に戻して再起動する）
4. 長く続いている文がほかを待たせている場合: その文を止める（`SELECT pg_cancel_backend(<pid>);`）。`[MUTATING — needs approval]`（ロールバック: なし。止めた処理の利用者に再実行を依頼する）
5. 負荷で `acquire_timeout` が続く場合: `database.write_max_connections` か `database.read_max_connections` を上げて再起動する。DB のサーバー側の `max_connections` を超えないこと。`[MUTATING — needs approval]`（ロールバック: 元の値に戻して再起動する）

## notify queue（FG-DB-0023 NotifyQueueBacklog、plan 18 S2）

**Trigger**: PostgreSQL の NOTIFY の待ち行列が半分以上埋まっている
（`pg_notification_queue_usage()` ≥ 0.5。`context` の `usage` は 0〜1、`threshold` は 0.5）。知らせる経路は 2 つ:

- `forge serve` と `forge worker` の wake hub が、自分の LISTEN の接続で 5 秒ごとに待ち行列を読む。半分を超えたらログに
  `FG-DB-0023`（`operation=wake_listen`）を出す。超えた時に 1 回、続いている間は 1 分に 1 回。
- `forge doctor` の `db.notify_queue`。書き込み用のプール（プライマリ）で読む。スタンバイではこの関数はいつも 0 を返すので、
  スタンバイで読んだ 0 は「空いている」の証拠にならない。

**Impact**: 待ち行列はクラスター全体で 1 つ。満杯（1.0）になると、NOTIFY を送るトランザクションはすべて COMMIT で失敗する。
GitStation ではジョブの投入（`forge_jobs`）、イベントの番号付け（`forge_events_sequenced`）、CI ジョブを待ちにする変更が失敗する。
半分の時点ではまだ何も失敗していない（SEV3）。0.9 を超えたら SEV2。満杯で失敗が出ていれば SEV1。

**Diagnose（読み取りのみ）**:

1. **プライマリで** `SELECT pg_notification_queue_usage();` を 1 分おきに 2〜3 回読み、増えているか確かめる。
2. 待ち行列は、LISTEN しているセッションのうち一番読み進んでいないものの位置までしか縮まない。そのセッションを探す:
   `SELECT pid, usename, application_name, state, wait_event_type, wait_event, xact_start, state_change FROM pg_stat_activity WHERE xact_start IS NOT NULL OR wait_event = 'ClientWrite' ORDER BY xact_start NULLS LAST, state_change LIMIT 10;`
   （`query` 列は値を含むことがあるので表示しない）。原因になるのは次の 2 つの形:
   - 長い間 `state = 'idle in transaction'` のままの、LISTEN しているセッション（トランザクションが終わるまで通知を受け取らない）。
   - 通知を受け取る側のクライアントがソケットを読まないセッション: `wait_event = 'ClientWrite'` が消えない
     （`xact_start` が無いこともあるので、`state` と `wait_event` で探す）。
3. GitStation 自身の LISTEN の接続（`forge serve` と `forge worker` に 1 本ずつ、`application_name = 'forge'`）はトランザクションを開かない。
   まず同じクラスターのほかのクライアント（相乗りしているアプリ、開いたままの `psql`）を疑う。
4. GitStation のログに `operation=wake_listen` の `FG-DB-0001` / `FG-DB-0002`（LISTEN の接続の失敗）が続いていないか。

**Mitigate**:

1. 原因のセッションの持ち主に、トランザクションを終える（COMMIT / ROLLBACK）よう頼む。終われば待ち行列は自分で縮む。
2. 持ち主が対応できず、0.9 を超えて増え続ける場合: そのセッションを終了する（`SELECT pg_terminate_backend(<pid>);`）。
   `[MUTATING — needs approval]`（ロールバック: なし。終了したセッションの処理は持ち主がやり直す。GitStation 以外のセッションなら、その担当者の了解も取る）
3. それまでの間も、ジョブは保険の確認（`jobs.safety_poll_ms`、既定 10 秒）で見つかる。通知が届かないぶん、始まるのが遅れるだけ。

**Verify**: `forge doctor` の `db.notify_queue` が `ok`（`0.0 % used` に近い）。ログに新しい `FG-DB-0023` が出ていない。
COMMIT の失敗（`FG-DB-0002` で `db_message` に NOTIFY の待ち行列が出るもの）がない。

**LISTEN / NOTIFY について知っておくこと**:

- **LISTEN はトランザクション単位のプーラーを通すと働かない**（PgBouncer の `pool_mode = transaction` など）。wake hub の接続は
  PostgreSQL に直接つなぐこと。プーラー越しにすると、エラーは出ないまま通知が届かず、仕事は保険の確認だけで見つかり、
  `FG-DB-0024` が繰り返し出る（`job-queue-backlog.md`）。
- **NOTIFY は COMMIT を直列にする**。NOTIFY を送ったトランザクションは、COMMIT の間クラスター全体で 1 つのロックを取る。
  ジョブの投入の遅延が書き込みの量と一緒に伸びてきたら、次を見る（読み取りのみ）:
  - `SELECT count(*) FROM pg_locks WHERE locktype = 'object' AND classid = 1262 AND objid = 0 AND NOT granted;`
    — このロックを待っている COMMIT の数。いつも 0 でないなら、NOTIFY の直列化が効いている。
  - `pg_stat_activity` で `state = 'active'`、`wait_event_type = 'Lock'` の COMMIT が並んでいないか。
  - 待ち行列の使用率（上の手順 1）が上がっていないか。
  対処は開発者（Claude のセッション）に渡す（通知をまとめる、送る数を減らす）。設定で NOTIFY を止める手段はない。

## Verify

- 新しい FG-DB-0001 / FG-DB-0002 が出ていない
- 起動時のログに `forge.db.connected`（`db_target`、`tls_mode`、`server_version_num`）が出ている

## Escalate

- DB のサーバー自体の障害（ディスク、クラッシュ、レプリケーション）は DB の担当者へ。
- パスワードや接続文字列の値を、障害記録やチャットに貼らない。
