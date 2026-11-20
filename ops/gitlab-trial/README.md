# PFS 試験サーバーへの配置（GitLab CI/CD）

GitStation を、PFS（IoT 基盤）の試験サーバーに**社内 GitLab の CI/CD から**置くための一式。
同じサーバーの BankMasalah（`deploy/pfs-trial/`）と同じ形: サーバー上の**プロジェクト専用の shell runner** が、
サーバーの Docker でイメージを作り、パイプライン画面の ▶ で配置する。手順書は runbook `gitlab-trial-deploy`。

本番（TLS・systemd）の導入は `ops/linux/` と runbook `linux-install`。ここは**試験サーバー専用**で、平文 HTTP を使う。

## 流れ

1. GitStation の `main` を社内 GitLab（remote `gitlab`）に push する（`main` は保護されている）
2. パイプラインの `build:image` が、サーバー上で `gitstation/forge:<短い sha>` を作る（`ops/gitlab-trial/Dockerfile`。
   web を `npm run build` → `forge-cli` を `--features embed-ui` の release でビルド → Debian slim に git と一緒に入れる）
3. パイプライン画面で `deploy:pfs-trial` の ▶ を押す → `ops/gitlab-trial/deploy.sh` が配置する

MR やブランチのパイプラインは**作られない**（`.gitlab-ci.yml` の `workflow:`）。レビュー前のコードを、Docker と秘密のある
サーバーで動かさないため。GitStation 自身のビルド・テストは `.forge-ci.yml` と GitHub の CI が受け持つ。

## サーバー側の構成

| もの | 場所 / 名前 |
|---|---|
| runner | `gitlab-runner-gitstation`（systemd）、shell 型、ユーザー `gitstation-ci`（`docker` グループ）、設定 `/home/gitstation-ci/.gitlab-runner/config.toml` |
| runner の登録 | プロジェクト runner、タグ `gitstation-pfs`、**Protected**（保護されたブランチだけ）、**Locked**、タグなしのジョブは取らない |
| 配置先 | `/srv/gitstation`（所有者 `gitstation-ci`、750） |
| 設定 | `/srv/gitstation/forge.toml`、`compose.yml`（配置のたびにコミットのものを写す。手で直さない） |
| サーバーの値 | `/srv/gitstation/.env`（600）: `PUBLISH_ADDR`、`PUBLISH_PORT`（3306）、`PUBLIC_URL`、`OIDC_ISSUER`。秘密は書かない |
| 秘密 | `/srv/gitstation/secrets/`（700、中身 600）: `db-app.pw`、`cursor-keys`、`audit-seal-keys`、`web-session-keys`、`ci-secret-keys`、`webhook-secret-keys`、`oidc-client-secret`、`deploy-ssh-key`（forge-deploy の ssh の秘密鍵。公開鍵は `/srv/gitstation/deploy-ssh-key.pub`、644） |
| 特権の秘密 | `/srv/gitstation/secrets-privileged/`（700、中身 600）: `db-migrate.pw`、`db-maint.pw`、`pg-superuser.pw`。`migrate` と `db` のコンテナだけに見せる |
| リポジトリ | `/srv/gitstation/data/repositories`（bind mount、`gitstation-ci` の持ち物） |
| DB | コンテナ `gitstation-trial-db`（`timescale/timescaledb:2.28.1-pg16`）、ボリューム `gitstation-trial-pgdata`。**公開しない**（ネットワーク `gitstation-trial-net` の中だけ） |
| DB のバックアップ | `/srv/gitstation/backups/pre-deploy-<UTC>-<sha>.dump`（配置のたび、新しい 10 個） |
| 配置の記録 | `/srv/gitstation/deployed.log`（時刻、sha、パイプライン、実行者、前の版） |
| compose のプロジェクト名 | `gitstation-trial`（コンテナ `gitstation-trial-{db,serve,worker,migrate}`） |
| 公開 | `PUBLISH_ADDR:3306` → serve の 8080。ブラウザは `PUBLIC_URL`（`http://<サーバー>:3306`） |
| イメージ | `gitstation/forge:<短い sha>`。使用中のものと新しい 3 個を残す |

