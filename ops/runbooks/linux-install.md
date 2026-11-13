# Runbook: linux-install（Linux サーバーへの新規導入）

GitStation を、PFS（IoT 基盤）と同じ Linux サーバーに**初めて**入れる手順。更新は `linux-upgrade`、パッケージの中身と設計の判断は
`ops/linux/README.md`（リリースの tar.gz では `README.md`）。

- 本番のサーバーを変える手順はすべて `[MUTATING — needs approval]`。**人が承認した手順としてだけ**実行する（CLAUDE.md §3.7）。
  新しい環境で利用者はまだいないが、**同じサーバーで PFS が動いている**ので、PFS に触れる可能性のある手順（パッケージの導入、
  ポート、nginx、PostgreSQL）は PFS チームの合意を取ってから行う。
- 秘密の値はこの手順書・チャット・チケットに書かない。`<…>` は置き換える印。
- 下のコマンドは root（`sudo -i`）で実行する前提。`forge` のコマンドは必ず `forgectl` 経由（`forge` ユーザーで動く）。

## Trigger

- GitStation の本番の環境を新しく作るとき（PFS のサーバーへの初回の導入、サーバーの作り直し）。

## Impact

- 導入が終わるまで GitStation は使えない（新しい環境なので既存の利用者への影響はない）。
- PFS への影響: CPU・メモリ・ディスク・ポートを分け合う。手順 0 の事実で、PFS の余力を確かめてから進める。

## 0. PFS チームに確認すること（導入の前に、すべて埋める）

| # | 事実 | なぜ要るか | 記入 |
|---|---|---|---|
| 1 | OS とその版（`cat /etc/os-release`） | パッケージの名前、systemd の版、ビルド機の glibc を本番以下にする | |
| 2 | CPU のアーキテクチャ（`uname -m`: `x86_64` / `aarch64`） | リリースの target（`x86_64-unknown-linux-gnu` / `aarch64-unknown-linux-gnu`） | |
| 3 | systemd の版（`systemctl --version`） | 240 未満は `Type=exec` が無い（`simple` として動く）。247 未満は一部の保護の設定が無視される | |
| 4 | 空きメモリ（`free -m`、PFS のピーク時） | `MemoryHigh` / `MemoryMax`、PostgreSQL を専用にできるか（約 1 GiB） | |
| 5 | 空きディスクと置き場（`df -h`）。バックアップ用の**別のディスク**があるか | `storage.root`（リポジトリ）、`backup.dir`、PostgreSQL のデータ | |
| 6 | コンテナの実行環境の有無（Docker / Podman、rootless か）と、使ってよいか | PostgreSQL をコンテナで動かすか、pgBackRest の包み | |
| 7 | 使用中のポート（`ss -ltnp`）と、GitStation に使ってよいポート | `server.bind`（仮 18080）、専用 PostgreSQL（仮 55433）、nginx の 443 / 80 | |
| 8 | 443 を既に使っているか（PFS の nginx / 他のプロキシ） | 使っていれば、その nginx に `server` ブロックを足すか、別の名前・ポートにする | |
| 9 | PostgreSQL を共有するか（`ops/linux/postgresql/README.md` の A / B）。B なら superuser で bootstrap を流せる人、`max_connections` の余力、既存の WAL アーカイブ | ロールと DB の作成、接続数、バックアップ | |
| 10 | Keycloak の URL とレルム（`issuer` そのまま）、GitStation のクライアントを作れる人、社員番号の claim 名、入場のロール名 | `auth.oidc.*` | |
| 11 | 社内 CA の証明書（PEM）と、GitStation のホスト名の証明書を出せる人 | nginx の TLS、`auth.oidc.ca_file`、`webhook.ca_file` | |
| 12 | GitStation のホスト名（DNS）と、Keycloak からそのホスト名へ届くか（バックチャネルのログアウト） | `web.base_url`、`/-/auth/backchannel-logout` | |
| 13 | 時刻の同期（`timedatectl`） | OIDC は 60 秒を超える時計のずれで失敗する | |
| 14 | 外への通信の制限（社内のミラー・プロキシ） | パッケージの導入、Kanflow への Webhook（`webhook.allowed_targets`） | |
| 15 | 運用者の OS アカウントと sudo の範囲 | `forgectl` は root（sudo）で実行する | |

## 1. 前提のパッケージ `[MUTATING — needs approval]`

