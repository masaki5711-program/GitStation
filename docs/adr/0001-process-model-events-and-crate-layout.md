# ADR-0001: プロセス構成、ドメインイベント、クレートの依存の向き

**Date**: 2026-10-26
**Status**: accepted
**Deciders**: Masaki Fujikawa（ユーザー）、Claude

## Context

GitStation は Git ホスティング、MR、CI/CD、ローカル LLM による検証、kanflow 連携、仕様駆動開発を
1 つの製品で提供する。機能が増えてもコアを書き換えずに済み、かつ Claude がログとコードだけで
障害を追える構造が必要。外部のキューやクラウドには依存できない（オンプレミス）。

## Decision

- 単一バイナリに `serve`（HTTP / API / Smart HTTP）と `worker`（Webhook 配信、通知、LLM 検証、保守）を持たせ、Runner は別バイナリにする。
- 状態の変更と同じトランザクションで **ドメインイベントを Outbox に書き**、Webhook・通知・LLM・連携・索引はすべてその購読者として作る。
- ジョブキューは PostgreSQL の `FOR UPDATE SKIP LOCKED` で実装する。
- 依存の向きは `web / cli → 機能クレート → auth / git / db → core → errors`。機能クレート同士は依存せず、イベントか `core` の trait で連携する。

## Alternatives Considered

### 外部のメッセージキュー（RabbitMQ、Kafka など）
- **Pros**: 実績があり、スループットが高い
- **Cons**: 運用するミドルウェアが増える。DB との二重書き込みで不整合が起きうる
- **Why not**: 想定規模（開発者 50〜300 人）では PostgreSQL で足り、Outbox で整合を保てる

### 機能ごとのマイクロサービス
- **Pros**: 独立にデプロイできる
- **Cons**: 障害の調査がサービスをまたぐ。運用の負担が大きい
- **Why not**: Claude が保守する前提では、単一バイナリ + 明確なクレート境界の方が追いやすい

## Consequences

### Positive
- 機能の追加がイベントの購読者の追加で済み、コアが安定する
- イベントの履歴が、kanflow 連携の「取りこぼしのない API」（ADR-0005）の土台になる

### Negative
- PostgreSQL への依存が強くなる（ADR-0002 で開発環境も PostgreSQL にする）

### Risks
- Outbox とキューのテーブルの肥大 → 保持期間と定期的な削除をジョブにし、`doctor` で監視する

## Amendment (2026-11-01, M1 phase 8b MR7a): 最初の機能クレート `forge-hosting`

- グループとプロジェクトの書き込み（`POST /api/v1/groups`、`POST /api/v1/groups/{id}/subgroups`、`GET|PATCH /api/v1/groups/{id}`、
  `POST /api/v1/projects`、`PATCH /api/v1/projects/{id}`、`PUT /api/v1/projects/{id}/visibility`）のコマンドを、最初の機能クレート
  **`forge-hosting`** に置く。依存の向きは `forge-web → forge-hosting → forge-auth / forge-db → forge-core → forge-errors`
  （`forge-cli` も `forge-hosting` に依存してよい）。`forge-web` は forge-db の型を直接使わず、forge-hosting の API と forge-core の
  trait だけを読む。
- `forge-hosting` は SQL を持たない（db-write-boundary ゲート）。トランザクションは forge-db の `*_decided` コマンドが開き、対象行を
  ロックし、呼び出し元のロールを membership 行の `FOR SHARE` の下で読み直し、呼び出し元の users 行を `FOR SHARE` で読んだうえで、
  その事実を `decide` に渡す。`forge-hosting` の `decide` は `forge_auth::Authorized::reconfirm`（同じ呼び出し元・同じアクションで
  `authorize()` をもう一度）で、拒否ならロールバックする。ルート登録（registry）の証明は入口の判定、トランザクション内の判定が
  書き込みを縛る判定であり、判定点は 1 つ（`authorize()`）のまま、確認と使用の間の隙間（TOCTOU）をなくす。
- 依存の向きはテスト `crates/forge-hosting/tests/dependency_direction.rs` が全クレートの `Cargo.toml` から検査する（表にない
  クレート、表にない辺は失敗）。機能クレート同士は依存しない（表に書かない）。