秘密は**リポジトリにも GitLab の CI/CD 変数にも置かない**。コンテナの利用者は `gitstation-ci` と同じ uid/gid でイメージに作る
（`build:image` が `--build-arg FORGE_UID=$(id -u)` を渡す）ので、0600 の秘密のファイルをそのまま読める。

### runner の権限（重要）

runner の利用者 `gitstation-ci` は `docker` グループに入っている。**`docker` グループは、この共有サーバーの root と同じ**
（どのコンテナも起動でき、ホストのどのファイルもマウントできる。PFS・bankmasalah のデータにも届く）。だから:

- `main` は保護し、**MR の承認を必須**にし、マージできるのは Maintainer だけにする（手順 1）。`main` に入ったコードは
  この権限で動く
- runner は Protected（保護されたブランチのジョブだけ）、Locked、タグ付きのジョブだけ。MR・ブランチのパイプラインは作らない
- 配置のボタンは保護環境 `trial-gitstation` の許可者だけが押せる
- 配置先・バックアップの数は `deploy.sh` に固定（パイプライン変数では変えられない）

### 外への通信（ビルドに要る）

`build:image` はサーバーの Docker で、次へ出る（社内のミラー・プロキシがあればそれ。npm は CI/CD 変数 `NPM_CONFIG_REGISTRY` で替えられる）:

| 行き先 | 何のため |
|---|---|
| `docker.io`（Docker Hub）| 土台のイメージ（rust・node・debian はダイジェストで固定、`timescale/timescaledb`） |
| `registry.npmjs.org` | `web/` の `npm ci` |
| `index.crates.io` / `static.crates.io` | `cargo build --locked` のクレート |
| `deb.debian.org` | 実行イメージの `apt-get`（git・curl・openssh-client・tini） |

動かしているとき（serve / worker）の外への通信は Keycloak（`OIDC_ISSUER`）と、デプロイのターゲットへの ssh（下の「自前の CI/CD」。使うときだけ）。

### 平文 HTTP について

このサーバーは TLS なし。forge は既定では http の `web.base_url` を拒み、ブラウザは http のオリジンから `__Host-`・`Secure` の
クッキーを受け取らない。そのため `forge.toml` で次の 2 つを明示している（**本番の設定には写さない**）:

- `web.allow_insecure_http = true` — loopback 以外の http の base URL を許し、セッションのクッキーを `gstn_3306_session` /
  `gstn_3306_oidc`（名前に公開のポート。`Secure` なし、`HttpOnly`・`SameSite=Lax` は残る）で出す（ADR-0021 Amendment 2026-11-20）。起動のたびに WARN
  `forge.serve.plain_http`、`forge doctor` の `auth.oidc` は warn になる。
- `auth.oidc.allow_insecure_issuer = true` — http の Keycloak（`OIDC_ISSUER`）を許す。

クッキー・トークン・git のパスワードは社内 LAN を平文で流れる。試験の利用者・データに限る。

**残るリスク:** ブラウザはクッキーをポートで分けない。同じサーバー（同じアドレス）の別のアプリ（PFS、bankmasalah ほか）は、
意図すれば GitStation のセッションのクッキーを読み、植えられる（名前にポートを入れて防げるのは偶然の衝突だけ）。同居するアプリを
信頼できる試験の間だけの割り切り。本当の対処は GitStation 専用のホスト名か TLS（そのとき `web.allow_insecure_http` を外す）。
`gstn`（CLI）は localhost 以外の http を受け付けないので、このサーバーには使えない（git の clone / push と Web は使える）。

## 初めて置くとき

### 手順 1. GitLab の設定（Maintainer）

- 設定 → リポジトリ → 保護ブランチ: `main` を保護（**push もマージも Maintainer だけ**、force push 不可）。`main` は GitStation 側で
  レビュー済みのものを Maintainer が `git push gitlab main` で写す