- `git`（**2.38 以上**。`git --version`。足りないと forge が `FG-GIT-0001` で起動しない）、`nginx`、`openssl`、`util-linux`（`runuser`）、`tar`。
- バックアップ: `pgbackrest`（PostgreSQL をホストで動かすとき。コンテナならコンテナの中）。
- Debian / Ubuntu: `apt-get install git nginx openssl pgbackrest`、RHEL 系: `dnf install git nginx openssl pgbackrest`（EPEL / PGDG）。
- ロールバック: 入れたパッケージを外す（PFS が使っているものは外さない。入れる前に `rpm -q` / `dpkg -l` で記録する）。

## 2. リリースを置く `[MUTATING — needs approval]`

ビルド機で `ops/linux/build-release.sh --target <手順 0 の #2>` が作った `gitstation-<版>-<target>.tar.gz` と `.sha256` を運ぶ。
`.sha256` は署名ではない: tar.gz と同じ経路で運んだ値は、改ざんされた tar.gz と一緒に差し替えられうる。ビルドした人から**別の経路**
（社内チャットで値を読み上げる、別の保管場所）で sha256 を受け取り、下の `sha256sum -c` の結果と照らす。ビルドは `.dirty`（未コミットの変更）を
既定で拒否する。`BUILD-INFO` の `commit` が `.dirty` で終わる版は本番に入れない。

```sh
cd /tmp && sha256sum -c gitstation-<版>-<target>.tar.gz.sha256      # OK でなければ止める
install -d -m 0755 /opt/gitstation/releases
tar -xzf gitstation-<版>-<target>.tar.gz -C /opt/gitstation/releases
mv /opt/gitstation/releases/gitstation-<版>-<target> /opt/gitstation/releases/<版>
ln -sfn /opt/gitstation/releases/<版> /opt/gitstation/current
install -m 0755 /opt/gitstation/current/share/bin/forgectl /usr/local/sbin/forgectl
/opt/gitstation/current/bin/forge --version                          # 版が出る（glibc の誤りなら手順 0 の #1 を見直す）
cat /opt/gitstation/current/BUILD-INFO
```

ロールバック: `rm -rf /opt/gitstation /usr/local/sbin/forgectl`。

## 3. ユーザーとディレクトリ `[MUTATING — needs approval]`

バックアップのディスクを `/srv/forge-backup` に先にマウントする（`/etc/fstab`。別の場所なら `forge.tmpfiles`、`forge-backup.service` の
`ReadWritePaths`、`forge.toml` の `backup.dir` を同じパスに揃える）。

```sh
install -m 0644 /opt/gitstation/current/share/systemd/forge.sysusers /etc/sysusers.d/forge.conf
install -m 0644 /opt/gitstation/current/share/systemd/forge.tmpfiles /etc/tmpfiles.d/forge.conf
systemd-sysusers /etc/sysusers.d/forge.conf
systemd-tmpfiles --create /etc/tmpfiles.d/forge.conf
id forge && ls -ld /etc/forge /etc/forge/secrets /etc/forge/secrets-privileged /var/lib/forge/* /srv/forge-backup /run/forge-admin
```

ロールバック: `rm /etc/sysusers.d/forge.conf /etc/tmpfiles.d/forge.conf`、`userdel forge`、作ったディレクトリを消す（中身が無いことを確かめて）。

## 4. PostgreSQL `[MUTATING — needs approval]`

1. `ops/linux/postgresql/README.md`（`share/postgresql/README.md`）で A（専用インスタンス、推奨）か B（PFS のクラスターに相乗り）を決める。
2. A のとき: `compose.yml`（コンテナ）か、ホストの 2 つ目のクラスター（`forge-postgresql.conf`）で起動する。
3. bootstrap: runbook `db-bootstrap` の手順 1〜4 を **superuser で**流す。SQL は `/opt/gitstation/current/share/db/bootstrap/`。
   パスワードは `\password forge_app`（`forge_migrator`、`forge_maint` も）で入れ、同じ値を手順 5 のファイルに書く（画面に残さない）。

ロールバック: 新しい環境なら、A はインスタンスごと消す。B は `DROP DATABASE forge` と `forge_*` のロールを消す（PFS の DBA と）。

## 5. 秘密のファイル `[MUTATING — needs approval]`

```sh
umask 077
cd /etc/forge/secrets
# パスワードは手順 4 で入れた値。エディタで 1 行だけ書く（シェルの履歴に残さない）
vi db-app.pw                                        # forge_app
printf '1 %s\n' "$(openssl rand -hex 32)" > cursor-keys
printf '1 %s\n' "$(openssl rand -hex 32)" > audit-seal-keys
printf '1 %s\n' "$(openssl rand -hex 32)" > web-session-keys
printf '1 %s\n' "$(openssl rand -hex 32)" > webhook-secret-keys
printf '1 %s\n' "$(openssl rand -hex 32)" > ci-secret-keys
openssl rand -hex 32 > backup.key                   # 写しを金庫（別の場所）にも保管する。失うとバックアップは読めない
vi oidc-client-secret                               # Keycloak のクライアントの秘密（手順 9）
chown forge:forge /etc/forge/secrets/* && chmod 0600 /etc/forge/secrets/*

cd /etc/forge/secrets-privileged
vi db-migrate.pw                                    # forge_migrator
vi db-maint.pw                                      # forge_maint
chown root:root /etc/forge/secrets-privileged/* && chmod 0600 /etc/forge/secrets-privileged/*
```

