# Runbook: m4-test-environment（試験環境の Runner と自動デプロイ。関連: FG-DEPLOY-0003 / 0005 / 0006 / 0013、FG-SYS-0002）

M4 の完了の条件（計画 14 §8）をこの PC の試験環境（runbook `m3-test-environment`）で確かめるための追加。**試験専用**。
すべて compose `gs-m3-test` の新しいコンテナ（`ops/test-env/compose.m4.yml`）と `ops/test-env/.local/m4/`（gitignore 済み）だけを使う。
本番の配置（PFS の Linux サーバー）は runbook `deploy`・`ci-runner` のとおり。証跡は `docs/handoff/2026-11-17-m4-completion.md`。

## 構成

| 部品 | 中身 |
|---|---|
| `deploy-target` | alpine + sshd（127.0.0.1:22022）。host key は初回に作り volume `deploy-target-hostkey` に残す。利用者 `deploy` の鍵は forge の鍵だけで `restrict,command="/opt/forge-deploy/dispatch"`。合成アプリ `hello-app`（busybox-extras の httpd、コンテナの中の 127.0.0.1:8080 だけ。公開しない） |
| `deploy-forwarder.mjs` | forge は loopback のターゲットを拒む（FG-DEPLOY-0013 `denied_address`）ので、Hyper-V 内部のアドレス（`ipconfig` の「vEthernet (WSL …)」）:22022 → 127.0.0.1:22022。LAN からは届かない。接続元がこの PC 自身のときだけ通す |
| `runner-m4` | この branch の `ops/runner/Dockerfile` のビルド（`gitstation/forge-runner:m4`）。tags `linux,podman,m4`。レジストリは `gs-runner-test_runner` のネットワークの `registry:5000`（compose `gs-runner-test` には触れない） |
| 合成アプリの元 | `ops/test-env/m4-hello-app/`（`public/`・`tests/check.sh`・`.forge-ci.yml`）。試験環境のプロジェクト `dora/m4-hello-app` に push して使う |

## 手順

1. 鍵とトークン（画面やログに出さない。ファイルへ直接）:
   - デプロイの鍵: `ssh-keygen -t ed25519 -N "" -f .local/m4/deploy-ssh-key`、公開鍵を `.local/m4/forge-deploy.pub` に写す。
   - アプリの取得用: プロジェクトの Reporter（例 `bob`）に `forge admin token issue --user bob --scopes read_repository --expires-in 30d`。
     出力は表の形なので、`fgp_…` の 1 行だけを `.local/m4/hello-app-read.token` に置く（`grep -oE 'fgp_[A-Za-z0-9_]+'`）。
   - Runner: `forge admin runner register --name docker-test-m4 --tags linux,podman,m4 --admin dora`。`fgr_…` の 1 行だけを
     `.local/m4/runner/token` に置く。**出力をそのままリダイレクトすると表の全体が入り、Runner は FG-SYS-0002 で拒む**
     （`ops/runner/README.md` の手順 1 も同じ注意が要る）。`runner/config.toml` は `ops/runner/config.example.toml` と同じ。
2. 起動（`GS_M4_REPO_URL` はターゲットから見たリポジトリ、`http://host.docker.internal:28080/<namespace>/<project>.git`）:

   ```sh
   GS_M4_LOCAL=<.local/m4 の絶対パス> GS_M4_REPO_URL=… \
     docker compose -p gs-m3-test -f ops/test-env/compose.m4.yml up -d --build deploy-target runner-m4
   docker compose -p gs-m3-test -f ops/test-env/compose.m4.yml run --rm --no-deps runner-m4 check
   ```

   他のサービスは「orphan」と警告されるが、`--remove-orphans` は**付けない**。`check` は
   `ok: configuration, token, rootless podman, job isolation`。runner-m4 のイメージのビルドは重い（cargo）。cargo の枠のロックを取る。
3. host key: コンテナのログの最初の行が指紋（`ssh-keygen -lf` の出力）。`docker exec … cat /etc/ssh/keys/ssh_host_ed25519_key.pub`
   の base64 をターゲットの `host_key` に登録し、応答の `host_key_fingerprint` がログの指紋と同じことを確かめる。
4. forwarder（PowerShell、切り離して起動）:
   `Start-Process node -ArgumentList '<.local/m4>\deploy-forwarder.mjs','<vEthernet の IPv4>','22022','22022' -WindowStyle Hidden -RedirectStandardOutput <.local/m4>\forwarder.log`。
   PC の再起動で vEthernet のアドレスが変わったら、ターゲットの `host` と `FORGE_DEPLOY_ALLOWED_NETWORKS` も直す。