- 設定 → マージリクエスト: **承認を必須**（1 人以上）。承認のルールを Developer が外せないようにする
- 設定 → CI/CD → 変数: **パイプライン変数を使える最小のロール = Maintainer**（「Minimum role to use pipeline variables」）。
  秘密の CI/CD 変数は作らない
- 設定 → CI/CD → Runners: **共有 runner とグループ runner を無効**にし、プロジェクト runner を作る:
  タグ `gitstation-pfs`、**Protected**、**Lock to current projects**、「タグなしのジョブを実行」は**オフ**。表示された `glrt-…` のトークンを控える
- 操作 → 環境: `trial-gitstation` を作り（最初の配置でもできる）、外部 URL に `http://<サーバー>:3306` を入れる
- 設定 → CI/CD → 保護環境: `trial-gitstation` を**必ず**保護し、配置できるのを Maintainer（または指名した人）だけにする

### 手順 2. サーバー（root）

リポジトリの `ops/gitlab-trial/setup-server.sh` をサーバーに運んで実行する（何度実行してもよい。**既存の秘密は上書きしない、表示しない**）:

```sh
sudo bash setup-server.sh            # 無いものだけ作る
sudo vi /srv/gitstation/.env         # <…> をすべて置き換える（PUBLISH_ADDR、PUBLIC_URL、OIDC_ISSUER）
sudo bash setup-server.sh --check    # 足りないものを表示するだけ（何も変えない。足りなければ終了コード 1）
```

`PUBLIC_URL` は Keycloak の `gitstation` クライアントの redirect URI（`<PUBLIC_URL>/-/auth/callback`）と**一字一句**同じにする。
`OIDC_ISSUER` は Keycloak の `<issuer>/.well-known/openid-configuration` の `issuer` と同じにする（コンテナからその URL に届くこと）。

### 手順 3. Keycloak のクライアントの秘密

PFS の Keycloak（realm `iotpf`）の `gitstation` クライアント（confidential）の秘密を、**画面やシェルの履歴に残さず**ファイルに書く:

```sh
sudo -u gitstation-ci sh -c 'umask 077; cat > /srv/gitstation/secrets/oidc-client-secret'   # 貼り付けて Enter、Ctrl-D
sudo -u gitstation-ci chmod 600 /srv/gitstation/secrets/oidc-client-secret
```

クライアントの設定（PFS 側）: Valid redirect URI `<PUBLIC_URL>/-/auth/callback`、Post logout redirect URI `<PUBLIC_URL>/`、
Backchannel logout URL `<PUBLIC_URL>/-/auth/backchannel-logout`、`staff_id` と入場のロール `access` がアクセストークンに載ること（runbook `oidc-login`）。

### 手順 4. runner の登録（root）

`gitlab-runner` のパッケージは BankMasalah で入っている。

```sh
gitlab-runner register --non-interactive --config /home/gitstation-ci/.gitlab-runner/config.toml \
  --url <社内 GitLab の URL> --token <glrt-…> --executor shell --name gitstation-pfs-<サーバー名>
chown gitstation-ci: /home/gitstation-ci/.gitlab-runner/config.toml
gitlab-runner install --service gitlab-runner-gitstation --user gitstation-ci \
  --working-directory /home/gitstation-ci --config /home/gitstation-ci/.gitlab-runner/config.toml
systemctl enable --now gitlab-runner-gitstation
```

`setup-server.sh` が `~gitstation-ci/.bash_logout` を消している（Ubuntu 既定のものは shell 型のジョブを失敗させる）。
`gitstation-ci` を `docker` グループに入れた後に runner を起動すること（先に起動していたら `systemctl restart gitlab-runner-gitstation`）。

### 手順 5. 最初の配置と管理者

1. `main` のパイプラインで `build:image` が通るのを待ち、`deploy:pfs-trial` の ▶ を押す。初回は DB の bootstrap
   （`ops/db/bootstrap` の 01〜03 とロールのパスワード）も行う。