- 社内 CA の PEM を `/etc/forge/ca/internal-ca.pem`（0644）に置く。
- 監査の封印の鍵（`audit-seal-keys`）は**足すだけで消さない**（古い封印の検証に要る）。鍵のファイルはすべて手順 14 のとおり別の場所にも控える。

ロールバック: 作ったファイルを消す（`shred -u`）。

## 6. 設定 `[MUTATING — needs approval]`

```sh
install -m 0640 -o root -g forge /opt/gitstation/current/share/etc/forge.toml.example /etc/forge/forge.toml
install -m 0600 -o root -g root  /opt/gitstation/current/share/etc/forgectl.env.example /etc/forge/forgectl.env
vi /etc/forge/forge.toml /etc/forge/forgectl.env    # PLACEHOLDER をすべて手順 0 の事実で置き換える
grep -n PLACEHOLDER /etc/forge/forge.toml           # 残っていないこと（コメントの印も消すか、確認済みと書き換える）
```

決めること: `server.bind` のポート、`database.url` / `forgectl.env` の host・port、`web.base_url`、`auth.oidc.*`、`webhook.allowed_targets`、
`backup.dir`、`backup.pgbackrest_bin`。キーの意味は `docs/deploy/test-server-guide.md` §3.4.3、誤りは `FG-SYS-0002`（runbook `config-invalid`）。

ロールバック: `rm /etc/forge/forge.toml /etc/forge/forgectl.env`。

## 7. スキーマ `[MUTATING — needs approval]`

```sh
sudo forgectl db migrate             # forge_migrator。forge のプロセスが動いていると拒否される（ここではまだ起動していない）。初回は stderr に WARN の JSON（DDL の監査の記録）が多数出るが異常ではない
sudo forgectl db status --check      # 終了コード 0、serve compatible
sudo forgectl doctor                 # FAIL 0。warn は users.administration（管理者なし）と outbox（worker 未起動）程度
```

ロールバック: 新しい環境なら DB を作り直す（手順 4）。down マイグレーションは無い（ADR-0014）。

## 8. サービス `[MUTATING — needs approval]`

```sh
install -m 0644 /opt/gitstation/current/share/systemd/forge-{serve,worker,backup}.service /etc/systemd/system/
install -m 0644 /opt/gitstation/current/share/systemd/forge-backup.timer /etc/systemd/system/
# 手順 0 の #4 から MemoryHigh / MemoryMax を決め、変えるなら drop-in で: systemctl edit forge-serve
systemd-analyze verify /etc/systemd/system/forge-serve.service /etc/systemd/system/forge-worker.service
systemctl daemon-reload
systemctl enable --now forge-worker forge-serve
systemctl status forge-worker forge-serve --no-pager
journalctl -u forge-serve -o cat -n 20     # forge.serve.listening、OIDC があれば forge.serve.oidc_enabled
curl -fsS http://127.0.0.1:18080/healthz   # {"status":"ok"}（ポートは server.bind）
```

起動しないとき: runbook `server-start-failed`（ポート、`LimitNOFILE`）、`config-invalid`（FG-SYS-0002）、`db-migration`（FG-DB-0004）。
ロールバック: `systemctl disable --now forge-serve forge-worker`、ユニットのファイルを消して `daemon-reload`。

## 9. Keycloak（PFS）のクライアント `[MUTATING — needs approval]`（PFS チームの作業）

- クライアント ID `gitstation`（`auth.oidc.client_id`）、confidential、Standard flow のみ。
- Valid redirect URI: `https://<GitStation のホスト名>/-/auth/callback`（`web.base_url` + 固定のパス）。
- Backchannel logout URL: `https://<GitStation のホスト名>/-/auth/backchannel-logout`。
- 社員番号の claim（既定 `employee_number`）と入場のロール（既定 `access`）をアクセストークンに載せる。名前が違えば
  `auth.oidc.employee_number_claim` / `auth.oidc.access_role` を合わせる。詳細は runbook `oidc-login`。
- クライアントの秘密を手順 5 の `oidc-client-secret` に書き、`systemctl restart forge-serve`。

## 10. nginx `[MUTATING — needs approval]`

