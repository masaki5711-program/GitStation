# GitStation の Linux 本番パッケージ

GitStation（バイナリ `forge`）を、PFS（IoT 基盤）と同じ Linux サーバーで本番として動かすための一式。
手順は runbook にある: **新規の導入** `ops/runbooks/linux-install.md`、**更新と戻し** `ops/runbooks/linux-upgrade.md`。

前提: systemd のディストリビューション（x86_64 または aarch64）。Docker / Podman は**無くてもよい**（PostgreSQL をコンテナで動かすときだけ使う）。
`git` 2.38 以上が `PATH`（`/usr/bin`）にあること。

## 中身

| ファイル | 置き場（サーバー） | 役割 |
|---|---|---|
| `build-release.sh` | —（ビルド機） | リリースの tar.gz と sha256 を作る（`--dry-run` で確認だけ） |
| `systemd/forge-serve.service` | `/etc/systemd/system/` | `forge serve`（Web・API・git の Smart HTTP） |
| `systemd/forge-worker.service` | `/etc/systemd/system/` | `forge worker`（リポジトリの作成・ジョブ・封印・保守・権限の写し） |
| `systemd/forge-backup.service` / `.timer` | `/etc/systemd/system/` | 毎日 01:15 の `forge backup`（保守の窓 02:00〜05:00 の前） |
| `systemd/forge.sysusers` | `/etc/sysusers.d/forge.conf` | サービスのユーザー `forge`（ログイン不可） |
| `systemd/forge.tmpfiles` | `/etc/tmpfiles.d/forge.conf` | ディレクトリと権限（下の表） |
| `etc/forge.toml.example` | `/etc/forge/forge.toml` | 設定の見本。秘密は書かず、ファイルのパスで指す |
| `etc/forgectl.env.example` | `/etc/forge/forgectl.env` | `forgectl` が使う移行・保守のログインの URL |
| `bin/forgectl` | `/usr/local/sbin/forgectl` | 運用者のコマンドの入口（下の「forgectl」） |
| `nginx/gitstation.conf` | `/etc/nginx/conf.d/` | TLS のリバースプロキシ |
| `postgresql/` | — | PostgreSQL の置き方（専用インスタンス推奨）、メモリを絞った設定、コンテナの compose |

リリースの tar.gz では、上が `share/` の下に入り、DB の bootstrap の SQL（`share/db/bootstrap/`）と runbook（`share/runbooks/`）も同梱する。

## サーバーの配置

| パス | 持ち主・権限 | 中身 |
|---|---|---|
| `/opt/gitstation/releases/<版>/` | root、0755 | 展開したリリース（`bin/forge`、`share/`、`BUILD-INFO`）。**前の版を消さずに残す**（戻しに使う） |
| `/opt/gitstation/current` | root（シンボリックリンク） | 動かす版。ユニットはここの `bin/forge` を起動する。更新・戻しはリンクの付け替え |
| `/etc/forge/forge.toml` | root:forge 0640 | 設定（秘密なし） |
| `/etc/forge/secrets/` | forge:forge 0700、中身 0600 | serve / worker / backup が読む秘密（`db-app.pw`、鍵のファイル、OIDC の秘密、バックアップの鍵） |
| `/etc/forge/secrets-privileged/` | root:root 0700、中身 0600 | `db-migrate.pw`、`db-maint.pw`。**サービスからは見えない**（`InaccessiblePaths`） |
| `/etc/forge/ca/` | root 0755 | 社内 CA の PEM（公開してよい） |
| `/var/lib/forge/repositories` | forge 0750 | `storage.root`（ベアリポジトリ）。シンボリックリンク不可 |
| `/var/lib/forge/state-copy` | forge 0700 | `backup.state_copy_dir`（worker が 5 分ごとに書く） |
| `/srv/forge-backup` | forge 0700 | `backup.dir`（別のディスクを推奨） |
| `/run/forge-admin` | root 0711 | `forgectl` が秘密を一時的に置く場所（コマンドの終わりに消す） |

ログは **journald**（forge は JSON を stderr に出す）。ファイルのログのディレクトリは無い:
`journalctl -u forge-serve -o cat | jq 'select(.error_code != null)'`。

## 設計の判断

- **秘密は別ファイル**: forge は秘密のファイルが絶対パスで、グループ・他人が読めない（`0600`）ことを起動時に確かめる
  （`FG-SYS-0002 reason=insecure_permissions`）。`forge.toml` には秘密を書かない。
- **移行・保守のログインをサーバーのプロセスから隠す（ADR-0018）**: `forge serve` / `forge worker` は `database.migrate_password_file` /
  `database.maint_password_file` を持つ設定では起動しない。ファイルは root だけが読め、`forgectl` がコマンドの間だけ
  `/run/forge-admin/run.XXXX/` に forge の持ち物として写し、環境変数でパスを渡す。3 つのユニットは
  `InaccessiblePaths=` でこの 2 か所を見えなくしている（同じ `forge` ユーザーでも、サービスの中からは読めない）。
- **運用者のコマンドも `forge` ユーザーで動かす**: root で動かすと、リポジトリに root の持ち物のファイルができ、git が
  「持ち主が違う」と拒否する。`forgectl` は `runuser -u forge` で実行し、監査の行に残る操作者名（`USER`）には sudo した人を入れる。