2. ブラウザで `PUBLIC_URL` を開き、Keycloak で一度ログインする（OIDC の初回ログインで利用者ができる。名前は `staff_id` の小文字）。
3. その利用者を管理者にする（サーバーで `gitstation-ci` として）:

   ```sh
   cd /srv/gitstation
   docker exec gitstation-trial-serve forge admin bootstrap-admin --user <利用者名>
   docker exec gitstation-trial-serve forge doctor
   ```

   `doctor` の FAIL が 0 であること（`auth.oidc` の warn は平文 HTTP のため。`backup.*` の warn は下の「バックアップ」）。

### この版からの更新（既存のサーバー）

`forge.toml` に `[deploy]` が入ったので、`deploy.sh` は `secrets/deploy-ssh-key` が無いと**何も止めずに** preflight で止まる。
この版を配置する前に、root で `sudo bash setup-server.sh` をもう一度実行する（鍵を作り、**公開鍵だけ**を表示する。既存の秘密は変えない）。

## 自前の CI/CD（GitStation の Runner と自動デプロイ）

GitStation 自身の CI/CD と forge-deploy（ADR-0031、runbook `deploy`）で、合成のデモアプリ `hello-app`
（`ops/test-env/m4-hello-app` と同じもの）を**同じ試験サーバー**に自動で置く。`main` に push → GitStation の Runner で
パイプライン → 環境 `dev`（`auto_deploy_ref: refs/heads/main`、承認なし）が自動でデプロイ → ヘルスチェック → 失敗なら前の版に戻す。
**使うときだけ**の追加（無くても GitStation は動く）。

| 部品 | 中身 |
|---|---|
| デプロイの鍵 | 秘密鍵 `/srv/gitstation/secrets/deploy-ssh-key`（600）を worker が `ssh -i` で使う。公開鍵 `/srv/gitstation/deploy-ssh-key.pub` |
| ターゲット | このサーバー自身の sshd（22）。利用者 `gs-deploy`（`docker` グループ）。鍵は `restrict,command="/opt/forge-deploy/dispatch"` だけ |
| dispatch | `/opt/forge-deploy/dispatch`。`SSH_ORIGINAL_COMMAND` が**ちょうど** `<script> <deploy\|health\|rollback> <40 桁の hex>` のときだけ、許可表のスクリプト（`hello-app`）を引数の配列で `exec`。シェルは解釈しない。ログ `/var/log/forge-deploy/dispatch.log` |
| デモアプリ | `/opt/forge-deploy/scripts/hello-app`。コミットの `public/` **だけ**を取り出し、キットの**固定の** Dockerfile で `gs-demo/hello-app:<sha>` を作り、compose プロジェクト `gs-demo-hello`（コンテナ `gs-demo-hello-web`、64 MiB・0.25 CPU）で `DEMO_BIND:DEMO_PORT` に出す。`deploy` と `rollback` は同じ `flock`。ログ `/var/log/forge-deploy/hello-app.log` |
| ターゲットの設定 | `/etc/forge-deploy/hello-app.env`（root:gs-deploy 640、秘密なし: `FORGE_REPO_URL`・`DEMO_BIND`・`DEMO_PORT`）、`/etc/forge-deploy/hello-app-read.token`（`read_repository` のトークン、gs-deploy 600） |
| Runner | `gitstation-trial-runner`（compose プロジェクト同名、`compose.runner.yml`）。`ops/runner/Dockerfile` のビルド。中で rootless Podman、Docker のソケットなし、特権なし、ポートの公開なし。3 GiB・2 CPU・pids 2048。タグ `linux,podman,trial` |
| Runner の設定 | `/srv/gitstation/runner/config.toml`（640）、トークン `/srv/gitstation/secrets-runner/token`（640。serve / worker が mount する `secrets/` の外） |

ポート `18090`・プロジェクト `demo/hello-app`・ターゲット名 `trial-1` は例。サーバーのアドレスは `<サーバー>`（例 `192.0.2.10`）と書く。

