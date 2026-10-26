# Runbook: git-unavailable（FG-GIT-0001）

## Trigger

- 起動時、または git の処理で `error_code=FG-GIT-0001` が出た
- clone / push がすべて失敗する

## Impact

Git の操作（clone、fetch、push、リポジトリの作成）がすべて止まる（SEV1 相当）。Web と API の読み取りの一部は動く。

## Diagnose（読み取りのみ）

1. ログの `reason` を読む。
   - `spawn_failed`: `git_bin` のパスに git がない、または実行権限がない
   - `unparseable_version`: `git version` の出力が想定外（`output` に先頭 100 文字）。ラッパーや別の実行ファイルの可能性
   - `too_old`: `found` が `minimum`（2.38.0）未満
2. サーバー上で、forge と同じユーザーで `<git_bin> version` を実行する（例: `sudo -u forge git version`）。
3. 直近に OS のパッケージ更新や git の設定変更がなかったか確認する（`/var/log/dpkg.log`、`dnf history`）。

`forge doctor` の `git` の行で、PATH 上の git とそのバージョンを確認できる（読み取りのみ）。

## Mitigate

1. git 2.38 以上をインストールする（Debian 12 / RHEL 9 / Ubuntu 24.04 の標準パッケージで足りる）、または forge の設定の git のパスを正しいものに直す。
   `[MUTATING — needs approval]` ロールバック: 変更前のパッケージのバージョンに戻す（`apt install git=<旧版>` / `dnf downgrade git`）、または設定ファイルを変更前の版に戻す。
2. forge を再起動する（`systemctl restart forge`）。`[MUTATING — needs approval]` ロールバック: 手順 1 を戻してから再起動する。

## Verify

- `<git_bin> version` が 2.38 以上を表示する
- テスト用のリポジトリで clone / push が成功し、`FG-GIT-0001` が新たに出ない

## Escalate

- OS のパッケージで 2.38 以上が入らない場合は、サポートする OS の一覧を見直す（デプロイ手順書 `deploy-forge-linux` は M1 の最後に作成予定）。
