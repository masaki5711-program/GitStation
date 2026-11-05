# ADR-0016: ドメインイベントの Outbox、カーソル方式のイベントの履歴、監査ログ

**Date**: 2026-10-27
**Status**: accepted（ADR-0001・ADR-0012 の詳細化。ADR-0005 の土台。**対の ADR-0018（DB のロール、追記のみの強制、監査の封印）と同時に受理する**（ユーザー決定 2026-10-27）。
ADR-0008・ADR-0010・ADR-0012・ADR-0013・ADR-0014 への追記を伴う — 末尾の「他の ADR と計画書への追記」）
**Deciders**: Masaki Fujikawa（ユーザー）、Claude

## Context

ADR-0001 は「状態の変更と同じトランザクションでイベントを Outbox に書き、Webhook・通知・LLM・連携・索引はすべて購読者にする」、
ADR-0012 は「`seq` をシーケンサーが振り、読み手は `seq` で読む」と決めたが、テーブル、書き込みの API、購読と配信のモデル、
保持期間、監査ログの扱いはまだ決まっていない。ユーザーは「kanflow と同期する仕組みを入れる」ことを求めており
（UI は GitHub に倣い、Issue とプロジェクトは kanflow が持つ）、kanflow の現在の問題は「定期取得なので、マージ・クローズ後の MR を取りこぼす」
こと（`02` §8.2）である。ADR-0017（自動修正と kanflow からの承認・マージ）もこの履歴に結果を流す。前提と制約は次のとおり。

- 規模は開発者 50〜300 人、リポジトリ 〜2,000 個、単一サーバー、HA なし、計画停止は可（`01` §2）。外部のブローカーやクラウドは使わない（CLAUDE.md §3-8）。
- 取りこぼしがなく、何度読んでも同じ結果になるイベントの履歴（`02` §5.7）。保持期間より古いカーソルは `410` と `FG-INT-0007`。
- 監査ログは「誰が・いつ・何を・どこから」、追記のみ（`01` §4.2）。代理実行は「kanflow（○○さんの代理）」と残す（`02` §5.3）。
- 履歴・Webhook・受信の口は認可の外に出る経路である。**見えてはいけないプロジェクトやイベントの種類が 1 件でも出れば情報漏えい**（CLAUDE.md §3-4）。
- マイグレーションは前進のみ・追記のみで、1 ファイル = 1 トランザクション（ADR-0014）。依存の向きは CLAUDE.md §3-9。
- 障害時に Claude が、ログの `request_id` だけで「HTTP 要求 → イベント → 購読者 → ジョブ → Webhook の配信」を辿れること。

想定量（上限側の見積もり）: 1 人 1 日 100 イベント × 300 人 = 3 万件/日 ≈ 1,100 万件/年。90 日の保持で 約 270 万行、
1 行 1〜2 KB で数 GB。監査ログは同程度の件数で、年 5 GB 前後。

## Decision

イベント・購読・ジョブ・監査を PostgreSQL のテーブルで実装する。**状態を変える書き込みは `forge-db` の型付きのコマンドだけ**が行い、
コマンドは内部の `ChangeTx` で状態・イベント・監査を 1 つのトランザクションに書く。これを **ゲートと、同じトランザクションの同じリソースに
監査があることを確かめる遅延制約トリガー** で裏打ちする。DB のロール・接続の種類・番号付けの関数・追記のみの強制・監査の封印は **ADR-0018** で決める。
内部の購読者は購読者ごとのカーソルで `seq` の順に読み、外部への呼び出しはフェンシング付きのリースのジョブにする。kanflow には
**カーソル方式のイベントの履歴（pull）を正**として提供し、可視性（プロジェクト × イベントの種類）は **読むたびに、同じスナップショットで読み込んだ材料で
`forge_auth::authorize()` が判定し、SQL は `forge-auth` だけが作れる `VisibleSet` だけを使う**。Webhook（push）は同じイベントの合図として併存させ、送り先は運用者の許可リストに限る。

### 1. テーブル（DDL の概略。実際のマイグレーションは M1 の実装時に ADR-0014 / ADR-0018 の形式で書く。すべてスキーマ `forge`）

`id`・`seq`・`sequenced_at`・`txid`・`occurred_at` は `forge_app` に INSERT の権限がなく、既定値と `BEFORE INSERT` のトリガー（`txid := pg_current_xact_id()`、`occurred_at := now()`）だけが決める（ADR-0018 §1・§3）。

```sql
CREATE TABLE forge.events (                 -- ドメインイベント（Outbox）。外部キーは張らない（削除後も履歴を残す）
  id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, -- 挿入の順。カーソルには使わない
  seq             bigint UNIQUE,             -- forge.assign_seq() だけが NULL → 値（ADR-0012、ADR-0018 §3）
  event_id        uuid   NOT NULL UNIQUE,    -- UUIDv7。外部表記 evt_…
  event_type      text   NOT NULL CHECK (event_type ~ '^[a-z][a-z_]*(\.[a-z][a-z_]*){1,3}$'),
  schema_version  smallint NOT NULL CHECK (schema_version >= 1),
  aggregate_type  text   NOT NULL,
  aggregate_id    bigint NOT NULL,
  project_id      bigint,                    -- NULL = インスタンス全体。連携と Webhook には決して出さない
  actor_kind      text   NOT NULL CHECK (actor_kind IN ('user','service_account','user_via_service_account','system')),
  actor_user_id   bigint,                    -- user / 代理された本人 / service_account ならそのアカウント
  actor_via_id    bigint,                    -- 代理実行したサービスアカウント（ADR-0017 の kanflow）
  CONSTRAINT events_actor_consistent CHECK (
         (actor_kind = 'user'                     AND actor_user_id IS NOT NULL AND actor_via_id IS NULL)
      OR (actor_kind = 'service_account'          AND actor_user_id IS NOT NULL AND actor_via_id IS NULL)
      OR (actor_kind = 'user_via_service_account' AND actor_user_id IS NOT NULL AND actor_via_id IS NOT NULL)
      OR (actor_kind = 'system'                   AND actor_user_id IS NULL     AND actor_via_id IS NULL)),
  request_id      text   NOT NULL,
  caused_by_seq   bigint,                    -- 購読者が書いた派生イベントの直接の元
  root_seq        bigint,                    -- 派生の連鎖の根
  causation_depth smallint NOT NULL DEFAULT 0 CHECK (causation_depth BETWEEN 0 AND 8),
  CHECK ((caused_by_seq IS NULL) = (root_seq IS NULL) AND ((caused_by_seq IS NULL) = (causation_depth = 0))),
  dedup_key       text   UNIQUE CHECK (length(dedup_key) <= 200),
  txid            xid8   NOT NULL DEFAULT pg_current_xact_id(),   -- トリガーが強制
  occurred_at     timestamptz NOT NULL DEFAULT now(),             -- トリガーが強制
  sequenced_at    timestamptz,
  payload         jsonb  NOT NULL CHECK (pg_column_size(payload) <= 65536)
) WITH (autovacuum_vacuum_scale_factor = 0.05);  -- seq は索引の列なので更新は HOT にならない（1 行 2 版）。掃除を早める
CREATE INDEX events_unsequenced ON forge.events (id) WHERE seq IS NULL;
CREATE INDEX events_project_seq ON forge.events (project_id, seq) WHERE seq IS NOT NULL;
CREATE INDEX events_root        ON forge.events (root_seq) WHERE root_seq IS NOT NULL;

CREATE TABLE forge.event_feed_state (       -- 1 行だけ。forge_app は SELECT だけ（書くのは ADR-0018 の関数）
  singleton                boolean PRIMARY KEY DEFAULT true CHECK (singleton),
  epoch                    uuid   NOT NULL,  -- 復元・巻き戻しの検出で作り直す（ADR-0018 §5）
  system_identifier        bigint NOT NULL,
  timeline_id              integer NOT NULL,
  high_seq                 bigint NOT NULL DEFAULT 0,
  retained_from            bigint NOT NULL DEFAULT 1,  -- これより小さい seq は削除済み。空なら high_seq + 1
  audit_high_seq           bigint NOT NULL DEFAULT 0,
  audit_sequencing_enabled boolean NOT NULL DEFAULT false,  -- confirm_sink_compare() / audit_seal_resync() / sink_override() だけが true に、rotate_epoch() が false に
  rollback_handled_seal_id bigint,           -- 同じ原因で epoch を 2 度作り直さない
  epoch_rotated_at         timestamptz,
  sequencer_heartbeat      timestamptz NOT NULL
);

CREATE TABLE forge.event_subscribers (
  name               text PRIMARY KEY,       -- 'webhook-dispatch'、'notify'、'work-item-sync'、'autofix-trigger' …
  ordering           text NOT NULL CHECK (ordering IN ('ordered','independent')),
  enabled            boolean NOT NULL DEFAULT true,
  retention_blocking boolean NOT NULL DEFAULT true,
  last_seq           bigint NOT NULL,        -- 新しい購読者は high_seq から（0 は明示したときだけ）
  last_txid          xid8,                   -- last_seq を最後に進めたトランザクション（派生イベントの検査に使う）
  updated_at         timestamptz NOT NULL,
  failures           integer NOT NULL DEFAULT 0,
  failing_since      timestamptz,
  last_error_code    text CHECK (last_error_code ~ '^FG-[A-Z]+-[0-9]{4}$')
);
CREATE TABLE forge.subscriber_dead_letters (  -- independent の購読者が隔離したイベント。30 日で削除
  subscriber text NOT NULL, seq bigint NOT NULL, error_code text NOT NULL, attempts integer NOT NULL,
  quarantined_at timestamptz NOT NULL DEFAULT now(), PRIMARY KEY (subscriber, seq)
);

CREATE TABLE forge.jobs (
  id                bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  kind              text    NOT NULL,
  payload           jsonb   NOT NULL CHECK (pg_column_size(payload) <= 16384), -- 参照だけ（秘密・URL・本文は入れない）
  state             text    NOT NULL CHECK (state IN ('queued','running','succeeded','dead')),
  attempts          integer NOT NULL DEFAULT 0,
  max_attempts      integer NOT NULL CHECK (max_attempts >= 1),
  run_after         timestamptz NOT NULL DEFAULT now(),
  lease_until       timestamptz,
  worker_id         text,
  timeout_ms        integer NOT NULL CHECK (timeout_ms BETWEEN 1 AND 3600000),
  dedup_key         text CHECK (length(dedup_key) <= 200),
  origin_request_id text    NOT NULL,
  origin_event_seq  bigint,
  last_error_code   text CHECK (last_error_code ~ '^FG-[A-Z]+-[0-9]{4}$'),
  last_error        jsonb,                   -- エラー契約の安全な文脈だけ。応答の本文は入れない
  created_at        timestamptz NOT NULL DEFAULT now(),
  finished_at       timestamptz,
  CHECK (attempts BETWEEN 0 AND max_attempts),
  CHECK (state <> 'running' OR (lease_until IS NOT NULL AND worker_id IS NOT NULL)),
  CHECK (state <> 'dead' OR last_error_code IS NOT NULL),
  CHECK (state NOT IN ('succeeded','dead') OR finished_at IS NOT NULL)
) WITH (fillfactor = 70, autovacuum_vacuum_scale_factor = 0.02);  -- 状態の更新が多い（索引にない列の更新は HOT になりうる）
CREATE UNIQUE INDEX jobs_dedup ON forge.jobs (kind, dedup_key) WHERE dedup_key IS NOT NULL;
-- 投入は ON CONFLICT (kind, dedup_key) WHERE dedup_key IS NOT NULL DO NOTHING（部分索引と同じ述語）
CREATE INDEX jobs_ready    ON forge.jobs (run_after)   WHERE state = 'queued';
CREATE INDEX jobs_lease    ON forge.jobs (lease_until) WHERE state = 'running';
CREATE INDEX jobs_finished ON forge.jobs (finished_at) WHERE state IN ('succeeded','dead');

CREATE TABLE forge.audit_log (              -- 追記のみ（強制と封印は ADR-0018）
  id               bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  seq              bigint UNIQUE,            -- forge.assign_seq() が振る（封印の範囲）
  sequenced_at     timestamptz,
  txid             xid8 NOT NULL DEFAULT pg_current_xact_id(),    -- トリガーが強制
  occurred_at      timestamptz NOT NULL DEFAULT now(),            -- トリガーが強制
  actor_kind       text NOT NULL CHECK (actor_kind IN ('user','service_account','user_via_service_account','system','anonymous')),
  actor_user_id    bigint,
  actor_via_id     bigint,
  credential_id    bigint,                   -- 使ったトークン / セッションの ID（値ではない）
  act_as_claimed   text,                     -- Forge-Act-As で主張された相手の鍵付き HMAC（鍵は DB の外、ADR-0013）
  act_as_key_id    smallint,                 -- その HMAC の鍵の ID
  via_assertion_id text,                     -- 代理実行の ADR のための予約（本人の確認の主張の ID）。封印の対象
  effective_scope  text[],                   -- 判定に使ったスコープの共通部分
  CONSTRAINT audit_actor_consistent CHECK (
         (actor_kind = 'user'                     AND actor_user_id IS NOT NULL AND actor_via_id IS NULL)
      OR (actor_kind = 'service_account'          AND actor_user_id IS NOT NULL AND actor_via_id IS NULL)
      OR (actor_kind = 'user_via_service_account' AND actor_user_id IS NOT NULL AND actor_via_id IS NOT NULL
                                                  AND credential_id IS NOT NULL AND act_as_claimed IS NOT NULL
                                                  AND act_as_key_id IS NOT NULL)
      OR (actor_kind IN ('system','anonymous')    AND actor_user_id IS NULL     AND actor_via_id IS NULL)),
  CHECK ((act_as_claimed IS NULL) = (act_as_key_id IS NULL)),
  actor_label      text CHECK (length(actor_label) <= 64),  -- 個人情報の列（封印の外）
  action           text NOT NULL,
  outcome          text NOT NULL CHECK (outcome IN ('succeeded','denied','failed')),
  resource_type    text NOT NULL,
  resource_id      bigint,
  project_id       bigint,
  source_ip        inet,                     -- 個人情報の列
  user_agent       text CHECK (length(user_agent) <= 256),  -- 個人情報の列
  request_id       text NOT NULL,
  error_code       text CHECK (error_code ~ '^FG-[A-Z]+-[0-9]{4}$'),
  occurrences      integer NOT NULL DEFAULT 1 CHECK (occurrences >= 1),  -- 集約した拒否の件数
  details          jsonb NOT NULL DEFAULT '{}' CHECK (pg_column_size(details) <= 8192)
) WITH (autovacuum_vacuum_scale_factor = 0.05);
CREATE INDEX audit_log_unsequenced  ON forge.audit_log (id) WHERE seq IS NULL;
CREATE INDEX audit_log_txid         ON forge.audit_log (txid, resource_type, resource_id);
CREATE INDEX audit_log_project_time ON forge.audit_log (project_id, occurred_at);
CREATE INDEX audit_log_actor_time   ON forge.audit_log (actor_user_id, occurred_at);
CREATE INDEX audit_log_request      ON forge.audit_log (request_id);

-- 連携と Webhook（M3。概略）
CREATE TABLE forge.integrations (
  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, service_account_id bigint NOT NULL,
  enabled boolean NOT NULL DEFAULT true,
  event_types text[] NOT NULL,               -- 許可リスト（§5）。既定は §5 の連携向けの種類
  inbound_secret_ciphertext bytea NOT NULL, inbound_secret_key_id smallint NOT NULL,
  inbound_secret_prev_ciphertext bytea, inbound_secret_prev_until timestamptz
);
CREATE TABLE forge.integration_bindings (    -- 連携あたり最大 200（コマンドが検査）
  integration_id bigint NOT NULL, project_id bigint NOT NULL, PRIMARY KEY (integration_id, project_id)
);
CREATE TABLE forge.webhooks (                 -- プロジェクトあたり最大 20
  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, project_id bigint NOT NULL, owner_user_id bigint NOT NULL,
  url text NOT NULL, event_types text[] NOT NULL, enabled boolean NOT NULL DEFAULT true,
  secret_ciphertext bytea NOT NULL, secret_key_id smallint NOT NULL,       -- AEAD、AAD = hook_id
  secret_prev_ciphertext bytea, secret_prev_until timestamptz,
  consecutive_failures integer NOT NULL DEFAULT 0, failing_since timestamptz
);
```

**表の台帳と遅延制約トリガー**: すべての表を `crates/forge-db/tables.toml` で `audited`（監査の対象の状態）か `derived`（派生の状態）に分け、
`audited` の表には監査のリソースの対応（例 `memberships` → `("project", project_id)`）を、すべての表には `forge_ro` に読ませる列（`ro_columns`。資格情報と個人情報を除く）と
ADR-0018 の守る対象かどうかを書く。テスト `every_table_is_in_the_ledger` が `pg_class` と台帳を、テスト `guarded_objects_match_the_ledger` が `forge.guarded_objects` と台帳を突き合わせる。
`audited` の表には `AFTER INSERT OR DELETE OR UPDATE OF <監査の対象の列>`（`tokens.last_used_at` のような計数の列は除く）の `DEFERRABLE INITIALLY DEFERRED` の制約トリガーを付け、
コミットの時点で **`txid = pg_current_xact_id()` で同じ `(resource_type, resource_id)`** の監査の行がなければ `XF002`（`FG-DB-0013`）でコミットを失敗させる。
`events` には次の 2 つを付ける:
- `AFTER INSERT`（遅延）`WHEN (NEW.caused_by_seq IS NULL)`: `txid = pg_current_xact_id()` の監査の行があること（シーケンサーの UPDATE には付けない）。
- **派生イベントの検査**: `BEFORE INSERT WHEN (NEW.caused_by_seq IS NOT NULL)` で、元のイベントが存在して番号付け済みであり、`root_seq = coalesce(元.root_seq, 元.seq)`、
  `causation_depth = 元.causation_depth + 1` であること。さらに遅延の `AFTER INSERT` で、同じトランザクションで `last_seq = caused_by_seq` に進めた購読者
  （`event_subscribers.last_txid = pg_current_xact_id()`）があること。満たさなければ `XF002`。この検査は **購読者の 1 つのトランザクションが元のイベントを 1 件だけ処理する**（まとめて処理しない）ことを前提にし、
  事故の水準の制御である（アプリの資格情報を持つ者は購読者の行も書ける）。
`derived`（`jobs`、`event_subscribers`、`subscriber_dead_letters`、通知、検索の索引、`integration_feed_reads`）には付けない。
`forge doctor` は `audited` のすべての表と `events` にトリガーがあり `ENABLE ALWAYS` であることを確かめる（ADR-0018 §3 の `append_only.guards`）。

### 2. 保持期間と分割

| 対象 | 既定 | 方式 |
|---|---|---|
| `events` | 90 日（`retention_policy`、最小 7。`forge_app` は書けない） | `events_retention()`（ADR-0018 §3。1 回の呼び出しで 1 バッチ・最大 1 万行。呼び出し側がくり返す）: `retained_from` から 1 万行の範囲だけを `seq` の順に調べ（有界の走査。`occurred_at` の索引は作らない）、`occurred_at >= cutoff` の最初の行の手前までを候補にし、`retention_blocking` で `enabled` な購読者の `min(last_seq) + 1` で抑え、候補を消し、**コミットの直前に** `event_feed_state` の行をロックして `retained_from = 消した最大の seq + 1` にする。`seq IS NULL` の行は決して消さない。0 行なら監査の行を書かない |
| 　（上限） | 保持期間の 1.5 倍 | 遅れた購読者がいても 1.5 倍を過ぎた行は消す。その購読者は `FG-DB-0008 reason=retention_overrun` で止まる。使わない購読者は `forge admin subscriber retire <name>` |
| `subscriber_dead_letters` | 30 日 | 保守のジョブが削除 |
| `jobs` | succeeded 7 日、dead 30 日 | 重複排除の窓 = この削除の周期 |
| `audit_log` | 無期限 | 削除と伏せ字は ADR-0018 §3 の関数だけ（削除は 365 日の下限と封印の範囲の単位。個人情報の列は 365 日、匿名・ログインの失敗・`denied` は 180 日で NULL。下限は関数の中） |

分割は M1 では行わない。`doctor` が行数・`pg_total_relation_size`・死んだ行の比率を報告し、`events` 5,000 万行 / 20 GB、`audit_log` 1 億行 / 50 GB を超えたら月ごとの範囲分割を新しい ADR で検討する。

### 3. 監査ログの封印（概要。詳細は ADR-0018 §4・§5）

`forge.assign_seq()` が `audit_log` にも `seq` を振り、保守のジョブが `seq` の範囲ごとに **版付き**（`hash_version`）のハッシュの鎖 + HMAC で封印し、
別のホストの `forge seal-sink` に相互 TLS で送る。毎日の全件の突き合わせ、直近 24 時間の再計算、7 日で一巡する全件の再計算で、検知の遅れは最近の行で ≤ 1 時間、
全体で ≤ 7 日（ユーザー決定 2026-10-27）。行がないことを許すのは、封印済みの `audit.purged` / `audit.restore_gap` の範囲の中だけ。
PITR やスナップショットへの巻き戻しは送出先との比較で検出し、`seal-resync` の手順で鎖を続ける。封印の外は個人情報の列だけ。**M1 の完了条件**。

### 4. DB の権限の要約（詳細は ADR-0018 §1〜§3）

- `forge_app` は `events`・`audit_log` に INSERT（`id`・`seq`・`sequenced_at`・`txid`・`occurred_at` を除く）と SELECT だけを持ち、**UPDATE も DELETE も持たない**。
  番号付けと epoch は `forge_seq` が所有する関数、削除と伏せ字は `forge_purge` が所有する関数だけ。`event_feed_state` は `forge_app` から読めるだけ。
- 読み取りは `ReadPool`（`forge_ro`、読み取り専用）。信頼の境界は「superuser と所有者の権限による改ざんは防げず、検知する」（ユーザー決定 2026-10-27）。

### 5. イベントの形式、名前、版、種類ごとの可視性

