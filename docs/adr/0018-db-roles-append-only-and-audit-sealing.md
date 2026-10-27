# ADR-0018: DB のロールと導入の手順、接続の種類、追記のみの強制、監査ログの封印

**Date**: 2026-10-27
**Status**: accepted（ADR-0016 の対の ADR。**ADR-0016 と同時に受理する**（ユーザー決定 2026-10-27）。ADR-0014 を改訂する）
**Deciders**: Masaki Fujikawa（ユーザー）、Claude

## Context

ADR-0016 は監査ログとイベントを「追記のみ」とし、監査の改ざんを検知できることを M1 の完了条件にした。初版の「アプリのロールが
表の所有者で、トリガーと GUC で守る」方式は、アプリのロールがトリガーを外せるので名目にすぎない（ADR-0016 のレビュー第 1〜4 回）。
今の `forge-db` は 1 つのプールでマイグレーションも行い（`migrate()` がプールの接続を `detach()` して DDL を流す）、`search_path` を設定せず、
URL の `options=` を拒否し、`error_map` は SQLSTATE `25006` を再試行できる `FG-DB-0001 reason=read_only` にしている。
前提: PostgreSQL ≥ 16、単一サーバー、オンプレミス、DB の superuser は人（DB の管理者）が持つ。PgBouncer のトランザクションプーリングは
`SET ROLE`、LISTEN、advisory lock と両立しないので **非対応**とする。

**ユーザー決定（2026-10-27）**
1. **信頼の境界は「防止ではなく検知」**: superuser と、表の所有者のロール（`forge_owner`）に入れる人は DB の中の記録を何でも変えられる。この ADR が DB の中で防ぐのは
   **アプリ（`forge_app`）の不具合と、アプリの資格情報の漏えい**まで。それより強い権限による改ざんは、DB の外への封印の送出・突き合わせ・全件の再計算で検知する。
2. **送出先に届かないときは正しさを優先**: 送出先との比較が成功するまで、連携の履歴は `503`、監査の番号付けは停止。forge の画面と git の操作は動き続ける（§5）。
3. **古い行の改ざんの検知の遅れは最大 7 日**（全件の再計算の一巡、§4）。

## Decision

### 1. ロール（導入の手順で superuser が作る）

| ロール | LOGIN | 用途 | 権限 |
|---|---|---|---|
| `forge_owner` | NOLOGIN | スキーマ `forge` と全テーブルの所有者。DB の所有者（`public` も持つ） | DDL |
| `forge_migrator` | LOGIN | `forge db migrate`（`database.migrate_url`、`application_name = forge-migrate`）。`forge_owner` のメンバー。接続後に `SET ROLE forge_owner` | — |
| `forge_seq` | NOLOGIN | 番号付けと epoch の関数（`assign_seq()`、`rotate_epoch()`、`confirm_sink_compare()`、`audit_seal_resync()`、`sink_override()`）の所有者 | `forge` の USAGE・CREATE。`events`・`audit_log` の SELECT と UPDATE(`seq`, `sequenced_at`)。`event_feed_state` の SELECT・UPDATE。`audit_seal` の SELECT・INSERT。`audit_log` の INSERT（自分の監査の行）。EXECUTE: `cluster_identity()`（`confirm_sink_compare()` と `rotate_epoch()` が `SECURITY DEFINER` として `forge_seq` の権限で呼ぶため） |
| `forge_purge` | NOLOGIN | 削除・伏せ字の関数（`events_retention()`、`audit_redact_expired()`、`audit_purge()`、`audit_anonymize_user()`、`events_redact_user()`）の所有者 | `forge` の USAGE・CREATE。`events`・`audit_log` の SELECT・DELETE。`audit_log` の個人情報の列の UPDATE、`events` の `payload` の UPDATE（`events_redact_user()` だけ。§3）。`event_feed_state` の SELECT・UPDATE（`retained_from`）。`event_subscribers`・`retention_policy`・`audit_seal` の SELECT（封印の範囲と `hash_version` を引くため。`audit_log` の BEFORE UPDATE (c) は `current_user` を見るので呼び出し側の権限で動く）。`audit_log` の INSERT（自分の監査の行） |
| `forge_app` | LOGIN | `serve` / `worker`（`database.url`）。`forge_ro` のメンバー | `events`・`audit_log`: 列単位の INSERT（**`id`・`seq`・`sequenced_at`・`txid`・`occurred_at` を除く**。これらは既定値かトリガーだけが決める）と SELECT。**UPDATE は持たない**。`event_feed_state`: SELECT だけ。`audit_seal`: INSERT、UPDATE(`shipped_at`)、SELECT。状態のテーブル: 必要な DML。TRUNCATE・TRIGGER・REFERENCES はなし。EXECUTE: `assign_seq()`、`rotate_epoch()`、`confirm_sink_compare()`、`events_retention()`、`audit_redact_expired()`、`cluster_identity()` |
| `forge_ro` | NOLOGIN | 読み取りのプール（`forge_app` で接続し `SET ROLE forge_ro`） | 表の台帳（ADR-0016 §1）の `ro_columns` に載る列だけの列単位の SELECT。資格情報（トークンのハッシュ、秘密の暗号文）と監査の個人情報の列は載せない |
| `forge_maint` | LOGIN | `forge admin audit purge` / `anonymize-user` / `seal-resync` / `sink-override` / `events redact-user` / `ddl-audit ack`（`database.maint_url`。本番では承認済みの Runbook の手順） | EXECUTE: `audit_purge()`、`audit_anonymize_user()`、`events_redact_user()`、`audit_seal_resync()`、`sink_override()`。`ddl_audit_ack` の INSERT |