**権限について:** `gs-deploy` の `docker` グループはこのサーバーの root と同じ。鍵でできることは dispatch（固定の 3 語と許可表）と
`hello-app`（コミットからは静的ファイルしか使わない）だけに絞っている。Runner の `seccomp=unconfined` と `/dev/fuse` は
入れ子の rootless Podman のため（`ops/runner` と同じ。**試験サーバーだけ**）。

**外への通信（追加）:** ターゲットのイメージのビルドで `docker.io`（alpine、ダイジェストで固定）と `dl-cdn.alpinelinux.org`
（`busybox-extras`。2 回目からは Docker のキャッシュ）。Runner は `PUBLIC_URL` とジョブのイメージの `docker.io`。

### 手順 A. 鍵と許すネットワーク（root、GitStation の配置の前）

1. `sudo bash setup-server.sh` — `deploy-ssh-key` を作り、最後に `deploy public key (…): ssh-ed25519 …` を表示する。
2. `/srv/gitstation/.env` に `DEPLOY_ALLOWED_NETWORKS=<サーバー>/32` を足す（forge はこの外のターゲットに繋がない）。
3. `deploy:pfs-trial` で配置する（`openssh-client` の入ったイメージと `[deploy]` の設定が入る）。

### 手順 B. デモのプロジェクト（Web と git）

1. Web でグループ `demo` とプロジェクト `hello-app`（種類 `system`）を作る。
2. 元を用意して push する（パスワードは自分のトークン）:

   ```sh
   mkdir hello-app && cd hello-app && git init -b main
   cp -R <repo>/ops/test-env/m4-hello-app/public <repo>/ops/test-env/m4-hello-app/tests .
   cp <repo>/ops/gitlab-trial/deploy-target/hello-app.forge-ci.yml .forge-ci.yml
   git add public tests .forge-ci.yml && git commit -m "hello-app v1"
   git push http://<サーバー>:3306/demo/hello-app.git main
   ```

   Runner がまだ無いので、このパイプラインは待ちのまま（手順 D の後に動く）。`auto_deploy_ref` は成功した push の
   パイプラインの版だけをデプロイする。

### 手順 C. ターゲット（root）

```sh
sudo bash ops/gitlab-trial/deploy-target/setup-target.sh \
  --pubkey /srv/gitstation/deploy-ssh-key.pub \
  --repo-url http://<サーバー>:3306/demo/hello-app.git --port 18090
```

取得用のトークン（Reporter だけの利用者で。値は画面に出さない）を置いて、もう一度確かめる:

```sh
( umask 077
  docker exec gitstation-trial-serve forge admin token issue --user <Reporter の利用者> --scopes read_repository --expires-in 90d \
    | grep -oE 'fgp_[A-Za-z0-9_]+' > /etc/forge-deploy/hello-app-read.token )
chown gs-deploy:gs-deploy /etc/forge-deploy/hello-app-read.token && chmod 600 /etc/forge-deploy/hello-app-read.token
sudo bash ops/gitlab-trial/deploy-target/setup-target.sh --check    # MISSING が無く終了コード 0
```

トークンは出力の全体をリダイレクトしない（表の全体が入り、取得が失敗する）。期限が切れたら置き直す。

最後に `host key fingerprint`・`host_key_type`・`host_key (base64 …)` を表示する。**指紋は、サーバーのコンソールで
`ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub` を自分で実行した結果と照合する**（`ssh-keyscan` だけを信じない）。
`DEMO_BIND` は既定で `127.0.0.1`（LAN から見せない）。LAN から見せるなら `--bind <サーバー>`（`0.0.0.0` は拒む）。

worker のコンテナからサーバーの 22 番に届くこと（ファイアウォールが Docker のブリッジからの ssh を止めていないこと）:
`docker exec gitstation-trial-worker timeout 5 curl -s telnet://<サーバー>:22 </dev/null | head -c 8`（`SSH-2.0-` が出る）。
sshd に `AllowUsers` / `AllowGroups` があれば `gs-deploy` を足す（setup-target は sshd の設定を変えず、警告だけ出す）。