- 名前は `<集約>.<過去形の動詞>`、ADR-0017 の `merge_request.autofix.applied` のような 3 段まで（CHECK は 2〜4 段）。`push` は `repository.pushed` に改名する。
  `forge-core` の `DomainEvent` の列挙が唯一の定義で、購読者は `_` なしで網羅的に match する。テスト `every_event_type_passes_the_db_check`。
- **種類ごとの最小の権限**: `DomainEvent` の定義に `visibility_class()` を持たせ、履歴と Webhook の両方で `authorize()` が判定する（§7）。

| 分類 | 種類 | 出せる先 | 最小の権限 |
|---|---|---|---|
| `integration` | `merge_request.*`、`repository.pushed`、`branch.*`、`tag.created`、`pipeline.*`、`check.*`、`work_item.*`、`commit.linked` | 連携の履歴、Webhook | プロジェクトの Reporter |
| `maintainer` | `member.*`、`protected_ref.*`、`project.visibility_changed` | 管理者の履歴、その種類を明示した Webhook | Maintainer |
| `deployment` | `deployment.*` | 管理者の履歴、明示した Webhook | Developer |
| `admin` | `token.*` とインスタンス全体のイベント | 管理者の履歴だけ（`Resource::InstanceEvents`） | 管理者 + `admin` スコープ |

  `deployment.*` の payload にはホスト名・接続先・資格情報の参照を入れない（環境の名前と結果だけ）。
- **payload は自分の `project_id` の情報だけ**: 別のプロジェクトにまたがる情報（フォーク、移管）は、読み手が両方を見られるときだけ読み取りの時点で足すか、プロジェクトごとの別のイベントに分ける。
- `payload` はその版の公開の `data`（型付きの構造体を serde で書く）。保存した行は書き換えない（例外は承認済みの緊急の伏せ字だけ。`DomainEvent` が `display_name_paths` で宣言した表示名の項目に限る、§9）。
  JSON Schema は型から `Git/docs/events/` に生成する。
- **版の規則**: フィールドの追加は同じ版。削除・改名・意味の変更は版を上げ、API v1 は v1 の形に落として返す（落とせない変更は v2 だけ）。
  `forge-core` は保持期間内に存在しうるすべての版の変換を持ち、起動時と `doctor` が `SELECT DISTINCT event_type, schema_version` に変換のない組がないことを確かめる（なければ `FG-DB-0010 reason=unknown_version`）。
  変換の削除は、その版の書き込みをやめてから保持期間の上限（1.5 倍）を過ぎるまでテストが禁止する。
- **識別子**: 外部向けの ID は不透明な文字列（`prj_…`、`usr_…`、`evt_…`）で、識別は ID で行う。`project.path`（ADR-0015 により旧パスは墓標になる）、MR の `iid` とタイトル、
  `source_branch`（例 `feature/PBI-123-login-form`）、操作者の `usr_…` と表示名は、その時点の表示用のスナップショット。**email は payload に入れない**。
  `user_email` スコープを持つ連携にだけ、読み取りの時点で `actor.email` を足す（**kanflow の連携には与える**、ユーザー決定 2026-10-27）。
  `actor.via` を必ず出す。作業項目は書くときに解決した `work_items: [{provider, project_id, ref}]`（M3）。
- 本文・差分・ファイルの中身・秘密情報は入れない。`repository.pushed` は 1 回の push に 1 件で、ref の更新は最大 100 件 + `total_refs`、コミットは ref ごとに 20 件 + `total_commits`。
  上限を超えた分は `refs_truncated: true` で明示し、受け手は API で取る（黙って切り詰めない）。64 KiB を超えたら `FG-DB-0010` で書き込みを失敗させる。

### 6. 書き込みの API と、忘れられない仕組み

- **型付きのコマンドだけが公開**: 状態を変える公開の API は `forge-db` の型付きのコマンド（例 `projects::delete(&ChangeContext, DeleteProject) -> Result<Recorded<ProjectDeleted>, ForgeError>`）だけ。
  コマンドは内部の `ChangeTx`（`pub(crate)`）で状態・監査・イベントを書く。`ChangeTx::commit` は `ChangeRecord` を必須の引数に取り、1 つの `ChangeRecord` のイベントは最大 100 件
  （超えたら `FG-DB-0010 reason=too_many_events`。呼び出し側は集約のイベントにする）。
- **プール**: 書き込みのプールは `forge-db` の外に出さず、`Db::pool()` は `ReadPool`（ADR-0018 §2。事故の防止）。迂回の防止はゲート（`forge-db` の外の `.begin(`、
  `INSERT`/`UPDATE`/`DELETE` を含む `sqlx::query*`、生の `BEGIN`、プールへの `execute(` を検出。エージェントモデル側で先に追加）と §1 の遅延制約トリガーが担う。
- M1 のイベントの一覧から「1 つの変更 → 監査 1 件 + イベント 1 件」のテストを生成する。
- `ChangeRecord::housekeeping(理由)` は `derived` の表にしか書けない（コマンドの型で区別する）。`integration_feed_reads` の更新もこの経路（書き込みのプール）。
- **派生の書き込み**: 購読者には `ChangeTx` ではなく `DerivedTx` を渡し、`derived` の表への書き込みと派生イベントの投入だけを許す。`DerivedTx` は 1 件の元のイベントだけを扱い、
  `last_seq` と `last_txid` を同じトランザクションで進める（§1 の検査の前提）。
- 監査の書き込みが失敗したら、状態を変える要求は全体を失敗させる。
- **拒否の監査**は `forge-web` の認可のミドルウェア 1 か所が書く（ハンドラは呼ばない）。見えないリソースの 404（`FG-REPO-0005`）も `denied` で記録する。
  **集約**: 匿名の拒否とログインの失敗は `(actor_label, action, source_ip, 1 分)`、認証済みの拒否は `(actor_user_id, action, resource_type, 1 分)` ごとに 1 行（`occurrences`）。
  集約の表はプロセスあたり 10,000 キーまでで、溢れた分は `source_ip` ごとの 1 行にまとめ、`FG-DB-0016`（warn）を記録する。
  書き込みが失敗しても構造化ログ（`error_code`、`request_id`）は 1 件ごとに必ず出す。異常終了で失うのは最大 1 分の集約だけ。
- **集約の順序**: コマンドは集約の行を `SELECT … FOR UPDATE` で取ってからイベントを挿入する。保証するのは同じ集約の中の順序だけ。
- `ChangeTx` は 30 秒で打ち切る。
- **git の ref の更新**（ADR-0008）は DB のトランザクションに入らない。git の終了後に監査とイベントを書き、`dedup_key = push:<repo_id>:<ref の更新の一覧のハッシュ>`。
  再投入で `ON CONFLICT (dedup_key) DO NOTHING RETURNING` が 0 行なら、`commit` は `Deduped` を返して状態と監査をロールバックする。

### 7. シーケンサー、購読者、ジョブ、履歴 API

**シーケンサー・購読者・ジョブ**

- **シーケンサー**（`forge worker`）: 周期 200 ms で `forge.assign_seq(1000)`（ADR-0018 §3。`seq = high_seq + 1 + バッチの中の順位`、件数の確認を関数の中で行う）を呼び、
  番号を振ったら `NOTIFY forge_events_sequenced`（1 バッチに 1 回）。書き手は NOTIFY しない。監査の番号付けは `audit_sequencing_enabled` が true のときだけ。
- **復元・巻き戻しの検出と、送出先に届かない間**は ADR-0018 §5: 各プロセスは起動時と DB への再接続のたびに比較の状態を `unknown` に戻し、比較が成功するまで連携の履歴は `503`（`FG-DB-0012`）。
  巻き戻しを検出したら `rotate_epoch()` が同じ原因につき 1 回だけ `epoch` を作り直す（`FG-DB-0014`）。同じクラスターでの DB の作り直しは、復元の Runbook の `forge admin events new-epoch`（M1）。
- **遅れの判定**（`FG-DB-0005`）: `seq IS NULL` の見える行があり `sequencer_heartbeat` が 30 秒より古いこと（監査の番号付けを意図して止めている間は監査の行を数えない）。
- **購読者**: NOTIFY で起きるほか、5 秒ごとに `last_seq` と `high_seq` を比べて起きる。LISTEN は ADR-0018 §2 の専用の接続。
  各トランザクションの最初に `SELECT last_seq … FOR UPDATE NOWAIT`。`55P03`（別の worker が処理中）は正常の経路で、FG のコードは記録しない（debug のログだけ）。
  `last_seq + 1` の 1 件を処理し、`UPDATE … SET last_seq = $n, last_txid = pg_current_xact_id() WHERE name = $1 AND last_seq = $prev`（単調）を同じトランザクションで行う。
  派生イベントには `caused_by_seq`・`root_seq`・`causation_depth` を付ける（§1 のトリガーが検査。深さ 8 まで）。**根あたりの派生は 100 件まで**。
  - `ordered`（`work-item-sync` など）: 続けて失敗した、または根あたりの上限を超えたら止めて警報（`FG-DB-0008 reason=stuck` / `fanout_limit`）。飛ばすのは `forge admin subscriber skip`（承認済みの Runbook、監査に残る）だけ。
  - `independent`（`notify`、`webhook-dispatch`）: 5 回失敗した、または根あたりの上限を超えたら `subscriber_dead_letters` に隔離して先に進む（`reason=quarantined` / `fanout_limit`）。
  - ログとジョブは元のイベントの `request_id`・`seq`・`event_id` を引き継ぐ。
- **ジョブ**: `claim` は短いトランザクションで `FOR UPDATE SKIP LOCKED`、`state = running`、`attempts = attempts + 1`、`worker_id`、`lease_until = now() + timeout + 30 秒` にしてすぐコミット。
  実行は `tokio::time::timeout(timeout_ms)`。完了・失敗は `WHERE id = $1 AND state = 'running' AND worker_id = $2 AND attempts = $3`（フェンシング。0 行なら結果を捨てて `FG-DB-0007`）。
  `reap`（30 秒ごと）は 1 つの SQL で、リース切れを `attempts >= max_attempts` なら `dead`（`last_error_code = FG-DB-0007`）、それ以外は `queued`（バックオフ付き）に戻す。
  `dead` は内部のジョブなら `FG-DB-0006`、Webhook の配信なら `FG-INT-0004`。`doctor` は種類ごとの待ち行列の深さを警報にする。

**イベントの履歴 API（pull）**

- **`Action` と主体**: すべての口は `authorize()` を通り、権限マトリクスに行を持つ。`IntegrationEventRead`（`GET /integrations/{id}/events`）、
  `IntegrationEventCursorRead`（`…/events/cursor`）、`IntegrationInboundEvent`（`POST /integrations/{id}/events`、§8）、`EventFeedRead`（管理者の履歴）、
  `WebhookManage`、`WebhookRedeliver`、`AuditRead`（§9）。連携の読み取りの主体は、その連携のサービスアカウントのトークンで、パスの `{id}` がそのアカウントの連携でなければ 404。
  無効にした連携・失効したサービスアカウントは拒否。`Forge-Act-As` は履歴の読み取りには使えない。
- **可視性の判定（SQL で認可を再実装しない）**:
  1. 履歴の口（`forge-web`）が `forge-db` の `ReadSnapshot`（`ReadPool` の `REPEATABLE READ READ ONLY` のトランザクション）を開く。
  2. 同じスナップショットで、認可の材料（連携、`event_types`、結びつけ、サービスアカウントの所属と実効ロール、グループの階層、プロジェクトの可視性、トークンのスコープ）を、
     すべての認可と共通の読み込み関数で読む。
  3. `forge_auth::prove_visible(&principal, &材料)` が、結びつけたプロジェクト（最大 200）× 可視性の分類（§5）ごとに
     `authorize(&principal, IntegrationEventRead, &Resource::ProjectEvents { project, class })` を呼び（管理者の履歴は `Resource::InstanceEvents`）、連携の `event_types` で狭め、
     **`forge_core::VisibleSet`** を返す。結果は 1 回の要求の中だけで使い、要求をまたいで保存しない（最大 800 回の呼び出しは純粋な関数で軽い）。
  4. `forge-db` の `feed::read(&snapshot, &VisibleSet, cursor, limit)` は `VisibleSet` の組（`VisibleSet::Projects` / `VisibleSet::Instance`）だけを SQL の条件に使い、ロールや所属の判断を SQL に書かない。
  - **`VisibleSet` を作れるのは `forge-auth` だけ**: Rust はクレートをまたいで構築子を隠せないので、`forge-core` の `VisibleSet` の構築子は
    `VisibleSet::from_authorized_decisions(…)` 1 つにし、ゲートが `forge-auth` の外からの呼び出しを禁止する。`feed::read` を呼んでよいのは履歴の口だけ（同じゲート）。
    どちらも `authz-matrix-auditor` のチェックリストに入れる。
  クライアントの絞り込みは狭めるだけ。`project_id IS NULL` のイベントは連携に決して出さない。結びつけの解除・非公開化・権限の喪失の後は新旧どちらのイベントも返さない。
  管理者の履歴 `GET /api/v1/admin/events` は管理者 + トークンの `admin` スコープだけ。
- **マトリクスの deny の行**: 別の連携の `{id}`、無効・失効した連携、結びつけの外のプロジェクト、結びつけの解除後、非公開化の後、許可リストにない種類、
  最小の権限を満たさない分類、`admin` スコープのない `/admin/events`、代理での読み取り。**`authz-matrix-auditor` のレビューを受理の条件にする。**
- **カーソル**: 不透明な文字列。`(版, kid, integration_id, epoch の UUID 全体, VisibleSet のハッシュ, seq)` を AEAD（AES-256-GCM、鍵は DB の外、ADR-0013）で暗号化して base64url にする。
  `VisibleSet` のハッシュは、そのカーソルを出したときの `VisibleSet` の正規化した並びの SHA-256 の先頭 128 ビット。鍵の入れ替えの間は新旧 2 つの `kid` を受け付け、
  検証は AEAD のタグで定数時間に行う。別の連携のカーソル・改ざん・書式の誤りは `400`（`FG-INT-0008`、値はログに出さない）。`…/events/cursor` は **その連携に見える最後のイベントの位置**を返す。
- **読み取り**: 上のスナップショットの中で、`seq > $after AND seq <= LEAST($high_seq, $after + 5000)` の窓を `seq` の順に、`limit` 既定 100・最大 500、応答は 2 MiB まで。
  `has_more` は `next_cursor` の後に見えるイベントが残っているときだけ true。見えるイベントが残っていなければ `next_cursor` は `high_seq` まで進める（暗号化されているので活動量は漏れない）。
  M3 の作業項目の絞り込みは `event_work_items (work_item_ref, seq)` の索引。流量制限は連携ごと（`429`）。
- **410 Gone**（`FG-INT-0007`）: `after.seq + 1 < retained_from`、`epoch` の不一致、`after.seq > high_seq`、**`VisibleSet` のハッシュの不一致**（`reason=visible_set_changed`。
  結びつけ・所属・ロール・グループの階層・可視性・`event_types` のどれが変わっても、見えなかった間の隙間を黙って飛ばさない。変化の原因ごとに世代を数える仕組みは要らない）。
  本文は `reason` だけ。黙って途中から返さない。テスト `visible_set_change_returns_410`。
- **取りこぼさない保証**: 状態と同じトランザクション、欠番のない `seq`、`high_seq` までの読み取り、kanflow は処理後にカーソルを保存（少なくとも 1 回。重複は `event_id` で除く）、
  遅れすぎ・復元・巻き戻し・可視性の変化は 410 で必ず気づく。410 からの復旧: 先に `events/cursor` を取る → 開発状況 API（`state=all`）で全体を取り直す → 取っておいたカーソルから読む。
- `integration_feed_reads`（M3）は連携ごとに 1 分に 1 回まで、housekeeping の経路で更新する。失敗は `FG-DB-0002` でログに残し、応答は返す。

### 8. Webhook（push、M3）と kanflow からの受信

- **所有と可視性**: Webhook は 1 つのプロジェクトに属し（プロジェクトあたり最大 20）、そのプロジェクトのイベントだけを配る。作成・変更は `WebhookManage`（プロジェクトの Maintainer 以上）。
  種類は §5 の最小の権限を作成者が満たすものだけ選べる。作成者の権限と分類ごとの権限を **投入の時点と配信の時点の両方** で `prove_visible()`（§7 と同じ材料の読み込み）で判定し直し、
  失えば配らずに `FG-INT-0010 reason=owner_lost`。再送は `(hook_id, delivery_id)` の組で探し `WebhookRedeliver` で判定する（IDOR の deny の行）。手動の再送には流量制限を掛ける。
- **送り先は許可リストだけ**: `webhook.allowed_targets`（ホスト名か CIDR + 許可するポート）。**既定は空**で、運用者が kanflow などの社内の受信先を登録するまで Webhook は配れない。
  `https` だけ（開発環境だけ `webhook.allow_http = true`、本番では `doctor` が警告）。URL の userinfo は拒否し、ログには scheme とホストだけを出す。
  許可リストに関係なく常に拒否: ループバック、リンクローカル、メタデータ（`169.254.169.254` など）、`0.0.0.0/8`、マルチキャスト、ブロードキャスト、未指定、
  IPv4 射影の IPv6（中の IPv4 で判定）、NAT64（`64:ff9b::/96`）、6to4（`2002::/16`）。CGNAT（`100.64.0.0/10`）は許可リストにあるときだけ。
  URL を変えるたびに検査し直す。違反の `400` は「この送り先は許可されていません」だけを返す（`FG-INT-0010`。理由はログだけ）。
- **接続**: 配信のたびに名前を解決し直して検査し、**検査したアドレスに接続を固定**する（DNS rebinding の対策）。TLS は元のホスト名（SNI）で常に検証する。
  リダイレクトは追わない。プロキシの環境変数は無視する。接続 3 秒・全体 10 秒。応答は最大 4 KiB まで読む。履歴と `last_error` には状態コードと FG のコードだけを残す。
- **配信**: `webhook-dispatch`（independent）が `webhook.deliver` ジョブを投入する。payload は `(hook_id, event_seq)` の参照だけで、URL と秘密は実行時に読む。
  署名は `Forge-Signature: t=<unix秒>,v1=<HMAC-SHA256(secret, t + "." + delivery_id + "." + body)>`（`t` は再試行のたびに作り直す。秘密の入れ替えの 24 時間は新旧 2 つの `v1`）。
  秘密は AEAD（AAD = `hook_id`）で暗号化して鍵の ID と保存する（鍵は DB の外）。Webhook ごとの同時実行は 1（最大 2）。50 回続けて失敗するか 24 時間失敗し続けたら自動で無効にする（`FG-INT-0011`）。
- **推奨**: kanflow には「履歴が正、Webhook は『読みに来て』の合図」とする。
- **kanflow → forge**（`02` §5.6）: 大きさの上限（64 KiB）と署名（`abs(now - t) ≤ 300 秒`、定数時間の比較、入れ替えの間は新旧 2 つの秘密）を、**監査やジョブを書く前に** 検証する。
  **存在しない連携の `{id}` と署名の不正は同じ `401`**（`FG-INT-0005`。どちらかはログの `reason` だけ）。
  検証が通った要求だけが主体 `Principal::SignedIntegration { id }`（ADR-0010 の構築の規則に従う）として `authorize(IntegrationInboundEvent, …)` を通り、
  内容が触れてよいのはその連携に結びついたプロジェクトと作業項目だけ。重複排除は署名された本文の中のイベント ID から `inbound:<integration_id>:<event_id>`（保持 7 日 > 窓 5 分）。
  deny の行: 別の連携の `{id}`、無効・失効した連携、結びつけの外のプロジェクト、署名なし、古い `t`。

### 9. 秘密情報と個人情報を構造上混ぜない

- 監査の `details` は操作ごとの型付きの構造体（`AuditDetails` の列挙）からしか書けず、個人情報の項目を持てない（型の規則 + テスト `audit_details_have_no_pii_fields`）。
  数は ±2^53 − 1 の整数だけを数として持ち、それ以外は文字列にする（封印の直列化のため。ADR-0018 §4）。`token.created` は ID と接頭辞だけ。
- `Secret<T>` は `Serialize` を実装しない。テスト `no_token_like_string_reaches_audit_events_or_jobs` が結合テストの後に `audit_log`・`events`・`jobs` からトークンの形の文字列を探す。
- ログインの失敗の名前: 64 文字で切り、制御文字を除き、ログイン名の書式に合わないものは保存せず `invalid_format` とだけ記録する。表示時はエスケープする。
- **代理実行**: `act_as_claimed` は主張された相手の鍵付き HMAC で、鍵の ID を `act_as_key_id` に残す（鍵を替えても古い行を突き合わせられる）。
  対象が存在しない・無効・権限がないの 3 つは **同じ応答**（`FG-INT-0006`）。ボット `forge-autofix` を対象にする拒否は `authorize()` とマトリクス（ADR-0017）で行う。
  DB の制約での禁止は、ボットの ID が DDL の時点で決まらないので見送る。
- `source_ip`: `server.trusted_proxies` に設定したプロキシのアドレスを `X-Forwarded-For` の右から取り除き、右から最初の信頼しないアドレスを使う。
- **監査の閲覧**（`AuditRead`）: M1 では、全体の監査は管理者 + `admin` スコープだけ（書き込みのプールの型付きの問い合わせ）。プロジェクト・グループの Owner の画面（ADR-0010 の既存の行）は
  `ReadPool`（`forge_ro` は個人情報の列を読めない）で `project_id` に絞る。監査の閲覧そのものも `audit.read` として監査に残す。
- **利用者の匿名化**（Runbook `user-anonymization.md`）: 監査は `audit_anonymize_user()` で個人情報の列を伏せる。イベントの表示名は保持期間の上限（最長 135 日）まで残り、
  配信済みの Webhook の本文は取り消せないことを Runbook に書く。緊急の削除は `events_redact_user()`（ADR-0018 §3、`forge_maint`、承認済みの手順）で表示名の項目だけを置き換える。

### 10. 段階