- `forge_owner` は `forge_seq`・`forge_purge` のメンバー（関数の所有者を移すのに要る）なので、**所有者は信頼の境界の内側**。
  `forge_app` と `forge_maint` は、`forge_owner`・`forge_seq`・`forge_purge` の **いずれのメンバーでもない**。
- 関数の所有権を移すため、`forge_seq`・`forge_purge` に `forge` スキーマの CREATE を与える（PostgreSQL の要件）。
- `ALTER ROLE forge_app SET search_path = forge, public`、`forge_migrator` は `public, forge`（`_sqlx_migrations` は `public`）。
  **マイグレーションの中のオブジェクトはすべて `forge.` で修飾する**（ゲートが検査する）。URL の `options=` は今の方針どおり拒否する。
- `REVOKE TEMP ON DATABASE forge FROM PUBLIC`、`REVOKE CREATE ON SCHEMA public FROM PUBLIC`。最初のマイグレーションで `GRANT SELECT ON public._sqlx_migrations TO forge_app, forge_ro`。
- **関数の権限**: すべての `SECURITY DEFINER` の関数は、作成と同じファイルで `REVOKE ALL ON FUNCTION … FROM PUBLIC` し、上の表のロールにだけ GRANT する。
  `SET search_path = pg_catalog, forge, pg_temp`（`pg_temp` は最後）で、本文はすべて `forge.` で修飾する。
- **関数の自己監査**: 削除・伏せ字・再同期・epoch・比較の関数は、**同じトランザクションで自分のシステムの監査の行**（`action` = `audit.purged` / `audit.redacted` / `audit.anonymized` /
  `events.redacted` / `events.retention` / `audit.seal_resynced` / `feed.epoch_rotated` / `feed.sink_compared` / `feed.sink_override`、範囲、件数、理由、`session_user`）を書く。
  **0 行しか変えなかった保持・伏せ字の呼び出しは行を書かない**（毎回の空の呼び出しで監査を埋めない）。

**導入の手順（superuser。`ops/db/bootstrap.sql`、版 `bootstrap_version` 付き、Runbook `db-bootstrap.md`）**

| 順 | 内容 |
|---|---|
| 1 | ロールの作成とメンバーの関係。`forge_app`・`forge_maint` は `NOSUPERUSER NOCREATEROLE NOCREATEDB NOBYPASSRLS NOREPLICATION` |
| 2 | `CREATE DATABASE forge OWNER forge_owner`、`REVOKE TEMP`、`public` の CREATE の取り消し、ALTER ROLE の `search_path` |
| 3 | `CREATE SCHEMA forge AUTHORIZATION forge_owner`、`forge.ddl_audit`・`forge.ddl_audit_ack`・`forge.bootstrap_log`（`forge_owner` が所有、追記のみのトリガー付き）、`forge.guarded_objects`、ガードの関数 |
| 4 | DDL のイベントトリガー（§3。3 の表と関数が先にあること） |
| 5 | `forge.cluster_identity()`（§5）: `pg_control_system()`・`pg_control_checkpoint()` から `system_identifier` と `timeline_id` だけを返す `SECURITY DEFINER`。**所有者は superuser**（`pg_control_*` の実行に要る。`database.privileged_members` の監視の外なので、`doctor` が所有者と本文のハッシュを固定の値と比べる）。`SET search_path = pg_catalog, pg_temp`、`REVOKE ALL … FROM PUBLIC`、`GRANT EXECUTE … TO forge_app, forge_seq`（`doctor db.bootstrap` が両方の `has_function_privilege` を確かめる） |
| 6 | `forge.bootstrap_log` に `(bootstrap_version, txid)` を書く（手順 3〜5 と同じトランザクション。§3 の DDL の照合に使う） |
| 7 | `forge doctor db.bootstrap` で 1〜6 を確かめる（ロールの属性、メンバーの関係、上の表の GRANT のすべて、`bootstrap_version`） |

### 2. 接続の種類（`forge-db` の公開 API の変更）

