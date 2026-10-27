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
| FG-DB-0006 | JobDeadLettered | alert | — | 内部のジョブが上限に達した。`job_id`、`kind`、`attempts`、`last_error_code`、`origin_request_id` |
| FG-DB-0007 | JobLeaseExpired | warn | — | リース切れの回収、またはフェンシングで結果を捨てた。`job_id`、`kind`、`worker_id`、`attempt` |
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

- 新規の登録（実装時、テストと同じコミット）: FG-DB-0005〜0008、0010、0013、0014、0016、FG-INT-0004〜0012（0006 は既存の定義を確定）。ADR-0018 の分は ADR-0018 §6。
- 新しい Runbook（alert のもの）: `outbox-sequencer-lag.md`、`job-dead-letter.md`、`subscriber-stuck.md`、`event-rejected.md`、`change-without-audit.md`。
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
  `details` は 8 KiB・`payload` は 64 KiB まで。
- **個人の入力の扱い**: `actor_label`（64 文字）と `user_agent`（256 文字）は拒否の理由にせず、文字の境界で切り詰めて制御文字を除く。長い値で拒否の監査そのものが書けなくなることを防ぐ。
