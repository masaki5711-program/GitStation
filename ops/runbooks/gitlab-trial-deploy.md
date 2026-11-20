# Runbook: gitlab-trial-deploy（PFS 試験サーバーへの GitLab CI/CD 配置）

GitStation を PFS の試験サーバーに社内 GitLab の CI/CD（`build:image` → 手動の `deploy:pfs-trial`）で置く・更新する・戻す手順。
構成・初回の準備（runner、GitLab の設定、秘密、管理者）は **`ops/gitlab-trial/README.md`**。本番（TLS・systemd）は `linux-install` / `linux-upgrade`。

- 試験サーバーは PFS・bankmasalah などと共有。触れてよいのは名前が `gitstation-trial-` で始まるコンテナ・ボリューム・ネットワークと
  `/srv/gitstation` だけ。
- 配置（`deploy:pfs-trial` の ▶）と DB を戻す手順は `[MUTATING — needs approval]`。▶ を押せるのは保護環境 `trial-gitstation` の許可者だけ。
- 秘密の値はこの手順書・チャット・チケット・CI/CD 変数に書かない。`<…>` は置き換える印。

## Trigger

- `main` の新しいコミットを試験サーバーに出すとき（パイプラインの `deploy:pfs-trial`）。
- 配置した版に問題があり、前の版に戻すとき。
- `deploy:pfs-trial` / `build:image` のジョブが失敗したとき。

## Impact

- 配置の間（serve と worker を止めてから `/readyz` が 200 になるまで、数分）、試験サーバーの GitStation の Web・git は使えない。
- PFS ほかへの影響: 同じサーバーの CPU・メモリ（`compose.yml` の上限: DB 1 GiB、serve 1.5 GiB、worker 1 GiB）と、`build:image` の
  間のビルドの負荷（`CARGO_BUILD_JOBS=4`）。

## Diagnose（読み取りのみ）

1. ジョブのログの `[deploy …]` の行で、どの段で止まったかを見る（preflight → database → backup → stop → bootstrap → migrate → start → smoke）。
   失敗のときは `compose ps` と serve / worker のログの末尾 80 行が続けて出る。
2. サーバーで（`gitstation-ci` として）:

   ```sh
   cd /srv/gitstation
   tail -n 5 deployed.log
   # compose.yml needs IMAGE_TAG: the last deployed one (any value works for ps / logs)
   IMAGE_TAG="$(tail -n 1 deployed.log | cut -d' ' -f2)" docker compose -p gitstation-trial --env-file .env -f compose.yml ps -a
   docker logs --tail 100 gitstation-trial-serve 2>&1 | grep -E '"error_code"|"level":"(WARN|ERROR)"'
   docker exec gitstation-trial-serve forge doctor
   ```

3. よくある原因:

   | 症状 | 原因 | 見るもの |
   |---|---|---|
   | `image gitstation/forge:<sha> not found` | `build:image` が別の版を消した、または未実行 | そのパイプラインの `build:image` をやり直す |
   | `secret … is missing` / `must be owned by` | `setup-server.sh` 未実行、`oidc-client-secret` 未配置、権限 | `sudo bash setup-server.sh --check`（README 手順 2・3） |
   | `.env still has <placeholders>` | `.env` を編集していない | README 手順 2 |
   | serve が起動しない、`FG-SYS-0002` | 設定の誤り（`config_key` と `reason`） | runbook `config-invalid`。`web.base_url` の `insecure_http` は `forge.toml` の `web.allow_insecure_http` |
   | `FG-DB-0004 reason=db_ahead` | 新しいマイグレーションの入った DB で古い版を起動した | 下の「DB を戻す」 |
   | `FG-AUTH-0015`、ログインが戻ってこない | Keycloak に届かない、issuer の不一致、redirect URI の不一致 | runbook `oidc-login`、`.env` の `OIDC_ISSUER` / `PUBLIC_URL` |
   | `/healthz is not reachable from this host` | `PUBLISH_ADDR` の誤り、ファイアウォール、3306 の衝突 | `ss -ltnp` で 3306 を使うプロセス |

## Mitigate

### 配置する `[MUTATING — needs approval]`