| 段階 | 範囲 |
|---|---|
| **M1（手順 C）** | ADR-0018 のすべて（ロール、導入の手順、接続の種類、番号付け・epoch・比較・削除の関数、追記のみの強制、DDL の記録と読み取りの時点の照合、封印・送出・突き合わせ・再計算・再同期 = 完了条件）、`events`・`event_feed_state`・`event_subscribers`・`subscriber_dead_letters`・`jobs`・`audit_log`・`retention_policy`、表の台帳と遅延制約トリガーと派生イベントの検査、型付きのコマンド・`DerivedTx`・ゲート、シーケンサー、購読者とジョブの実行器、保持期間と伏せ字のジョブ、M1 のイベント（`project.*`、`repository.*`、`repository.pushed`、`branch.*`、`tag.created`、`member.*`、`token.*`）、管理者の履歴と `…/cursor`（`EventFeedRead`、`VisibleSet::Instance`）、`AuditRead` の絞り込みと `audit.read`、認可のミドルウェアでの拒否の監査と集約、`doctor` の検査、復元の Runbook の `new-epoch`、エラーコード |
| **M3（第 1 段）** | MR のイベント（`02` §5.8 と ADR-0017 の `merge_request.*`、AI のコミットによる `merge_request.unapproved` `reason=autofix_commit` を含む）と状態遷移のテスト、`work_items` と `event_work_items`、`integrations`・`integration_bindings`・連携の履歴（§7 のすべて、`prove_visible()`）、kanflow の `user_email` スコープ、`integration_feed_reads`、`webhooks` と §8 のすべて、kanflow からの受信、`work-item-sync`（F-8） |
| **M4** | `pipeline.*`、`deployment.*`、ADR-0017 の `merge_request.autofix.*` |

### 11. 失敗の扱いと観測（実装時に `errors.toml` に登録する。ADR-0018 §6 のコードと合わせて）

| コード | 名前 | 重大度 | HTTP | 内容と文脈の項目 |
|---|---|---|---|---|
| FG-DB-0005 | OutboxSequencerLag | alert | — | 未採番の行があり心拍が 30 秒より古い。`unsequenced_count`、`heartbeat_age_ms`、`lock_holder_pid` |
| FG-DB-0006 | JobDeadLettered | alert | — | 内部のジョブが `dead` になった。`reason` = `attempts_exhausted` / `permanent_failure` / `lease_expired` / `dead_lettered`（doctor の `jobs.dead`、直近 24 時間）。`job_id`、`kind`、`attempts`、`last_error_code`、`origin_request_id` |
| FG-DB-0007 | JobLeaseExpired | warn | — | リース・実行が通常でない形で終わった。`reason` = `lease_expired`（回収）/ `fenced`（結果を捨てた）/ `execution_timeout`（`timeout_ms` を超えて中断、再試行）/ `abort_pending`（中断したハンドラーが 5 秒で終わらない）/ `released_on_shutdown`（停止で返した、試行回数を戻す）/ `left_leased`・`release_timeout`（停止時に返せずリースが残る）。`job_id`、`kind`、`worker_id`、`attempt`、`origin_request_id` |
| FG-DB-0017 | JobQueueBacklog | alert | — | （C6 で追加）doctor の待ち行列の検査。`reason` = `queue_depth`（種類ごとの実行可能なジョブが 1000 件超）/ `queue_wait`（最も古いものが 10 分超）/ `reaper_behind`（リース切れが 60 秒超回収されない）。`kind`、`ready`、`oldest_wait_ms`（`reaper_behind` は `expired_leases`、`job_id`、`worker_id`） |
| FG-DB-0008 | SubscriberLag | alert | — | `reason` = `stuck` / `behind` / `retention_overrun` / `quarantined` / `fanout_limit`。`subscriber`、`seq`、`event_type`、`lag_events` |
| FG-DB-0010 | EventRejected | alert | 500 | 上限超過、`too_many_events`、直列化の失敗、`unknown_version`。`event_type`、`schema_version`、`reason` |
| FG-DB-0013 | ChangeWithoutAudit | alert | 500 | 遅延制約トリガーが、同じリソースの監査のないコミットや、系譜の合わない派生イベントを拒否した（`XF002`）。`table`、`check`。必ず不具合 |
| FG-DB-0014 | FeedEpochChanged | warn | — | 復元・巻き戻しを検出して epoch を作り直した。`reason`（`system_identifier` / `timeline` / `behind_sink` / `sink_override` / `manual`）、`seal_id` |
| FG-DB-0016 | AuditAggregationOverflow | warn | — | 拒否の集約の表が 10,000 キーを超え、IP ごとにまとめた。`dropped_keys` |
| FG-INT-0004 | WebhookDeadLettered | warn | — | `hook_id`、`delivery_id`、`event_id`、`last_status` |
| FG-INT-0005 | InboundEventRejected | warn | 401/413 | 署名の不正・古い `t`・存在しない連携・大きさの超過。`reason`（応答では区別しない） |
| FG-INT-0006 | ActAsTargetInvalid | info | 403 | （`02` §9 の既存）対象が存在しない・無効・権限がないを区別しない |
| FG-INT-0007 | FeedCursorExpired | info | 410 | `reason` = `retention` / `epoch_mismatch` / `ahead_of_head` / `visible_set_changed` |
| FG-INT-0008 | FeedCursorInvalid | info | 400 | 書式・AEAD・別の連携。値は出さない |
| FG-INT-0009 | FeedConsumerLag | warn | — | N 時間読みに来ない、または遅れが保持期間の 50 % 超（M3）。`integration_id`、`last_read_at` |
| FG-INT-0010 | WebhookTargetForbidden | warn | 400 | 許可リスト外・常に拒否の範囲・userinfo・所有者の権限の喪失。`hook_id`、`reason` |
| FG-INT-0011 | WebhookDisabled | warn | — | 連続失敗で自動で無効にした。`hook_id`、`failures`、`failing_since` |
| FG-INT-0012 | IntegrationLimitReached | info | 409 | 結びつけ 200・Webhook 20 の上限。`limit` |

- 流量制限（`429`）は `02` §5.1 の共通のコードを使う（未登録なら WEB の領域で登録する）。
- `INT`・`LLM`・`REVIEW` の領域はユーザーが承認済み。登録の前に、エージェントモデル側で `scripts/validate-error-registry.js` の `AREAS` と `forge-error-contract.md` に加える。
- `forge doctor`: `outbox.sequencer_lag`、`outbox.unsequenced_count`、`subscribers.lag`、`subscribers.dead_letters`、`jobs.dead`、`jobs.expired_leases`、`jobs.queue_depth`、
  `events.retention`、`events.size` / `audit_log.size`（行数・バイト・死んだ行の比率）、`events.schema_versions`、`feed.epoch`、`feed.rollback_check`、`webhook.allow_http`、`webhook.allowed_targets`、
  ADR-0018 の `db.bootstrap`・`db.connections`・`append_only.guards`・`audit.seal_verify`・`audit.seal_shipping`、M3 で `feed.consumer_lag`。`forge diag bundle` に上の値と `event_feed_state` を含める。
- 指標: `forge_outbox_unsequenced`、`forge_outbox_high_seq`、`forge_subscriber_lag_events{name}`、`forge_jobs{kind,state}`、`forge_job_duration_seconds{kind}`、
  `forge_audit_unsealed_rows`、`forge_feed_requests_total{integration,outcome}`、`forge_webhook_deliveries_total{outcome}`。

## 他の ADR と計画書への追記（主セッションが適用する）

- **ADR-0014**: ADR-0018 の末尾の「ADR-0014 の改訂」の文面を Amendment として追記する。
- **ADR-0010（追記）**: 「新しい `Action`: `EventFeedRead`、`IntegrationEventRead`、`IntegrationEventCursorRead`、`IntegrationInboundEvent`、`WebhookManage`、`WebhookRedeliver`。
  新しいリソース `Resource::ProjectEvents { project, class }` と `Resource::InstanceEvents`（ADR-0016 §5 の可視性の分類）。`forge_auth::prove_visible()` が `authorize()` の結果から
  `forge_core::VisibleSet` を作る唯一の関数（構築子の呼び出し元はゲートが検査）。新しい主体 `Principal::SignedIntegration { id }` は、受信の署名の検証が通った後に
  `forge-web` の受信の口だけが作る。`AuditRead` は、全体は管理者 + `admin` スコープ、プロジェクト・グループの Owner は個人情報の列のない絞り込みの画面だけ。
  履歴と Webhook の可視性は `VisibleSet` だけで決め、SQL に認可を再実装しない。マトリクスの deny の行は ADR-0016 §7・§8 の一覧。」
- **エージェントモデル（先に入れる）**: `authz-matrix-auditor` のチェックリストに「`feed::read` / Webhook の配信が `VisibleSet` だけを使い、SQL にロールの判断がない」
  「`VisibleSet::from_authorized_decisions` を `forge-auth` の外で呼ばない」を加え、eval を足す。
  ゲート: `forge-db` の外の書き込み、マイグレーションの `forge.` の修飾と `db.migrate` の行（ADR-0018 §3 の守る対象の定義と同じ）、`feed::read` と `VisibleSet` の構築子の呼び出し元。
- **ADR-0012（追記）**: 「番号付けは `forge_seq` が所有する `forge.assign_seq()` が行い（`seq = high_seq + 1 + バッチの中の順位`）、`audit_log` にも同じロックの中で `seq` を振る。
  遅れは `sequencer_heartbeat` で判定する。復元・巻き戻しの検出で `epoch` を作り直す（ADR-0016 §7、ADR-0018 §5）。」
- **ADR-0013（追記）**: シークレットファイル `feed_cursor_keys`（AEAD、`kid` 付きで複数）、`webhook_secret_keys`（AEAD、鍵の ID 付き）、`audit_seal_keys`（`key_id` 付き。worker と `forge_maint` の CLI）、
  `act_as_hmac_keys`（`kid` 付き）、`seal_sink_client_cert` / `key`。設定 `database.migrate_url`、`database.maint_url`、`database.write_max_connections`、`database.read_max_connections`、
  `database.privileged_members`、`server.trusted_proxies`、`webhook.allow_http`、`webhook.allowed_targets`、`audit.seal_sink_url`。
- **ADR-0008（追記）**: 「再投入は `dedup_key = push:<repo_id>:<ref の更新の一覧のハッシュ>`。既にあれば `Deduped` で状態と監査をロールバックする。イベント名は `repository.pushed`。」
- **計画書 `02`**: §5.5 の `push` → `repository.pushed`、署名の文字列を `t + "." + delivery_id + "." + body` に、`actor` の例から `email` を除き `user_email` スコープの注記（kanflow には付与）。
  §5.7 にカーソル（暗号化、連携と `VisibleSet` ごと）・可視性・種類の許可リスト・`events/cursor` の意味・410 の理由（`visible_set_changed` を含む）・本文・送出先に届かない間の `503`。
  §9 に FG-INT-0008〜0012、0005 の拡張。
- **ADR-0017 との整合**: 代理実行は `actor_kind = user_via_service_account`（`actor_user_id` = 本人、`actor_via_id` = kanflow）。`forge-autofix` の操作は `actor_kind = service_account`、
  MR の作成者は `AuditDetails::Autofix`。AI のコミットは常にすべての承認を取り消し、`merge_request.unapproved`（`reason=autofix_commit`、主体はボット）を出す。
  `merge_request.autofix.*` は 3 段の名前として CHECK を通り、§5 の `integration` の分類（`merge_request.*`）に入る。

## Alternatives Considered

### LISTEN/NOTIFY だけで配信する
- **Why not**: 接続していない間の通知は消え、履歴がない。シーケンサーからの合図と 5 秒の確認の組み合わせにする

### 論理レプリケーション / CDC（pgoutput、wal2json、Debezium）
- **Pros**: 書き手がイベントを書き忘れても変更を拾える
- **Cons**: 届くのは行の変化でドメインの意味ではない。消費が止まるとスロットが WAL を溜め、DB 全体が止まる
- **Why not**: 失敗の影響が DB の停止まで広がる。書き忘れは遅延制約トリガーで防ぐ

### 外部のブローカー（Kafka、NATS、RabbitMQ）
- **Why not**: ADR-0001 で却下済み

### すべてをジョブに展開する（購読者のカーソルを持たない）
- **Why not**: `jobs` が膨らみ、順序が失われる。kanflow 用の履歴は結局 `events` から読む

### イベントと監査ログを 1 つの表にする
- **Why not**: 保持期間、読み手、内容、可視性の規則が違う

### 可視性を書き込みの時点で決める / 種類を問わずプロジェクトの単位で出す
- **Why not**: 解除・非公開化の後も過去のイベントが読める。`member.*`・`token.*` が連携に漏れる

### 可視性を SQL の結合（所属・ロールの表）で決める（第 2 回の案）
- **Why not**: 認可の判断が `authorize()` と SQL の 2 か所になり、片方だけ変わる（CLAUDE.md §3-4 の単一判定点に反する）

### 結びつけの世代の番号をカーソルに入れる（第 3 回の案）
- **Why not**: 世代を上げるべき変化（所属、ロール、グループの階層、可視性、種類）を漏れなく列挙しなければならない。`VisibleSet` のハッシュなら判定の結果そのものを比べるので漏れがない

### Webhook の送り先を拒否リストだけで守る（第 1 回の案）
- **Why not**: 社内の任意のホストに要求を出せる。オンプレミスでは送り先が限られるので許可リストだけにする

### HMAC だけのカーソル
- **Why not**: `seq` が読め、全体の活動量が漏れる。暗号化する

## Consequences

### Positive
- 「マージ・クローズ後の MR を取りこぼす」問題が、定期取得のままの kanflow でも構造的に起きなくなる
- `request_id` が HTTP 要求 → イベント → 購読者 → ジョブ → 配信まで 1 本で通る
- 監査のない書き込みと系譜の偽装が、型・ゲート・DB の制約トリガーで防がれる。トランザクションの ID と時刻はアプリが偽れない
- 履歴・Webhook・受信の口が、プロジェクトと種類の両方で認可の単一判定点とマトリクスに載り、SQL に認可が漏れ出さない

### Negative
- 履歴の読み取りのたびに、結びつけたプロジェクト × 分類の数だけ `authorize()` を呼ぶ（最大 200 × 4。純粋な関数なので軽いが、材料の読み込みが要る）
- 可視性が少しでも変わると kanflow は 410 で取り直す（変化はまれなので受け入れる）
- Webhook は運用者が許可リストに送り先を登録するまで使えない
- 表を足すたびに台帳への登録と遅延制約トリガーが要る
- 送出先に届かない間、連携の履歴が 503 になる（ADR-0018 §5、ユーザー決定）

### Risks
- 可視性の判定の誤り → マトリクスの deny の行と `authz-matrix-auditor` のレビュー。テスト `feed_stops_returning_events_after_unbinding`、`feed_never_returns_member_or_token_events_to_integrations`、`visible_set_change_returns_410`
- Webhook からの SSRF → 許可リストと常に拒否の範囲。テスト `webhook_to_private_address_is_rejected_after_dns_rebinding`
- 派生イベントの暴走・偽装 → 深さ 8、根あたり 100 件、1 つの変更あたり 100 件、系譜のトリガー（事故の水準）
- 失敗したログインの洪水 → 集約と 10,000 キーの上限。異常終了で失う集約は最大 1 分（ログには残る）
- シーケンサーの停止・復元・改ざん → ADR-0018 と FG-DB-0005 / 0014 / 0011

## Error codes and runbooks affected

- 新規の登録（実装時、テストと同じコミット）: FG-DB-0005〜0008、0010、0013、0014、0016、0017（C6 で追加）、FG-INT-0004〜0012（0006 は既存の定義を確定）。ADR-0018 の分は ADR-0018 §6。
- 新しい Runbook（alert のもの）: `outbox-sequencer-lag.md`、`job-dead-lettered.md`、`job-queue-backlog.md`、`subscriber-stuck.md`、`event-rejected.md`、`change-without-audit.md`。
  ほかに `user-anonymization.md`、`webhook-allowed-targets.md`（許可リストの登録）。ADR-0018 の Runbook は ADR-0018 §6。
- 更新する Runbook: 復元の手順（`forge admin events new-epoch` と、送出先との比較・`seal-resync` を必須に、M1）、ADR-0008 の FG-GIT-0012 の再投入。
- README の更新（実装時）: `forge-db`（型付きのコマンド、`DerivedTx`、`ReadPool` と `ReadSnapshot`、`feed::read`、接続の種類、Outbox・ジョブ・監査、表の台帳、所有するコード）、
  `forge-core`（`DomainEvent` と `visibility_class()`・`display_name_paths`、`Actor`、`ChangeContext`、`AuditRecord` / `AuditDetails`、`FeedCursor`、`VisibleSet`、`Secret<T>`）、
  `forge-auth`（新しい `Action`・`Resource::ProjectEvents` / `InstanceEvents`・`SignedIntegration`・`prove_visible()`）、`forge-web`（認可のミドルウェアでの拒否の監査と集約、履歴の口の可視性の判定、受信の検証）。

## レビューで変えた点

節の番号は第 3 回以降の版のもの（第 2 回までの §8 履歴 → §7 後半、§9 Webhook → §8、§10 秘密と個人情報 → §9、§11 段階 → §10、§12 エラー → §11）。

### 第 1 回

| 指摘 | 変更 |
|---|---|
| C-1 履歴の可視性 | §7: サーバー側で読むたびに判定、`project_id IS NULL` は管理者だけ、解除後は新旧とも返さない、代理での読み取り禁止、deny の行、`authz-matrix-auditor` |
| C-2 SSRF | §8 の送り先の規則と接続の規則。FG-INT-0010 |
| H-3 追記のみが名目 | ロールの分離を決定に（ADR-0018 §1・§3） |
| H-4 改ざんの検知 | 封印を M1 の完了条件に（ADR-0018 §4） |
| H-5 ChangeTx の迂回 | §6 の型付きのコマンド、ゲート、§1 の遅延制約トリガー、生成するテスト、housekeeping の許可リスト |
| H-6 保持期間とカーソル | §2 の `seq` の境界での削除、1.5 倍の上限、`retire`、§7 の 1 つのスナップショット |
| H-7 復元 / PITR | §7 と ADR-0018 §5、UUID 全体の比較 |
| H-8 購読者の排他 | §7 の `FOR UPDATE NOWAIT`、単調な更新、`ordered` / `independent` |
| H-9 リース | §7 の `attempts + 1`、フェンシング、1 つの SQL の `reap` |
| H-10 Webhook の所有と秘密 | §8 |
| H-11 秘密と個人情報 | §5・§9 |
| H-12 `push` の名前 | `repository.pushed`、CHECK を 2〜4 段に、全名のテスト |
| Med（代理の記録、カーソル、負荷、洪水、jobs の制約、シーケンサーの分離、連鎖、push の再投入、NOTIFY、版、受信と拒否の監査） | §1・§6〜§8 の該当の箇所 |
| Low（SQLSTATE、遅れの誤報） | `XF001` / `XF002`、心拍での判定と `ChangeTx` の 30 秒 |

### 第 2 回

| 指摘 | 変更 |
|---|---|
| D1〜D4 関数の権限・所有者・search_path・プール | ADR-0018 §1〜§3 と ADR-0014 の改訂文 |
| S1 受信と履歴の認可 | §7・§8 の `Action`、`SignedIntegration`、deny の行。ADR-0010 への追記 |
| S2 種類ごとの可視性 | §5 の分類の表と最小の権限 |
| S3 SSRF の許可リスト | §8 |
| S4〜S6 役割の例外・封印の穴・送出先の突き合わせ | ADR-0018 §1・§3・§4 |
| D5 制約トリガーの範囲 | §1 の台帳と `(resource_type, resource_id)` の一致 |
| D6 イベントトリガー | ADR-0018 §3 |
| D7 保持のバッチ | §2 |
| D8 fillfactor の誤り | §1 |
| D9 復元の検出 | ADR-0018 §5 |
| D10 購読者の起床 | §7 |
| S7 拒否の集約 | §6。`denied` の種類別の削除は見送り（鎖の検証を保つため、180 日の伏せ字で代える） |
| S8〜S16 | §5〜§9 の該当の箇所。DB でのボットの禁止は見送り |
| Low（`X-Forwarded-For`、匿名化） | §9 |

### 第 3 回

| 指摘 | 変更 |
|---|---|
| H1 封印の列に版がない | ADR-0018 §4: `hash_version`、版 1 の列と直列化の規則、golden vector のテスト |
| H2 PITR の後に鎖が止まる | ADR-0018 §5: `seal-resync`、その間に出すコード |
| H3 `forge_app` の `UPDATE(seq)` | ADR-0018 §1・§3: `forge_seq` の `assign_seq()`。本文 §4・§7 |
| H4 検知の穴 | ADR-0018 §3・§4: 7 日で一巡する再計算、守る対象の DDL の照合、CI のゲート |
| H5 SQL での認可の再実装 | §7: 同じスナップショットの材料で `authorize()`、SQL は結果の集合だけ |
| db N3〜N11、sec N4〜N11、Low | ADR-0018 §1〜§5、本文 §1・§2・§7〜§9 の該当の箇所 |
| ユーザー決定 | 信頼の境界は検知、kanflow に `user_email`、ADR-0018 は別の ADR で同時に受理、ADR-0015 の旧パスの墓標 |

### 第 4 回