| 接続 | 作り方 | 用途 |
|---|---|---|
| 書き込みのプール | `forge_app`。**`forge-db` の外に出さない** | 型付きのコマンド（ADR-0016 §6）、シーケンサー、購読者、ジョブ、管理者の全体の監査の読み取り（型付きの問い合わせ） |
| 読み取りのプール | 型 **`ReadPool`**（newtype。書き込みのプールと型で区別）。`forge_app` で接続し、`after_connect` で `SET ROLE forge_ro` と `default_transaction_read_only = on`。`Db::pool()` はこれを返す | 画面と API の読み取り、`status()`、`check_schema()` |
| マイグレーションの接続 | `Db::connect_migrator(&DatabaseConfig)`: プールではない 1 本の接続、`migrate_url`、`SET ROLE forge_owner` の後に `current_user` を確かめる（違えば `FG-SYS-0002 reason=wrong_role`）。`migrate()` はこの接続を取る。`status()` は実行器（`PgExecutor`）について汎用 | `forge db migrate` |
| LISTEN の接続 | `Db::listen_options()`: `statement_timeout`・`idle_in_transaction_session_timeout` を付けず、TCP keepalive を付ける。切れたら再接続し、直後に 1 回状態を確かめる | ADR-0016 §7 |

- **誤った経路の書き込みのエラー**: `ReadPool` の上で起きた `25006` と、どの接続でも `42501` は、再試行しない `FG-DB-0002 reason=write_on_read_pool` / `reason=permission_denied`
  にする（不具合の印。今の `25006 → FG-DB-0001 read_only` は書き込みのプールの上だけに残す。スタンバイへの接続のため）。
- **接続の予算**: 既定はプロセスごとに書き込み 10・読み取り 10（`database.write_max_connections`・`database.read_max_connections`）。
  合計 = `(serve + worker) × (書き込み + 読み取り) + LISTEN 1 + マイグレーション 1 + 保守 1`。既定で 2 × 20 + 3 = 43。
  `forge doctor db.connections` がこの合計を `max_connections - superuser_reserved_connections` の 80 % 以下か確かめる。
- **`ReadPool` と `forge_ro` は事故の防止**であって、迂回の防止ではない（同じプロセスが書き込みの資格情報を持ち、`RESET ROLE` で外せる）。迂回の防止は、ゲートと ADR-0016 の遅延制約トリガーが担う。

### 3. 追記のみの強制

- **表ごとのトリガー関数**（共通の 1 つの関数で存在しない列を参照しないよう、表ごとに分ける。SQLSTATE `XF001`、すべて `ENABLE ALWAYS`）:

| 表 | BEFORE INSERT | BEFORE UPDATE で通すもの | BEFORE DELETE | TRUNCATE |
|---|---|---|---|---|
| `events` | `seq`・`sequenced_at` が NULL でなければ拒否。`NEW.txid := pg_current_xact_id()`、`NEW.occurred_at := now()` を強制 | (a) `current_user = 'forge_seq'` で `seq`・`sequenced_at` が NULL から値になり、新しい `seq` が `high_seq` より大きく `high_seq + 1000` 以下、ほかの列が同一。(b) `current_user = 'forge_purge'` で `payload` だけが変わる（`events_redact_user()`。§1 の表） | `forge_purge` だけ | 拒否 |
| `audit_log` | 同上（`audit_high_seq` に対して） | (a) 同上。(c) `current_user = 'forge_purge'` で個人情報の列（`actor_label`・`source_ip`・`user_agent`）だけを NULL にし、**封印の対象の列**が同一。対象の列の一覧は、その行が封印済みならその範囲の `hash_version`、未封印なら現在の版のもの | `forge_purge` だけ | 拒否 |
| `audit_seal` | — | `shipped_at` が NULL から値になり、**`shipped_at` 以外のすべての列が同一** | 拒否 | 拒否 |
| `ddl_audit`・`ddl_audit_ack`・`bootstrap_log` | — | 拒否 | 拒否 | 拒否 |

  テスト `ddl_audit_rejects_update_and_delete`（`forge-db` の結合テスト。導入の手順の SQL をそのまま使う）で `ddl_audit` のトリガーを確かめる。
- **番号付け**: `forge.assign_seq(max_batch int, include_audit boolean)`（`forge_seq` が所有、`max_batch ≤ 1000`）が advisory lock、`event_feed_state` の `FOR UPDATE`、
  `seq IS NULL` の行を `id` の順に取り **`seq = high_seq + 1 + バッチの中の順位`** で振り、`to_seq - from_seq + 1 = 件数` を関数の中で確かめ（違えば例外）、
  `high_seq`・`audit_high_seq`・`sequencer_heartbeat` を更新して `(from_seq, to_seq, audit_from, audit_to)` を返す。監査の番号付けは `audit_sequencing_enabled` が true のときだけ。
- **削除・伏せ字の関数**: `events_retention()`（引数なし、`retention_policy` を読み、7 日の下限、1 回で 1 バッチ、`event_subscribers` を読んで遅れた購読者を守る）、
  `audit_redact_expired()`（関数の中に 180 日 / 365 日の下限）、`audit_purge(before date)`（365 日の下限、封印の範囲の単位）、`audit_anonymize_user(user_id)`、
  `events_redact_user(user_id)`（緊急の手順だけ。`events.payload` の中の、`DomainEvent` が宣言した表示名の項目（`display_name_paths`）だけを置き換える）。`retention_policy` は `forge_owner` だけが書ける。