### 手順 D. Runner（`gitstation-ci` として、リポジトリのチェックアウトから）

```sh
bash ops/gitlab-trial/runner.sh init        # runner/ と secrets-runner/、config.toml（server = PUBLIC_URL）
( umask 027; docker exec gitstation-trial-serve forge admin runner register --name trial-1 --tags linux,podman,trial --admin <管理者> \
    | grep -oE 'fgr_[A-Za-z0-9_]+' > /srv/gitstation/secrets-runner/token )
chmod 640 /srv/gitstation/secrets-runner/token
bash ops/gitlab-trial/runner.sh up          # イメージのビルド（cargo、2 ジョブ、nice。重い）→ check → 起動
```

`check` は `ok: configuration, token, rootless podman, job isolation`。トークンは `fgr_…` の 1 行だけ（表の全体を入れると FG-SYS-0002）。

### 手順 E. 環境とターゲット（API。プロジェクトの Maintainer の `api` のトークン）

`gstn` はこのサーバー（http）には使えないので curl で送る。`<id>` は `GET /api/v1/namespaces/resolve?path=demo/hello-app` の
プロジェクトの id。

```sh
T=<api のトークンのファイル>; U=http://<サーバー>:3306/api/v1/projects/<id>
H=(-H "Authorization: Bearer $(cat "$T")" -H 'Content-Type: application/json')
curl -fsS "${H[@]}" -X POST "$U/protected-refs" -d '{"pattern":"refs/heads/main","push_min_role":"maintainer"}'
curl -fsS "${H[@]}" -X PUT "$U/deploy/environments/dev" \
  -d '{"requires_approval":false,"auto_rollback":true,"auto_deploy_ref":"refs/heads/main"}'
curl -fsS "${H[@]}" -X PUT "$U/deploy/environments/dev/targets/trial-1" \
  -d '{"host":"<サーバー>","port":22,"user":"gs-deploy","host_key_type":"ssh-ed25519","host_key":"<手順 C の base64>","script":"hello-app"}'
```

応答の `host_key_fingerprint` が手順 C で照合した指紋と同じこと。Web のプロジェクトの「デプロイ」の画面でも設定できる。

### 手順 F. 確かめる

1. `public/index.html` の `release: v1` を `v2` にして `main` に push する。
2. パイプライン（`build` → `test`、Runner `trial-1`）が成功し、デプロイ（`dev`、`requested_by` system）が `succeeded` になる
   （Web のデプロイの一覧、または worker のログ `forge.deploy.requested`）。
3. サーバーで `curl -s http://127.0.0.1:18090/version` がその SHA、`/var/log/forge-deploy/hello-app.log` に `live` と `healthy`。
4. 失敗の確かめ: `public/health` を `ng` にして push（CI は通る）→ `rolled_back`、`error_code = FG-DEPLOY-0005`。
   `/version` は前の SHA に戻る。`public/health` を `ok` に戻して push する。
   その環境の**最初の**デプロイが失敗したときは前の版が無いので `FG-DEPLOY-0006 no_previous_version`（仕様）。

詰まったら runbook `deploy`（FG-DEPLOY-0003 接続できない、0004 host key の不一致、0008 鍵のファイル、0013 `denied_address` /
`outside_allowed_networks`）と runbook `gitlab-trial-deploy`。

### 自前の CI/CD の戻し方（サーバーの変更をすべて元に戻す）

上から順に。どれも GitStation 本体（`gitstation-trial-db` / `serve` / `worker`）には触れない。

1. GitStation: 自動デプロイを止める（`PUT …/deploy/environments/dev` を `"auto_deploy_ref":null` で）。ターゲットを止める
   （`PUT …/targets/trial-1` に同じ内容と `"enabled":false`）。Runner を止める（`forge admin runner disable`）。
   取得用と API のトークンを失効させる。デモのプロジェクトは要らなければ Web で消す。
