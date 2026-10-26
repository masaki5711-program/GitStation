# Runbook: git-process-failed（FG-GIT-0003）

## Trigger

- `error_code=FG-GIT-0003` のログが出た（git のサブプロセスが想定外の 0 以外で終了した、または出力の読み取り・終了の待機に失敗した）

## Impact

対象のリポジトリの操作が失敗する。多数のリポジトリで同時に出ている場合は、保存先の障害の可能性が高い（SEV1〜2）。

## Diagnose（読み取りのみ）

1. ログの `subcommand`、`exit_code`、`stderr`（git の標準エラーの先頭。URL の認証情報は伏せてある）、`request_id` を読む。
   `exit_code` がない場合は、読み取りや待機の失敗で、原因は `source` にある。
2. 1 つのリポジトリだけか、多数か（`error_code=FG-GIT-0003` をリポジトリごとに集計）。
3. 1 つだけ: `forge admin repo-path <repo_id>` でパスと状態を確認し、読み取り専用で `git --git-dir=<path> fsck --no-dangling` を実行する。
4. 多数: 保存先の空き容量（`df -h <保存先>`）、所有者と権限（`ls -ld <保存先>`）、マウントの状態（`findmnt <保存先>`）を確認する。

## Mitigate

1. 空き容量の不足: 不要なファイル（古いログ、退避済みのバックアップ）を削除する、または容量を増やす。
   `[MUTATING — needs approval]` ロールバック: 削除の前に対象の一覧と退避先を記録し、必要なら退避先から戻す。
2. 権限の誤り: 保存先の所有者を forge の実行ユーザーに直す（`chown -R forge:forge <保存先>`）。
   `[MUTATING — needs approval]` ロールバック: 変更前に `stat -c '%U:%G %a %n'` で記録した所有者と権限に戻す。
3. リポジトリの破損: バックアップから復元する（復元の手順書は M3 で作成予定。それまではミラーから `git clone --mirror` で取り直す）。
   `[MUTATING — needs approval]` ロールバック: 復元の前に、破損したディレクトリを別名で退避しておき、必要なら戻す。

## Verify

- 同じ操作が成功し、`FG-GIT-0003` が新たに出ない
- `git --git-dir=<path> fsck --no-dangling` がエラーを出さない

## Escalate

- 原因が `stderr` から分からなかった場合は、`errors.toml` の `diagnosis` を改善するフィードバックアクションを起票する。
- 想定内の失敗（存在しない ref など）でこのコードが出ていた場合は、呼び出し側が専用のコードを使うよう修正する（警報が誤報になるため）。
