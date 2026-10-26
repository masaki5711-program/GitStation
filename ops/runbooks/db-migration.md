# Runbook: db-migration（FG-DB-0003 / FG-DB-0004）

## Trigger

- `forge serve` が起動時に `error_code=FG-DB-0004` SchemaIncompatible で止まった
- `forge db migrate` が `error_code=FG-DB-0003` MigrationFailed（または FG-DB-0004）で失敗した
- リリースの手順で、新しいバイナリにマイグレーションが含まれている

## Impact

- FG-DB-0004 で `forge serve` が起動しない場合、GitStation 全体が使えない（SEV1）。
- FG-DB-0003 では何も適用されない（各マイグレーションは 1 つのトランザクションで実行される）。古いバイナリは引き続き動く。

## Diagnose（読み取りのみ）

1. ログの `context` を読む。
   - FG-DB-0004 の `reason` と `versions`
     - `pending`: 未適用のマイグレーションがある（新しいバイナリを入れたが、まだ migrate していない）
     - `db_ahead`: DB に、このバイナリの知らない版がある（バイナリを古い版に戻した）
     - `checksum_mismatch`: 適用済みのマイグレーションのファイルが書き換えられている（禁止されている変更）
     - `dirty`: 以前の実行が失敗として記録されている
   - FG-DB-0003 の `reason`、`version`、`sqlstate`、`db_message`
     - `sql_failed`: その版の SQL が失敗した（ロールバック済み）。`sqlstate=55P03` なら、混んでいるテーブルへの DDL がロック待ちで `database.lock_timeout_ms` を超えた。空いている時間に再実行する
     - `locked`: 別の `forge db migrate` がロックを持っていて、`database.lock_timeout_ms` を超えた
     - `timeout`: 実行全体がクライアント側の上限（`timeout_ms`）を超えた。DB との通信や、長い DDL を疑う
     - `-- no-transaction` の付いたマイグレーションが途中で失敗した場合は `dirty` が残る。その版は 1 文だけで、何度実行しても同じ結果になるように書かれている（ADR-0014）ので、原因を直してから再実行する
     - `source` / `driver`: `source` を読む
2. 現在の状態を確認する（読み取りのみ。何も変更しない）。
   - `forge db status`
   - 出力の `serve` の行が、`forge serve` が起動できるかどうかを示す
3. バイナリの版と、リリースノートに書かれたマイグレーションの版を照らし合わせる。

## Mitigate

1. `pending` の場合: マイグレーションを適用する。`[MUTATING — needs approval]`
   - 事前に DB のバックアップ（スナップショット）があることを確認する
   - `forge db migrate`
   - ロールバック: down マイグレーションはない（ADR-0014）。問題があれば、修正するマイグレーションを新しいリリースで出す。データに影響した場合は、バックアップから復元する（DB の担当者と判断する）
2. `db_ahead` の場合: DB に合う版のバイナリを入れ直す。`[MUTATING — needs approval]`（ロールバック: 元のバイナリに戻す）。DB を古い版に戻すことはしない
3. `checksum_mismatch` / `dirty` の場合: 本番の DB を手で直さない。
   - 書き換えられたファイルを、バージョン管理にある元の内容に戻したバイナリを用意する
   - `dirty` の原因になった失敗を、障害記録で調べる
   - 直す方針（新しいマイグレーションを出す、記録を修正する）は、DB の担当者と決めて承認を取る。`[MUTATING — needs approval]`
4. `locked` の場合: もう一方の `forge db migrate` が終わるのを待ってから、再実行する（`forge db status` で確認する）

## Verify

- `forge db status --check` が終了コード 0 で終わる（`serve` の行が `compatible`）
- `forge db migrate` の JSON ログに `forge.db.migrate.applied` が出ている（`pending_before` と `duration_ms` 付き）。読み直しに失敗した旨の警告が出ていたら、`forge db status` で適用済みを確認する
- `forge serve` が起動し、ログに `forge.serve.listening` が出る

## Escalate

- `sql_failed` が続く、またはデータへの影響が疑われる場合は、DB の担当者と開発者（Claude）へ。マイグレーションの修正は、新しいリリースで出す
- 適用済みのマイグレーションが書き換えられていた場合は、どの変更が原因かを障害記録に残し、フィードバックアクションを起票する