- **停止**: `forge` は SIGTERM で受付を止め、実行中の要求（30 秒）、マージ（30 秒）、push（90 秒）を待つ。`TimeoutStopSec=180`、
  `KillMode=mixed`（git の子プロセスには SIGTERM を送らず、forge が片づける）。sd_notify は使わない（`Type=exec`）。
  systemd 240 未満（RHEL 8 系）は `Type=exec` を知らないので `Type=simple` として扱われる（動作は同じ、起動失敗の検出が遅れるだけ）。
- **メモリ**: `MemoryHigh` / `MemoryMax` は**仮の値**。PFS と同居するので、空きのメモリを聞いてから決める。大きな clone の
  `git pack-objects` は serve の cgroup の中で動く。
- **ポート**: `forge serve` は `127.0.0.1:18080`（仮。PFS の Keycloak が 8080 を使うことが多いため既定の 8080 を避けた）。外へは nginx の 443 だけ。

## forgectl

```sh
sudo forgectl doctor                      # 読み取りのみ
sudo forgectl db status --check           # 読み取りのみ
sudo forgectl db migrate                  # forge_migrator のパスワードを一時的に渡す
sudo forgectl admin token list --user <名前>
sudo forgectl restore finish --generation <世代>   # forge_maint のパスワードを一時的に渡す
```

- **ちょうど** `db migrate`、`restore finish`、`admin events new-epoch` のときだけ（先頭の語で判定）、`/etc/forge/forgectl.env` の URL と
  `/etc/forge/secrets-privileged/` のパスワードを渡す。どちらも root の持ち物でグループ・他人が触れない（`0600` / `0700`）ことを
  `stat` で確かめ、違えば止める。
- **この 3 つは `forge` ユーザーのプロセスが 1 つでも動いていると拒否する**（`forge-serve`・`forge-worker`・`forge-backup` を先に止める）。
  同じ uid のプロセスは `/proc/<pid>/root` から一時的な写しに届きうるため。ユニットは加えて `ProtectProc=invisible`・`ProcSubset=pid`・
  `InaccessiblePaths=` で守る。`admin events new-epoch` も止めてから流す（短い計画停止）。別の uid で動かす案（`systemd-run --uid`、
  専用の `forge-admin` ユーザー）は、`forge.toml` の秘密（`db-app.pw` など）とリポジトリの持ち主が `forge` なので採らなかった。
- それ以外は `forge --config /etc/forge/forge.toml <引数>` を `forge` ユーザーで実行するだけ。`--config` は固定（引数に書くと拒否）。
- バイナリは `/opt/gitstation/current/bin/forge`（root の持ち物であること）。更新の前に新しい版で試すときは
  `forgectl --release <版> …`（`/opt/gitstation/releases/<版>/bin/forge`）。パスを環境変数で替える方法は無い。
- 終了コードは forge のもの。forgectl 自身の誤り（root でない、ファイルが無い、権限が広い）は 2 で、forge は実行しない。
- 環境は空にしてから渡す（`PG*` などの変数は持ち込まない。forge は `PGPASSWORD` などがあると拒否する）。`RUST_LOG` だけは引き継ぐ。
- sudo の設定は既定の `Defaults env_reset` を保つ（`env_keep` に `FORGE_*`・`PG*`・`LD_*` を足さない）。運用者に forgectl だけを許すなら
  `<運用者> ALL=(root) /usr/local/sbin/forgectl` の 1 行にする。

## ビルド

```sh
ops/linux/build-release.sh --dry-run                                   # 入力と手順の確認（何も実行しない）
ops/linux/build-release.sh --target x86_64-unknown-linux-gnu           # dist/gitstation-<版>-x86_64-unknown-linux-gnu.tar.gz
ops/linux/build-release.sh --target aarch64-unknown-linux-gnu          # aarch64（ネイティブのホスト、またはクロスの設定。スクリプトの先頭の注）
```

- `web/` を `npm ci` → `npm run build` し、`cargo build --release --locked -p forge-cli --features embed-ui --target <triple>`
  （CI の release のジョブと同じ機能）。`--skip-web` で既存の `web/dist` を使う。
- バイナリは**ビルド機の glibc** に動的にリンクする。本番の OS より新しいディストリビューションでビルドしない
  （本番で `GLIBC_2.xx not found` になる）。迷ったら本番と同じ OS の版のコンテナでビルドする。
- 成果物: `dist/gitstation-<版>-<target>.tar.gz`、`.sha256`、`dist/SHA256SUMS`（追記）。版の既定は `<workspace の版>+g<コミット 12 桁>`。

## 検証

- `crates/forge-cli/tests/linux_package.rs`: `forge.toml.example` を本物の設定の読み込み（`forge_core::config::load`）で読み、
  serve / worker / forgectl（移行・保守の URL の追加）の組み合わせで通ること、ユニットが同じ設定とバイナリを指し、
  特権の秘密を `InaccessiblePaths` で隠していることを確かめる。
- シェル: `bash -n ops/linux/build-release.sh ops/linux/bin/forgectl`、`ops/linux/build-release.sh --dry-run`。
  `shellcheck` があればそれも。サーバーでは `systemd-analyze verify /etc/systemd/system/forge-*.service` と `nginx -t`。
