# ops/loadtest — マージの負荷試験（合成データのみ）

試験環境（runbook `m3-test-environment` / `m4-test-environment`）と、本番と同じ構成の Linux の試験ホストだけで使う。本番・Kanflow・ほかの compose には触れない。
結果の記録: `docs/handoff/2026-11-17-merge-load-test.md`。計画: `docs/plan/18-merge-throughput.md`（S0 の測定）。

| ファイル | 中身 |
|---|---|
| `merge-load.mjs` | シナリオ（下の表）と `sample`（forge の CPU・メモリ、PG の接続数、ジョブの滞留、空きメモリを 10 秒ごと） |
| `run-all.sh` | 共有のロックを取り（自分が取ったときだけ、終了時に必ず外す）、rustc / nextest / vitest が無いのを待ち、サンプラーを動かして、渡した名前を 1 回ずつ（既定は a→b→c→d）。知らない名前はロックを取る前に断る。空きメモリが下限（既定 3 GB）を切るとサンプラーが `STOP` を書き、次の段で止まる。失敗したシナリオは再実行しない |
| `lib.mjs` | API と git の呼び出し（トークンは `Authorization` ヘッダーだけ。URL・`.git/config`・出力に出さない）。手元のリポジトリだけを触る git（`localGit`）はトークンを持たない |
| `docs-fixture.mjs` | 計画 13 形式の合成の文書型 `lt.procedure` と文書 |

## シナリオ

| 名前 | 中身 | 主な結果 |
|---|---|---|
| `a` | 文書 N 本を別々のファイルで MR、波（`--burst` 本ずつ、`--gap` 秒おき）で投入 | 作成→マージの p50/p95、スループット |
| `a-burst` | 文書 200 本の MR を**一度に**作る（波なし、人のエンキューなし。自動マージだけで流す） | `summary.drain`（最後の作成から最後のマージまでのマージ数/分）、`create_to_merged_s` の p50/p95、`span_s`（全体）、`create_ms`（作成の p95。作成の失敗は `error_codes` の `create:<コード>`） |
| `b` | 同じ文書への MR（重なる / 重ならない） | 競合の扱い |
| `c` | 小さな CI + マージキュー、組み合わせでだけ壊れる MR を含む | 列車の本数、原因の特定 |
| `d` | 並列の push と clone の時間 | push / clone の時間（マージはしない） |
| `e` | 長時間: `--count` 本のマージを `--rate` 本/分で流し、`--burst-every` 分ごとの 1 分だけ `--burst` 本にする。滞留（マージされていない MR、キューの深さ、実行できるのに取られていないジョブ）、DB の大きさ、リポジトリの大きさを `--sample-every` 秒ごとに採る | `backlog`（滞留が伸びるか: `grows`）、`growth`（マージ 1 件あたりの DB / リポジトリの増分、表ごとの大きさ）、`summary`、`revisions_missing_on_main` |
| `spawn-bench` | **サーバーもトークンも使わない。** `GS_LT_WORK` の下に使い捨てのリポジトリを作り、`git rev-parse HEAD` を 100 回、forge の試しマージと同じ `git merge-tree --write-tree --name-only -z <main> <枝>` を 100 回、順に動かす | 1 回あたりの p50/p95（ms）。計画 18 §2.2 の E1（プロセスの起動）と E2（merge-tree の仕事 = merge-tree の p50 − rev-parse の p50） |

どのシナリオの結果にも、バッチごとの時間 `summary.batches.duration`（`merge_queue_batches` の作成〜終了。全体、状態ごと、着地したバッチの大きさごと、1 件あたり、次のバッチまでの間）が入る。

### `main` の整合の検査（`a`・`a-burst`・`b`・`c`・`e` の結果の `integrity`）

- `main` の木に競合の印（`<<<<<<<` など）のあるファイルがない。
- `main` の first-parent のコミットに、成功していない push パイプラインがない。
- 文書のシナリオ: `forge/doc-lint` が **`main` の first-parent のコミットすべて**で `success`（先頭だけではない）。自分のチェックの無いコミットは `missing` に数える（`missing_merged_head_success` は、取り込んだ枝の先頭が `success` だった数）。列車の途中のコミットは、列車の doc-lint（計画 18 S1）が入るまで `missing` になりうる。
- `integrity.ok` が全部の結論。`d` はマージをしないので検査しない。

## 使い方（Git Bash）

