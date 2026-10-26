# Runbook: config-invalid（FG-SYS-0002）

## Trigger

- 起動時または設定の再読み込み時に `error_code=FG-SYS-0002` が出た
- サーバーが起動しない

## Impact

サーバーが起動できない場合は全機能が停止（SEV1 相当）。再読み込み時の失敗なら、旧設定のまま稼働を継続する。

## Diagnose（読み取りのみ）

1. ログの `context` にある `config_key` と `reason` を読む。
2. `config_key` が指す値を、優先度の低い順に確認する。あとのものが前のものを上書きする。
   - 設定ファイル（`--config` または `FORGE_CONFIG` が指す TOML）
   - 環境変数。名前はキーから決まる（`database.url` → `FORGE_DATABASE_URL`）
   - コマンドラインのフラグ（`--bind` など）

   `config_key` の値ごとの意味は次のとおり。
   - `config_file`: 設定ファイルそのものの問題。`path` を見る。
   - `FORGE_...`: 未知の環境変数（つづりの誤り）。
   - `reason`: `missing`（未設定）、`invalid`（形式の誤り）、`zero`（0 は不可）、`unknown_key`（設定ファイルのつづりの誤り）、`unknown_env`、`unreadable`、`parse`、`password_twice`（URL とパスワードファイルの両方にパスワードがある）、`empty`、`above_max`、`not_unicode`。

   環境変数の値は `printenv | grep ^FORGE_` で見る。パスワードは値を表示しない（`database.password_file` は存在と権限だけを `ls -l` で確かめる）。設定ファイルの全体を検証する `forge doctor` は M1 phase 2 の後半で追加する。追加したらこの手順を置き換える。
3. 直近で設定ファイル・環境変数を変更したか確認する（誰が・いつ・何を）。

## Mitigate

1. `config_key` が指す値を、設定ファイルまたは環境変数で修正する。
   `[MUTATING — needs approval]`（ロールバック: 変更前の設定ファイルに戻す）
2. サーバーを再起動する。`[MUTATING — needs approval]`

## Verify

- サーバーが起動し、ログに設定の検証の成功が出る（`forge doctor` の追加後は、その設定の項目が ok を返すことでも確認する）
- サーバーが起動し、`FG-SYS-0002` が新たに出ない

## Escalate

- 秘密情報（パスワード・トークン）の値が原因の場合、値をログや障害記録に貼らない。
- 原因が分かりにくかった場合は、`errors.toml` の `diagnosis` を改善するフィードバックアクションを起票する。
