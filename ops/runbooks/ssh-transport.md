# Runbook: ssh-transport（SSH の git 転送）

対象のコード: FG-GIT-0035（SSH の接続の上限・認証の時間切れ）、FG-GIT-0036（SSH の層そのものの失敗）。あわせて、利用者から
見える FG-GIT-0033（git 以外のコマンド）、FG-GIT-0034（プロトコル v2 が要る）、FG-AUTH-0023（SSH 鍵の登録の拒否）、SSH の
起動を止める FG-SYS-0002（`config_key=git.ssh.*`）と FG-SYS-0004（`listener=ssh`）。
仕組みは ADR-0032、実装は `crates/forge-web/src/ssh/`（`mod.rs` の待ち受けと接続、`limits.rs` の上限、`handler.rs` の認証と
チャネル、`command.rs` のコマンドの解析、`bridge.rs` の Smart HTTP への橋渡し、`host_key.rs` のホスト鍵）、鍵の保存は
`crates/forge-db/src/ssh_keys/`。git の操作そのもの（権限、保護された ref、pre-receive、push の記録、ストリームの上限・
時間切れ）は Smart HTTP と同じ経路で動くので、それらのコードは runbook `smart-http` / `push-checks` / `push-record-failed` に従う。

## Trigger

- `git clone ssh://…` / `git push` が失敗すると利用者が報告した
- ログに `error_code=FG-GIT-0035`（warn）が続けて出ている、または `FG-GIT-0036`（alert）が出た
- `forge serve` が `FG-SYS-0002 config_key=git.ssh.host_key_file`（または `git.ssh.bind`）で起動しない

## Impact

- FG-GIT-0035: 上限に当たった接続元（または全員）の SSH の接続がその間だけ断られる。HTTPS は影響を受けない（SEV3、全員なら SEV2）。
- FG-GIT-0036 `reason=server_stopped`: `forge serve` が動いていても SSH の待ち受けが無い。SSH の利用者全員が使えない（SEV2）。HTTPS は使える。
- FG-GIT-0036 `reason=session` / `bridge`: その 1 回の操作が失敗する。続くなら不具合（SEV3）。
- ホスト鍵の問題で起動しない: `git.ssh.enabled = true` のまま鍵が無い・壊れていると **HTTP も含めて** `forge serve` が起動しない（SEV1〜2）。

## 設定（`[git.ssh]` の表または `FORGE_GIT_SSH_*`。起動時に検証し、誤りは FG-SYS-0002）

| キー | 既定 | 意味 |
|---|---|---|
| `git.ssh.enabled` | `false` | SSH の待ち受けを始める |
| `git.ssh.bind` | `127.0.0.1:2224` | 待ち受けの `host:port` |
| `git.ssh.host_key_file` | なし（有効なら必須、絶対パス） | ホスト鍵（Ed25519、OpenSSH 形式、Unix では 0600） |
| `git.ssh.clone_host` | `web.base_url` のホスト + `bind` のポート | clone の URL と SSH 鍵の画面に出す `host:port` |
| `git.ssh.max_connections` | 64 | 接続の総数 |
| `git.ssh.max_connections_per_peer` | 8 | 接続元ごとの同時の接続（IPv6 は /64 ごと） |
| `git.ssh.connections_per_minute_per_peer` | 60 | 接続元ごとの 1 分の新しい接続 |
| `git.ssh.auth_failures_per_minute_per_peer` | 20 | 認証できずに終わった接続がこの数に達した接続元は、その 1 分の残りを断る |
| `git.ssh.auth_timeout_ms` | 30 000 | SSH の識別子の交換と認証を終えるまで |
| `git.ssh.max_auth_attempts` | 6（20 まで） | 1 接続の鍵の試行 |
| `git.ssh.inactivity_timeout_ms` | 600 000（`git.http.idle_timeout_ms` 以上） | 無通信で切る |

git のストリームの上限と時間切れ（`git.http.max_streams`、`git.http.idle_timeout_ms` など）は SSH にもそのまま効く。

## 初めて有効にする（承認された手順）

1. ホスト鍵を作る（1 回だけ。既存のファイルは上書きしない: `FG-SYS-0002 reason=target_exists`）:
   `forge --config /etc/forge/forge.toml admin ssh-host-key generate`
   出力の `fingerprint` を利用者への案内に載せる（SSH 鍵の画面にも出る）。鍵のファイルは `forge` の実行ユーザーだけが読めること。
2. `[git.ssh]` に `enabled = true`、`bind`（例 `0.0.0.0:2224`。社内のネットワークだけに開ける）、`host_key_file` を書く。
3. `forge serve` を再起動し、`forge.serve.ssh_listening`（`bind`、`host_key`、`clone_host`）のログを確かめる。
4. 確認: 自分の鍵を SSH 鍵の画面で登録し、`ssh -T -p 2224 git@<host>` が「… として SSH 鍵で認証されました」と出して終わること、
   `git ls-remote ssh://git@<host>:2224/<group>/<project>.git` が通ること。

ホスト鍵を入れ替えると、利用者全員の `known_hosts` が合わなくなる（`REMOTE HOST IDENTIFICATION HAS CHANGED`）。漏れたときだけ
行い、新しい指紋を先に周知する（承認された手順。古いファイルを退避してから `generate`）。