2. Runner（`gitstation-ci`）: `runner.sh` の先頭のコメントの「Rollback」（`gitstation-trial-runner` のコンテナ・ボリューム・
   イメージと `/srv/gitstation/{runner,secrets-runner,compose.runner.yml}` だけを消す）。
3. ターゲット（root）: `deploy-target/setup-target.sh` の先頭のコメントの「Rollback」（`gs-demo-hello` のコンテナ、
   `gs-demo/hello-app` のイメージ、利用者 `gs-deploy`、`/opt|/etc|/srv|/var/log` の `forge-deploy`）。
4. 鍵と設定: `.env` から `DEPLOY_ALLOWED_NETWORKS` を消す。鍵を捨てるなら `deploy-ssh-key` と `deploy-ssh-key.pub` を消す
   **前に**、この版より前の版を配置しておく（この版以降の `deploy.sh` は鍵が無いと止まる）。鍵を残すだけなら害はない。
5. イメージの `openssh-client` と `forge.toml` の `[deploy]` は、ターゲットが無ければ何もしない（戻すなら前の版を配置する）。

## 前の版に戻す

GitLab で戻したい版のパイプラインを開き、`deploy:pfs-trial` をもう一度実行する。イメージは直近の 3 版（と動いている版）まで
サーバーに残る。それより古い版は、そのパイプラインの `build:image` をやり直してから配置する。

- **マイグレーションは足すだけで戻らない。** 戻す先の版が知らないマイグレーションが DB にあると、古い forge は
  `FG-DB-0004 reason=db_ahead` で起動しない（`deploy.sh` は失敗を表示し、前の版の起動を試みて止まる）。そのときは DB も戻す。
  DB を戻すのは**ダンプの後のデータを失う操作**なので、人が判断して行う（runbook `gitlab-trial-deploy` の「DB を戻す」。
  今の DB は消さずに名前を変えて残す）。リポジトリ（`data/repositories`）はダンプに入っていない。

## してはいけないこと

- `docker compose … down -v`、`docker volume rm gitstation-trial-pgdata`、`docker system prune --volumes`（DB が消える）
- 他のプロジェクトのコンテナ・ボリューム・ネットワークに触れる（名前が `gitstation-trial-` で始まるものだけが GitStation）
- `forge.toml` の平文 HTTP の 2 行を本番の設定に写す
- 秘密のファイルを CI/CD 変数・チャット・チケットに貼る

## バックアップ

配置のたびの `pg_dump`（新しい 10 個）だけ。`forge backup`（pgBackRest、リポジトリを含む）は試験サーバーでは設定していない
（`forge doctor` の `backup.*` は warn）。リポジトリを残したいときは `data/repositories` を別に写す。

## 確かめ方（ローカル）

```sh
for f in ops/gitlab-trial/{deploy.sh,setup-server.sh,runner.sh} \
         ops/gitlab-trial/deploy-target/{dispatch,hello-app,setup-target.sh,test-dispatch.sh}; do bash -n "$f" || echo "BAD $f"; done
bash ops/gitlab-trial/deploy-target/test-dispatch.sh            # dispatch の引数の検証（root・ssh 不要）
IMAGE_TAG=x PUBLISH_ADDR=192.0.2.1 PUBLIC_URL=http://192.0.2.1:3306 OIDC_ISSUER=http://192.0.2.1:18180/realms/iotpf \
  docker compose -f ops/gitlab-trial/compose.yml config -q
RUNNER_TAG=x RUNNER_FILES_GID=1000 docker compose -f ops/gitlab-trial/compose.runner.yml config -q
APP_SHA=x DEMO_BIND=127.0.0.1 DEMO_PORT=18090 docker compose -f ops/gitlab-trial/deploy-target/hello-app.compose.yml config -q
cargo nextest run -p forge-cli --test gitlab_trial_package     # forge.toml を本物の読み込みで読む
```