| 指摘 | 変更 |
|---|---|
| R1 `expected` を DDL の時点で決められない | ADR-0018 §3: `ddl_audit` は事実だけ（`expected` の列を削除）、ビュー `forge.ddl_audit_status` が読み取りの時点で `db.migrate`（同じ `txid`、`guarded_changes`）と `bootstrap_log` に照合、最初のマイグレーションは `audit_log` を作った後に自分の行を書く |
| R2 `audit_sequencing_enabled` の書き手がない | ADR-0018 §5: `confirm_sink_compare()`（自分で `cluster_identity()` と高水位を読み直す）、`rotate_epoch()` が同じトランザクションで false、`audit_seal_resync()` が最後に true、`sink_override()` は `forge_maint` だけ、すべて自己監査、`rotate_epoch()` の連続の拒否、`doctor` の EXECUTE の検査に追加。§1 の DDL に `rollback_handled_seal_id` |
| R3 守る対象の DDL の穴 | ADR-0018 §3: `object_identity` か親が守る対象なら、コマンドの種類を問わず守る対象への変更。`forge.guarded_objects` を台帳から生成し、CI のゲートも同じ定義 |
| R4 HMAC の鍵が DB の外 | ADR-0018 §5: 取り込む封印の HMAC は CLI が検証、SQL の関数は `seal_sha256` とつながりだけ。`restore` の封印は鍵を持つ worker の封印のジョブが作る（CLI が作らない理由: 行が 1 行以上要るため） |
| R5 再同期の後の状態 | ADR-0018 §5: `restore` の封印 = 再開後の最初の通常の封印（`first_seq` = 送出先の最新の `last_seq + 1`、`seal_epoch + 1`、`events_high_seq` = その時点の `high_seq`）、常に最新の封印と比較、同じ原因で 1 回だけ作り直す（`rollback_handled_seal_id`） |
| R6 `forge_purge` の権限と書く列 | ADR-0018 §1: `event_subscribers` の SELECT、`events` は `payload` の `display_name_paths` だけ |
| R7 存在しない列を参照するトリガー | ADR-0018 §3: 表ごとのトリガー関数の表、`audit_seal` は `shipped_at` 以外すべて同一、伏せ字は封印済みならその範囲の版、未封印なら現在の版、`ddl_audit` のトリガーのテスト |
| R8 削除と所有者の削除の区別 | ADR-0018 §4: 行がないことを許すのは、封印済みで送出先にもある `audit.purged`（365 日の下限以下）/ `audit.restore_gap` の範囲の中だけ、それ以外は `gap`。一部だけ取り込まれた範囲は行の再計算ができないと明記 |
| R9 結びつけの世代の漏れ | §7: カーソルに `VisibleSet` のハッシュを入れ、不一致は 410 `visible_set_changed`。`binding_generation` を削除。テスト `visible_set_change_returns_410` |
| R10 `txid` / `occurred_at` / `id` を偽れる | ADR-0018 §1・§3 と §1: INSERT の権限から除外、`BEFORE INSERT` で強制、遅延のトリガーは `pg_current_xact_id()` と比べる |
| R11 `VisibleSet` の構築 | §7: `forge-core` の `VisibleSet` の構築子は 1 つで、`forge_auth::prove_visible()` だけが呼ぶ（ゲートで検査。Rust ではクレートをまたいで隠せないと明記）、`Resource::InstanceEvents` と `VisibleSet::Instance` |
| R12 再接続の後の状態 | ADR-0018 §5: 再接続のたびに `unknown`、比較が成功するまで閉じた状態 |
| Low | `details` の数の範囲（§9、ADR-0018 §4）、`cluster_identity()` は superuser の所有で監視の外と明記し所有者と本文のハッシュを検査、メンバーの関係の文言、`assign_seq()` の式と件数の確認、派生の検査は 1 トランザクション 1 件で事故の水準、`rotate_epoch()` を `forge_app` が呼べることを受け入れる理由、`evtenabled` と所有者の検査、0 行の呼び出しは監査を書かない、`authorize()` の結果は要求をまたいで保存しない |
| ユーザー決定（第 4 回） | 送出先に届かないときは正しさを優先（ADR-0018 Context・§5）、古い行の検知の遅れは最大 7 日（§3、ADR-0018 §4） |

## ユーザーの決定が要る点

なし（2026-10-27 までの決定をすべて反映した。送出先のホストと保持期間の数値は既定のまま）。

## Amendment（2026-10-27、手順 C3 のレビューの後）

- **派生イベントも監査の行が要る（C5 まで）**: §1 の `events_audit_present` の `WHEN (NEW.caused_by_seq IS NULL)` は、C5 の派生イベントの検査があることを前提にしていた。
  C3 の時点ではその検査がないため、マイグレーション `20261027100500` で `WHEN` を外し、すべてのイベントに同じトランザクションの監査の行を求める。
  C5 で `DerivedTx` と派生イベントの検査を入れるとき、このトリガーを §1 の形（`WHEN` 付き）に戻す。`forge doctor` は現在の定義（`WHEN` なし）と完全に一致することを確かめる。
- **`ChangeTx` の時間の上限**: 変更の全体（開始から COMMIT の送信まで）は 30 秒。COMMIT は変更の上限で取り消さず、別の 10 秒の上限を持つ。
  COMMIT の待ちが上限を超えたときは結果が確定しているかもしれないため `FG-DB-0002`（`reason=commit_outcome_unknown`）とし、単なる失敗としない。
  変更の上限を超えたときは `reason=change_timeout`（`timeout_ms` 付き）で、トランザクションはロールバックされる。
- **書く前の検査（`FG-DB-0010`）の追加**: 成功でない変更はイベントを出さない（`events_on_unsuccessful`）、監査の行がプロジェクトを指すときイベントも同じプロジェクト（`event_scope_mismatch`）、
  代理実行の資格情報は要求の資格情報と一致する（`credential_mismatch`）、`details` と `payload` の整数は ±2^53−1 以内（`unsafe_number`。封印の JSON を正確に読み戻すため、ADR-0018 §4）、
  `details` は 8 KiB・`payload` は 64 KiB まで（表の CHECK と同じく DB が `pg_column_size` で測る。JSON の文字列の長さとは一致しないため）。
- **利用者から来る値は拒否の理由にしない**: `actor_label`（64 文字）、`user_agent`（256 文字）、`act_as_claimed`・`via_assertion_id`（128 文字）、`effective_scope`（32 件・各 64 文字）は文字の境界で切り詰め、
  制御文字と表示を変える見えない文字（双方向の制御、ゼロ幅など）を除く。成功でない変更の `details` が上限を超える、または安全でない整数を含むときは `{"details_dropped": "too_long" | "unsafe_number"}` に置き換える。
  長い値を送って拒否の監査そのものを書かせない、という回避を防ぐ。
- **手順 D で決める（未決）**: `dedup_key` はインスタンス全体で一意なので、型付きのコマンドはサーバー側の ID だけから作る（利用者の値をそのまま使わない）。キーにプロジェクトや種類の接頭辞を強制するか、
  プロジェクトを指さない監査の行の変更がプロジェクトのイベントを出してよいかは、最初の呼び出し元を入れる手順 D で決める。

## Amendment（2026-10-28、手順 C4: シーケンサーと `forge worker`）

- **監査の番号付けは C9 まで行わない**: `forge worker` は `forge.assign_seq(1000, include_audit)` に常に `include_audit = false`（`AuditSequencing::Skip`）を渡す。
  ADR-0018 §5 のプロセスごとの比較の状態（`unknown` / `ok`）は C9 で入り、その状態が `ok` のときだけ `true` を渡す。それまで監査の行は番号を待つが、
  `audit_sequencing_enabled` も `false` のまま（変えられるのは C9 の `forge_seq` の関数だけ）なので、`FG-DB-0005` の遅れには数えない。
- **NOTIFY はイベントに番号を振ったバッチだけ**: 監査の行だけを番号付けしたバッチは `forge_events_sequenced` を送らない（購読者はイベントだけを読む）。payload はバッチの最後の `seq`。
- **`event_feed_state` の行は `assign_seq` の最初の呼び出しが作る**（advisory lock の中で、`forge_audit.cluster_identity()` と新しい `epoch`）。その表の保護のトリガーは UPDATE・DELETE だけを見るので、
  `forge_seq` の INSERT は通る（行は主キーで 1 行だけ）。`forge_app` は INSERT を持たない。
- **LISTEN の接続（`Db::listen_options()`）は C5 に送る**: シーケンサーは LISTEN しない。最初の LISTEN の利用者は購読者（C5）。
- **C4 のレビューの後（2026-10-28）**:
  - 何も待っていない間はシーケンサーは書かない: `forge.assign_seq` は待つ行がなく heartbeat が 5 秒より新しいとき、`event_feed_state` を更新せずに戻る
    （migration `20261027110100_outbox_sequencer_idle.sql`。関数の所有者 `forge_seq` として置き換える）。heartbeat は番号を振ったときか 5 秒より古いときだけ書かれる。
  - worker の失敗の記録: 連続の始まり、コードか `operation` / `query` / `reason` が変わったとき、同じ失敗が続く間は 60 秒ごと（`failures` と同じ `request_id`）。
    shutdown の送り手が shutdown を要求せずに消えたら `FG-SYS-0001` `reason=shutdown_channel_closed` で終了コード 1（正常終了にしない）。
  - **受け入れたリスク**: (1) advisory lock は `forge_app` のどのセッションからも取れるので、worker 以外が持ち続けると番号付けが止まる。
    `lock_holder_pid`（`forge doctor` と FG-DB-0005 の `context`）で見える。(2) `include_audit` は呼び出し側が決める（ADR-0018 §5、C9 までは常に `false`）。
    (3) heartbeat は最大で約 5 秒古い（遅れの判定 30 秒に対して十分小さい）。

## Amendment（2026-10-28、手順 C6: ジョブの待ち行列）

- **実装**: `forge-db` の `jobs`（`enqueue` / `ChangeTx::enqueue_job`、`claim`、`complete` / `fail` / `release`、`reap`、`JobRunner`、`health`）、`forge worker` の `job_runner` と `job_reaper`。
  §7 のとおり `claim` は `FOR UPDATE SKIP LOCKED` の 1 文、完了・失敗・返却は `(id, state = 'running', worker_id, attempts)` でフェンシング、`reap` は 1 文（1 回に最大 1000 件）。
- **バックオフ**: 失敗・リース切れの後 10 秒 × 2^(attempt − 1)、上限 1 時間。
- **配信は少なくとも 1 回**: ハンドラーの副作用は冪等にするか `lease.attempt` でフェンシングする。`abort()` はハンドラーの次の `.await` まで効かないので、
  タイムアウト・停止の後は実行器がハンドラーのタスクの終了を最大 5 秒待ってから記録・返却する（終わらなければ `FG-DB-0007 reason=abort_pending` を記録して進む）。
- **実行のタイムアウトは `FG-DB-0007 reason=execution_timeout`**（新しいコードにしない）: リースが切れる前に実行器がリースを終わらせるもので、性質がリース切れと同じ。再試行する。
- **停止時の返却**: `forge worker` の停止の要求で、実行中のジョブは 10 秒の猶予の後に中断し、ハンドラーが終わったら 1 つのフェンシング付きの UPDATE で
  `queued`・`run_after = now()`・`attempts - 1`・リースなしに戻す（`FG-DB-0007 reason=released_on_shutdown`）。停止はジョブの責任ではないので試行回数を使わない
  （再起動を繰り返しても健全なジョブが `dead` にならない）。ハンドラーが終わらない・返却が 5 秒で答えないときはリースを残し、リース切れの回収に任せる。
- **重複の防止と `dead`**: `jobs_dedup` を `state <> 'dead'` の部分一意索引にした（migration `20261027120000`）。`queued` / `running` / `succeeded` のジョブはキーを持ち続け、
  `dead` のジョブの仕事は同じキーで再投入できる（Runbook `job-dead-lettered.md` の回復手順）。同じ migration で doctor 用の `jobs_ready_kind (kind, run_after) WHERE state = 'queued'`。
- **待ち行列の警報は専用の `FG-DB-0017` JobQueueBacklog**（§11）: `FG-DB-0006` は「ジョブが死んだ」だけに使う。0011 / 0012 / 0014〜0016 は ADR が予約済みのため 0017。
- **受け入れた制限（C8・保持期間のジョブの前に見直す）**: 1 つの worker は 1 度に 1 件だけ実行する（head-of-line）。長いジョブがほかの種類を待たせる。
  `queue_wait` の警報で見える。見直しの候補は種類ごとの実行器か、上限付きの並行実行。
- **規模**（plan 01 §3.10）: 内部の仕事用。エッジ端末 1 台ごとにジョブを作らない（`queue_depth` の警報がその誤用を捕まえる）。
- **`supervise.rs` の Runner の契約**: ジョブの実行器はトランザクションではなくリースと冪等性で契約を満たす（中断された実行のジョブはリース切れの後に再実行される）。
## Amendment（2026-10-28、手順 C5: 購読者と `DerivedTx`）

- **派生イベントの検査**（migration `20261027140000`）: `events_lineage`（`BEFORE INSERT WHEN (NEW.caused_by_seq IS NOT NULL)`。元が番号付け済み、元の深さが 8 未満、
  `root_seq = coalesce(元.root_seq, 元.seq)`、`causation_depth = 元 + 1`、根あたりの派生が 100 件未満）と `events_derived_progress`（遅延の `AFTER INSERT`。
  同じトランザクションで `last_seq = caused_by_seq`・`last_txid = pg_current_xact_id()` に進めた購読者があること）。どちらも `XF002` で、`CONSTRAINT` に
  `derived_lineage` / `derived_depth` / `derived_fanout` / `derived_progress` を入れ、`FG-DB-0013` の `reason` になる。C3 の Amendment どおり
  `events_audit_present` に `WHEN (NEW.caused_by_seq IS NULL)` を戻し、`forge doctor` の期待値もそれに合わせた。トリガー関数は他の保護の関数と同じく
  `forge_owner` の所有で `SECURITY DEFINER` ではない（挿入するロールの権限で `events` と `event_subscribers` を読む）。
- **派生イベントの主体と要求**: 派生イベントは `actor_kind = 'system'` で、元のイベントの `request_id` を引き継ぐ（購読者が行ったことで、利用者の操作ではない。
  `request_id` で元の要求に辿れる）。
- **1 回の処理**: ハンドラーはセーブポイントの中で動き、20 秒で打ち切って失敗に数える（`reason=handler_timeout`）。処理の全体は 30 秒、COMMIT は 10 秒
  （`commit_outcome_unknown`）。失敗はセーブポイントを戻してから、同じトランザクションで `failures` / `failing_since` / `last_error_code` を記録する。
- **止まる・隔離する**: 続けて 5 回の失敗で、`ordered` は `FG-DB-0008 reason=stuck`（同じイベントを再試行し続け、自分では飛ばさない）、`independent` は
  `subscriber_dead_letters` に隔離して進む（`reason=quarantined`）。派生の上限（深さ・根あたり 100 件）は 1 回目で `reason=fanout_limit`（`limit` = `depth` / `per_root`）。
  `DerivedTx` は上限に達したことを覚えていて、ハンドラーがエラーを握りつぶしても上限として扱う。
- **worker**: 購読者のタスク（`subscribers`）は登録された購読者があるときだけ動く（今は具体的な購読者がないので、LISTEN の接続も開かない）。
  1 回の実行 = 遅れている購読者ごとに最大 100 件、その後 NOTIFY か 5 秒の確認まで待つ。失敗を続ける購読者は、その購読者だけ 200 ms から倍々・最大 60 秒の間隔で再試行する
  （他の購読者を待たせず、ログも 1 分に 1 回程度に抑える）。shutdown は待ちの最大 5 秒の後に効く。
- **LISTEN の接続**（ADR-0018 §2、C4 から送られた分）: `Db::listen_options()` は `statement_timeout`・`idle_in_transaction_session_timeout` を付けない。
  sqlx 0.8 はクライアントのソケットに TCP keepalive を設定できないため、サーバー側の `tcp_keepalives_idle = 30`・`interval = 10`・`count = 3` を起動のパケットで送る。
  クライアント側は 5 秒の確認ごとに上限付きの `SELECT 1` で半分切れた接続を見つけ、切れたら作り直し、作り直した直後に必ず状態を確かめる（`Woken::Connected`）。
- **`forge doctor`**: `subscribers.lag`（`ordered` で失敗 5 回 → `stuck`、次のイベントの番号付けから 60 秒超 → `behind`）、`subscribers.dead_letters`
  （直近 24 時間の隔離 → `quarantined`。それより古いものは数だけ）。
- **見送り**: `forge admin subscriber skip` は設計だけ（Runbook `subscriber-lag.md`: `forge_maint` だけが実行できる `forge_owner` 所有の `SECURITY DEFINER`
  関数 `forge.subscriber_skip(name, seq, reason)` が、隔離の行・カーソルの前進・`subscriber.skipped` の監査の行を 1 つのトランザクションで書く）。
  `forge_maint` の接続が `forge-db` にまだないため、それを入れる段階で実装する。`reason=retention_overrun` は保持（C7）。
- **受け入れたリスク**: 根あたりの件数の数え方は 1 つの文の中だけで、同じ根から 2 つのトランザクションが同時に派生すると合計で 100 件をわずかに超えうる（事故の水準）。
- **レビュー後の修正（C5）**: イベントに数える失敗を広げた — ハンドラーの失敗・タイムアウト・派生の上限に加えて、復号できないイベント（`FG-SYS-0001 reason=decode`）、
  ハンドラーが中断状態にしたセーブポイントの RELEASE の失敗、COMMIT で派生イベントの検査に拒まれたこと（`FG-DB-0013 reason=derived_*`。新しいトランザクションで数える）。
  どれも 5 回で `stuck` / 隔離になる。DB の障害・購読者の行がない・`ordering` の不一致・`high_seq` より下のイベントがない・カーソルが動いた、は購読者の再試行（バックオフ）で、
  イベントには数えない。隔離の行と `last_error_code` には、上限で止まったときもハンドラー自身のコードを残す。1 回の round は 30 秒を過ぎたら新しい処理を始めず
  `more` を返す（最長は 30 秒 + 処理中の 1 件）。LISTEN の待ちの失敗はタスクの失敗にせず、1 回記録して 5 秒待ってから次の round に進む。
  XF002 で知らない `CONSTRAINT` 名は `no_audit_row` ではなく `xf002_unknown`。
- **後で見直す（migration を足すとき）**: (1) 上の根あたりの数え超え — 根の `seq` で advisory lock を取れば防げる。(2) `events_audit_present` は同じ `txid` の監査の行が
  あるかだけを見ている（行の中身との対応までは見ない）。(3) 上限に達した状態で同じ `dedup_key` の派生イベントを出し直すと、`ON CONFLICT` の前に上限のエラーになる
  （出し直しが冪等にならない）。

## Amendment（2026-10-28、手順 D1: 状態の表の台帳と `forge.audited_change_present`）

- **台帳の 4 つの列**（`crates/forge-db/tables.toml`、`kind = "audited"` の表に必須。`bootstrap::check` が実際の表とトリガーを突き合わせる）:
  - `audit_resource`: 監査のリソースの種類。リテラル（例 `"user"`）か、`=列名`（その列の値を種類にする。例 `namespaces` は `=kind`）。
  - `audit_id_column`: リソースの ID の列。整数列でなければならない。
  - `unaudited_columns`: 変わっても監査の行を求めない列（例 `updated_at`）。トリガーの `UPDATE OF` は、この列以外のすべての列である。
    列を足したらトリガーを作り直す（足したまま作り直さないと、doctor が `FG-DB-0009 guard=audited_trigger` の `detail=update_of` で検出する）。
  - `txn_audited_columns`: 変わったら、同じ種類の監査の行が同じトランザクションにあることだけを求める列（ID は変えない。例 `namespaces` の部分木の書き換えの `full_path`・`depth`）。
- **トリガーの関数** `forge.audited_change_present(audit_resource, audit_id_column, unaudited_columns, txn_audited_columns)`（migration `20261028100000_audited_change_present.sql`、
  product commit `bd28876`）。遅延の制約トリガー（`DEFERRABLE INITIALLY DEFERRED`、`ENABLE ALWAYS`）で、`WHEN` は付けない（`events_audit_present` の派生の例外とは別）。
  - 行の ID が変わった、または `=列` の値が変わったときは、古いリソースと新しいリソースの両方の監査の行が要る。
  - 変わったのが `txn_audited_columns` だけのときは、同じ種類の監査の行が 1 つあればよい。
  - 満たさなければ `XF002`（`FG-DB-0013 reason=audited_change`）でコミットを失敗させる。台帳と違うトリガーは `reason=audited_change_config`。
- **TRUNCATE**: 行のトリガーは TRUNCATE で迂回されるため、`kind = "audited"` の表には TRUNCATE を、どの役割にも与えない。
  表は `src/bootstrap/schema.rs` の `LATER_TABLES` と `expectations()` に、移行と同じコミットで載せる（載せ忘れは doctor が `guard=privileges`、`detail=audited_table_unlisted` で検出する）。
- **台帳の検査**: 台帳の解析は、リソースの列が `unaudited_columns` か `txn_audited_columns` にあること、同じ列が両方に入ることを拒否する。
  `audit_id_column` が整数列であることは、`bootstrap::check` が実際の表に対して確かめる（`FG-DB-0009 guard=audited_trigger`、`detail=id_column`）。
  テストは `every_table_is_in_the_ledger` と `guarded_objects_match_the_ledger`、D1 の表の検査は `users_schema_tests`。

## Amendment（2026-10-29、手順 D3: トークン）

- `token.created` / `token.revoked` は §10 のとおり M1 のイベント（分類 `admin`）として出す。監査の行は所有者のユーザーに付ける
  （`tables.toml` の `audit_resource = "user"`、`audit_id_column = "user_id"`）。詳細は ADR-0019 §5。
- トークンが最初のイベントの書き手なので、D3 で `forge-core` に `EventType` の列挙（`ALL`、`as_str`、`visibility_class()`。
  `DomainEvent.event_type` はこの型にする）と `DedupKey` を入れる。§11 の未決はこう閉じる: `dedup_key` は **サーバー側の ID とハッシュ
  だけから作る**（利用者の値から作れる構築子を持たない）、形は `^[a-z][a-z_]*(:[0-9a-z_-]{1,64}){1,4}$`・200 文字まで、
  形の違反は `FG-DB-0010 reason=invalid_dedup_key`。プロジェクトを指さない監査の行の変更がプロジェクトのイベントを出してよいかは、
  最初のプロジェクトのイベントを入れる D4 で決める。
- 拒否されたリクエストの監査の集約（§6）は引き続き見送り（ADR-0019「見送り」）。

## Amendment（2026-10-30、手順 D3 MR1b: `EventType` と `DedupKey` の実装）

