# ADR-0002: 開発環境も PostgreSQL にする（SQLite は使わない）

**Date**: 2026-10-26
**Status**: accepted
**Deciders**: Masaki Fujikawa（ユーザー）、Claude

## Context

当初は本番 PostgreSQL、開発 SQLite の予定だった。しかしジョブキュー（`SKIP LOCKED`）や JSONB など、
ADR-0001 の設計が PostgreSQL の機能に依存する。2 種類の SQL を保守すると、テストの抜けと
障害調査の負担が倍になる。

## Decision

開発環境とテストも PostgreSQL（コンテナ）を使う。SQLite はサポートしない。
データアクセスは trait の背後に置き、単体テストではメモリ上の実装を使える。

## Alternatives Considered

### 本番 PostgreSQL / 開発 SQLite
- **Pros**: 開発環境の準備が軽い
- **Cons**: SQL の方言の差でテストが本番の挙動を保証しない。キューの実装が 2 つ要る
- **Why not**: 「テストが通ったのに本番で壊れる」原因を作る

## Consequences

### Positive
- テストと本番の SQL が一致する。sqlx のコンパイル時検査も 1 種類で済む

### Negative
- 開発機にコンテナの実行環境が要る（Rancher Desktop は導入済み）

### Risks
- CI の Runner にも PostgreSQL が要る → サービスコンテナとして起動する