- **DDL の記録は事実だけ**: イベントトリガー（`ddl_command_end` と `sql_drop`。関数は `forge_owner` が所有）が `forge.ddl_audit` に
  `occurred_at`、`txid`、`session_user`、`application_name`、`command_tag`、`object_type`、`object_identity`、`parent_identity` を書き、`RAISE WARNING` も出す。
  **`expected` の列は持たない**（DDL の時点では、ファイルの末尾で書かれる `db.migrate` の行がまだないため）。
- **守る対象**: `forge.guarded_objects`（`audit_log`・`events`・`audit_seal`・`event_feed_state`・`ddl_audit`・`ddl_audit_ack`・`bootstrap_log`・`guarded_objects` と、それらのトリガー・トリガー関数、
  §1 のすべての `SECURITY DEFINER` の関数）。一覧は ADR-0016 の表の台帳から生成し、テストが一致を確かめる。`object_identity` か `parent_identity` が守る対象なら、
  **コマンドの種類を問わず**（`CREATE OR REPLACE FUNCTION`、`ALTER TABLE … OWNER TO / ALTER COLUMN / RENAME / … TRIGGER`、`DROP`、GRANT / REVOKE を含む）守る対象への変更とする。
- **判定は読み取りの時点**: ビュー `forge.ddl_audit_status` が `ddl_audit` の各行を次と照合し、`unexpected` を求める。
  - 守る対象への変更: 同じ `txid` に `action = 'db.migrate'` の監査の行があり、その `details.guarded_changes` が対象の名前を挙げていれば expected。
  - それ以外の変更: 同じ `txid` に `db.migrate` の行があり、`session_user = 'forge_migrator'`・`application_name = 'forge-migrate'` なら expected。
  - 導入の手順の DDL: 同じ `txid` の `bootstrap_log` の行があれば expected。
  - **最初のマイグレーション**は `audit_log` を作った後、同じファイルの末尾で自分の `db.migrate` の行を書く（同じ `txid` なので、それより前の DDL も照合できる）。
  `forge doctor` は未確認の unexpected の行を `FG-DB-0009 reason=unexpected_ddl` の警報にする。確認は `forge admin ddl-audit ack <id>`（`forge_maint` だけ）。
  これらは **事故の水準の制御**で、所有者の権限を持つ人の偽装は防げない（Context の決定 1）。
- **ゲート**（CI、エージェントモデル側で先に追加）: `guarded_objects` と **同じ定義**で、守る対象に触れるマイグレーションが `guarded_changes` を含む `db.migrate` の行を書いていること、
  すべてのオブジェクトが `forge.` で修飾されていること。初版の「`DISABLE TRIGGER` は同じファイルで戻す」ゲートはやめる。
- **ロールと権限の監視**（`forge doctor append_only.guards`。イベントトリガーはロールなど共有のオブジェクトを見ないため）:
  - `pg_has_role('forge_app' / 'forge_maint', r, 'MEMBER')` が `forge_owner`・`forge_seq`・`forge_purge` のすべてで false。両者の `rolsuper`・`rolcreaterole`・`rolcreatedb`・`rolbypassrls`・`rolreplication` が false。
  - `forge_owner`・`forge_seq`・`forge_purge` のメンバーの一覧が `database.privileged_members` と一致（変化は `reason=role_membership_changed`）。
  - トリガーがあり `tgenabled = 'A'`、イベントトリガーがあり `pg_event_trigger.evtenabled = 'A'` で所有者が `forge_owner`、`cluster_identity()` の所有者と本文のハッシュが固定の値。
  - `has_function_privilege('forge_app', f, 'EXECUTE')` が `audit_purge`・`audit_anonymize_user`・`events_redact_user`・`audit_seal_resync`・`sink_override` で false、
    `assign_seq`・`rotate_epoch`・`confirm_sink_compare`・`events_retention`・`audit_redact_expired`・`cluster_identity` で true。
  - `forge_app` に禁止の表・列の権限がない（`id`・`txid`・`occurred_at`・`seq`・`sequenced_at` の INSERT、`events`・`audit_log` の UPDATE、TRUNCATE など）、所有者が `forge_app` でない、
    ADR-0016 の `audited` の表と `events` のトリガーがある。崩れたら `FG-DB-0009 reason=guard_missing`。

### 4. 監査ログの封印

- **ハッシュの版**: `audit_seal.hash_version smallint`。版ごとに **対象の列の一覧と直列化**を固定し、`forge-db` の `seal::v1` のようにコードに 1 か所で定義する。
  対象の列を増やすときは新しい版を作り、古い範囲は古い版で検証する。テスト `audit_seal_v1_golden_vector`。
