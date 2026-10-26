# ADR-0012: Outbox のイベントに欠番のない順序番号を付ける（シーケンサー）

**Date**: 2026-10-26
**Status**: accepted（ADR-0001 の補足）
**Deciders**: Masaki Fujikawa（ユーザー）、Claude

## Context

kanflow はイベントの履歴をカーソルで読む（ADR-0005）。`bigserial` の ID をカーソルにすると、トランザクションが
ID の順とは違う順でコミットしたときに、読み手が後からコミットされた行を飛ばしてしまう。後から直すのは難しい。

## Decision

- `events` に `seq bigint NULL UNIQUE` を持たせ、書き込み時は NULL にする。
- `forge worker` の中で 1 つのシーケンサー（PostgreSQL の advisory lock で単一化）が、コミット済みで `seq` が NULL の行に、
  見えた順で `seq` を振る。読み手は `seq` で読む。
- シーケンサーの遅れ（`seq` が NULL のまま一定時間を超えた行）は `forge doctor` の検査項目と `FG-DB-0005` の警報にする。

## Alternatives Considered

### `bigserial` の ID をそのままカーソルにする
- **Why not**: コミットの順序の逆転で行を飛ばす

### `pg_snapshot_xmin` / xid8 で可視性を判定する
- **Pros**: シーケンサーが不要
- **Cons**: 実装とテストが難しく、読み手のクエリが複雑になる
- **Why not**: 単純さとテストのしやすさを優先する

## Consequences

### Positive
- 欠番がなく、何度読んでも同じ結果になるイベントの履歴が得られる

### Negative
- イベントが読めるようになるまでに、シーケンサーの周期分の遅れが出る

### Risks
- シーケンサーの停止 → doctor と警報で検知する。テスト `seq_is_gap_free_when_transactions_commit_out_of_order` で保証する
