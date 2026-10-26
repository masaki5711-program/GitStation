# Runbook: git-unavailable（FG-GIT-0001）

## Trigger

- 起動時、`forge doctor`、または git の処理で `error_code=FG-GIT-0001` が出た
- clone / push がすべて失敗する

## Impact

Git の操作（clone、fetch、push、リポジトリの作成）がすべて止まる（SEV1 相当）。Web と API の読み取りの一部は動く。

## Diagnose（読み取りのみ）

1. ログの `reason` を読む。
   - `spawn_failed`: `git_bin` のパスに git がない、または実行権限がない
   - `unparseable_version`: `git version` の出力が想定外（ラッパーや別の実行ファイル）
   - `too_old`: `found` が 2.38 未満
2. `forge doctor` で git のパスとバージョンの検査結果を確認する。
3. サーバー上で `<git_bin> version` を forge と同じユーザーで実行する。
4. 直近に OS のパッケージ更新や `git.bin` の設定変更がなかったか確認する。

## Mitigate

1. git 2.38 以上をインストールする（Debian 12 / RHEL 9 / Ubuntu 24.04 の標準パッケージで足りる）、または `git.bin` を正しいパスに直す。
   `[MUTATING — needs approval]`（ロールバック: 変更前のパッケージ・設定に戻す）
2. forge を再起動する。`[MUTATING — needs approval]`

## Verify

- `forge doctor` の git の項目が ok
- テスト用のリポジトリで clone / push が成功し、`FG-GIT-0001` が新たに出ない

## Escalate

- OS のパッケージで 2.38 以上が入らない場合は、サポートする OS の一覧（`deploy-forge-linux`）を見直す。
