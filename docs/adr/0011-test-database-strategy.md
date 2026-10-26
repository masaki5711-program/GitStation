# ADR-0011: テスト用の DB — docker compose の PostgreSQL とテストごとの新しい DB

**Date**: 2026-10-26
**Status**: accepted
**Deciders**: Masaki Fujikawa（ユーザー）、Claude

## Context

ADR-0002 で開発とテストも PostgreSQL にした。Windows の開発機（Rancher Desktop）と Linux の CI の両方で、
速く確実に DB のテストを動かしたい。fmt / clippy の Stop フックは DB なしでも動く必要がある。

## Decision

- `Git/ops/dev/compose.yml` で PostgreSQL 16 を起動する。テストは `#[sqlx::test]` を使い、テストごとに新しい DB を作って
  マイグレーションを適用する。CI ではサービスコンテナとして起動する。
- `.sqlx/` のオフラインデータをコミットし、`SQLX_OFFLINE=true` でビルドする（DB なしでもコンパイルできる）。
- DB に接続できないときはテストを **黙ってスキップせず**、「`docker compose -f ops/dev/compose.yml up -d` で DB を起動する」
  という案内とともに失敗させる。

## Alternatives Considered

### testcontainers
- **Why not**: Windows（Rancher の named pipe）で不安定。テストごとの起動が遅い

### 組み込みの PostgreSQL バイナリ
- **Why not**: プラットフォームごとのダウンロードが要る

## Consequences

### Positive
- テストが本番と同じ SQL で動き、テスト同士が独立する

### Negative
- 開発機で DB のコンテナを起動しておく必要がある

### Risks
- `.sqlx/` のオフラインデータの更新漏れ → `cargo sqlx prepare --check` を検証の手順に入れる
