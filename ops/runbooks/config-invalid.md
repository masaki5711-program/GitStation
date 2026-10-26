# Runbook: config-invalid（FG-SYS-0002）

## Trigger

- 起動時または設定の再読み込み時に `error_code=FG-SYS-0002` が出た
- サーバーが起動しない

## Impact

サーバーが起動できない場合は全機能が停止（SEV1 相当）。再読み込み時の失敗なら、旧設定のまま稼働を継続する。

## Diagnose（読み取りのみ）

1. ログの `context` にある `config_key` と `reason` を読む。
2. `forge doctor` を実行し、設定の検証結果を確認する（サーバーは起動しない）。
3. 直近で設定ファイル・環境変数を変更したか確認する（誰が・いつ・何を）。

## Mitigate

1. `config_key` が指す値を、設定ファイルまたは環境変数で修正する。
   `[MUTATING — needs approval]`（ロールバック: 変更前の設定ファイルに戻す）
2. サーバーを再起動する。`[MUTATING — needs approval]`

## Verify

- `forge doctor` が設定の項目で ok を返す
- サーバーが起動し、`FG-SYS-0002` が新たに出ない

## Escalate

- 秘密情報（パスワード・トークン）の値が原因の場合、値をログや障害記録に貼らない。
- 原因が分かりにくかった場合は、`errors.toml` の `diagnosis` を改善するフィードバックアクションを起票する。