- **版 1**: 対象の列 `seq`, `occurred_at`, `actor_kind`, `actor_user_id`, `actor_via_id`, `credential_id`, `act_as_claimed`, `act_as_key_id`, `via_assertion_id`, `effective_scope`,
  `action`, `outcome`, `resource_type`, `resource_id`, `project_id`, `request_id`, `error_code`, `occurrences`, `details`（外すのは個人情報の列 `actor_label`・`source_ip`・`user_agent` だけ）。
  直列化: 列名をキーにした JSON オブジェクトを RFC 8785 で正規化する。`bigint` は 10 進の文字列、`timestamptz` は UTC の ISO 8601 でマイクロ秒まで、`text[]` は順序をそのまま、
  `inet` は PostgreSQL の正規の文字列、`jsonb` は RFC 8785、NULL は `null`。**`details` の数は IEEE 754 で正確な整数の範囲（±2^53 − 1）だけを数として持ち、それ以外は文字列にする**
  （`AuditDetails` の直列化の規則とテスト `audit_details_numbers_are_jcs_safe`）。
- **計算**: 行のハッシュ = その JSON の SHA-256。`rows_sha256` = 行のハッシュを `seq` の順に連結した SHA-256。
  `seal_sha256 = sha256(be16(hash_version) ‖ be16(seal_epoch) ‖ be64(first_seq) ‖ be64(last_seq) ‖ be64(events_high_seq) ‖ rows_sha256 ‖ prev_seal_sha256)`。
  最初の封印の `prev_seal_sha256 = sha256("forge-audit-genesis")`。`hmac = HMAC-SHA256(key[key_id], seal_sha256)`、鍵は DB の外のシークレットファイル（ADR-0013）。
- **表**: `audit_seal(id, kind ('normal'|'imported'|'restore'), hash_version, seal_epoch, key_id, first_seq, last_seq, row_count, events_high_seq, rows_sha256, prev_seal_sha256, seal_sha256, hmac, sealed_at, shipped_at)`、
  `UNIQUE (first_seq)`、`UNIQUE (last_seq)`、`CHECK (last_seq >= first_seq)`、`CHECK (row_count = last_seq - first_seq + 1)`。
- **ジョブ `audit.seal`**（worker。1 分ごと、または未封印が 1,000 行を超えたら）: `pg_try_advisory_xact_lock`、前の封印の `last_seq + 1` から `audit_high_seq` までを封印する。
  範囲に欠番があれば封印せず `FG-DB-0011 reason=gap`。未封印の行が 5 分より古いまま残れば `FG-DB-0015`。
- **`events_high_seq` の意味**: 封印ごとにしか進まない粗い値で、**イベントの行そのものは封印しない**。イベントの巻き戻しの検出は封印の周期（1 分）の粒度にとどまる。
- **送出先**: 別のホストで動く `forge seal-sink`（同じバイナリのサブコマンド）。相互 TLS、追記だけのファイル、`POST /seals`（同じ `id` で同じ内容なら冪等）と `GET /seals?from=` / `GET /seals/latest`。
  保持は監査ログより長く（既定は削除しない）。10 分以上送れない・読めないなら `FG-DB-0012`。
- **再計算で行がないことを許す範囲**: 封印の範囲の行が DB にないのを不一致にしないのは、(1) その範囲が、封印済みで送出先にもあるシステムの監査の行 `audit.purged`
  （`before` が 365 日の下限以下）の範囲に含まれるとき、(2) `kind = 'imported'` の封印で、封印済みで送出先にもある `audit.restore_gap` の範囲に含まれるとき、だけ。
  それ以外は `FG-DB-0011 reason=gap`（所有者による削除を見逃さない）。**許可の根拠になる行は消さない**: `audit_purge()` は `actor_kind = 'system'` の `audit.purged` と `audit.restore_gap` の行を削除の対象から除く（2 回目以降の削除で、前回の範囲の根拠が消えて誤警報になるのを防ぐ）。根拠として信頼するのは `actor_kind = 'system'` で、封印済みかつ送出先に送出済みの行だけ（`forge_app` が同名の `action` の行を書いても根拠にならない）。`audit.restore_gap` より前に一部だけ取り込まれた範囲は、行の再計算ができない（封印の値と HMAC の照合だけになる）と明記する。
- **検知の遅れの上限**（ユーザー決定 3）:

| 検査 | 範囲 | 周期 | 検知の遅れ |
|---|---|---|---|
| `audit.seal_compare_sink` | DB と送出先の封印の全件（値と HMAC） | 毎日 + 起動時 | ≤ 24 時間 |
| `forge doctor audit.seal_verify` | 直近 24 時間の封印の行の再計算 | 監視は 1 時間ごと | 最近の行は ≤ 1 時間 |
| `audit.seal_reverify` | すべての封印の行の再計算を、7 日で一巡するよう 1 時間ごとに少しずつ | 1 時間ごと | 古い行は ≤ 7 日 |

  不一致・欠落・送出先にあって DB にない封印は `FG-DB-0011`（alert）。`audit_purge()` は封印の範囲ごと消し、`audit_seal` の行は残す（鎖は切れない）。

