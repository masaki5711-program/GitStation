# 引き継ぎ: 計画 18 S9・S10 の後のマージの負荷試験（2026-11-28）

計画 18 §S10 の証明「`a-burst` で Windows ≥ 400/分、1 件あたり `lock_wait_ms` + `land_ms` ≤ 15 ms」を、この PC で測った。
`merge_queue.speculation_depth` を 1 と 2 で 1 回ずつ。**製品のコードは変えていない**。負荷試験の道具は 1 か所だけ直した（§4）。データはすべて合成。

結論: **どちらの深さも目標に届かない**。捌く速さは 63.3/分（深さ 1）と 56.3/分（深さ 2）。1 件あたりの `lock_wait_ms` + `land_ms` は 252 / 273 ms で、目標の 15 ms の約 17 倍。
MR の消失・順の入れ替わり・`main` の破損・クラッシュは**無い**（§3）。

## 0. 条件

| 項目 | 値 |
|---|---|
| forge | worktree `D:/APP/GitStation-wt-mload`（`main` 13c60dc7、S9・S10 を含む）の release のビルド（`cargo build --release -p forge-cli`、`embed-ui` なし）。serve 1 つ、worker 1 つ |
| 待ち受け | `127.0.0.1:39181`（使い捨て。3000・28080・試験サーバーには触れていない） |
| DB | 開発の PostgreSQL `forge-dev-postgres-1`（127.0.0.1:55432、PG 16.14）に新しく作った DB `forge_mload`（`ops/db/bootstrap/02`・`03` → `forge db migrate`）。ロールは開発のクラスターの合成のパスワードのまま |
| 設定 | 既定のまま。違いは `[merge_queue] speculation_depth = 1` / `2` だけ（serve の起動ログ `forge.serve.merge_queue_started` で `speculation_depth` を確認）。`doc_batch_max` 32、`workers` 2、train lint on |
| 利用者 | 管理者 `ltadmin`（`forge admin bootstrap-admin --create`）のトークン 1 本 |
| 機械 | Windows 11 Pro、i9-14900K（論理 32）、メモリ 64 GB（空きは最小 17.8 GB）、git 2.53.0.windows.3、Node 25.9.0 |
| 実行 | `ops/loadtest/run-all.sh a-burst`（共有のロック `D:/APP/.cargo-slot.lock` を取り、rustc / nextest / vitest が無いのを待ってから）。2 回の間に serve と worker を止めて設定を替えて起動し直した |

## 1. 結果（`a-burst`、文書 200 本の MR を一度に作り、自動マージだけで流す）

時間は秒。「捌く速さ」は `summary.drain.per_min`（最後の作成の応答から最後のマージまで）。

| 項目 | 深さ 1（TAG `s10d1`） | 深さ 2（TAG `s10d2`） | 目標 |
|---|---|---|---|
| マージ | 200/200 | 200/200 | 200/200 |
| **捌く速さ** | **63.3/分**（188.6 s で 199 件） | **56.3/分**（212.3 s で 199 件） | S7 ≥ 200/分、S10 ≥ 400/分 |
| 全体（最初の作成〜最後のマージ） | 208 s、57.7/分 | 233.5 s、51.4/分 | — |
| 作成→キュー投入 p50 / p95 | 125.2 / 166.4 | 133.7 / 189.1 | S3: p95 ≤ 5 |
| 投入→マージ p50 / p95 | 15.8 / 40.8 | 18.3 / 42.5 | — |
| 作成→マージ p50 / p95 | 138.1 / 202.7 | 159.4 / 223.6 | — |
| MR の作成 p50 / p95（ms） | 2360 / 5983 | 2451 / 5787 | S8: p95 ≤ 100、503 なし |
| 作成の 503 | 0 | 0 | 0 |
| バッチ | 41 個、すべて landed（大きさ 1 が 13、32 が 3） | 36 個、すべて landed（1 が 12、32 が 2） | — |
| **1 件あたり `lock_wait_ms` + `land_ms`**（合計 / 件数） | **251.6 ms** | **272.8 ms** | S10: ≤ 15 ms |
| 同（バッチごと p50 / p95） | 351 / 750 ms | 407 / 749 ms | — |
| `lock_wait_ms` p50 / 最大 | 2 / 5 | 2 / 5 | — |
| `land_ms` 大きさ 1 / 32 の p50 | 529 / 5404 ms | 629 / 4570 ms | — |
| 着地の `git_spawns`（大きさ 1 / 32） | 8 / 101（≈ 1 件 3 回 + 5） | 8 / 101 | S10: ロックの下で起動なし |
| `build_ms` p50 / p95 | 1442 / 9973 | 1523 / 12153 | — |
| train lint `lint_ms` p50 / p95 | 168 / 248 | 209 / 1208 | — |
| 深さ 2 のバッチ（`merge_queue_batches.depth = 2`） | 0 | **0** | — |
| `verdict` / `integrity.ok` | ok / ok | ok / ok | — |