GitLab の `main` のパイプラインで `build:image` が成功したのを確かめ、`deploy:pfs-trial` の ▶ を押す。`deploy.sh` は配置の前に
`backups/pre-deploy-<UTC>-<sha>.dump` を取る（新しい 10 個を残す）。ロールバック: 下の「前の版に戻す」。

### 前の版に戻す `[MUTATING — needs approval]`

戻したい版のパイプラインの `deploy:pfs-trial` をもう一度実行する（イメージが消えていれば、そのパイプラインの `build:image` を先に）。
間にマイグレーションが入っていなければこれで終わり。`FG-DB-0004 reason=db_ahead` で起動しなければ「DB を戻す」。

配置が失敗したときの `deploy.sh` の動き: その配置でマイグレーションが 1 つも適用されていなければ、前の版を起動し直す。
適用されていれば、前の版は新しいスキーマを拒む（`db_ahead`）ので**何も起動し直さず**（新しい版は動いたまま、または止まったまま）、
「直した版を出す（fix forward）か DB を戻す」とログに出す。どちらにするかは人が決める。

### DB を戻す（未検証） `[MUTATING — needs approval]`

> **未検証の手順。** 試験サーバーではまだ一度も実行して確かめていない。実行する前に人が中身を読み、結果を障害記録に残して
> この手順を直す。

**ダンプの後に作られた DB の行は失われる**（リポジトリのファイルは戻らない）。今の DB は消さずに名前を変えて残す。
サーバーで `gitstation-ci` として:

```sh
cd /srv/gitstation
C="docker compose -p gitstation-trial --env-file .env -f compose.yml"
IMAGE_TAG=<戻す先の短い sha> $C stop serve worker
ls -1t backups/                       # 戻す先の版を「入れる前」のダンプ = 次の版の配置のときに取ったもの（ファイル名の sha はその配置で入れた版）
docker exec -u postgres gitstation-trial-db psql -X -v ON_ERROR_STOP=1 -d postgres \
  -c "ALTER DATABASE forge RENAME TO forge_before_restore_$(date -u +%Y%m%d%H%M%S)"
docker run --rm --entrypoint cat gitstation/forge:<戻す先の短い sha> /usr/local/share/forge/db/bootstrap/02-create-database.sql \
  | docker exec -i -u postgres gitstation-trial-db psql -X -v ON_ERROR_STOP=1 -v dbname=forge -d postgres -f -
docker exec -i -u postgres gitstation-trial-db pg_restore --exit-on-error -d forge < backups/pre-deploy-<UTC>-<sha>.dump
```

その後、戻す先の版の `deploy:pfs-trial` を実行し、`forge doctor`（`db.bootstrap`、`database`）を確かめる。
ロールバック（戻しをやめる）: serve / worker を止め、`forge` を `DROP` ではなく別名に変え、`forge_before_restore_<UTC>` を `forge` に戻す。
残した `forge_before_restore_*` は、確かめ終わってから人が判断して消す（`DROP DATABASE`）。

この「DB を戻す」は試験サーバーではまだ実行して確かめていない。最初に使うときは結果を障害記録に残し、この手順を直す。

## Verify

- ジョブのログの最後が `[deploy …] done: <sha> is live on http://<PUBLISH_ADDR>:3306`。`deployed.log` に 1 行増えている。
- ブラウザで `PUBLIC_URL` を開き、Keycloak でログインできる（`gstn_3306_session` のクッキーができる）。
- `docker exec gitstation-trial-serve forge doctor` の FAIL が 0（`auth.oidc` は平文 HTTP の warn、`backup.*` は warn でよい）。
- 試しのリポジトリに `git clone` / `git push` できる（`http://<サーバー>:3306/<グループ>/<プロジェクト>.git`、パスワードはトークン）。

## Escalate

- サーバーのメモリ・ディスクが足りず、PFS ほかの動作に影響が出る: `compose.yml` の上限の見直し、配置の停止（PFS チームと）。
- runner・保護ブランチ・保護環境の設定が GitLab で変えられない: GitLab の管理者へ。
- 「DB を戻す」が `pg_restore` の誤りで止まる: 今の DB（別名で残した）に戻し、障害として記録する。