### 5. 復元とロールバックの検出、封印の再同期

- **プロセスごとの比較の状態**（ユーザー決定 2）: `serve` と `worker` はそれぞれ、比較の状態を `unknown` / `ok` で持つ。起動時と **DB への再接続のたびに `unknown`** に戻り、
  `unknown` の間は閉じた状態（連携の履歴は `503`（`FG-DB-0012`）、そのプロセスは監査の番号付けを有効にしない）。forge の画面と git の操作は動き続ける。
  内部の購読者は動き続ける（DB の中で一貫しているため）。`forge doctor feed.rollback_check` が状態を報告する。
- **比較**（起動時、5 分ごと、再接続の後。`serve` は待ち受けの前にも）: `forge.cluster_identity()` と送出先の **最新の** 封印（`GET /seals/latest`）を読み、
  `system_identifier`・`timeline_id` の変化、`audit_high_seq < 最新の封印の last_seq`、`high_seq < 最新の封印の events_high_seq`（スナップショットへの巻き戻し）を調べる。
  `cluster_identity()` がない・実行できないなら起動しない（`FG-DB-0009 reason=guard_missing`）。
- **比較の書き手**: `forge.confirm_sink_compare(system_identifier, timeline_id, last_seal_seq, last_seal_events_high)`（`forge_seq` が所有、`forge_app` が EXECUTE）が、
  **自分で `cluster_identity()` と `audit_high_seq`・`high_seq` を読み直して**引数と矛盾がないときだけ `audit_sequencing_enabled = true` にし、`feed.sink_compared` の監査の行を書く
  （送出先から読んだ値そのものはアプリが渡すので、アプリの資格情報を持つ者は偽れる。それは毎日の突き合わせで検知する — Context の決定 1 の範囲）。
  プロセスの状態が `ok` になるのはこの関数が成功したときだけ。
- **番号付けを止める仕組みは 2 重**: DB の `audit_sequencing_enabled` に加え、worker は `assign_seq(max_batch, include_audit)` の `include_audit` に**自分のプロセスの状態が `ok` のときだけ `true`** を渡す（関数は両方が真のときだけ `audit_log` に番号を振る。イベントの番号付けは続ける）。PITR やスナップショットから戻した DB で `audit_sequencing_enabled` が `true` のまま戻っても、比較の前の worker は監査に番号を振らない。復元の Runbook の最初の手順でも `audit_sequencing_set(false)` を実行する。
- **巻き戻しを検出したとき**（worker と serve が `FG-DB-0011 reason=db_behind_sink`（alert）と `FG-DB-0014`（warn）を 1 回ずつ記録）:
  `forge.rotate_epoch(reason, seal_id)` が同じトランザクションで `epoch` を作り直し、`audit_sequencing_enabled = false` にし、`rollback_handled_seal_id = seal_id` を記録し、
  `feed.epoch_rotated` の監査の行を書く。**同じ原因（同じ `seal_id`）では 2 度目を作り直さない**（5 分ごとに 410 を出さない）。10 分以内の連続の呼び出しは理由に関係なく拒否する。
  `forge_app` が `rotate_epoch()` を呼べることは受け入れる（できるのは kanflow に取り直しをさせることだけで、流量を制限し監査に残る）。
  監査の番号付けは止まったまま（`FG-DB-0015` が 5 分後に出るのは想定どおりと Runbook に書く）。承認済みの Runbook の手順 `forge admin audit seal-resync`:
  1. CLI（`forge_maint`、封印の鍵を持つ）が送出先の封印を読み、**HMAC を CLI で検証**する（鍵は DB の外にあるため、SQL の関数では検証できない）。
  2. `forge.audit_seal_resync(seals)` が、各封印の `seal_sha256` の再計算と `prev_seal_sha256` のつながりだけを確かめ、DB にない封印を `kind = 'imported'` で取り込む。
  3. `audit_high_seq` を送出先の最新の封印の `last_seq` にし、失われた範囲（DB の元の `audit_high_seq + 1` 〜 送出先の最新の `last_seq`）を `audit.restore_gap` の行に記録し、
     `audit_sequencing_enabled = true` にし、`audit.seal_resynced` の行を書く。
  4. 再開後の **最初の通常の封印**（`first_seq` = 送出先の最新の `last_seq + 1`、行は 1 行以上）を `audit.seal` のジョブが `kind = 'restore'`・`seal_epoch + 1`・
     `events_high_seq` = その時点の `high_seq` で作る（鍵は worker が持つ）。`prev_seal_sha256` は取り込んだ最後の封印。
- **送出先に届かないまま進める**: 承認済みの Runbook の手順 `forge admin audit sink-override` だけが `forge.sink_override(reason)`（`forge_maint` だけ）を呼べ、
  念のため `epoch` を作り直し、`audit_sequencing_enabled = true` にし、`feed.sink_override` の監査の行を書く。