```sh
install -m 0644 /opt/gitstation/current/share/nginx/gitstation.conf /etc/nginx/conf.d/gitstation.conf
vi /etc/nginx/conf.d/gitstation.conf      # server_name、証明書、upstream のポート（= server.bind）
nginx -t && systemctl reload nginx
curl -fsS https://<GitStation のホスト名>/healthz
```

- PFS の nginx が既に 443 を持つなら、新しい nginx は入れず、その設定に `server` ブロックを足す（PFS チームの合意）。
- `client_max_body_size` と `proxy_*_timeout` は `git.http.push_max_bytes` / `git.http.timeout_ms` 以上に保つ。
- forge が見る接続元は**プロキシだけ**（信頼するプロキシの設定は無い）。1 人あたりの制限はプロキシの `limit_conn`（接続元ごと 32）と
  `limit_req`（サインインの開始とコールバック、接続元ごと 10 回 / 分）で行う。forge 側の `server.max_connections_per_peer` はプロキシ全体の
  上限になるので、例の設定の 512 から、想定する同時接続（利用者数 × 数本 + git の clone）に合わせて決める（`max_connections` 以下。runbook `connections`）。
  社内の NAT の後ろに多数の利用者がいて 1 つのアドレスに見えるなら、`limit_conn` を上げる。

ロールバック: 設定のファイルを消して `nginx -t && systemctl reload nginx`。

## 11. 最初の管理者 `[MUTATING — needs approval]`

Keycloak で一度ログインした利用者を管理者にする（OIDC の初回ログインで利用者ができる）:

```sh
sudo forgectl admin bootstrap-admin --user <利用者名>
```

Keycloak がまだ使えないときは `--create --email <メール>` で作り、`sudo forgectl admin token issue …` でトークンを出す
（`docs/deploy/test-server-guide.md` §3.6）。ロールバック: `sudo forgectl admin user disable <利用者名>`。

## 12. バックアップ `[MUTATING — needs approval]`

1. pgBackRest の設定（stanza `forge`、`archive_mode`、`stanza-create`、`check`）は runbook `backup-restore` の「設定」。
   PostgreSQL がコンテナなら、`backup.pgbackrest_bin` に `docker exec` / `podman exec` の包みを置く（forge が実行環境のソケットを
   使える = root 相当の権限。承認して決める）。ホストの PostgreSQL を `sudo -u postgres` で使うなら、`forge-backup.service` の
   先頭の注のとおり drop-in と sudoers の 1 行が要る。
2. タイマーを有効にし、1 回手で流す:

```sh
systemctl enable --now forge-backup.timer
systemctl start forge-backup.service && journalctl -u forge-backup -o cat -n 30
sudo forgectl doctor                       # backup.last_success が ok、backup.state_copy が ok
systemctl list-timers forge-backup.timer
```

3. 鍵の控え: `backup.key`、`audit-seal-keys`、`cursor-keys`、`web-session-keys`、`webhook-secret-keys`、`ci-secret-keys`、
   pgBackRest の `repo1-cipher-pass` を、サーバーの外（金庫・別の保管）に置く。**`backup.key` を失うとバックアップは読めない**。

ロールバック: `systemctl disable --now forge-backup.timer`。

## Verify

- `sudo forgectl doctor` が FAIL 0（終了コード 0）。`users.administration` が ok。
- `curl -fsS https://<ホスト名>/healthz` が `{"status":"ok"}`、`/readyz` が 200。
- ブラウザで `https://<ホスト名>/` を開き、Keycloak でログインできる。
- 試しのプロジェクトを作り（`gstn project create …`）、worker のログに `forge.repo.provisioned` が出て、`git clone` / `git push` が通る
  （`docs/deploy/test-server-guide.md` §3.8。push はプロキシ越しに 100 MB 程度のファイルでも試す）。
- `systemctl list-timers` に `forge-backup.timer`、手順 12 の 1 回目の世代が `<backup.dir>/<時刻>/manifest.json` を持つ。
- `systemctl restart forge-serve` が 180 秒以内に終わり、`journalctl -u forge-serve` に `forge.serve.stopped` が出る（止めるときの片づけ）。

## Escalate

- 手順 0 の事実が揃わない（メモリ・ディスクの余力が無い、コンテナも 2 つ目の PostgreSQL も置けない）: 導入の計画の見直し。
- PostgreSQL を共有するしかなく、PFS が既に WAL のアーカイブを持つ: `forge backup` の DB の部分の設計の変更（runbook `backup-restore` の前提が崩れる）。
- `systemd-analyze verify` が保護の設定（`SystemCallFilter` など）で起動を妨げる: その行だけを drop-in で外し、外した理由を障害記録に残す。