5. **`[deploy]` は forge.toml に書かない**（`test-env.mjs up` が毎回 forge.toml を書き直す）。環境変数で渡す:
   `.local/m4/deploy.env`（`FORGE_DEPLOY_SSH_KEY_FILE`・`FORGE_DEPLOY_SSH_PROGRAM`・`FORGE_DEPLOY_ALLOWED_NETWORKS=<vEthernet>/20`）を
   `set -a; . .local/m4/deploy.env; set +a` してから serve と worker を止め、`node ops/test-env/test-env.mjs up`。
   **`deploy-testenv.sh` はこれをしない**ので、その後はデプロイが止まる（worker のログに `forge.worker.deploy_lane_off`）。
   再デプロイの後はこの手順 5 をもう一度行う。
6. **Windows の ssh の回避（試験専用、製品の修正まで）**: forge-deploy は `-o UserKnownHostsFile="<path>"` を渡す。Windows では
   引数の中の引用符が `\"` にされ、ssh が `command-line line 0: invalid quotes`（終了 255）で止まる。forge はこれを FG-DEPLOY-0003
   `reason=connection_lost` と記録する（ssh の stderr はログに出ないので、forwarder と sshd に接続の跡が無いことで見分ける）。
   また Windows の OpenSSH（System32）は環境変数 `ProgramData` が無いと何も言わずに 255 で終わる（forge は PATH と SystemRoot しか
   渡さない）。どちらも Linux には無い。回避: `.local/m4/sshwrap.ps1` で `.local/m4/sshwrap.exe` を作り（`\"` を `"` に戻して Git の
   `usr/bin/ssh.exe` を呼び、終了コードと stderr を `.local/m4/sshwrap.log` に残す）、`FORGE_DEPLOY_SSH_PROGRAM` に指定する。
7. プロジェクトの設定（`dora` の `admin` 付きトークン。自動マージだけは Web のセッション）:
   - `POST …/protected-refs {"pattern":"refs/heads/main","push_min_role":"maintainer"}`、
     `PUT …/protected-refs/<id>/merge-rule {"required_approvals":0,"required_checks":["ci/pipeline"],"require_resolved_discussions":false,"direct_push_forbidden":true,"require_work_item_link":false,"expected_updated_at":null}`
   - `PUT …/deploy/environments/stg {"requires_approval":false,"auto_rollback":true,"auto_deploy_ref":"refs/heads/main"}`
   - `PUT …/deploy/environments/stg/targets/app-1 {"host":"<vEthernet>","port":22022,"user":"deploy","host_key_type":"ssh-ed25519","host_key":"<base64>","script":"hello-app"}`
   - `PUT …/auto-merge {"mode":"full"}` は Owner の **Web のセッション**だけ（トークンは FG-AUTH-0002 `session_required`）。
     Playwright で Keycloak にサインインし、`GET /api/v1/session` の `csrf_token` を `X-GitStation-CSRF` に付けて送る。
   - Git Bash から JSON に日本語を入れて curl すると文字化けで FG-WEB-0006 `json_syntax` になる。UTF-8 のファイルから `--data-binary @file` で送る。
     Git Bash で `/api/…` を引数に渡すときは `MSYS_NO_PATHCONV=1`。

## Verify

- MR を作る → MR のパイプライン → キューのパイプライン → 自動でマージ → `main` の push のパイプライン（`protected = t`）→
  worker のログ `forge.deploy.requested`（`requested_by` system）→ `SELECT status FROM forge.deployments WHERE id = …` が `succeeded`、
  ターゲットの `/srv/hello-app/deploy.log` に `healthy`。
- `/health` を壊す変更（CI は通る）: `rolled_back`、`error_code = FG-DEPLOY-0005`、`failed_target = app-1`。ターゲットは
  `previous_sha` に戻り、`health <previous_sha>` が `healthy`。
- その環境の最初のデプロイが失敗すると前の版が無いので `rollback_failed`（FG-DEPLOY-0006 `no_previous_version`、alert）。仕様。

## 片づけ

`docker compose -p gs-m3-test -f ops/test-env/compose.m4.yml rm -s -f deploy-target runner-m4`（volume も消すなら
`docker volume rm gs-m3-test_deploy-target-hostkey gs-m3-test_deploy-target-srv gs-m3-test_runner-m4-podman-store gs-m3-test_runner-m4-work`）、
forwarder を止める（`.local/m4/forwarder.pid`）、`forge admin runner disable <id> --admin dora`、トークンを `forge admin token revoke <id>`。

## Escalate

- FG-DEPLOY-0004（host key の不一致）が出たら、ターゲットの volume を作り直していないか確かめる。作り直したなら手順 3 で登録し直す。
- Windows の回避が要らなくなったか（製品の修正）は、`FORGE_DEPLOY_SSH_PROGRAM` を外して 1 回デプロイして確かめる。