`lock_wait_ms` などは serve のログ `forge.merge_queue.batch_landed` / `batch_built` から集計した（§2 のスクリプト）。

## 2. コマンド

```sh
# worktree とビルド（共有のロックを取ってから、CARGO_INCREMENTAL=0）
git -C D:/APP/git_station/Git worktree add -b docs/ops-merge-load-after-s10 D:/APP/GitStation-wt-mload main
CARGO_TARGET_DIR=D:/APP/gs-target-mload CARGO_BUILD_JOBS=4 CARGO_INCREMENTAL=0 cargo build --release -p forge-cli

# DB（開発のクラスターの中に別の DB）
C="docker exec -i forge-dev-postgres-1 psql -U forge_dev -v ON_ERROR_STOP=1 -q"
$C -d postgres -v dbname=forge_mload -f - < ops/db/bootstrap/02-create-database.sql
$C -d forge_mload -f - < ops/db/bootstrap/03-database-objects.sql
forge --config forge-migrate.toml db migrate            # migrate_url = forge_migrator@127.0.0.1:55432/forge_mload
forge --config forge-d1.toml admin bootstrap-admin --user ltadmin --create --email ltadmin@example.invalid
forge --config forge-d1.toml admin token issue --user ltadmin \
  --scopes api,write_repository,read_repository,merge_execute,admin --expires-in 3d --name loadtest \
  | grep -oE 'fgp_[A-Za-z0-9_]+' > <scratch>/loadtest.token

# forge-dN.toml: [server] bind = "127.0.0.1:39181"、[database] url = forge_app@127.0.0.1:55432/forge_mload、
#   [storage] [api] [audit] [web] は ops/test-env/test-env.mjs と同じ形（鍵は使い捨て）、[merge_queue] speculation_depth = N
forge --config forge-dN.toml serve   # と worker（PowerShell の Start-Process、PID を記録して止めた）

# 測定（深さ 1 → TAG=s10d1、深さ 2 → TAG=s10d2）
export GS_LT_BASE=http://127.0.0.1:39181 GS_LT_USER=ltadmin GS_LT_TOKEN_FILE=<scratch>/loadtest.token \
  GS_LT_OUT=D:/APP/gs-mload-env/out GS_LT_WORK=D:/APP/gs-mload-env/w \
  GS_LT_PG_CONTAINER=forge-dev-postgres-1 GS_LT_PG_USER=forge_dev GS_LT_PG_DB=forge_mload
TAG=s10d1 bash ops/loadtest/run-all.sh a-burst    # run-all rc=0
TAG=s10d2 bash ops/loadtest/run-all.sh a-burst    # run-all rc=0

# 着地の時間: serve のログ（JSON）の forge.merge_queue.batch_landed を集計
#   1 件あたり = Σ(lock_wait_ms + land_ms) / Σ size
```

結果のファイル（リポジトリの外。作業のスクラッチに写しを残し、`D:/APP/gs-mload-env` は消した）: `a-burst-s10d1.json`・`a-burst-s10d2.json`・`samples-*.jsonl`。数字はこの文書の表がすべて。

## 3. 正しさ（バグは見つからない）