- **`EventType` に入れるのは、書き手がある種類だけ**（ユーザーの決定 2026-10-30）。今は `repository.created`（D4 で使う。分類 `integration`）、
  `token.created`・`token.revoked`（分類 `admin`）。§10 の残りの種類（`branch.*`、`tag.created`、`project.*` など）は、その書き手を入れる変更で足す。
  型で不正な名前を作れなくなったので、書き込みの前の `invalid_event_type` の検査は外した。DB の CHECK は残る（テスト `every_event_type_passes_the_db_check`）。
- **`DedupKey` の形は Rust の型と書き込みの前の検査で守る**（ユーザーの決定 2026-10-30）。`DedupKey::new` は `&'static str` の接頭辞と、
  `Id(i64)`・`Hash(&[u8; 32])`・`Word(&'static str)` の部分だけを受け取る。`ChangeTx` と `DerivedTx` は書く前に形をもう一度確かめ、
  違反は `FG-DB-0010 reason=invalid_dedup_key`。`forge.events.dedup_key` に正規表現の CHECK は **足さない**（長さ 200 の CHECK だけ）。
  形の検査が `field_too_long field=dedup_key` に代わる。
- `AuditDetails::event_type()` が、変更の詳細から出すイベントの種類を返す（監査だけの操作は `None`）。

## Amendment（2026-10-31、手順 D4 MR2: イベントの範囲の規則と `project.created`）

- **イベントの範囲の規則**（D3 の Amendment の未決を閉じる）: `ChangeTx` は、変更のすべてのイベントの `project_id` が監査の行の `project_id` と
  **等しい** ことを求める。つまり、プロジェクトのイベントには同じプロジェクトを指す監査の行が要り、プロジェクトを指さない監査の行の変更は
  インスタンス全体のイベント（`project_id` なし）だけを出せる。違反は `FG-DB-0010 reason=event_scope_mismatch`（何も書かない）。
  理由: フィードはプロジェクトのイベントをそのプロジェクトの読み手に見せる（§5）ので、監査の行もどのプロジェクトが変わったかを示さないと、
  監査とフィードが食い違う。両方向のテスト `a_change_whose_audit_row_names_no_project_cannot_publish_a_project_event`。
  `DerivedTx`（監査の行のない派生イベント）はこの規則の外である。
  この規則は監査の行の `project_id` を信頼するので、リソースが `project` の監査の行は `project_id = resource_id` でなければならない
  （違反は `FG-DB-0010 reason=audit_scope_mismatch`、テスト `a_project_audit_row_must_name_its_own_project`）。
- **`EventType::ProjectCreated`**（`project.created`、分類 `integration`）を足す。書き手は `projects::create`。ペイロードは
  `AuditDetails::ProjectCreated` と同じ JSON（`project_id`、`namespace_id`、`parent_namespace_id`、`repository_id`、`kind`、`visibility`、`owner_id`。
  パスや名前などの個人に関わる値は入れない）。集約は `project` / `projects.id`。重複除去の鍵は付けない（プロジェクトの ID は新しいので、同じ変更は繰り返されない）。
  `project.visibility_changed` と `protected_ref.*` は計画 D4 のとおり `maintainer` のまま。
- **重複除去の鍵の接頭辞は `<aggregate>_<verb>`**（例 `repository_provision:<repo_id>`、MR3b の `repository_created:<repo_id>`）。プロジェクトの接頭辞を
  強制しない（範囲はイベントの `project_id` が示す）。ジョブの重複除去の鍵（`jobs::DedupKey`、種類ごとに一意）も同じ形にし、
  `forge_core::DedupKey::new` で作ってから渡す（`repositories::provision_job`）。
- `repository.provision` のジョブは、作成の変更の中で `ChangeTx::enqueue_job` で入れる（初めての本番の呼び出し元）。変更が確定したときだけジョブが存在する。
  新しいリポジトリの ID で `Deduped` になるのは不整合なので、`FG-SYS-0001 reason=unexpected_job_dedup` で作成全体を失敗させる。

## Amendment（2026-10-30、手順 D4 MR3b: `repository.created` と作成のやり直し）

- **`repository.created`** の書き手は `repositories::mark_ready`（`repository.provision` のジョブのハンドラーが呼ぶ）。一つの `ChangeTx` で、
  リポジトリの行を `FOR UPDATE` で取り、`state = ready`・`ready_at = now()` にし、監査の行 `repository.created`（リソース `project` /
  プロジェクトの ID、`project_id` 付き。行者は `system`、`request_id` はジョブの元の要求のもの）と、イベント `repository.created`
  （分類 `integration`、集約 `repository` / `repositories.id`、`project_id` = そのプロジェクト、ペイロードは `AuditDetails::RepositoryCreated`
  と同じ `{project_id, repository_id, kind}`、重複除去の鍵 `repository_created:<repo_id>`）を書く。範囲の規則（D4 MR2）を満たす。
  既に `ready` なら何も書かない（`MarkReady::AlreadyReady`。2 回目のイベントは出ない）。行の鍵で並行する呼び出しは直列になり、
  重複除去の鍵は最後の安全網である（行が `provisioning` なのに鍵が記録済みなら `FG-SYS-0001 reason=unexpected_dedup`）。
- **`repository.reprovision_requested`**（監査のみ、イベントなし）: `forge admin repo reprovision <repo_id>` が、`provisioning` のままの
  リポジトリの作成のジョブをもう一度入れたとき（ジョブと監査の行が一つの変更）。詳細は `{repository_id, job_id}`、リソースは `project`。
  運用者の復旧の操作で、連携先に知らせることではないので監査だけにした。`ready` のリポジトリは `FG-REPO-0014 reason=repository_ready`
  で断る（新しいコードは作らない）。同じ鍵の `queued` / `running` / `succeeded` のジョブがあれば何も書かない。
- 作成のジョブのハンドラーは失敗を自分で記録しない（ジョブの実行器が一度だけ記録する）。成功だけを `forge.repo.provisioned` で 1 行記録する。
- レビューを受けて次を加えた（同日）。
  - **鍵の連なり**: `jobs_dedup` は `queued` / `running` / `succeeded` のジョブの鍵を持ち続け、`dead` だけが手放す（C6 の
    Amendment。これは変えない）。そのため、`provisioning` のままなのに作成のジョブが `succeeded`（DB の復元、手の変更）だと、
    同じ鍵では入れ直せなかった。`reprovision` は行の鍵の下で鍵の連なりの最後のジョブを読み、`queued` / `running` なら何もしない
    （`AlreadyLive`。状態を返す）、`dead` / 無いならその鍵で、`succeeded` なら `repository_provision:<repo>:after:<そのジョブの id>`
    （`DedupKey` で作る）で入れる。監査の詳細は `{repository_id, job_id, holder_job_id, holder_state}`。doctor も同じ SQL
    （再帰の CTE、等号の検索だけ、最大 16 段）で連なりの最後を見る。
  - `mark_ready` は `UPDATE` の行数が 1 でなければ `FG-SYS-0001 reason=unexpected_row_count`。`mark_ready_within` は変更の
    上限を呼び出し側が渡す（作成のジョブは自分の残り時間を渡す）。同時の `mark_ready` 2 回、`mark_ready` と `reprovision` の
    両方の順序をテストで固定した。
  - doctor の `repositories.provisioning` は詰まったものを SQL で選ぶ（`job_not_live` を先、それぞれ古い順、古い 10 000 件を分類）。
    理由ごとの件数 `job_not_live` / `too_old` を `FG-REPO-0012` の context に入れる。

## Amendment（2026-10-31、手順 D4 MR4: プロジェクトの `member.*` のイベント）

- `EventType::{MemberAdded, MemberRoleChanged, MemberRemoved}`（`member.added`・`member.role_changed`・`member.removed`、分類 `maintainer`、§5）を足す。
  書き手は `projects::{add_member, change_member, remove_member}`。監査の行はプロジェクト（`project` / id、`project_id` あり）、イベントの集約も
  `project` / id、ペイロードは監査の詳細（`AuditDetails::ProjectMember*`、グループと同じ JSON の形 `{"user_id","role"}` / `{"user_id","from","to"}`）。
  重複除去の鍵は付けない（メンバーの変更は毎回新しい状態で、吸収すべき再試行ではない）。
- グループのメンバーの変更（`AuditDetails::Member*`）は監査だけのまま（計画 Q8、`event_type()` は `None`、テスト
  `project_member_details_have_the_group_shapes_and_publish_maintainer_events`）。

## Amendment（2026-10-31、手順 D4 MR5: `protected_ref.*` のイベント）

- §10 の M1 のイベントに `protected_ref.*` を足す: `EventType::{ProtectedRefCreated, ProtectedRefUpdated, ProtectedRefDeleted}`
  （`protected_ref.created`・`protected_ref.updated`・`protected_ref.deleted`、分類 `maintainer`、§5）。書き手は `protected_refs::{create, change, delete}`。
  監査の行とイベントの集約はプロジェクト（`project` / id、`project_id` あり）、ペイロードは監査の詳細（`{"protected_ref_id","pattern","push_min_role"}`、
  更新は `{"protected_ref_id","pattern","from","to"}`）。パターンは ref の名前の形で、個人に関わる値ではない。重複除去の鍵は付けない。

## Amendment（2026-11-01、M1 段階 9 MR2: `repository.pushed` と push の記録）

- **`repository.pushed`**（`EventType::RepositoryPushed`、分類 `integration`、§5）の書き手は `forge_db::repositories::record_push`
  だけ（forge-web は `forge_core::PushRecorder` を通して呼ぶ。forge-db に依存しない）。git が ref を更新した後に、状態の表は書かず、
  監査の行 `repository.pushed`（リソース `project` / そのリポジトリのプロジェクトの ID、`project_id` あり、行者は push した利用者）と
  イベント `repository.pushed`（集約 `repository` / `repositories.id`、同じ `project_id`）を 1 つの `ChangeTx` で書く。
- **ペイロード**: `forge_core::push::push_json`。`{project_id, repository_id, total_refs, refs_truncated, created, fast_forward,
  forced, deleted, total_commits, commits_truncated, replayed_from, refs: [{ref, old, new, kind, total_commits, commits,
  commits_truncated}]}`。`kind` は `create` / `fast_forward` / `force` / `delete`。種類ごとの件数は一覧に載らなかった分も含む。
  ref は最大 100 件、コミットは ref ごとに最大 20 件（§5 の上限）。並びは `force`、`delete`、`create`、`fast_forward` の順
  （それぞれの中は名前順）で、切り詰めても履歴の書き換えと削除が残る（データベースのレビューの MEDIUM）。
  `replayed_from` は再投入のときだけ `{user_id, credential_id}`、それ以外は `null`。
  §5 の「64 KiB を超えたら FG-DB-0010 で書き込みを失敗させる」は、push については **失敗させずに切り詰める** に改める:
  ref の名前は 1024 バイトまで許されるので、100 件 × 20 コミットは 64 KiB に収まらないことがある。書き込みを失敗させると
  push の記録そのものが失われる（ref は既に更新済み）ので、決まったバイト数の中で組み立て（イベントは 48 KiB、監査の詳細は
  6 KiB。保存の上限 64 KiB / 8 KiB に jsonb の分の余裕を残す）、収まらない ref はコミットなしで、それも収まらなければそこで
  一覧を止める。省いたものは必ず `refs_truncated` / `commits_truncated` と総数で示す（黙って切り詰めない）。監査の詳細と
  ペイロードは同じ形で、予算だけが違う（`AuditDetails::to_value` と `AuditDetails::event_payload`）。
- **重複除去の鍵**: §6 の `push:<repo_id>:<ref の更新の一覧のハッシュ>` を
  `repository_pushed:<repo_id>:<sha256(request_id ‖ 名前順の ref の更新) の 16 進 64 文字>` に改める（D4 の `<集約>_<動詞>` の接頭辞、
  `DedupKey` の `Id` と `Hash`。最長 18 + 1 + 19 + 1 + 64 文字で 200 に収まり、64 文字は部分の上限ちょうどなので切り詰めない）。
  ハッシュの入力は、領域の札 `forge.repository_pushed.v1`、push の要求の `request_id`、名前順の各更新の（名前、旧、新）で、
  すべて長さを前に付ける。種類とコミットは旧と新から決まるので入れない。
  - **request_id を入れる理由（A→B→A）**: ref の更新の一覧だけでは、`main` を A→B、B→A、もう一度 A→B と push したとき、
    3 回目が 1 回目と同じ鍵になり、別の push なのに `Deduped` で記録されない。要求ごとに違う request_id を入れて 3 件にする
    （テスト `aba_pushes_are_three_events`）。
  - **再試行と再投入は 1 件**: 同じ要求の再試行（MR4 の 3 回）と、Runbook `push-record-failed` の再投入
    （`forge admin repo record-push --from-log`）は、ログの `forge.git.push_applied` の行に残った request_id をそのまま使うので
    同じ鍵になり、既に記録されていれば `Deduped`（何も書かない）。
  - **再投入の行者は運用者**（データベースのレビューの MEDIUM）: ファイルは検証されていない入力なので、ログに書かれた
    利用者を行者にしない。`repositories::replay_push` の監査の行とイベントの行者は `system`、`actor_label` は
    `forge-cli:<OS のアカウント>`、`request_id` は push の要求のもの。元の push した利用者は `replayed_from: {user_id,
    credential_id}` に入れる。通常の push（MR4）の行者は push した利用者（`Actor::User`）のまま。
  - **再投入の照合**: 変更の中で、リポジトリのプロジェクトがログの `project_id` と同じこと、ログの `user_id` の利用者が
    存在することを確かめ、違えば `FG-GIT-0012 reason=push_log_invalid`（`field` = `project_id` / `user_id`）で何も書かない。
    CLI は書く前にファイルのすべての push を同じ照合（`repositories::check_replay`）にかける。書き込みは push ごとに 1 つの
    変更なので、途中で失敗したら同じファイルで再実行する（記録済みのものは `Deduped`）。
- **`forge.git.push_applied`**: forge-web が git の終了後、記録の前に必ず出すログの行の形は `forge_core::PushApplied`（serde の型、
  版 `v = 1`、未知の項目は拒否）で定め、書く側（`PushApplied::log`）と読む側（`PushApplied::from_log_line`）が同じ型を使う。
  記録が書けなければ `FG-GIT-0012` PushRecordFailed（alert、Runbook `push-record-failed`、`PushRecord::failed`）。
- 矛盾した記録（更新が 0 件か 1000 件超、同じ ref が 2 回、旧 = 新、種類と 0 の側の不一致、コミット付きの削除など）は
  `FG-DB-0010 reason=invalid_push_record`（`rule`）で、何も書かない。

## Amendment（2026-11-01、M1 段階 9 MR4: `repository.pushed` の書き手が動き始めた）

- Smart HTTP の push（`POST {path}.git/git-receive-pack`、ADR-0008 の M1 段階 9 MR4 の Amendment）が `record_push` を呼ぶ。
  git の終了後、report-status で `ok` になった ref だけを、種類（旧・新が 0 か、タグ、`rev-list` による祖先の判定）と
  新しいコミット（作成は push 前のどの ref からも届かないもの、更新は `<new> ^<old>`。ref ごとに 20 件まで、すべて数える）付きで
  記録する。数えるのは push 全体で 10 秒まで。超えたら件数は数えた分（下限）で、ref ごとの `total_commits_partial`（ログの行
  `forge.git.push_applied` でも同じ名前、`true` のときだけ書く。MR2 の行も読める）を立て、ペイロードの `commits_truncated` は
  件数にかかわらず `true` になる（黙って少なく見せない）。
- 順序: **先に** `forge.git.push_applied`（再投入の入力）をログに出し、それから `record_push` を 30 秒以内に 3 回まで
  （1 秒、2 秒の間を空けて）試す。`Deduped` は成功。FG-DB-0010（記録が不正）はやり直さない。すべて失敗したら
  `PushRecord::failed` の FG-GIT-0012（`attempts`、`cause_code`、同じ request_id）。種類や一覧の読み取りに失敗したら
  `force` / コミットなし（`total_commits_partial`）として記録し、その失敗もログに出す。
- 記録はクライアントへの応答を閉じてから行う（`git push` は記録を待たない）。`source_ip` は M1 では記録しない
  （リバースプロキシの背後で信頼できる値がまだない。段階 10 以降）。`credential_id` は使われたトークンの ID、
  `user_agent` は 512 バイトまで（保存時にさらに 256 文字へ）。

## Amendment（2026-11-01、M1 段階 8b MR7a: `project.visibility_changed` とプロジェクト・グループの更新）

- **`project.visibility_changed`**（`EventType::ProjectVisibilityChanged`、分類 `maintainer`、§5）の書き手は
  `forge_db::projects::change_visibility_decided`（`PUT /api/v1/projects/{id}/visibility`、Owner）。一つの `ChangeTx` で状態、監査の行
  （`project`/`projects.id`、`project_id` あり）とイベントを書く。ペイロードは監査の details と同じ `{project_id, from, to}`。重複除去の
  鍵なし（状態の変化ごとに新しい事実）。同じ可視性への変更は何も書かない。
- **`project.updated` / `group.updated`** は M1 では監査のみ（段階 8b の計画の回答）。details は `{project_id, fields}` /
  `{group_id, fields}` で、`fields` は変わった項目の名前（`display_name`、`description`）だけ — 値（自由入力の文字列）は残さない。
  `project.updated` の監査の行は `project_id` を持つ（イベントの範囲の規則）。値が変わらない更新は何も書かない。

## Amendment（2026-11-02、手順 D5 MR2: 変更ごとに複数の監査の行）

- `ChangeRecord` は、変更の監査の行（`audit`）に加えて、変更した他のリソースごとの監査の行（`affected`、最大 2000）を持てる。部分木の移動は、
  動かしたリソースの行（`project.moved` / `group.moved` / `user.renamed`）に加え、部分木の中のプロジェクトごとに `project.moved` の行
  （`project`/`projects.id`、`project_id` あり、details `{project_id, namespace_id, moved_namespace_id}`）を書く。ADR-0015 D1 の
  「部分木のパスの書き換えは 1 行の監査でまとめる」の注記を閉じる: 各プロジェクトの監査の履歴に移動が残る。
- 規則: すべての行は同じ outcome（`audit_outcome_mismatch`）。リソースが `project` の行は、どれも `project_id = resource_id`
  （`audit_scope_mismatch`）。**イベントの範囲の規則はイベントごとに確かめる**: イベントの `project_id` は、その変更のどれかの監査の行の
  `project_id` と等しい（`event_scope_mismatch`）。大きさの検査（8 KiB）も行ごと。
- `project.moved`、`group.moved`、`user.renamed`、`namespace_tombstone.released` は MR2 では監査のみ。

### D5 MR3: `project.deleted` / `project.restored`

`EventType::ProjectDeleted` / `ProjectRestored`（integration クラス、§10 の Amendment D5 の表どおり）。ペイロードは監査の details と同じ
`{project_id, deleted_at, purge_after, deleted_by}`（`deleted_by` は operator CLI の削除では null。restore では取り消す削除の値）。
`dedup_key` の形は ADR-0015 の D5 MR3 の節。

### D5 MR4: `project.purged`

`EventType::ProjectPurged`（**admin** クラス。手順 D5 の計画の答え「`project.purged` は admin、他の `project.*` は integration」のとおり:
プロジェクトはもう無いので、Reporter の役割で読める者を `authorize()` が決められない）。`project_id` は付ける（監査の行と同じ）。
ペイロードは監査の details と同じ `{project_id, repository_id, namespace_id, deleted_at, purge_after, deleted_by, tombstone_id,
redirects_ended, members_removed, protected_refs_removed, storage_removed}`（id・数・時刻だけ、パスや名前は入れない）。
`dedup_key` = `project_purged:<project_id>`（プロジェクトの id は再利用されない）。開始の `repository.purge_started` は監査のみ。

### D5 MR5: `project.moved` / `group.moved` / `user.renamed`

MR2 で監査のみにした移動に、**移動したリソースごとに 1 件** のイベントを付けた（MR2 の「形は MR5 で決める」を閉じる）。

| 種類 | クラス | `project_id` | 書き手 |
|---|---|---|---|
| `project.moved` | integration（計画の答え「他の `project.*` は integration」） | そのプロジェクト | `projects::move_decided` |
| `group.moved` | admin（インスタンス全体のイベント。プロジェクトを指さないので Reporter を `authorize()` で決められない、§5） | なし | `groups::move_decided` |
| `user.renamed` | admin（同上） | なし | `users::rename_decided` |

- **祖先の移動で動いたプロジェクトにはイベントを出さない**（監査の行はプロジェクトごとに残る）。1 つの変更のイベントは最大 100 件、
  グループの移動は最大 1000 のプロジェクトに及ぶため。連携はプロジェクトを id で識別し（§5）、パスは表示用のスナップショット
  なので、今のパスは API で読み直す。`namespace_tombstone.released` は監査のみのまま。
- **ペイロード**は監査の details から継承のロールの変化を除いたもの: `{<project_id|group_id|user_id>, namespace_id,
  from_parent_id, to_parent_id, renamed, nodes, projects, reclaimed_tombstones}`（`AuditDetails::event_payload`、
  `MoveSummary::event_value`）。誰が継承のロールを得た・失ったかは監査ログにだけ残す（他の利用者の権限を連携に出さない）。
  パスや名前は入れない。
- **`dedup_key`** は `project_moved:<project_id>:<µs epoch>` / `group_moved:<group_id>:<µs epoch>` /
  `user_renamed:<user_id>:<µs epoch>`。epoch はリソースの行をロックした後に同じ変更の中で読む `clock_timestamp()` の
  マイクロ秒（MR3 と同じ精度）。同じリソースの移動は行のロックで直列になるので 2 つの移動が同じ鍵になることはなく、同じ変更の
  繰り返しだけが `Deduped` になる。

## Amendment（2026-11-03、M3 I1a `feat/cross-integrations-bindings`: 連携とプロジェクトの結びつけ）

