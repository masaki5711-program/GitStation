# ops/loadtest — マージの負荷試験（合成データのみ）

試験環境（runbook `m3-test-environment` / `m4-test-environment`）だけで使う。本番・Kanflow・ほかの compose には触れない。
結果の記録: `docs/handoff/2026-11-17-merge-load-test.md`。

| ファイル | 中身 |
|---|---|
| `merge-load.mjs` | シナリオ `a`（文書 N 本を別々のファイルで MR、バースト投入）、`b`（同じ文書への MR、重なる/重ならない）、`c`（小さな CI + マージキュー、組み合わせでだけ壊れる MR を含む）、`d`（並列の push と clone の時間）、`sample`（forge の CPU・メモリ、PG の接続数、ジョブの滞留、空きメモリを 10 秒ごと） |
| `run-all.sh` | 共有のロック `D:/APP/.cargo-slot.lock` を取り（終了時に必ず外す）、rustc / nextest / vitest が無いのを待ち、サンプラーを動かして a→b→c→d を 1 回ずつ。空きメモリが下限（既定 3 GB）を切るとサンプラーが `STOP` を書き、次の段で止まる。失敗したシナリオは再実行しない |
| `lib.mjs` | API と git の呼び出し（トークンは `Authorization` ヘッダーだけ。URL・`.git/config`・出力に出さない） |
| `docs-fixture.mjs` | 計画 13 形式の合成の文書型 `lt.procedure` と文書 |

## 使い方（Git Bash）

```sh
# トークンは管理者が発行し、fgp_ の 1 行だけをファイルへ（画面に出さない）
forge --config ops/test-env/.local/forge.toml admin token issue --user <admin> \
  --scopes api,write_repository,read_repository,merge_execute,admin --expires-in 3d --name loadtest \
  | grep -oE 'fgp_[A-Za-z0-9_]+' > <リポジトリの外>/loadtest.token
export GS_LT_TOKEN_FILE=<…>/loadtest.token GS_LT_OUT=<…>/out GS_LT_WORK=<…>/w
export GS_LT_HTTPS_BASE=https://<LAN の名前> GS_LT_CA_FILE=<…>/lan-ca.crt   # d の https の clone（任意）
TAG=run1 bash ops/loadtest/run-all.sh          # 全部。1 つだけなら: bash ops/loadtest/run-all.sh c
```

- プロジェクトはグループ `loadtest/` に `docs-a-<TAG>`・`docs-b-<TAG>`・`sys-c-<TAG>`・`push-d-<TAG>` として新しく作る（同じ TAG は使えない）。
  `main` は保護（直接の push 禁止）し、必須チェックは文書が `forge/doc-lint`、システムが `ci/pipeline`。自動マージは新しいプロジェクトの既定（`full`）のまま。
- 結果: `$GS_LT_OUT/<a|b|c|d>-<TAG>.json`（集計 `summary` と DB から読んだキューの事実 `facts`）、`samples-<TAG>.jsonl`。
- DB は `docker exec <gs-test-pg> psql` で**読むだけ**。書き込みはすべて API と git から。
- c の CI はジョブ 1 つ（tags `linux, m4`、alpine をダイジェストで固定）。試験環境の Runner `docker-test-m4` 1 台だけが取る。
- 片づけ: プロジェクトの削除は Owner が画面か `DELETE /api/v1/projects/{id}` で。トークンは `forge admin token revoke <id>`。
