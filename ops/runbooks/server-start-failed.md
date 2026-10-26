# Runbook: server-start-failed（FG-SYS-0004 / FG-SYS-0005 / FG-WEB-0004）

## Trigger

- `forge serve` が起動時に終了した、または稼働中に止まった
  - `FG-SYS-0004` ServerBindFailed（待ち受けのアドレスを開けない）
  - `FG-SYS-0005` ServeFailed（実行基盤の準備に失敗、サーバーの処理が I/O エラーで停止、停止時の待ち時間を超過）
  - `FG-WEB-0004` RouteRegistryInvalid（ルートの登録に矛盾がある。リリースの不具合）
- サービス（systemd）が再起動を繰り返す

## Impact

GitStation の Web・API・Git（HTTP）がすべて使えない（SEV1）。

## Diagnose（読み取りのみ）

1. ログの `error_code` と `context` を読む。
   - `FG-SYS-0004`: `context` に `bind=<アドレス>`、`source` に OS のエラー
     - `Address already in use`: 別のプロセスがポートを使っている
     - `Permission denied`: 1024 未満のポートを一般ユーザーで開こうとした
     - `Cannot assign requested address`: そのホストにない IP アドレスを指定した
   - `FG-SYS-0005`: `context` の `operation=`（`build_runtime` / `serve` / `shutdown_deadline`）と `source`
   - `FG-WEB-0004`: `context` の `reason=` と `routes=`（サーバー側では直せない。リリースの不具合）
2. ポートを使っているプロセスを確認する（Linux: `ss -ltnp 'sport = :<port>'`、Windows: `netstat -ano | findstr :<port>`）。
3. 古い `forge serve` が残っていないか確認する（`systemctl status forge`、`ps -ef | grep forge`）。
4. `FG-SYS-0005` の `build_runtime` / `serve`: ファイル記述子の上限（`ulimit -n`、`systemctl show forge -p LimitNOFILE`）、メモリ、`journalctl -k` を確認する。
5. `FG-SYS-0005` の `shutdown_deadline`: 停止の直前のアクセスログで、長く続いていたリクエスト（大きな clone など）を探す。

## Mitigate

1. 古い forge のプロセスが残っている場合は停止する（`systemctl stop forge` の後に残っていれば `kill <pid>`）。
   `[MUTATING — needs approval]` ロールバック: `systemctl start forge` で起動し直す。
2. 別のサービスがポートを使っている場合は、forge の待ち受けポートを変更する（`--bind` または `FORGE_BIND`）。リバースプロキシの転送先も合わせて変更する。
   `[MUTATING — needs approval]` ロールバック: 変更前の `--bind` とプロキシ設定に戻す。
3. 1024 未満のポートは使わず、リバースプロキシ（443）から 1024 以上のポートへ転送する。
   `[MUTATING — needs approval]` ロールバック: 変更前の設定に戻す。
4. ファイル記述子の不足: サービスの `LimitNOFILE` を引き上げて再起動する。
   `[MUTATING — needs approval]` ロールバック: 変更前の値に戻して再起動する。
5. `FG-WEB-0004`: 直前のリリースに戻す（ルートの登録の不具合はサーバー側で直せない）。
   `[MUTATING — needs approval]` ロールバック: 新しいリリースへ戻す（修正版ができてから）。

## Verify

- `forge serve` が起動し、`curl -fsS http://<bind>/healthz` が `{"status":"ok",...}` を返す
- 対象のエラーコードが新たに出ない

## Escalate

- ポートを使っているプロセスが不明な場合は、ホストの管理者と確認する（想定外のプロセスはセキュリティの事象として扱う）。
- `FG-WEB-0004` が出たリリースは、評価（`/review`）とテストの抜けとして還元策を起票する。