## Diagnose（読み取りのみ）

1. ログを `error_code` で引く。SSH の層のログは `target=forge_web::ssh`。1 回のコマンドは `forge.ssh.command`（`request_id`、
   `ssh_key_id`、`user_id`、`service`、`exit_status`、`refused`、`http_request_ids`）にまとまる。`http_request_ids` の ID で
   Smart HTTP のルートのログ（権限・push・want の確認など）を引く。
2. 利用者の報告の文（`fatal: remote error: FG-… (request_id: …)`）の `request_id` は、ルートの拒否なら HTTP の要求の ID、
   SSH の層の拒否（FG-GIT-0033 / 0028 / 0030）なら SSH のコマンドの ID。どちらもログで引ける。
3. `Permission denied (publickey)`（コードは無い。認証の前なので要求の ID も無い）:
   - 鍵が登録されているか: `SELECT id, user_id, algorithm, fingerprint_sha256, last_used_at FROM forge.user_ssh_keys WHERE fingerprint_sha256 = '<SHA256:…>';`
     （利用者は `ssh-keygen -l -E sha256 -f ~/.ssh/id_ed25519.pub` で指紋を出せる）
   - 所有者が無効か: ログの `FG-AUTH-0004 reason=owner_disabled`（`transport=ssh`、`ssh_key_id`）。
   - RSA の鍵: ログの `forge.ssh.key_type_refused`（`algorithm=ssh-rsa`）。Ed25519 を作ってもらう。
4. FG-GIT-0035: `reason` を読む。
   - `connection_limit` / `peer_limit`: 開いている接続を数える `ss -tn state established '( sport = :2224 )' | wc -l`、接続元ごとに
     `ss -tn state established '( sport = :2224 )' | awk '{print $4}' | cut -d: -f1 | sort | uniq -c | sort -rn | head`。
   - `rate_limit`: CI やスクリプトが短い間に何度も接続している（`peer` で接続元が分かる）。
   - `auth_failures`: その接続元で認証に失敗した接続が続いている。鍵を何本も試すクライアント（エージェントに多くの鍵）、
     または走査。`suppressed` が大きければ走査を疑う。
   - `auth_timeout`（info）: 識別子の交換か認証が 30 秒で終わらなかった。走査や止まったクライアントでは普通。
5. FG-GIT-0036: `reason` を読む。
   - `accept`: たいていファイル記述子の不足（`cat /proc/$(pidof forge)/limits | grep 'open files'`、`ls /proc/$(pidof forge)/fd | wc -l`）。
   - `server_stopped`: 直前のログ（同じ時刻の FG-GIT-0036 `accept` や panic）を読む。
   - `session`: `source` の文と同じ接続のログ。続くなら不具合として記録する。
   - `bridge`: `http_status` と `http_request_id`。ルートの答えが JSON のエラーでなかった（SSH の層の不具合か、ルートの想定外の答え）。
6. FG-GIT-0034 が多い: 古い git（2.26 より前）、`protocol.version=0` の設定、plink など `GIT_PROTOCOL` を送れない SSH クライアント。
   利用者の `git --version` と `git config --show-origin protocol.version` を聞く。
7. 起動しない: FG-SYS-0002 なら `config_key` と `reason`（`missing` / `unreadable` / `invalid` / `insecure_permissions`）。
   `ls -l <host_key_file>`、`forge admin ssh-host-key show` で読めるか確かめる。FG-SYS-0004（`listener=ssh`、`bind`）は
   `git.ssh.bind` のポートが使えない（runbook server-start-failed）。

## Mitigate（変更は承認された手順）

- 上限に当たる（正当な負荷）: `git.ssh.max_connections_per_peer` / `connections_per_minute_per_peer` / `max_connections` を上げて再起動。
  git のストリームの上限（`git.http.max_streams`）も合わせて見る。
- 走査・総当たり: 接続元を社内のファイアウォールで閉じる。`git.ssh.auth_failures_per_minute_per_peer` を下げてもよい。
- SSH の層の不具合・russh の勧告への対応が間に合わない: `git.ssh.enabled = false` にして再起動（HTTPS の clone / push はそのまま使える。
  利用者には HTTPS の URL とアクセストークンを案内する）。
- ホスト鍵が読めない（権限）: `chown forge:forge <file>; chmod 600 <file>` の後に再起動。
- ファイル記述子の不足: systemd の `LimitNOFILE` を上げて再起動。

## Verify

- `forge.serve.ssh_listening` のログが出ている。
- `ssh -T -p <port> git@<host>` が認証の文を出し、`git ls-remote ssh://git@<host>:<port>/<path>.git` が通る。
- 15 分の間、新しい FG-GIT-0035（`connection_limit` / `peer_limit`）と FG-GIT-0036 が出ない。

## Escalate / Postmortem

- FG-GIT-0036 `session` / `bridge` が続く、または russh の panic がログにある: 不具合として incident に記録し、`git.ssh.enabled = false`
  の判断を人に仰ぐ。postmortem では回帰テスト（`crates/forge-web/src/ssh/bridge_tests.rs` か `crates/forge-cli/tests/ssh_e2e.rs`）を足す。
- ホスト鍵の漏えい: secret の漏えいとして扱う（鍵の入れ替え、利用者への新しい指紋の周知）。