計画 11 §5 I1 を 4 つに分けた最初の MR（I1a 結びつけ、I1b 作業項目のリンク、I1c イベントの履歴、I1d 開発状況）。§1 の DDL の概略から変えた点と、
実装で決めたこと。

1. **表**（追記のマイグレーション `20261114100000_integrations.sql`、台帳 `tables.toml` はどちらも `audited`）:
   - `forge.integrations`: `kind`（CHECK `integrations_kind_known`、今は `kanflow` だけ）、`service_account_id`（**連携ごとに 1 つ**、
     `integrations_one_per_service_account`）、`enabled`、`event_types`（1〜64 個、`integrations_event_types_count`）、`created_by`。
     §1 の受信の秘密の列（`inbound_secret_*`）は **まだ足さない**（受信の MR で追記のマイグレーションとして足す。使わない列を先に置かない）。
     ガード `integrations_service_account_guard`（保護の対象。`tables.toml` の `guarded_functions`）が、アカウントがサービスアカウントであること
     （制約名 `integrations_service_account_kind`）と、連携の一生のあいだ変わらないこと（`integrations_service_account_fixed`。結びつけが付けた
     メンバーはそのアカウントのもの）を守る。監査は (`integration`, id): `integration.created` / `integration.updated`。DELETE は無い（無効にするだけ）。
   - `forge.integration_bindings`: §1 の主キー `(integration_id, project_id)` に加えて **`project_id` の一意**
     （`integration_bindings_one_per_project`。02 §2: 1 つのリポジトリを 2 つの kanflow のプロジェクトに結ぶと `PBI-123` があいまいになる。
     §1 の概略では同じプロジェクトを別の連携にも結べたが、連携の種類が kanflow だけの今は、どの連携とでも 1 つに絞る。種類が増えたら
     一意の範囲を追記のマイグレーションで広げる）。`external_project_id`（CHECK `integration_bindings_external_project_id_format`:
     `A-Z a-z 0-9 . _ : -` の 1〜100 文字）、`granted_membership`、`created_by`。監査はプロジェクトの行:
     `integration.bound` / `integration.unbound`。結びつけはプロジェクトの完全削除（purge）で消える。
2. **結びつけはメンバーの変更でもある**（計画 11 Q6）: 結ぶとき、連携のサービスアカウントがまだメンバーでなければ **Reporter** にする
   （`granted_membership = true`。サービスアカウントはメンバーのプロジェクトしか見えない、ADR-0019 Addendum S0 §3）。解除のとき、
   結びつけが付けたメンバーで、**まだ直接の Reporter なら** 外す（手で付けた・手でロールを変えたメンバーは残す）。どちらも同じ変更に
   プロジェクトの `member.added` / `member.removed` の監査の行とイベント（maintainer の分類）を足す（`ChangeRecord::affected`）。
   `integration.*` 自体は監査のみで、イベントにしない（履歴 API の見える集合の変化は I1c の `VisibleSet` のハッシュで 410 にする、§7）。
3. **上限と衝突**: 1 つの連携に 200（`FG-INT-0012` IntegrationLimitReached、409、info）。数えるのは連携の行の `FOR UPDATE` の下なので、
   別のプロジェクトの同時の結びつけでも越えない。すでに結ばれたプロジェクトは `FG-INT-0016` BindingConflict（409、info。`reason` =
   `bound_to_other_integration` / `bound_to_other_external_project`）。同じ外部プロジェクトへの再度の結びつけは `unchanged`（何も書かない）。
   INT の 0013〜0015 は計画 11 §4.4 の O2 / N1 の予定の番号として空けた。forge-errors の AREAS に `INT` を足した。
4. **認可**（`authorize()` と権限の表の allow / deny の行、`matrix_rows_integrations.rs`）:
   - `IntegrationManage`（登録、有効・無効、`event_types`）: **管理者の Web セッションだけ**、人だけ（`is_session_only`、`is_people_only`）。
     管理者の `admin` スコープのトークンでも `FG-AUTH-0002 reason=session_required`（連携が何を読めるかは人が決める。S0 のサービスアカウントの
     作成と同じ形）。トークンで連携を止めたいとき（漏えい）は、サービスアカウントを無効化する（`UserDisable`、トークンはすべて失効）。
   - `IntegrationRead`（一覧・1 件・結びつけの一覧）: 管理者、人だけ（エージェント・サービスアカウント・ボットは不可）。トークンは `admin` と `read_api`。
   - `IntegrationBindingManage`（結ぶ・解除）: プロジェクトの **Maintainer 以上**、人だけ、`api`。見えないプロジェクトは `FG-REPO-0005`、
     削除待ちは `FG-REPO-0006`（`authorize()` の共通の判定。forge-db のコマンドも削除待ちを断る）。代理（kanflow の画面から人として結ぶ）は
     S2 の主張つきの代理で足す（計画 11 §4.3 の「可（主張が必須）」）。
   - プロジェクトの結びつけを読む `GET /api/v1/projects/{id}/integration-bindings` は `ProjectRead`（Reporter。結びつけたサービスアカウントも読める）。
5. **経路**: `/api/v1/admin/integrations`（GET・POST）、`/{id}`（GET・PATCH）、`/{id}/bindings`（GET）、
   `/api/v1/projects/{id}/integration-bindings`（GET）、`…/{integration_id}`（PUT `{external_project_id}` → 200 `outcome` = `bound` / `unchanged`、DELETE → 204）。
   結びつけの経路のローダーはプロジェクトだけを読み、連携の有無は判定の後にコマンドが行のロックの下で見る（判定の前に連携の id の有無を漏らさない）。
   gstn: `admin integration list|view|bindings`、`integration-binding list|set|remove`（登録と変更はブラウザだけ、C-1 の表で `BrowserOnly`）。
6. **層**: 計画 11 §5 は I1 で `forge-integ` のクレートを作るとしたが、I1a の結びつけはプロジェクトのメンバーの変更と同じ「判定を
   2 回（登録簿と、書き込みのトランザクションの中の `Authorized::reconfirm`）」の形なので、サービス層は **forge-hosting** に置いた
   （`integrations::bind_decided` / `unbind_decided` を呼ぶのは forge-hosting だけ、テスト `only_forge_hosting_calls_the_decided_commands`）。
   SQL は forge-db の `integrations`。`forge-integ` はイベントの履歴（I1c: カーソル、`prove_visible`）で作る。
7. **錠の順**: プロジェクトの行 `FOR UPDATE` → 呼び出し元のメンバーの行と利用者の行 `FOR SHARE` → `decide` → 連携の行 `FOR UPDATE` →
   プロジェクトの結びつけ → サービスアカウントのメンバーの行 `FOR UPDATE`・利用者の行 `FOR SHARE`（`projects::member_writes` と同じ向き）。
   連携の更新は連携の行だけを錠にする。

Runbook `integrations`（日本語）。テスト: `binding_grants_and_unbinding_removes_reporter_membership`、`binding_second_kanflow_project_is_refused`、
`binding_limit_200_is_int_0012`、`binding_refused_for_pending_deletion_project`（forge-db）、`integration_manage_is_admin_session_only`、
`binding_requires_maintainer`（forge-web）、制約名を確かめる `integration_checks_refuse_raw_rows_by_constraint_name` /
`binding_checks_refuse_raw_rows_by_constraint_name`、`a_purge_removes_the_projects_integration_binding`。

### I1a の独立レビュー（2026-11-03）で変えた点

1. **無効の連携・無効のサービスアカウントには結べない**（security HIGH / MEDIUM）: 結ぶたびにサービスアカウントへ Reporter を付けるため、
   止めた連携が新しいプロジェクトを得ないこと、無効のアカウントが「有効に戻すと生き返る」メンバーを集めないことを守る。
   `bind_decided` は連携の行を `FOR UPDATE`、サービスアカウントの利用者の行を `FOR SHARE` で読み（先に確定した無効化は見え、
   後の無効化は待つ）、`enabled = false` なら `FG-INT-0017` IntegrationDisabled `reason=integration_disabled`、アカウントが無効なら
   `reason=service_account_disabled`（409、info）。解除は無効でもできる（権限を減らすだけ）。無効のサービスアカウントには連携も作れない
   （`FG-WEB-0006 rule=service_account_disabled`）。
2. **付けたメンバーを人が外したら、付けた印を消す**（security / database MEDIUM）: `projects::remove_member` と `remove_member_decided` が
   同じ変更の中で、そのアカウントが結びつけの連携のサービスアカウントなら `granted_membership` を false にする
   （`integrations::clear_grant`。結びつけの監査のトリガーは、その変更の `member.removed` のプロジェクトの行で満たされる）。人がもう一度
   付けたメンバーを、後の解除が外さない。追記のマイグレーション `20261114100100` で forge_app に UPDATE を許した。
3. **`event_types` の要素の CHECK**（database MEDIUM）: `integrations_event_types_entries`（`20261114100100`）が NULL・空・空白・大文字・
   2 次元の配列を断る（要素は `*`、種類の名前、`<家族>.*` の形だけ。どの分類かは forge-core が確かめる）。
4. **結びつけの答えはトランザクションが書いた値から作る**（rust MEDIUM）: コミットの後に読み直さない（同時の解除で FG-SYS-0001 に
   ならないように）。`bind_decided` が書いた（`unchanged` なら見つけた）結びつけを返す。
5. **同時の作成**: 同じサービスアカウントへの 2 つの作成が同時に来て、負けた方が一意の制約（SQLSTATE 23505、
   `integrations_one_per_service_account`）に当たっても、`FG-WEB-0006 rule=service_account_in_use` にする。
6. **並行のテスト**: 199 件のときの 2 つの同時の結びつけは 1 つだけ通る（もう一方は `FG-INT-0012`）。解除と、人によるメンバーの削除・
   ロールの変更は、どちらの順でも同時でも整合する（どちらもプロジェクトの行を先に錠にする）。
7. **受け入れた残り**: 結びつけの経路の答えで、Maintainer は連携の id の有無を知れる（判定の後、無い連携は
   `FG-REPO-0005 resource=integration`）。連携の id は秘密ではなく、Maintainer は kanflow の設定で id を知る立場なので、受け入れる。
   `forge-db/src/integrations` は `admin` / `bind` / `read` に分けた（動きは同じ）。
8. **再レビュー（2026-11-03）**: forge_app の結びつけの UPDATE は `granted_membership` の列だけにした（追記のマイグレーション
   `20261114100200` が表の UPDATE を取り消し、列の UPDATE を許す。結びつけの他の列は作った後に変わらない）。人が付けたメンバーの
   **ロールを変えた** ときも（削除と同じく）同じ変更で `granted_membership` を false にする（どのロールへの変更でも、そのメンバーは人のもの）。
   サービスアカウントが無効の連携は有効に戻せない（`create` と同じ規則、`FG-WEB-0006 field=enabled rule=service_account_disabled`。
   無効にすること・`event_types` を変えることはできる）。

## Amendment（2026-11-03、M3 I1b `feat/cross-work-item-links`: 作業項目のリンクと `work-item-sync`）

計画 11 §5 I1 の 2 つ目の MR（案 B: 順序を守る購読者）。§1 の DDL の概略と §7 から変えた点と、実装で決めたこと。

1. **参照の検出**（02 §2）: `forge_core::find_refs` は kanflow の K1 と同じ規則
   `/(?<![a-z0-9])pbi[-_ ]?#?(\d{1,7})(?![0-9])/gi`（ASCII だけの大文字小文字の同一視。JavaScript の `u` なしと同じ）を手で書いた
   （Rust の `regex` は前後の読みを持たない）。参照は `PBI-` + 数（先頭の 0 を落とす。`PBI-007` は `PBI-7`）。数は途中で切らないので
   `PBI-430` が `PBI-43` になることはない。**共有のテストのベクタ** `docs/api/pbi-ref-vectors.json`（40 件、版 1）を両方の側の契約にする
   （テスト `pbi_ref_vectors_match_shared_contract`、`pbi_430_is_not_pbi_43`）。I1b の検出元は MR のタイトルとソースブランチの名前、
   push されたブランチの名前。コミットメッセージ（`commit.linked`）と MR の説明文は対象外（計画 11 Q5、02 §13 の決定）。
2. **表**（追記のマイグレーション `20261116100000_work_items.sql`。台帳 `tables.toml` はどれも `derived`）:
   - `forge.work_items`: (連携, 外部プロジェクト, `ref`) で 1 つ（`work_items_identity`）。`ref` は `PBI-<n>` の正規形
     （`work_items_ref_format`）。forge_app は SELECT と INSERT だけ（更新も削除もしない。リンクと履歴が指し続ける）。
   - `forge.merge_request_work_items`: リンクの **区間** の行。`source` = `auto`（検出）/ `manual`（人、`linked_by`）、
     自動は `linked_seq`（その処理のもとのイベント）を持つ（`merge_request_work_items_source_fields`）。終わりは `unlinked_at`・
     `unlink_source`・（人なら）`unlinked_by` を 1 回だけ書く（`merge_request_work_items_unlink_source_fields`）。
     **行は消さない**（02 §5.7「リンクは消えない」）。付け直しは新しい行（開いた区間は組ごとに 1 つ、`merge_request_work_items_one_active`）。
     ガード `merge_request_work_items_guard`（保護の対象、`guarded_functions`）が、リンクの列を変えないこと、終わった行を変えないこと、
     DELETE は削除待ちのプロジェクト（完全削除）だけであることを守る。forge_app の UPDATE は終わりの 3 列だけ。完全削除（purge）は
     MR より先にこの表の行を消す（作業項目とイベントの作業項目は残す。プロジェクトの行を指さない）。
   - `forge.event_work_items`: (イベントの `seq`, 作業項目) の組。§7 の「`event_work_items (work_item_ref, seq)` の索引」は
     `event_work_items_work_item_seq (work_item_id, seq)` にした（参照の文字列ではなく作業項目の id。連携と外部プロジェクトを含む）。
     `forge.events` への外部キーは置かない（保持 C7 で一緒に消す）。
   - **なぜ `derived` か**: 書き手の中心は購読者で、購読者は `DerivedTx` しか持たない（監査の行を書けない）。人のリンク・解除は
     同じ表を `ChangeTx` で書き、プロジェクトに `work_item.linked` / `work_item.unlinked` の監査の行とイベントを書く（`audited` にすると
     購読者が書けない）。
3. **購読者 `work-item-sync`**（`ordered`、forge-db `work_items::WorkItemSync`、`forge worker` の登録簿に追加。初回は先頭から = §1 の既定）:
   - `merge_request.*`（13 種すべて）: MR の行を `FOR NO KEY UPDATE` で錠にし（人のリンク・解除も MR の行を錠にする）、結ばれた
     プロジェクトなら、**今の** タイトルとソースブランチから見つけた参照（最大 20）に自動のリンクを合わせる。新しい参照は
     `source = auto` で付けて派生イベント `work_item.linked`、消えた参照の自動のリンクは終わらせて `work_item.unlinked`
     （`unlink_source = auto`、行は残る）。**人のリンクは自動では終わらせない。人が解除した作業項目は、タイトルにあっても自動では付け直さない**
     （人が付ければ付く）。保存したソースブランチの名前は変わらないので、ブランチの削除・マージ・クローズでもリンクは残る
     （テスト `link_survives_source_branch_deletion`）。そのイベントの時点ではなく今の行を読むので、同じ MR の後のイベントでいつも収束する。
     その後、そのイベントを MR の開いたリンクの作業項目で記録する（`event_work_items`）。
   - `repository.pushed`: 載っている `refs/heads/*` の名前の参照（結ばれたプロジェクトだけ）と、そのブランチから開いている MR の
     開いたリンクの作業項目で記録する（テスト `branch_name_in_push_links_work_item`）。切り詰めた一覧（`refs_truncated`）は載っている分だけ。
   - `work_item.linked` / `.unlinked`: その作業項目で記録する。ほかの種類は作業項目を持たない。知らない種類は飛ばす（新しい書き手）。
   - 上限: 自動のリンクは MR あたり 20（超えたら warn `forge.work_items.detected_capped`）、1 つのイベントの作業項目は 100
     （warn `forge.work_items.event_items_capped`）。どちらもログに残し、黙って切らない。20 にしたのは、結びつけの付け替えの直後でも
     1 回の処理の派生イベント（付ける + 終わらせる）が根あたりの上限 100 を超えないため。
   - 失敗: イベントの形の誤り（`FG-SYS-0001 reason=invalid_event_payload`、`field`）や DB のエラーは、C5 の約束どおり 5 回で
     `FG-DB-0008 reason=stuck`（doctor `subscribers.lag`、Runbook subscriber-lag）。自分では飛ばさない
     （テスト `work_item_sync_failure_stops_and_alerts`）。
   - 派生イベントは `merge_request` の集約、`actor_kind = system`、元の `request_id`、dedup は `work_item_linked:<link_id>` /
     `work_item_unlinked:<link_id>`（行は 1 回付き、1 回終わる）。payload は `{merge_request_id, iid, link_id, work_item_id,
     work_item: {provider, project_id, ref}, integration_id, source, by}`（02 §5.7 の `work_items` の形を含む。題名や本文は入れない）。
4. **イベントの種類**: `work_item.linked` / `work_item.unlinked` を `EventType` に足した（連携の分類。`EventType::ALL` は 35）。
   連携の `event_types` の `work_item.*` が使えるようになった。
5. **人のリンク**（計画 11 §4.3 `WorkItemLink`）:
   - 経路: `GET /api/v1/projects/{id}/merge-requests/{iid}/work-items`（`MergeRequestRead`。開いた・終わったすべての行、古い順、最大 500、
     `truncated`）、`PUT …/work-items/{ref}`（`WorkItemLink`、200 `outcome` = `linked` / `unchanged`）、`DELETE …/work-items/{ref}`
     （`WorkItemLink`、204。今付いていなければ `FG-REPO-0005 resource=work_item_link`）。`{ref}` は正規形の `PBI-<n>` だけ
     （それ以外は検索の前に `FG-WEB-0006 param=ref rule=format`）。
   - 認可: MR のプロジェクトの **Developer 以上**、状態は問わない（マージ済み・クローズでもリンクは付く）、`api`。人に限らない
     （エージェント・サービスアカウント・ボットのトークンも同じロールで可。マージの条件ではなく、検出はタイトルから同じことをする）。
     権限の表の行は `matrix_rows_work_items.rs`（allow 11、deny 12）。代理（kanflow の画面から人として）は S2 で足す。
   - コマンドは forge-db `review::link_work_item_mr_decided` / `unlink_work_item_mr_decided`（他の review のコマンドと同じく、トランザクションの中で
     錠を取った行で `Authorized::reconfirm`。forge-review だけが呼ぶ、`only_forge_review_calls_the_review_commands`）。
   - 新しいエラーコード: `FG-INT-0018` WorkItemProjectNotBound（409、info。結ばれていないプロジェクト。どの kanflow のプロジェクトの
     `PBI-123` か決まらない）、`FG-INT-0019` WorkItemLinkLimitReached（409、info。開いた手のリンクは MR あたり 50）。Runbook integrations。
   - gstn: `mr work-items`、`mr link-work-item --ref PBI-<n>`、`mr unlink-work-item --ref PBI-<n>`。
6. **S0 のレビューから持ち越した 2 件**:
   - サービスアカウントの git の push は `actor_kind = 'user'` で監査されていた（`forge_core::Pusher` にアカウントの種類が無かった）。
     `Pusher.kind`（認証した主体の `users.kind`、既定値なし）を足し、`push_context` がサービスアカウントを `Actor::ServiceAccount` にする
     （監査の行とイベントが `service_account`。テスト `a_service_accounts_push_is_audited_as_service_account`）。手順書の再生
     （`forge admin repo record-push`）は操作者として書くので、ログの行の部品は `Pusher` ではなく `PushReplay` と要求の id にした。
   - 購読者 `merge-request-refresh` は `actor_kind = 'user'` のイベントからだけ版の `pushed_by` を取っていた。`service_account` も受ける
     （テスト `a_service_accounts_push_keeps_pushed_by_on_the_version`）。
7. **受け入れた残り**: (1) 結ぶ前からある MR は、次のイベント（編集・push・コメントなど）までリンクされない（さかのぼりの取り込みは I1d の開発状況か、
   後の手順）。(2) `event_work_items` の保持による削除は C7 と一緒（今は削除の権限を与えない）。(3) 自動のリンクは今の行を読むので、
   古いイベントの処理が後のタイトルでリンクすることがある（同じ MR のイベントの順は守られ、最後には今のタイトルに一致する）。

### I1b の独立レビュー（2026-11-03）で変えた点

最初のマイグレーション `20261116100000` は変えず、追記のマイグレーション `20261116100100`（裏付け）と `20261116100200`（操作者の種類と終わり方）で直した。

1. **人のリンクは監査の行で裏付ける**（database HIGH）: 表は `derived` のままだが、遅延の制約トリガー
   `merge_request_work_items_audited`（ENABLE ALWAYS、保護の対象）が、同じトランザクションに次があることを COMMIT で確かめる。
   人のリンク（`source = manual` の INSERT）は `work_item.linked`、人の解除（`unlink_source = manual`）は `work_item.unlinked`、
   自動から人のリンクへの切り替え（`converted`）は `work_item.linked`、結びつけの解除（`unbound`）は `integration.unbound` の
   成功した監査の行（`resource_type = 'project'`、`resource_id = project_id`。制約名 `merge_request_work_items_audited`）。
   自動のリンクとその終わり（`auto`）は、同じトランザクションで購読者 `work-item-sync` が進んだこと（`event_subscribers.last_txid`、
   C5 の派生の規則と同じ。制約名 `merge_request_work_items_derived`）。どちらも XF002 → `FG-DB-0013`。
   ガードは、人のリンクを人の解除か結びつけの解除でしか終わらせない（`converted` も自動のリンクだけ）。
2. **DELETE は完全削除の間だけ**（database MEDIUM）: 削除待ち（`deleted_at`）ではなく、リポジトリが `purging` のとき
   （完全削除の印、`20261102100200`）だけ。リンクの `(merge_request_id, project_id)` は MR の組への外部キー
   `merge_request_work_items_merge_request_project`（MR 側の一意 `merge_requests_id_project` は R3 からある）。forge_app の INSERT は
   列ごと（`linked_at` は DB が決める）。