### 6. エラーコード（ADR-0016 の表と合わせて登録する）

| コード | 名前 | 重大度 | 内容 |
|---|---|---|---|
| FG-DB-0009 | AppendOnlyViolation | alert | `XF001` の拒否、`reason` = `guard_missing` / `unexpected_ddl` / `role_membership_changed` |
| FG-DB-0011 | AuditSealMismatch | alert | `reason` = `recompute` / `chain` / `hmac` / `gap` / `sink_mismatch` / `sink_missing` / `db_behind_sink` |
| FG-DB-0012 | AuditSealSinkFailed | alert | 送出先に 10 分以上送れない・読めない、または比較ができない（そのプロセスは閉じた状態、連携の履歴は 503） |
| FG-DB-0015 | AuditSealBacklog | alert | 未封印の行が 5 分より古い（封印のジョブの停止、または再同期の待ち） |

既存のコードの変更: `FG-DB-0002` に `reason=write_on_read_pool` / `permission_denied` を追加（`error_map.rs`）。
Runbook: `db-bootstrap.md`、`append-only-violation.md`、`audit-seal-mismatch.md`（`db_behind_sink` の再同期の手順を含む）、`audit-seal-sink.md`（`sink-override`）、
`audit-seal-backlog.md`、`audit-purge.md`、`ddl-audit-ack.md`。

## ADR-0014 の改訂（主セッションが Amendment として追記する文面）

「マイグレーションは `forge_migrator`（`database.migrate_url`、`application_name = forge-migrate`）で接続し、`SET ROLE forge_owner` の後に適用する。
`forge-db` は `Db::connect_migrator()` で 1 本の接続を作り、`migrate()` はプールではなくこの接続を受け取る。`status()` は実行器について汎用で、
`check_schema()` は読み取りのプール（`ReadPool`）で動く。`public._sqlx_migrations` の SELECT 権限を最初のマイグレーションで `forge_app`・`forge_ro` に与える。
マイグレーションの中のオブジェクトはすべて `forge.` で修飾する（ゲートが検査する）。各ファイルは末尾で `db.migrate` の監査の行を書き（最初のファイルは `audit_log` を作った後）、
ADR-0018 の守る対象に触れるときは `details.guarded_changes` に対象を挙げる。新しいテーブルは `forge_owner` が所有し、`forge_app` / `forge_ro` への GRANT と、
ADR-0016 の表の台帳（audited / derived、`ro_columns`、守る対象）への登録を同じ変更で行う。`forge db migrate` の後、`forge doctor append_only.guards` が通ることを確かめる。
導入の手順 `ops/db/bootstrap.sql` のオブジェクト（ロール、スキーマ、`ddl_audit`、`bootstrap_log`、イベントトリガー、`cluster_identity()`）はマイグレーションの外で、`bootstrap_version` で版を管理する。」

## Alternatives Considered

### アプリのロールが所有者のまま、トリガーと GUC で守る（ADR-0016 の初版）
- **Why not**: 所有者はトリガーを外せ、GUC はだれでも設定できる

### `forge_app` に `UPDATE(seq)` を与え、シーケンサーを worker の SQL で書く（第 2 回の案）
- **Why not**: アプリの資格情報で `seq` を任意に書け、欠番や重複を作れる。`forge_seq` の関数に閉じ込める

### DDL の時点で `expected` を決める（第 3 回の案）
- **Why not**: その時点では `db.migrate` の行がまだなく、最初のマイグレーションには `audit_log` もない。事実だけを記録し、読み取りの時点で照合する

### トリガーが「関数の中から呼ばれたか」を判定する
- **Why not**: トリガーには呼び出し元の関数が分からない。判定できるのは `current_user` だけなので、関数の所有者を専用のロールにする

### 封印の送出先を syslog だけにする
- **Why not**: 読み戻せず、毎日の突き合わせと巻き戻しの検出ができない

### 封印の対象の列を版なしで固定する（第 2 回の案）
- **Why not**: 列を足した瞬間に古い封印が検証できなくなる

### 送出先に届かないときは起動しない
- **Why not**: 送出先のホストの障害で forge 全体が止まる。危険な 2 つ（監査の番号付け、連携の履歴）だけを止める（ユーザー決定 2）

## Consequences

### Positive
- アプリの不具合や資格情報の漏えいでは、監査とイベントを消せない・書き換えられない・番号や時刻やトランザクションの ID を偽れない
- 所有者以上による改ざんも、最近の行は 1 時間、全体は 7 日の内に見つかる
- PITR やスナップショットへの巻き戻しの後も、封印の鎖を手順どおりに続けられ、kanflow には 1 回だけ 410 が出る

### Negative
- ロールが 7 つ、URL が 3 つ、superuser の導入の手順、別のホストの `forge seal-sink` が増える
- `migrate()` と `error_map` の実装の変更が要る（ADR-0014 の改訂）
- 送出先に届かない間、連携の履歴が 503 になる