- 両方の回で 200/200 がマージ、`main` に無い改訂 0、衝突の印 0、`main` の first-parent 201 コミットで `forge/doc-lint` が success でないもの 0（`missing` 160 / 165 は列車の途中の段で、README の通り欠陥ではない）。
- 順: 着地したバッチの終わりの時刻はバッチの id の順（入れ替わり 0）。古いエントリーが新しいエントリーより後のバッチに入ったもの 0。前のバッチが終わる前に作られたバッチ 0。
- panic・プロセスの停止なし。

## 4. 道具の修正（1 か所）

`ops/loadtest/merge-load.mjs` の DB の読み取りは `docker exec … psql -U postgres` に固定だった。開発の DB（`ops/dev/compose.yml`）の
superuser は `forge_dev` で `postgres` のロールが無いため、環境変数 `GS_LT_PG_USER`（既定 `postgres`、前と同じ）を足した。README に 1 行。

## 5. 見つかったこと

| # | 重さ | 内容 | 証跡 |
|---|---|---|---|
| M1 | 目標未達 | **捌く速さを決めているのは着地ではなく、キューに入るまで**。作成→投入 p50 125〜134 s に対し投入→マージ p50 16〜18 s。キューの生きたエントリーは多くの時間 0〜3 件で、列車は小さい（大きさ 1 が 13 / 12 個）。自動マージは 185 / 177 本の MR で「`forge/doc-lint` を head でもう一度」を頼んでいる（`forge.auto_merge.doc_lint_requested`、`errored=false`） | serve のログ、`summary.create_to_enqueue_s` |
| M2 | 目標未達（S10） | **着地の区間に git の起動が残る**: 大きさ 32 で `git_spawns` 101（1 件 ≈ 3 回）、1 件あたり 252 / 273 ms（目標 ≤ 15 ms）。`lock_wait_ms` は 2 ms で、待ちではなく区間の中の仕事。2026-11-28 の進捗の handoff の「S10: 着地の区間に 1 件あたり 2〜3 の git の起動が残る」と一致 | `forge.merge_queue.batch_landed` |
| M3 | 計画とのずれ | **文書のキューでは深さ 2 に効き目が無い**: 深さ 2 のバッチは 1 つもできない（`depth` はすべて 1）。`forge-core/src/config/merge_queue.rs` の説明の通り「文書のキューは組み立てたその回で着地するので、どの深さでも投機するものが無い」。計画 18 §S9・§S10 の「doc のキューは深さ 2 で組み立てを着地と並べる」（上限 ≈ 500/分の見積もりの前提）は実装されていない。深さ 2 の数字が少し悪いのは揺れの範囲と見る | `merge_queue_batches` の `depth`、serve の起動ログ |
| M4 | 目標未達（S8） | MR の作成 p95 ≈ 6 s（同時 32）。深さ 2 の回では、作成の中の「文書の改訂の記録」が 58 回 5 s で打ち切り（`forge.doc_revision.unavailable`、`FG-WEB-0002 operation=doc_revision timeout_ms=5000`、「one-revision rule is skipped this time」）。深さ 1 の回でも p95 は同じ ≈ 6 s | serve のログ、request_id `5a411820abda458183206bad19c9480b` |
| M5 | 注意 | 深さ 1 の回で、自動マージが deadlock（`FG-DB-0002 reason=deadlock`、`projects.update.lock` と `merge_queue.queue`）で `FG-REVIEW-0028`（alert の符号）を ERROR で 2 回。どちらの MR も後でマージされた（再試行で回復）。worker でも `review.merge_result_record` の deadlock が 5 回（retryable、回復） | request_id `110f2f96f78a4933bc168892ee7b78a9`、`ad7b89254ddf4e4b80c9b4fdd7f83559` |

どれも直していない（依頼の範囲は測定）。M1〜M3 が S10 の目標に届かない理由で、次の手は計画 18 の Q レーンと P レーンで決める。

## 6. 片づけ

- serve / worker は記録した PID だけを止めた。DB `forge_mload` は開発のクラスターに残している（不要なら `DROP DATABASE forge_mload`）。
- `D:/APP/gs-mload-env`（storage、作業ディレクトリ、結果）と `D:/APP/gs-target-mload` は消した。トークンはその DB の中だけのもの。
