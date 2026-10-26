# ADR-0014: マイグレーションの方針 — 明示的に適用し、起動時に検査し、前に向かってだけ直す

**Date**: 2026-10-26
**Status**: accepted
**Deciders**: Masaki Fujikawa（ユーザー）、Claude

## Context

M1 phase 2 で DB の土台（forge-db）ができた。これからテーブルを足していくので、スキーマの変え方を先に決めておく必要がある。決めたいことは 3 つある。

- 本番のスキーマを、いつ、誰の承認で変えるか。契約 §3-7 は「本番の変更には人の承認が要る」としている。
- バイナリと DB の版がずれたときにどうするか。バイナリを戻した場合も含む。
- 適用済みのマイグレーションは変えない（契約 §3-6、`migration-immutability` フック）。これをどう運用で守るか。

## Decision

- **明示的に適用する。** マイグレーションを適用するのは `forge db migrate` だけとする。本番では runbook `db-migration` の承認付きの手順になる。`forge serve` はマイグレーションを適用しない。
- **起動時に検査する。** `forge serve` は待ち受けを始める前に DB へ接続し、スキーマがバイナリと合うかを検査する。合わなければ FG-DB-0004 SchemaIncompatible で起動しない。合わない理由は次の 4 つで、深刻なものから順に判定する。
  - `dirty`: 失敗の記録がある
  - `checksum_mismatch`: 適用済みのファイルが書き換えられている
  - `db_ahead`: DB に、このバイナリの知らない版がある
  - `pending`: 未適用の版がある
- **DB がバイナリより新しいとき（`db_ahead`）も起動しない。** 新しいスキーマに古いバイナリが書き込むと、データを壊すおそれがある。バイナリを戻すときは、DB に合う版のバイナリを使う。
- **前に向かってだけ直す。** 元に戻すためのマイグレーション（down）は作らない。誤りは新しいマイグレーションで直す。
- **ファイル名は `YYYYMMDDHHMMSS_<説明>.sql`（UTC）とする。** 並行のブランチで番号がぶつからない。各ファイルは 1 つのトランザクションで実行されるので、途中まで適用された状態は残らない。
- **改行は LF に固定する**（`.gitattributes` の `eol=lf`）。チェックサムはファイルの中身から計算されるため、Windows の CRLF のまま埋め込むと、Linux の本番と一致しなくなる。
- **同時に実行しても安全にする。** `forge db migrate` は sqlx のアドバイザリロックで直列化する。ロックの待ちは `database.lock_timeout_ms` で上限を持つ（超えたら FG-DB-0003 `reason=locked`）。適用は専用の接続で行い、`database.migrate_statement_timeout_ms`（既定 10 分）を使う。この長いタイムアウトは pool に戻さない。
- **トランザクションの外で実行するマイグレーションは例外として扱う。** `CREATE INDEX CONCURRENTLY` のように、トランザクションの中では実行できない文がある。そうしたファイルには、先頭に sqlx の `-- no-transaction` を付ける。その場合は「1 ファイル = 1 文」に限り、`IF NOT EXISTS` などで何度実行しても同じ結果になるように書く。途中で失敗すると `dirty` が残るが、修正したマイグレーションを再実行すれば安全に進められる。
- **マイグレーションの実行全体に、クライアント側の上限を付ける。** 上限は「未適用の数 + 1」× `database.migrate_statement_timeout_ms` とする。サーバー側の statement / lock のタイムアウトは、通信が半分切れた状態では効かないためである（超えたら FG-DB-0003 `reason=timeout`）。各 DDL のロック待ちには、マイグレーション中も `database.lock_timeout_ms` が効く。混んでいるテーブルへの DDL は `reason=sql_failed`（SQLSTATE `55P03`）で失敗するので、空いている時間に再実行する。
- **状態を見るだけのコマンドを用意する。** `forge db status` は読み取りだけで、管理用のテーブル `_sqlx_migrations` も作らない。applied / pending / unknown / modified / dirty と、`forge serve` が起動するかどうかを表示する。`--check` を付けると、起動できない状態なら FG-DB-0004 を返して終了コード 1 で終わる（スクリプトや runbook の確認用）。
- **適用の記録を残す。** `forge db migrate` は、適用を終えた時点で JSON のログ `forge.db.migrate.applied`（info）を出す。その後に確認のための読み直しが失敗しても、適用の事実を失敗として報告しない（警告を出し、`forge db status` での確認を促す）。
- `forge db migrate` は確認のプロンプトを出さない。本番での承認は、runbook の手順（契約 §3-7）として行う。
- 製品のマイグレーションは `crates/forge-db/migrations/` に置く（今は 0 本）。テスト用の版は `crates/forge-db/tests/fixtures/migrator/` に置く。immutability フックの対象外にするためである。

## Alternatives Considered

### `forge serve` の起動時に自動で適用する
- **Pros**: 運用の手順が 1 つ減る
- **Cons**: 本番のスキーマが承認なしに変わる。複数のサーバーが同時に起動すると、適用が競合する
- **Why not**: 契約 §3-7 に反する

### 新しい DB に古いバイナリが接続した場合は、警告だけにする
- **Pros**: バイナリを戻しやすい
- **Cons**: 新しい列や制約を知らないバイナリが書き込み、データを壊すおそれがある
- **Why not**: 安全側に倒す（ユーザーの判断、2026-10-26）

### 連番のファイル名（`0001_...`）
- **Why not**: 並行の worktree で同じ番号を取り合う。タイムスタンプ形式ならぶつからない

### down マイグレーション
- **Why not**: 実際の障害では、データを伴う down は試されておらず、危険である。前方への修正に一本化する

## Consequences

### Positive
- スキーマの変更は、必ず人の承認がある 1 つの手順を通る
- バイナリと DB のずれは、起動の前に理由と版の番号付きで分かる（`forge db status` と FG-DB-0004）

### Negative
- リリースの手順が「`forge db migrate` → `forge serve` の再起動」の 2 段になる
- 互換性を保つ変更（列の追加 → コードの切り替え → 古い列の削除）を、複数のリリースに分ける必要がある

### Risks
- 長い DDL によってロックが長く続く → マイグレーション中も `lock_timeout` を有効にし、待ちすぎたら失敗させる。大きなテーブルへの変更は `CONCURRENTLY` などで別のマイグレーションに分ける（database-reviewer が確認する）
- 誰かが適用済みのファイルを編集する → フックで止め、それでも `checksum_mismatch` で起動しない