### Risks
- 導入の手順の漏れ → `forge doctor db.bootstrap` と `append_only.guards`、起動の拒否
- 送出先のホストの停止 → FG-DB-0012。封印は DB に溜まり、復旧後に順に送る
- 封印の直列化の実装の違い → 版ごとの固定の仕様と golden vector のテスト
- アプリの資格情報で `confirm_sink_compare()` に偽の値を渡す → 毎日の突き合わせ（FG-DB-0011）で検知する

### 第 5 回のレビューで変えた点（メインのセッションが反映）

| 指摘 | 変更 |
|---|---|
| DB NEW-1 / security MEDIUM: `forge_seq` が `cluster_identity()` を実行できない | §1 の表と導入の手順 5 で `forge_seq` にも EXECUTE を与え、doctor が確かめる |
| DB NEW-2: `forge_purge` が `audit_seal` を読めない | §1 の表に SELECT を追加 |
| DB NEW-3: 2 回目以降の削除で根拠の行が消えて誤警報 | §4: system の `audit.purged` / `audit.restore_gap` は削除の対象外、根拠は system かつ送出済みの行だけ |
| security H-1 / DB MEDIUM-1: 復元した DB の大域フラグが真のまま番号付けが走る | §3・§5: `assign_seq(max_batch, include_audit)` でプロセスの状態も条件にする（2 重）、Runbook の最初で false に戻す |
| DB MEDIUM-2 / MEDIUM-3、security MEDIUM・LOW の残り | 実装時に扱う（`ddl_audit` と `db.migrate` の行の保持の対応、`confirm_sink_compare` の監査は状態の変化時だけ、GRANT/REVOKE は doctor の権限検査が担保、`rotate_epoch` の署名の統一、10 分の連続拒否の Runbook への記載）。受理を妨げない |

## Amendment 2026-10-27: 実装（C1）で決めた点

実装の段階 C1（ロール、導入の手順、プールの分離）で、本文と異なる、または本文が決めていなかった点を次のとおり決めた。

| 点 | 決定 | 理由 |
|---|---|---|
| 導入の手順のファイル | `ops/db/bootstrap.sql` 1 つではなく `ops/db/bootstrap/01-cluster-roles.sql`・`02-create-database.sql`・`03-database-objects.sql` の 3 つ | `CREATE DATABASE` はトランザクションの中で実行できず、03 は新しい DB に接続して実行する必要がある。02 は psql の変数 `:"dbname"` を 1 つだけ持つ |
| イベントトリガーと保護の関数の所有者 | イベントトリガー、`forge.ddl_audit_record()`、`forge.reject_log_mutation()`、`forge.cluster_identity()` は、導入の手順を実行した superuser が所有する（§3 の「`forge_owner` が所有」を改める） | PostgreSQL ではイベントトリガーの所有者は superuser でなければならない。関数も superuser が所有すれば、`forge_owner` は置き換えられない。`forge doctor db.bootstrap` は所有者が superuser であることと、本文が出荷したファイルと同一であることを確かめる |
| `_sqlx_migrations` の読み取り | 最初のマイグレーションの GRANT ではなく、03 の `ALTER DEFAULT PRIVILEGES FOR ROLE forge_owner IN SCHEMA public GRANT SELECT ON TABLES TO forge_app, forge_ro` | 最初のマイグレーションより前（とテストの fixture）にも、読み取りのプールで `status()` が動く。`public` にはマイグレーションの記録しか置かない（アプリのオブジェクトはすべて `forge` に置く。ゲートが検査する） |
| 読み取りのプールの設定 | `after_connect` ではなく、起動のパケットの `role=forge_ro`・`default_transaction_read_only=on`。接続時に 1 本の probe で `current_user = forge_ro` と読み取り専用を確かめる（違えば `FG-SYS-0002 config_key=database.url reason=wrong_role`） | 最初の文より前に効き、追加の往復がない |
| マイグレーションのログイン | superuser のログインは `wrong_role role_check=superuser` で拒否する | superuser は信頼の境界の外（Context の決定 1）。DDL の記録の照合も `forge_migrator` を前提にする |
| 接続の数の設定 | `database.max_connections` を `database.write_max_connections`・`database.read_max_connections` に分け、旧名の環境変数は `renamed` で拒否する。`min_connections` は両方のプールに使い、小さい方の最大値以下 | §2 の予算 |
| LISTEN の接続 | `Db::listen_options()` は最初の利用者（worker、段階 C4）と一緒に入れる | 利用者のない API を先に作らない。sqlx 0.8 には TCP keepalive の設定がないため、そのときに方法を決める |
| `forge.guarded_objects` | 導入の手順のオブジェクトの行を 03 が入れる。行の変更・削除も拒否する（追記のみ） | 行を消すと保護の対象から外れてしまう |
| 接続の予算の違反 | `FG-SYS-0002 config_key=database.write_max_connections reason=connection_budget`（`needed`・`usable`） | 設定とサーバーの不一致で、保護の仕組みの問題ではない |