```sh
# トークンは管理者が発行し、fgp_ の 1 行だけをファイルへ（画面に出さない）
forge --config ops/test-env/.local/forge.toml admin token issue --user <admin> \
  --scopes api,write_repository,read_repository,merge_execute,admin --expires-in 3d --name loadtest \
  | grep -oE 'fgp_[A-Za-z0-9_]+' > <リポジトリの外>/loadtest.token
export GS_LT_TOKEN_FILE=<…>/loadtest.token GS_LT_OUT=<…>/out GS_LT_WORK=<…>/w
export GS_LT_HTTPS_BASE=https://<LAN の名前> GS_LT_CA_FILE=<…>/lan-ca.crt   # d の https の clone（任意）
export GS_LT_STORAGE_ROOT=<forge.toml の storage.root>                      # e のリポジトリの大きさ（任意）
TAG=run1 bash ops/loadtest/run-all.sh          # a→b→c→d。1 つだけなら: bash ops/loadtest/run-all.sh c
TAG=s0 bash ops/loadtest/run-all.sh a a-burst c   # 計画 18 S0 の測定
TAG=s0 bash ops/loadtest/run-all.sh e-rehearsal   # e の短い練習: 200 本、約 6 分 + 流し終わるまで
TAG=s0 bash ops/loadtest/run-all.sh e             # e の本番: 10,000 本、約 7 時間
```

`spawn-bench` はトークンが要らない（`GS_LT_OUT` と `GS_LT_WORK` だけ）:

```sh
GS_LT_OUT=<…>/out GS_LT_WORK=<…>/w TAG=s0 bash ops/loadtest/run-all.sh spawn-bench
# ロックを取らずに直接:
GS_LT_OUT=<…>/out GS_LT_WORK=<…>/w node ops/loadtest/merge-load.mjs spawn-bench --n 100 --tag s0
```

- プロジェクトはグループ `loadtest/` に `docs-a-<TAG>`・`docs-ab-<TAG>`（a-burst）・`docs-b-<TAG>`・`sys-c-<TAG>`・`push-d-<TAG>`・`docs-e-<TAG>` として新しく作る（同じ TAG は使えない。`run-all.sh` の `e-rehearsal` は TAG に `s` を足す）。
  `main` は保護（直接の push 禁止）し、必須チェックは文書が `forge/doc-lint`、システムが `ci/pipeline`。自動マージは新しいプロジェクトの既定（`full`）のまま。
- 結果: `$GS_LT_OUT/<シナリオ>-<TAG>.json`（集計 `summary` と DB から読んだキューの事実 `facts`）、`samples-<TAG>.jsonl`、`e` は `e-<TAG>-samples.jsonl` も。
- DB は `psql` で**読むだけ**（セッションを `default_transaction_read_only=on` にする。書き込みは PostgreSQL が断る）。書き込みはすべて API と git から。
- c の CI はジョブ 1 つ（tags `linux, m4`、alpine をダイジェストで固定）。試験環境の Runner `docker-test-m4` 1 台だけが取る。
- 片づけ: プロジェクトの削除は Owner が画面か `DELETE /api/v1/projects/{id}` で。トークンは `forge admin token revoke <id>`。

### `e`（長時間）の引数

| 引数 | 既定 | 意味 |
|---|---|---|
| `--count` | 10000 | 作る MR の数 |
| `--rate` | 21 | ふつうの 1 分に作る数（1 分の中で等間隔） |
| `--burst` | 100 | バーストの 1 分に作る数（0 でバーストなし） |
| `--burst-every` | 30 | 何分ごとにバーストの 1 分にするか |
| `--duration` | 0 | 投入を打ち切る分（0 は `--count` まで） |
| `--docs` | 500 | 合成の文書の数（`--rate`・`--burst` 以上。MR k は文書 `((k−1) mod docs)+1` の手順を 1 つ書き換える） |
| `--sample-every` / `--repo-size-every` | 30 / 300 | 滞留と DB の大きさ / リポジトリの大きさを採る間隔（秒） |
| `--timeout` | 3600 | 投入の後、流し終わるのを待つ上限（秒） |

