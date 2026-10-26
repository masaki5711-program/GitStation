# Runbook: server-bind-failed（FG-SYS-0004）

## Trigger

- `forge serve` の起動時に `error_code=FG-SYS-0004` が出て、プロセスが終了した
- サービス（systemd）が再起動を繰り返す

## Impact

GitStation の Web・API・Git（HTTP）がすべて使えない（SEV1）。

## Diagnose（読み取りのみ）

1. ログの `bind`（待ち受けのアドレスとポート）と `source`（OS のエラー）を読む。
   - `Address already in use`: 別のプロセスがポートを使っている
   - `Permission denied`: 1024 未満のポートを一般ユーザーで開こうとした
   - `Cannot assign requested address`: そのホストにない IP アドレスを指定した
2. ポートを使っているプロセスを確認する（Linux: `ss -ltnp 'sport = :<port>'`、Windows: `netstat -ano | findstr :<port>`）。
3. 古い `forge serve` が残っていないか確認する（`systemctl status forge`、`ps -ef | grep forge`）。

## Mitigate

1. 古い forge のプロセスが残っている場合は停止する（`systemctl stop forge` の後に残っていれば `kill <pid>`）。
   `[MUTATING — needs approval]` ロールバック: `systemctl start forge` で起動し直す。
2. 別のサービスがポートを使っている場合は、forge の待ち受けポートを変更する（`--bind`）。リバースプロキシの転送先も合わせて変更する。
   `[MUTATING — needs approval]` ロールバック: 変更前の `--bind` とプロキシ設定に戻す。
3. 1024 未満のポートは使わず、リバースプロキシ（443）から 1024 以上のポートへ転送する。
   `[MUTATING — needs approval]` ロールバック: 変更前の設定に戻す。

## Verify

- `forge serve` が起動し、`curl -fsS http://<bind>/healthz` が `{"status":"ok",...}` を返す
- `FG-SYS-0004` が新たに出ない

## Escalate

- ポートを使っているプロセスが不明な場合は、ホストの管理者と確認する（想定外のプロセスはセキュリティの事象として扱う）。
