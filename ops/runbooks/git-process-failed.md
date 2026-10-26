# Runbook: git-process-failed（FG-GIT-0003）

## Trigger

- `error_code=FG-GIT-0003` のログが出た（git のサブプロセスが 0 以外で終了した）

## Impact

対象のリポジトリの操作が失敗する。多数のリポジトリで同時に出ている場合は、保存先の障害の可能性が高い（SEV1〜2）。

## Diagnose（読み取りのみ）

1. ログの `subcommand`、`exit_code`、`stderr`（git の標準エラーの先頭数行）、`request_id` を読む。
2. 1 つのリポジトリだけか、多数か（`error_code=FG-GIT-0003` をリポジトリごとに集計）。
3. 1 つだけ: `forge admin repo-path <repo_id>` でパスを確認し、読み取り専用で `git -C <path> fsck --no-dangling` を実行する。
4. 多数: 保存先の空き容量（`df`）、権限、マウントの状態を確認する。`forge doctor` の保存先の項目を見る。

## Mitigate

1. 空き容量の不足: 不要なファイルを削除する、または容量を増やす。`[MUTATING — needs approval]`
2. 権限の誤り: 保存先の所有者を forge の実行ユーザーに直す。`[MUTATING — needs approval]`
3. リポジトリの破損: バックアップから復元する（M3 の復元手順）。`[MUTATING — needs approval]`

## Verify

- 同じ操作が成功し、`FG-GIT-0003` が新たに出ない
- `git fsck` がエラーを出さない

## Escalate

- 原因が `stderr` から分からなかった場合は、`errors.toml` の `diagnosis` を改善するフィードバックアクションを起票する。