3. **知らない種類で止まる**（security / rust MEDIUM）: `work-item-sync` は知らないイベントの種類を `Ok` で飛ばしていた。
   `FG-SYS-0001 reason=unknown_event_type`（`event_type`、`seq`）で失敗し、5 回で `FG-DB-0008 reason=stuck`（新しい書き手の
   イベントのリンクを黙って失わない）。`merge-request-refresh` は `independent`（失敗すると隔離になる）なので warn
   `forge.subscribers.unknown_event_type` を出して進む。
4. **今の結びつけの作業項目だけを記録する**（security MEDIUM）: `event_work_items` には、処理の時点のプロジェクトの結びつけの作業項目だけを書く
   （結びつけがなければ何も書かない）。手の上限 50 も今の結びつけのリンクだけを数える。
5. **結びつけの解除はリンクを終わらせる**（security MEDIUM）: `integrations::unbind_decided` が同じ変更で、そのプロジェクトの開いたリンクのうち
   その結びつけの作業項目のものを `unlink_source = unbound`（解除した人）で終わらせ、数を `integration.unbound` の監査の
   `work_item_links_ended` に残す。**`work_item.unlinked` のイベントは出さない**（レビューの依頼から外した点）: 結びつけを解いた
   プロジェクトのイベントはどの連携の履歴にも見えず（I1c の `VisibleSet`）、1 つの変更のイベントは最大 100 件なので、リンクの多い
   プロジェクトの解除が失敗する。付け替えは「解除してから結ぶ」（`FG-INT-0016`）なので、この 1 か所で足りる。解除は作業項目の id でも
   できる（`DELETE …/work-items/{work_item_id}`、`gstn mr unlink-work-item --ref <id>`）。
6. **人でない操作者は検出を止めない**（security MEDIUM）: `linked_by_kind` / `unlinked_by_kind`（`human`、`agent` = 人の
   エージェントのトークン、`service_account`、`bot`）を記録し、API の答えと `work_item.*` の payload（`by_kind`）に出す。
   検出を止めるのは `human` の `manual` の解除だけ。リンクと解除の権限（権限の表の行）は変えない。
7. **自動のリンクを人がリンクすると人のリンクになる**（rust MEDIUM）: 以前は `unchanged` で、後のタイトルの編集が人の頼んだリンクを
   終わらせていた。自動の区間を `converted` で終わらせ、人の区間を始める（`outcome = linked`、イベントの `replaces_link_id`）。
8. **錠の順**（LOW）: リンクを書くものはすべて プロジェクトの行 → MR の行 の順。人のコマンドと解除はプロジェクトを `FOR UPDATE`、
   購読者は `FOR SHARE` で取ってから MR を `FOR NO KEY UPDATE`（購読者の処理の間に結びつけが変わらない）。`find_refs` の重複は集合で除く。
9. **受け入れた残り**（LOW）:
   - `forge.work_items` の行は消さない（リンクと履歴が指す）。行は (連携, kanflow のプロジェクト, 番号) ごとに 1 つで、見つけた参照の数
     （kanflow の PBI の数が上限の目安。プロジェクトあたり数千）しか増えない。
   - `forge.event_work_items` は連携の分類のイベントごとに 0〜100 行（ふつうは 0〜2 行、1 行 約 40 バイト + 索引）。1 日 1 万イベントで
     年に数百万行・数百 MB 程度。イベントの保持（C7、既定 90 日）でイベントと一緒に消すまで増え続けるので、C7 はこの表の削除を含める。
   - `GET …/work-items` の答えは、MR を読める人（Reporter 以上）に結びつけの kanflow のプロジェクトの id（`project_id`）を見せる。
     同じ値は `GET /api/v1/projects/{id}/integration-bindings`（`ProjectRead`）でも読め、秘密ではないので受け入れる。

### I1b の再レビュー（2026-11-03）で変えた点

1. **FG-DB-0013 の `reason`**: トリガーの制約名を `reason` に対応させた（`merge_request_work_items_audited` →
   `work_item_link_unaudited`、`merge_request_work_items_derived` → `derived_work_item_link`。後者は `derived_` で始まるので、購読者の中なら
   C5 の派生の拒否と同じくイベントの失敗に数える）。以前は `xf002_unknown` になっていた。
2. **監査の行は作業項目を名指す**（追記のマイグレーション `20261116100300`）: 人のリンク・解除・`converted` は、同じトランザクションの
   `work_item.*` の監査の行のうち `details.work_item_id` がその行の作業項目のものだけで満たされる（結びつけの解除の `unbound` は、
   多くのリンクを一度に終わらせるので `integration.unbound` の行 1 つで満たす）。**受け入れた残り**: これは事故の水準の制御で、
   forge_app は監査の行も自分で書ける（C5 の派生イベントの検査と同じ。ADR-0018 の封印が後から改ざんを見つける）。
3. **錠の順**: 購読者は `repository.pushed` と `work_item.*` の処理でも、`merge_request.*` と同じく先にプロジェクトの行を `FOR SHARE` で取る。
4. **I1c への依存**: 結びつけの解除では `work_item.unlinked` を出さない（上の 5）。これが正しいのは、I1c の履歴のカーソルが
   結びつけの世代に結びつき、解除・付け替えのあとは必ず 410（`visible_set_changed`）になって kanflow が開発状況 API から取り直すこと
   が前提である。I1c はこれを満たすこと（満たせないなら、解除で終わらせたリンクのイベントを別の方法で届ける）。

## Amendment（2026-11-03、M3 I1c `feat/cross-integration-event-feed`: 連携のイベントの履歴）

計画 11 §5 I1 の 3 つ目の MR。§7 の「イベントの履歴 API」と §2 の保持を実装した。§1・§7 の概略から変えた点と、実装で決めたこと。
API の契約は `docs/api/integration-feed.md`（schema_version 1、欄は足すだけ）、例は `docs/api/integration-feed.example.json`（kanflow の偽の GitStation の golden）。

1. **層**: 新しい機能のクレート **`forge-integ`**（forge-web / forge-cli → forge-integ → forge-auth / forge-db → forge-core。forge-git にも
   他の機能のクレートにも依存しない。テスト `forge_integ_has_no_edge_to_other_feature_crates`）。SQL は forge-db の `feed`。
2. **`Action` と `Resource`**: `IntegrationEventRead`（`GET /api/v1/integrations/{id}/events`）と `IntegrationEventCursorRead`（`…/events/cursor`）。
   どちらも `read_api`。経路の判定は `Resource::IntegrationFeed { own, enabled, act_as }`（`IntegrationFeedCtx`）: その連携のサービスアカウント
   **だけ**。人（管理者のセッション・`admin` のトークンも）・ボット・別の連携・存在しない連携は同じ `FG-REPO-0005`（`reason` =
   `not_integration_account` / `foreign_integration`）。エージェントのトークン・`Forge-Act-As` の付いた要求・無効の連携は `FG-AUTH-0003`
   （`agent_token` / `act_as_refused` / `integration_disabled`）。プロジェクトごとの判定は `Resource::ProjectEvents { project, class }`:
   連携の分類だけを、サービスアカウントの **メンバーのロール** が Reporter 以上のときに許す（管理者の権限は通さない）。maintainer・
   deployment・admin の分類は常に拒否（`class_not_in_feed`）。権限の表の行は `matrix_rows_feed.rs`。
3. **`prove_visible`**（forge-auth `feed_ctx.rs`）: §7 のとおり、スナップショットで読んだ材料（`forge_core::FeedMaterial`）で、まず連携を
   新しい事実で判定し直し（その間に無効にされたら 403）、結びつけたプロジェクト × 4 つの分類を `authorize()` で判定し、連携の分類で
   許されたものだけで `VisibleSet` を作る。連携の分類以外が許されたら（`authorize()` の不具合）`FG-SYS-0001 reason=feed_class_allowed`
   で止める。`VisibleSet::from_authorized_decisions` を呼ぶのは forge-auth だけ、forge-db の `read_feed_page` を呼ぶのは forge-integ だけ、
   `Feed::read_events` / `head_cursor` を呼ぶのは forge-web の `routes/integration_feed.rs` だけ（forge-integ `tests/decided_callers.rs`。
   §7 の「同じゲート」はこのテスト。エージェントモデルのゲートは必要なら別に足す）。`VisibleSet` は §7 の `Projects` / `Instance` の
   列挙ではなく、連携の分類だけの構造体にした（管理者の履歴は後の MR）。
4. **結びつけの世代**（§7 と「第 3 回の案」の扱いを変えた点。I1b Amendment の「I1c への依存」を満たす）: `integration_bindings.generation`
   （`GENERATED ALWAYS AS IDENTITY`、追記のマイグレーション `20261118100000`。forge_app は選べず、更新もできない）を `VisibleSet` の
   ハッシュに入れた。§7 は「ハッシュなら判定の結果そのものを比べるので世代は要らない」としたが、**同じプロジェクトを解除して結び直すと
   判定の結果は同じ**になり、I1b が解除で `work_item.unlinked` なしに終わらせたリンクを kanflow が知らないまま続けてしまう。世代を足した
   ハッシュなら、解除と結び直し（同じ kanflow のプロジェクトでも）の後は必ず 410 `visible_set_changed`（テスト
   `rebind_to_same_project_returns_410`、`a_rebind_changes_the_visible_set_digest`）。ロール・可視性・削除待ち・`event_types` の変化は
   §7 のとおり判定の結果のハッシュで捕まえる。ハッシュの正規形は `forge-feed-visible-set/1` の文字列（連携、許す種類の名前、
   `<project>:<generation>` の並び）の SHA-256 の先頭 128 ビット。
5. **カーソル**: §7 の `(版, kid, integration_id, epoch, VisibleSet のハッシュ, seq)` を `forge_core::cursor` の AES-256-GCM で封じる
   （purpose `integration.events`、鍵は `api.cursor_keys_file`、一覧と共用）。payload は `1 | integration:u64 | epoch:16 | digest:16 | seq:u64`。
   別の連携のカーソルは `FG-INT-0008 rule=other_integration`、改ざん・書式は `rule` 付きの `FG-INT-0008`（値はログに出ない。
   テスト `tampered_cursor_value_never_logged`）。**カーソルの寿命は保持期間の 1.5 倍**（既定 135 日。`api.cursor_max_age_s` の 24 時間は
   一覧だけのもの）で、それより古い本物のカーソルは `FG-INT-0007 reason=retention`。410 の判定の順は epoch → `ahead_of_head`
   （`seq > high_seq`）→ `retention`（`seq + 1 < retained_from`）→ `visible_set_changed`。410 の本文は標準の 3 つの欄と `reason` だけ
   （forge-web の `error.rs`。`FG-INT-0007` のときだけ、決まった 4 語から）。
6. **読み取り**: 1 つの `REPEATABLE READ READ ONLY` のスナップショット（読み取りのプール）で、状態・材料・イベントを読む。上限は
   **`min(high_seq, work-item-sync の last_seq)`**（作業項目を決める前のイベントを返さない。購読者が無ければ 0、テスト
   `events_beyond_work_item_sync_watermark_withheld`）。§7 の「`after + 5000` の窓」は使わず、`seq > after AND seq <= 上限 AND project_id =
   ANY(見えるプロジェクト) AND event_type = ANY(許す種類)` を 100 行ずつ読む（`has_more` を正確にするため）。最大 500 件・約 2 MiB
   （1 件 = payload + 1 KiB + 作業項目ごとに 160 バイトで数える）。カーソルが無ければ保持されている最も古いイベントから。
   `next_cursor` は、残っていれば最後に返したイベント、残っていなければ上限（先頭まで進む）。各イベントに自分の直後のカーソルを付けた。
   作業項目は `event_work_items` のうち **この連携の** 作業項目（`{provider, project_id, ref}`）。`actor` は `kind`・`user`・`via`
   （`id`・`username`・`display_name` は読み取りの時点のもの）。`user_email` スコープで `email`、**新しいスコープ `user_employee_number`**
   （計画 11 Q7、`user_email` と同じ形: 何も満たさず、何にも満たされない。追記のマイグレーションで `tokens_scopes_known` を広げた）で
   `employee_number` を足す。読み取りのプールはこの 2 列を読めないので、スコープがあるときだけ書き込みのプールで読む。
7. **`integration_feed_reads`**（`derived`）: 読み取りごとに、前の記録から 1 分以上たっていれば 1 行を上書きする（`ON CONFLICT … WHERE
   last_read_at <= now() - 1 分`）。列は `last_read_at`、`cursor_seq`（答えた `next_cursor` の位置）、`read_bound_seq`、`oldest_unread_at`
   （その後の最初の見えるイベントの時刻）。§6 の `ChangeRecord::housekeeping` はまだ無いので、forge-db の中の 1 文で書く。書けなかったら
   そのエラーを 1 回ログに出し、答えは返す。記録を書いたときに `oldest_unread_at` が保持期間の半分より古ければ **FG-INT-0009**
   （warn、`reason=behind`）をその要求の `request_id` でログに出す（テスト `lagging_integration_logs_int_0009`）。`forge doctor`
   `integrations.feed_lag` は、有効で結びつけのある連携が 24 時間読みに来ない（一度も読んでいなければ最初の結びつけから 24 時間）か
   遅れが保持期間の半分を超えたら FG-INT-0009。
8. **保持**（§2 の C7 のうちイベントの分）: `forge.retention_policy`（1 行、`events_days` 既定 90・7〜3650。forge_app は書けない。
   変えるのは追記のマイグレーション）と `forge.events_retention(max_rows, request_id)`（forge_purge の SECURITY DEFINER、EXECUTE は
   forge_app）。`retained_from` から最大 1 万の `seq` の窓で、保持期間より古い続きの行を、`retention_blocking` の有効な購読者の
   `min(last_seq) + 1` で抑えて（保持期間の 1.5 倍を過ぎた行は抑えない）、`event_work_items` ごと消し、`retained_from` を進め、
   `events.retention` の監査の行を書く。`forge worker` のタスク `events_retention`（1 時間ごと、窓いっぱいならすぐ次）。doctor
   `events.retention`（1.5 倍 + 1 日より古い行が残っていれば警告）。購読者の `retention_overrun`（FG-DB-0008）の検出と
   `subscriber_dead_letters` の 30 日の削除は後の手順に残した。
9. **epoch**: `forge.feed_new_epoch(reason, request_id)`（forge_seq の SECURITY DEFINER、EXECUTE は **forge_maint だけ**。`reason` は今は
   `manual` だけ）が、シーケンサーの advisory lock の下で `epoch` と `epoch_rotated_at` を書き換え、`feed.epoch_rotated`（`old_epoch`・
   `new_epoch`、`actor_label` = ログインの名前）の監査の行を書く。`forge admin events new-epoch`（forge-db `Db::connect_maint`、
   `database.maint_url`）が呼び、**FG-DB-0014** FeedEpochChanged（warn）を出す。`rotate_epoch()` による巻き戻しの自動検出
   （`system_identifier`・タイムライン・送出先）は C9。doctor の定義者の関数の検査（`bootstrap::definer`）は 3 つの関数を見る。
10. **送出先との比較**（計画 11 Q1）: `feed.require_sink_compare` は M3 では `false` だけ（`true` は `FG-SYS-0002 reason=unsupported`。
    効かない設定を効いたように見せない）。doctor `feed.sink_compare` が常に警告する。FG-DB-0012 は送出先ができるまで使わない。
11. **doctor**: `integrations.feed_lag`、`subscribers.work-item-sync`（登録されていない・無効なら警告。履歴の上限）、`events.retention`、
    `feed.sink_compare`。
12. **エラーコード**: FG-INT-0007 FeedCursorExpired（410、info）、FG-INT-0008 FeedCursorInvalid（400、info）、FG-INT-0009
    FeedConsumerLag（warn）、FG-DB-0014 FeedEpochChanged（warn）。Runbook `integration-feed-lag`（日本語）。

テスト（02 §8.2 の回帰）: `closed_without_merge_is_returned_and_dev_status_closed`（履歴の部分。開発状況は I1d）、
`close_reopen_merge_in_order`、`events_survive_branch_deleting_merge`、`open_then_close_between_polls_returns_both`、
`expired_cursor_is_410_retention`。ほかに `epoch_mismatch_is_410`、`ahead_of_head_is_410`、`visible_set_change_returns_410`、
`rebind_to_same_project_returns_410`、`other_integrations_cursor_is_400_int_0008`、`tampered_cursor_value_never_logged`、
`same_cursor_same_events`、`next_cursor_advances_to_head_when_nothing_visible`、`unbound_project_old_and_new_events_hidden`、
`project_null_and_maintainer_class_never_in_feed`、`act_as_header_on_feed_is_refused`、`events_beyond_work_item_sync_watermark_withheld`、
`response_capped_2mib_limit_500`、`feed_reads_written_once_per_minute`、`lagging_integration_logs_int_0009`、
`feed_principal_must_be_the_integrations_service_account`、`the_feed_answers_the_golden_example_shape`。

**受け入れた残り**: (1) 1 回の判定は結びつけごとにプロジェクトの access の 1 文（最大 200 文）。結びつけの多い連携で重くなれば
1 文にまとめる。(2) 連携ごとの流量制限（429）はまだ無い。(3) `username` / `display_name` は読み取りの時点のもの（§5 の「その時点の
スナップショット」は、payload に入っている欄だけ）。(4) `event_types` を許可リストに無い名前から作ることはない（`forge-core` の
`allowed_event_types` はこのビルドの `EventType` から選ぶ）ので、古いビルドが知らない種類は新しいビルドになるまで出ない。

### I1c の独立レビュー（2026-11-04）で変えた点

最初のマイグレーション `20261118100000`（O3 との番号の衝突でユーザーの承認を得て `20261117100000` から改名。中身は同じ）は変えず、
追記のマイグレーション `20261118100100` で直した。

1. **世代を forge_app が選べない**（database MEDIUM）: 表の INSERT の権限では `OVERRIDING SYSTEM VALUE` で古い `generation` を書けた
   （結び直しの 410 を外せる）。INSERT を列ごと（`integration_id`・`project_id`・`external_project_id`・`granted_membership`・`created_by`）にした。
   テスト `forge_app_cannot_choose_a_binding_generation`（42501）。
2. **ページの読み取りが backlog の長さによらない**（database MEDIUM）: 2 段にした。(1) プロジェクトごとに `events_project_seq`
   の範囲を `limit + 1` で切って LATERAL で合わせ、`seq` の順に `limit + 1` を取る（触れる行は「見えるプロジェクト数 × (limit + 1)」と
   途中の他の分類のイベントだけ）。(2) その `seq` の行を 100 件ずつ、バイトの予算の中で読む。未読の最も古いイベントもプロジェクトごとに
   1 件の索引の探索。テスト `the_page_reads_a_bounded_number_of_rows_over_a_large_backlog`（見える 6 000 件 + 見えない 5 000 件で、ページが読む
   表と索引の行が 1 100 以下、古いほうの探索が 10 以下。以前の文は 6 000 行を読んでいた）。
3. **結びつけごとの access の判定を 1 文に**（security MEDIUM）: 解決の文（`PROJECT_ACCESS_SQL`）を `unnest` の LATERAL で全ての結びつけに
   まとめた（最大 200 文 → 1 文）。判定は変わらず `prove_visible`。
4. **連携ごとに読み取りは 1 つだけ**（security MEDIUM）: forge-integ の `Feed` が連携ごとの「処理中」の印を持ち、同じ連携の 2 つ目の同時の
   読み取り（`…/events` と `…/events/cursor`）はすぐに `FG-WEB-0010` RateLimited（429、`reason=feed_read_in_flight`、`Retry-After: 1`）。
   待たせない（並べない）。印はプロセスごと（`forge serve` が複数なら、その数まで同時に読める）。テスト
   `a_second_concurrent_read_of_the_same_integration_is_429`。**残る上限**: 1 回は最大 500 件・約 2 MiB・30 秒（forge-web）、1 つの
   スナップショット。時間あたりの回数の制限（トークン・連携ごとの 429 の窓）はまだ無い。
5. **権限の表**（authz / security MEDIUM）: `IntegrationEventCursorRead` に `IntegrationEventRead` と同じ deny の行を足し、管理者の権限を持つ
   サービスアカウント（メンバーでない private のプロジェクト）と internal のプロジェクトの Guest の行を足した。`matrix_rows_feed.rs` は
   allow 8、deny 50。
6. **保持が番号付けを止めない**（database WARN）: `forge.events_retention` は状態の行を最初に錠にせず、行を消した後、最後の UPDATE
   （`retained_from = GREATEST(…)`）でだけ錠にする。1 回の量は 2 000（`RETENTION_BATCH`、関数は 1 万まで受ける）、窓いっぱいのときは
   worker が 500 ms 待ってから次を走らせる。1.5 倍の上限で遅れた購読者を越えて消したときは、監査の行の `overridden` に購読者と
   `last_seq` を残す。テスト `retention_does_not_starve_the_sequencer`、`retention_records_the_subscribers_it_overrode`。
7. **制約と関数の拒否のテスト**（database WARN）: `retention_policy_events_days`・`retention_policy_singleton`・
   `integration_feed_reads_cursor_seq`・`integration_feed_reads_bound` を制約名で、両方の関数の引数の検査（22023）、
   `feed_new_epoch` を forge_app・forge_ro・forge_purge が実行できない（42501）、`events_guard` の 7 日の下限（forge_purge でも XF001）、
   `tokens_scopes_known` の `user_employee_number`。
8. **スナップショットを落としたとき**（LOW）: 30 秒の打ち切りなどで `FeedSnapshot` が `close` されずに落ちても、sqlx は ROLLBACK を
   予約し、その接続の次の利用の前に実行する（加えて `database.idle_in_transaction_timeout_ms` がサーバー側で切る）。テスト
   `a_dropped_snapshot_leaves_no_open_transaction`。