- 既定の 10,000 本は約 7 時間（30 分ごとに 29×21 + 100 = 709 本）。練習と本番は同じコードで、引数だけが違う。
- 1 分ごとに: `main` を fetch → その分の文書をいまの `main` から読む → 枝をまとめて作って push → MR を等間隔に作る。push に失敗した分は飛ばして記録し、5 回続いたら止める。
- `backlog.grows`: 投入の間の標本で、最後の 1/4 の平均 > 1.5 × 2 番目の 1/4 の平均 + 1 分の投入数（`--rate`）なら `true`。標本が 8 つ未満なら `null`。練習（`e-rehearsal`）はコードの確認で、この判定は本番の長さで読む。
- 止め方: `$GS_LT_OUT/STOP` を置く（空きメモリが `GS_LT_MIN_FREE_GB` を切ると自分でも書く）。新しい MR を作るのをやめ、そこまでの結果を `e-<TAG>.json` に書いて終了コード 3 で終わる。再開はしない。
- リポジトリの大きさは `GS_LT_STORAGE_ROOT`（forge の `storage.root`。このホストから読めるとき）を渡したときだけ採る。

## Linux のホストで動かす（本番と同じ構成: systemd の forge + ネイティブの PostgreSQL）

必要なもの: Node 20 以上、git、`psql`。リポジトリの作業ツリー（`ops/loadtest/` だけあればよい）。

```sh
# 1) spawn-bench だけなら forge もトークンも要らない（E1 / E2 をそのホストで測る）
GS_LT_OUT=/var/tmp/gs-lt/out GS_LT_WORK=/var/tmp/gs-lt/w \
  node ops/loadtest/merge-load.mjs spawn-bench --n 100 --tag linux1

# 2) サーバーを使うシナリオ
export GS_LT_BASE=http://127.0.0.1:18080         # forge serve の待ち受け（例。別のホストからなら https://gitstation.example.internal や 192.0.2.10）
export GS_LT_USER=<トークンの持ち主>
export GS_LT_TOKEN_FILE=/var/tmp/gs-lt/loadtest.token GS_LT_OUT=/var/tmp/gs-lt/out GS_LT_WORK=/var/tmp/gs-lt/w
export GS_LT_PSQL_DIRECT=1 GS_LT_PG_DB=forge     # docker exec の代わりに、このホストの psql
export PGHOST=127.0.0.1 PGPORT=55433 PGUSER=<読み取りのできるロール> PGPASSFILE=<…>/pgpass   # パスワードは pgpass に。引数や環境変数に書かない
export GS_LT_STORAGE_ROOT=/var/lib/forge/repositories   # e のリポジトリの大きさ（読める利用者で動かすとき）
TAG=linux1 bash ops/loadtest/run-all.sh a a-burst c spawn-bench
TAG=linux1 bash ops/loadtest/run-all.sh e-rehearsal
TAG=linux1 bash ops/loadtest/run-all.sh e
```

- Linux では `run-all.sh` のロックの既定は `${TMPDIR:-/tmp}/gs-loadtest.lock`（`GS_LT_LOCK` で変えられる）。`sample` は `/proc` から forge のプロセスを読む。
- `PGUSER` のロールは `forge` スキーマの表と `pg_database_size` を読めること（読み取りだけでよい。セッションは読み取り専用にする）。
- HTTPS で社内 CA のとき: `NODE_EXTRA_CA_CERTS=<ca.pem>`（API）と `GIT_SSL_CAINFO=<ca.pem>`（git）。

## ログの項目（計画 18 S0）

負荷試験の数字と突き合わせる、forge 側のログ:

| イベント | 項目 |
|---|---|
| `forge.merge_queue.batch_built`（info） | `build_ms`（列車を組む時間）、`git_spawns`（その間に起動した git の数） |
| `forge.merge_queue.batch_landed`（info） | `lock_wait_ms`（マージのロックを待った時間）、`land_ms`（ロックを取ってから着地を記録し終わるまで）、`lint_ms`（列車の lint。S1 が入るまで 0）、`git_spawns`（ロックを頼んでからの git の数） |
| `forge.jobs.succeeded` / `forge.jobs.retry_scheduled`（**debug**） | `ready_to_claim_ms`（ジョブが実行できるようになった `run_after` から claim まで。DB の時計） |

- `ready_to_claim_ms` は debug の行に載る。測るときは forge を `RUST_LOG=info,forge_db=debug` で動かす（既定の `info` では出ない）。
- ジョブの claim そのものの時間（キューに約 1,000 行あるとき）は Rust の計測で測る（claim は更新なので、読み取りだけの負荷試験からは測れない）。開発用の PostgreSQL を使う:

```sh
cargo nextest run -p forge-db --run-ignored only -E 'test(claim_bench_1000_queued_rows)' --no-capture
# → BENCH jobs.claim queued≈1000 claims=100 busy_keys=8  p50 … ms  p95 … ms  max … ms  mean … ms
```