9. **カーソルなしの読み取り**: 保持されている最も古いイベント（`retained_from`）から読む。保持期間の中なら **結びつけより前の履歴も**
   含む（判定は読む時点で行い、サービスアカウントは今 Reporter だから）。kanflow の最初の同期は、まず `…/events/cursor` を取り、
   開発状況 API（I1d）で全体を取り、その位置から読む（`docs/api/integration-feed.md`）。
10. **受け入れた残り**: `FeedMaterial` は forge-core の型なので、Rust ではクレートをまたいで作り手を forge-db に限れない。代わりに
    `prove_visible` を呼ぶのは forge-integ（と forge-auth 自身・forge-db のテスト）だけ、とテスト
    `only_forge_integ_proves_what_the_feed_sees` で縛り、forge-integ は材料を必ず forge-db のスナップショットから読む。

### I1c の再レビュー（2026-11-04）で変えた点

追記のマイグレーション `20261118100200`。

1. **見せない種類を歩かない**（database MEDIUM）: 索引 `events_project_type_seq (project_id, event_type, seq) WHERE seq IS NOT NULL`
   を足し、ページと未読の最も古いイベントは（結びつけたプロジェクト × 見せる種類）ごとの範囲を `limit + 1`（古いほうは 1）で切って
   合わせる。同じプロジェクトの maintainer の分類（`member.*`）や、狭めた許可リストの外の種類は読まない（テスト
   `per_project_reads_skip_types_the_feed_does_not_show`: 3 000 件の `member.added` の後の 1 件で、読む行が 50 以下。索引の前は 3 001 行）。
   触れる行の上限は「組 × (limit + 1)」（組が空なら索引の 1 回の探索だけ）。
2. **200 の結びつけでの正しさ**（database MEDIUM）: 200 のプロジェクトに見せる種類と見せない種類を混ぜて交互に並べ、ページを順にたどった
   結果が全件の走査と同じ（増え続け、抜けも重なりもない）こと、`has_more` が `limit` ちょうどで false、1 少ないと true であることを
   テスト `two_hundred_bound_projects_page_exactly` で確かめる。
3. **保持のロックの順**（database MEDIUM）: 前のテスト（番号付けが止まらない）は、ループ中にイベントを書かないので番号付けが
   何もしない道を通り、意味がなかった。代わりに、状態の行を別の接続が `FOR UPDATE` で持っている間に保持の 1 回が、すでに
   `forge.events` の行の排他ロック（DELETE）を持ったまま最後の UPDATE で待つことを `pg_locks` で確かめる
   （`a_retention_batch_locks_the_feed_state_only_at_its_end`。最初に `FOR UPDATE` する以前の版では赤になる）。
4. **権限の表**（security MEDIUM）: プロジェクトの単位の行も含め、`IntegrationEventRead` のすべての行に `IntegrationEventCursorRead` の
   同じ行がある（16 行を足した）。テスト `the_cursor_route_mirrors_every_feed_row` が、片方だけの行を見つけると落ちる。
5. **小さな点**（LOW）: 保持は `pg_try_advisory_xact_lock` で同時に 1 回だけ（2 つ目はすぐ戻る）、消した行が 0 件なら監査の行を書かない
   （`retained_from` は進める。`an_empty_retention_batch_writes_no_audit_row`）。結びつけの access をまとめた文は、行の列名とぶつからない
   `feed_binding_project_id` で結び、行のプロジェクトがその id でなければ `FG-SYS-0001 reason=feed_access_row_mismatch`。Reporter・
   Guest・メンバーでなくなった・削除待ちの 4 つを 1 文で読み分けるテスト（`the_batched_access_read_gives_each_binding_its_own_facts`）。
   `prove_visible` の呼び出し元のテストは名前だけで探す（コメントの行は除く）。経路で 429 に `Retry-After: 1` が付くテスト
   （`a_concurrent_feed_read_answers_429_with_retry_after`）。

### I1c: gstn の対応（C-1、2026-11-04）

履歴の 2 つの経路はブラウザだけの経路ではない（その連携のサービスアカウントのトークンだけが読める）ので、`BrowserOnly` にはせず、
gstn に読み取りのコマンドを足した: `gstn integration events <id> [--cursor C] [--limit N] [--json]`（`GET /api/v1/integrations/{id}/events`）と
`gstn integration cursor <id> [--json]`（`…/events/cursor`）。どちらも `read_api`。サービスアカウントのトークン（`GSTN_TOKEN`）で、
運用者が連携の読み取りを確かめるのに使う。カーソルは gstn も読まない（`A-Z a-z 0-9 - _` の 1024 文字までだけを確かめて送る）。
forge-client の `coverage.rs` に `Command` の行、ヘルプのスナップショットを更新した（テスト `integration_feed_commands_read_the_feed_routes`）。

## Amendment（2026-11-04、M3 I1d `feat/cross-work-item-dev-status`: 開発状況）

計画 11 §5 I1 の 4 つ目の MR。計画 02 §5.2 の開発状況 API と、410 からの復旧（§11.2: `/cursor` → 開発状況 → その位置から読む）の
2 段目を実装した。API の契約は `docs/api/dev-status.md`（schema_version 1、欄は足すだけ）、例は `docs/api/dev-status.example.json`
（kanflow の偽の GitStation の golden）。作業項目のリンクのマージの条件は ADR-0024 の I1d の節。

1. **経路**（計画 11 の決定 6: URL は既存に合わせてハイフン。02 §5.2 の `/work_items/{provider}/{ref}/dev_status` は連携の下に移した）:
   `GET /api/v1/integrations/{id}/work-items/{ref}/dev-status?project_id=&state=`（1 つの PBI）と
   `GET /api/v1/integrations/{id}/dev-status?cursor=&limit=&state=`（すべての作業項目をページで。復旧用）。`state` は `all`（既定）・
   `open`・`merged`・`closed`。`project_id`（kanflow のプロジェクト）を省くと、連携の見える結びつけのすべての kanflow のプロジェクトの、
   その参照の作業項目（1 つの連携が複数の kanflow のプロジェクトを結べるので、`PBI-12` だけでは 1 つに決まらない）。
2. **判定**: 新しい `Action::IntegrationDevStatusRead`（`read_api`）。経路の判定は履歴と同じ `Resource::IntegrationFeed`（その連携の
   サービスアカウントだけ。人・管理者・ボット・別の連携・存在しない連携は FG-REPO-0005、エージェント・`Forge-Act-As`・無効の連携は
   FG-AUTH-0003）。見える範囲は履歴の `prove_visible` の `VisibleSet` をそのまま使う（同じスナップショット）。権限の表の行は
   `matrix_rows_dev_status.rs`: `IntegrationEventRead` のすべての行の双子（テスト `the_dev_status_routes_mirror_every_feed_row`）。
   連携の `event_types` が `merge_request.*` を許さないなら MR を出さない（`VisibleSet::shows_merge_requests`）。レビューで、承認は `merge_request.approved` / `.unapproved`、チェックは `check.completed` を許すときだけ出す（`shows_approvals` / `shows_checks`、出さないときは `null`。許可リストは連携が
   読むものを狭める）。gstn: `integration dev-status <id> --ref PBI-<n>`・`integration dev-status-list <id>`（I1c と同じく、
   サービスアカウントのトークンで運用者が確かめる読み取りのコマンド。人の経路が無いので `gstn pbi` は作らない）。
3. **答え**: 作業項目ごとに、**開いたリンク**の MR（新しい順、最大 100、`merge_requests_truncated`）を、MR のプロジェクトがその作業項目の
   結びつけで、見えるときだけ。各 MR は `project {id, path}`・`iid`・`title`・`state`・`draft`・ブランチ・`head_sha`・時刻（`merged_at` は
   適用されたマージの試みの `finished_at`、`closed_at`）・`approvals.given`（今の head と target の生きている承認。数えるかは状態の API）・
   `checks`（C1: 今の head の実行すべて）と `checks_state`（`none` / `pending` / `failure` / `neutral` / `success`。**報告が無いのは
   `none` で成功ではない**）・`link {source, linked_at}`。02 §5.2 の例の `branches`・`deployments`・`done_candidate` は後の MR（欄を足す）。
4. **`as_of_seq` と `feed_cursor`**: 1 つの `REPEATABLE READ READ ONLY` のスナップショットで読み、`as_of_seq` = 履歴の読む上限
   `min(high_seq, work-item-sync の last_seq)`（その位置までのイベントの変化はすべて反映。MR の行はイベントと同じトランザクション、
   リンクは購読者がその位置まで進んでいる）。`feed_cursor` はその位置の履歴のカーソル（`/events/cursor` がその時点で返すものと同じ位置）で、
   そこから履歴を読めば取りこぼさない（その後の変化が既に反映されていることはある: 少なくとも 1 回）。したがって §11.2 の順番
   （`/cursor` → 開発状況 → その位置）でも、開発状況の `feed_cursor` からでもよい。
5. **ページ**: 作業項目の id の順。`limit` 1〜100（既定 50）、1 ページの MR が約 1000 件を超えるとそこで切る。ページのカーソルは
   `forge_core::DevStatusPosition`（purpose `integration.dev_status`、履歴のカーソルと別の purpose なので互いに通らない）: 最初のページの
   履歴の位置（連携・epoch・`VisibleSet` のハッシュ・`seq`）と最後の作業項目の id。**どのページも最初のページの `as_of_seq`** を返し、
   後のページは履歴のカーソルと同じ順で確かめる（epoch → `ahead_of_head` → `retention` → `visible_set_changed`、FG-INT-0007。
   この連携のものでなければ FG-INT-0008）。たどる途中の結びつけ・ロール・`event_types` の変化は 410 で、最初からやり直す。
   追記のマイグレーション `20261119100100` が `work_items (integration_id, id)` の索引を足した。
6. **エラーコード**: 新しいコードは無い。結ばれていない kanflow のプロジェクトの参照（または見える結びつけが無い）は
   **FG-REPO-0005**（`resource=work_item reason=unbound_ref`、計画 11 の `unbound_ref_is_404`）。まだ誰も名指していない参照は 404 ではなく
   空の `merge_requests`。履歴と同じ「同時に 1 つ」の 429（FG-WEB-0010）、30 秒の打ち切り（FG-WEB-0002 `operation=integration_dev_status`）。
7. **呼び出し元**: `Feed::work_item_dev_status` / `Feed::dev_status_page` を呼ぶのは forge-web の `routes/integration_dev_status.rs` だけ
   （forge-integ `tests/decided_callers.rs`）。SQL は forge-db `feed::dev_status`（`FeedSnapshot` のメソッド、`VisibleSet` の project id だけを SQL にする）。

テスト: `dev_status_returns_open_merged_closed`、`no_checks_is_distinct_from_success`、`bulk_dev_status_paginates_all`、`unbound_ref_is_404`、
02 §8.2 の `closed_without_merge_is_returned_and_dev_status_closed`（開発状況の部分。forge-integ）、経路の
`dev_status_principal_must_be_the_integrations_service_account`、`dev_status_parameters_are_checked`、
`the_dev_status_answers_the_golden_example_shape`（forge-web）、`a_dev_status_cursor_round_trips_and_is_bound_to_its_purpose`（forge-core）、
gstn の `integration_dev_status_commands_read_their_routes`。Runbook `integration-feed-lag` の復旧に開発状況 API を書いた。

**受け入れた残り**: (1) `approvals.given` は数えるかを判定しない（判定は git と規則が要る。状態の API を使う）。(2) `project.path` は読む時点の
パス。(3) 1 回の答えは最大 100 作業項目 × 100 MR を 1 文で読む（上限の中）。

## Amendment（2026-11-05、M3 N1 `feat/cross-notifications`: メールの通知）

計画 11 §5 N1 と §10 Q6。社内の SMTP の事実（ホスト・送信元・認証）は分かっていない（Q6 の答えは該当の MR に着手する前に利用者に聞く、と
2026-11-02 に決めた）。**分かっていないものはすべて設定の鍵にし、起動時に検証し、`notify.enabled = true` と中継の名前が揃うまで何も送らない**
形で実装した。実際のホストには一度も接続していない（試験は同じプロセスの偽の中継）。

1. **通知する範囲（この MR の最小限の集合）**: 計画 11 §3 は「レビューの依頼・承認・マージ・ミラーの失敗」を挙げる。いま発行されているイベントで
   受け手が決まるものは次の 3 つで、**どれも MR の作者に伝える**。自分の操作は伝えない。
   | イベント | 種類（`notification_preferences.muted_kinds` の値） | 伝えない場合 |
   |---|---|---|
   | `merge_request.approved` | `mr_approved` | 承認した人が作者 |
   | `merge_request.merged` | `mr_merged` | マージした人（`merged_by`）が作者 |
   | `merge_request.commented` | `mr_commented` | コメントした人が作者、またはスレッドの解決・再開（`action != note_added`） |
   **レビューの依頼**: レビュー担当者という概念もイベントも R1〜R6 に無い（`merge_request.review_requested` は 02 §5.8 の一覧にあるが未発行）→ そのイベントを足す
   MR と一緒に種類を足す。**ミラーの失敗**: O2 のイベントと一緒。**メンション**: コメント本文の解析が無い → 後。種類の追加は
   `NotificationKind`（forge-core、`EventType` を `_` なしで網羅）と表の CHECK（追記のマイグレーション）を一緒に変える。
2. **購読者 `notify`（`independent`）**: イベントごと・受け手ごとに 1 行を、購読者の進みと同じトランザクションで `forge.notifications` に書く
   （`UNIQUE (event_id, user_id)` なので、繰り返された手順は 2 行目を足せない）。書く時点で判断し、理由を残す: `pending`（+ ジョブ `notification.send`）、
   `skipped`（`muted` / `opted_out`）、`suppressed`（`recipient_inactive` / `recipient_not_person` / `no_address` / `address_invalid` / `not_visible` /
   `merge_request_gone`）。「なぜメールが来ないのか」は表を見れば答えが出る。未知のイベントの型は警告して通す（`forge.subscribers.unknown_event_type`）。
   `notify.enabled = false` のワーカーは購読者を登録しない（送る者のいない通知を溜めない）。**無効の間のイベントは後で有効にしても遡らない**（受け入れた制限）。
3. **見える人だけ（送る時にもう一度）**: 可視性は `forge_auth::prove(MergeRequestRead)` を受け手を「セッションの人」として呼ぶ（画面で MR を見せるのと同じ判断。
   この crate に権限の規則は無い）。**登録時と、メールを作る直前の 2 回**判断する: 間にメンバーから外された・無効にされた・メールを止めた人には送らない
   （`suppressed` / `skipped` と理由）。プロジェクトの削除待ち・存在しないプロジェクトも「見えない」。
4. **配信 = ジョブ `notification.send`（ジョブの待ち行列の上）**: 再試行・待ち（10 秒から倍々、上限 1 時間）・鍵・リース・doctor のジョブの健康は待ち行列のもの。
   `max_attempts` は `notify.max_attempts`。**ワーカーの専用のランナー**（保守のレーンと同じ。中継が遅いと 1 件が `notify.send_timeout_ms` まで塞ぎ、1 つずつしか
   動かない主のランナーのマージ・作成のジョブを遅らせるため）。ジョブの内容は通知の id だけ。
5. **二重に送らない（at-most-once）**: 状態機械は forge-db `notify`（`begin_send` が行を `FOR UPDATE` で取って `sending` にし、`finish` は実行の番号で守る）。
   - 相手が受け取っていないと分かる失敗（接続・TLS・ログイン・時間切れ・4xx）→ `retrying`、ジョブを再試行。最後の試行は `failed`（`tries_used_up`）。**ジョブは成功で終える**
     （落とすと `dead` = FG-DB-0006 のアラートが SMTP の障害の件数だけ鳴る。結果は通知の行が持つ）。5xx → `failed`（`rejected_permanent`）、再試行しない。
   - **本文を渡し終えたのに応答が無い**（接続が切れた・時間切れ）→ `unknown`（`outcome_unknown`）。中継が受け取ったかもしれないので**再び送らない**。
   - 送っている最中に実行が死んだ → 次の実行が `sending` を見つけて `unknown`（`interrupted`）にする。終わった行へのジョブの再実行は何もしない。
   - 安定した `Message-ID`（`<notification-{id}@…>`）を付ける（人にも中継にも同じ通知と分かる）。
6. **SMTP の自前のクライアント（`smtp.rs`）— crate を調べた上で**: `lettre` を調べた（MIT、事実上の標準）。依存が 6 つ増え（async-trait・email-encoding・nom・
   quoted_printable ほか）、**失敗が「本文を渡す前」か「後」かを呼び出し側に教えない**（接続が切れたことしか言わない）。5. の at-most-once はその区別がなければ作れない
   ので、段階を持つ小さなクライアントを書いた（接続 → TLS → EHLO → STARTTLS → AUTH PLAIN → MAIL → RCPT → DATA → 本文 → QUIT）。段階の失敗は `FG-INT-0015` の
   `stage` / `reason` / `smtp_code` になる。本文の組み立ては `mail.rs`（ヘッダーは 1 行・RFC 2047、本文は base64、`Auto-Submitted: auto-generated`）。
   - **接続は `notify.connect_timeout_ms`、会話全体は `notify.send_timeout_ms`**（1 つの締め切りをすべての段階が共有）。応答の行は 2 KiB・64 行まで。
   - **TLS は設定どおりに必須**: `starttls` で中継が STARTTLS を出さない・拒む場合は clear text で続けない（`tls_failed`、MAIL FROM の前に止まる）。STARTTLS の前に
     届いていたバイトがあれば拒む（CVE-2011-0411 型）。信頼する根は `notify.smtp_ca_file` の PEM **だけ**（`starttls` / `tls` では必須、無ければ起動時に FG-SYS-0002 `reason=missing`。OS の信頼の保管庫も公開の根の束も使わない。レビュー指摘で保管庫への格下げを削除）（公開の根の束は入れない）。
     rustls は ring（reqwest が既に使うもの。`aws-lc-rs` は入れない）。
   - **認証は AUTH PLAIN のみ、TLS の上でだけ**（`tls = none` と認証情報の組は起動時に FG-SYS-0002 `credentials_without_tls`）。パスワードは `notify.smtp_password_file`（0600）。
     ログにもエラーにも `Debug` にも出ない。
   - 宛先・送信元はメールボックス 1 つの形だけ通す（`forge_core::config::valid_mailbox`: ASCII、`@` 1 つ、点のあるドメイン）。改行や `>` を含む値は命令にならない。
7. **メールの中身**: 受け手が GitStation で見られるものだけ（プロジェクトのパス、MR の番号とタイトル、操作した人の表示名、`web.base_url` があればリンク
   `{origin}/{path}/-/pull/{iid}`）。ブランチ名・コミット・パス・エラー・コードは入れない。タイトルと名前は 1 行にし（制御文字は空白）、長いものは切る。
   言語は本人の設定、無ければ `notify.default_language`（`ja`）。ja / en / id の 3 つの文と、止め方（`gstn notifications set`）を末尾に書く。
8. **本人の設定（`notification_preferences`、`GET|PUT /api/v1/user/notification-preferences`）**: メールの入り切り・言語・止める種類。**本人だけ**（ルートに利用者が無く、
   `Resource::NotificationPreferences { owner }` を他人のものにすると、管理者でも `FG-REPO-0005`）。`Action::NotificationPreferencesRead`（`read_api`）/ `…Write`（`api`）、
   許可と拒否の行は `matrix_rows.rs`。**管理者の読み取りは作らない**: 利用者の連絡の好みは本人のもので、サポートは通知の行（理由つき）を見れば足りる。
   エージェントの資格情報は拒まない（`people_only` ではない）。**監査はしない**（誰かのアクセスも他人のデータも変えない。変更はこの表の `updated_at` だけ）。
   PUT は全体の置き換えで、止める種類は並べ替えて重複を除く。未知の言語・種類は `FG-WEB-0006`（欄の名前はログだけ）で何も変えない。設定は `notify.enabled` に
   関係なく保存する（後でメールを有効にしたときに効く）。
9. **失敗・観測**: `FG-INT-0015`（warn、`reason`: `connect_failed` / `connection_lost` / `tls_failed` / `auth_failed` / `timeout` / `rejected_temporary` /
   `rejected_permanent` / `tries_used_up` / `outcome_unknown`、`stage`、`smtp_code`、`notification_id`、`attempt`）をジョブの境界で 1 回、`request_id` つきで出す。宛先・本文・
   中継の返事の文はログに出さない。`forge doctor`: `notify.queue`（10 分より古い待ち・24 時間の `failed` / `unknown`）と `notify.relay`（TCP の接続だけ、何も送らない）。
   Runbook `notification-email`。
10. **受け入れた残り**: (1) TLS の握手の通し試験は無い（証明書の生成の依存を試験に入れない判断）。設定・CA の読み込み・格下げの拒否・段階の分類は試験している。
    (2) 通知の行の保持期間は決めていない（1 行 約 200 バイト。アプリ内の通知 U4 が同じ表を使うので、そこで `read_at` と一緒に決める）。
    (3) 1 通知 = 1 通（ダイジェストなし。計画 08 の「まとめて」は後）。(4) 送るのは 1 件ずつ（レーンのランナーが 1 つ）。
    (5) 本文の言語は受け手の設定で、操作した人のものではない。

テスト: forge-core の `exactly_the_planned_events_notify`、`notify_*`（設定の検証）。forge-db の `notify::tests`（設定・1 行 1 ジョブ・`begin_send` の 1 回・`unknown`・理由つきの終わり・
backlog）。forge-notify の `tests/smtp.rs`（偽の中継で配信・段階ごとの失敗の分類・時間切れ・AUTH・格下げの拒否・`smtp_failures_are_int_0015`）、`tests/pipeline.rs`（購読者・見えなくなった人・
設定・言語・内部の情報が無い・再試行の上限・永続的な拒否・unknown・二重に送らない）。forge-web の `notification_routes`。forge-cli のワーカーの通し試験
`the_worker_turns_an_approval_event_into_a_mail` と doctor の `notify::tests`。gstn の `tests/notifications.rs`（`get_reads_the_own_settings_route`、`set_changes_only_what_it_is_told_and_keeps_the_rest` ほか）。
