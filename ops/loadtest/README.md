# ops/loadtest — マージの負荷試験（合成データのみ）

試験環境（runbook `m3-test-environment` / `m4-test-environment`）と、本番と同じ構成の Linux の試験ホストだけで使う。本番・Kanflow・ほかの compose には触れない。
結果の記録: `docs/handoff/2026-11-17-merge-load-test.md`。計画: `docs/plan/18-merge-throughput.md`（S0 の測定）。

| ファイル | 中身 |
|---|---|
| `merge-load.mjs` | シナリオ（下の表）と `sample`（forge の CPU・メモリ、PG の接続数、ジョブの滞留、空きメモリを 10 秒ごと） |
| `run-all.sh` | 共有のロックを取り、rustc / nextest / vitest が無いのを待ち、サンプラーを動かして、渡した名前を 1 回ずつ（既定は a→b→c→d）。知らない名前・誤った引数・誤った環境変数はロックを取る前に断る（終了コード 2）。空きメモリが下限（既定 3 GB）を切るとサンプラーが `STOP` を書き、次の段で止まる。失敗したシナリオは再実行しない。終わり方は下の「終了コード」 |
| `lib.mjs` | API と git の呼び出し（トークンは `Authorization` ヘッダーだけ。URL・`.git/config`・出力に出さない）。手元のリポジトリだけを触る git（`localGit`）はトークンを持たない |
| `docs-fixture.mjs` | 計画 13 形式の合成の文書型 `lt.procedure` と文書 |

## シナリオ

| 名前 | 中身 | 主な結果 |
|---|---|---|
| `a` | 文書 N 本を別々のファイルで MR、波（`--burst` 本ずつ、`--gap` 秒おき）で投入 | 作成→マージの p50/p95、スループット |
| `a-burst` | 文書 200 本の MR を**一度に**作る（波なし、人のエンキューなし。自動マージだけで流す） | `summary.drain`（最後の作成から最後のマージまでのマージ数/分）、`create_to_merged_s` の p50/p95、`span_s`（全体）、`create_ms`（作成の p95。作成の失敗は `error_codes` の `create:<コード>`） |
| `a-xref` | 単独では緑で、**組み合わせると参照が壊れる** MR の組を `--n` 組（既定 20）と、ふつうの改訂を同じ数。組 k は「文書 3k−2 を消す MR」と「文書 3k−1 から `[[LT-…]]` でそれを参照する MR」。全部を一度に作り、人もエンキューする（計画 18 S1 の列車の doc-lint の確認） | `broken_refs_on_main`（`main` の first-parent の**全コミット**を手元の git で読み、参照先の無い `[[LT-…]]` を数える。0 が合格）、`lint_removals`（`FG-CI-0032` で外れたエントリー。組ごとに 1 件）、`merged_by_kind` |
| `b` | 同じ文書への MR（重なる / 重ならない） | 競合の扱い |
| `c` | 小さな CI + マージキュー、組み合わせでだけ壊れる MR を含む | 列車の本数、原因の特定 |
| `d` | 並列の push と clone の時間 | push / clone の時間（マージはしない） |
| `e` | 長時間: `--count` 本のマージを `--rate` 本/分で流し、`--burst-every` 分ごとの 1 分だけ `--burst` 本にする。滞留（マージされていない MR、キューの深さ、実行できるのに取られていないジョブ）、DB の大きさ、リポジトリの大きさを `--sample-every` 秒ごとに採る | `backlog`（滞留が伸びるか: `grows`）、`growth`（マージ 1 件あたりの DB / リポジトリの増分、表ごとの大きさ）、`summary`、`revisions_missing_on_main` |
| `spawn-bench` | **サーバーもトークンも使わない。** `GS_LT_WORK` の下に使い捨てのリポジトリを作り、`git rev-parse HEAD` を 100 回、forge の試しマージと同じ `git merge-tree --write-tree --name-only -z <main> <枝>` を 100 回、順に動かす | 1 回あたりの p50/p95（ms）。計画 18 §2.2 の E1（プロセスの起動）と E2（merge-tree の仕事 = merge-tree の p50 − rev-parse の p50） |

どのシナリオの結果にも、バッチごとの時間 `summary.batches.duration`（`merge_queue_batches` の作成〜終了。全体、状態ごと、着地したバッチの大きさごと、1 件あたり、次のバッチまでの間）が入る。

### `main` の整合の検査（`a`・`a-burst`・`a-xref`・`b`・`c`・`e` の結果の `integrity`）

- `main` の木に競合の印のあるファイルがない。数えるのは `<<<<<<<` か `>>>>>>>` で始まる行のあるファイルだけ（`=======` だけの行は Markdown の見出しの下線でもあるので、それだけでは数えない）。
- `main` の first-parent のコミットに、成功していない push パイプラインがない。
- 文書のシナリオ: `main` の first-parent のコミットに、`success` でない `forge/doc-lint` のチェックがない（先頭だけでなく全部を見る）。**チェックがあって `success` でないものは必ず失敗**にする。
- 自分のチェックの無いコミットは `missing` に数えて、必ず結果に載せる（`missing_merged_head_success` は、取り込んだ枝の先頭が `success` だった数）。**既定では失敗にしない**（`GS_LT_DOC_LINT_MISSING_FATAL=1` で失敗にする）。計画 18 S1 の列車の doc-lint が入った後も既定は 0 のまま: 列車の lint がチェックを記録するのは**列車の最後の段のコミット（着地する木）だけ**で、しかも列車のどれかの MR が文書（`.md`）か文書型（`.forge/doctypes/**`）を変えたときだけ。列車の途中の段のコミットと、文書を変えない列車の先頭は、自分のチェックを持たないので `missing` のままになる（欠陥ではない）。途中のコミットまで含めた確認は `a-xref` の `broken_refs_on_main` がする。
- 最後のマージの直後でまだ動いているパイプラインとチェックは、最大 5 分待ってから判定する。待っても終わらないものは成功していないものに数える。
- `integrity.failures` が失敗の一覧、`integrity.ok` はそれが空であること。`d` はマージをしないので検査しない。

### 結論（`verdict`）と終了コード

マージをするシナリオ（`a`・`a-burst`・`a-xref`・`b`・`c`・`e`）は、**結果のファイルを書いてから**、結果を信じてよいかを `verdict`（`ok` と理由の一覧 `reasons`）に書き、終了コードで返す。理由は標準エラーにも `NOT OK: …` で出る。

| 終了コード | 意味 |
|---|---|
| 0 | 信じてよい |
| 1 | 失敗した、または終わっていない（キューが `--timeout` までに落ち着かない、`e` で API・git・DB が続けて答えない、例外）。結果のファイルが無いこともある |
| 2 | 引数か環境変数の誤り（トークンのファイルが無い・中身の形が違う、も含む）。何も始めていない（作業ディレクトリ `GS_LT_WORK` も作らず、通信もしない） |
| 3 | `STOP` で止めた |
| 4 | 最後まで動いて結果も書いたが、**合格として読んではいけない**（下の表のどれか） |

終了コード 4 になる理由:

| 理由 | 対象 |
|---|---|
| `integrity.failures` が空でない | 全部 |
| 作れなかった MR がある（1 本でも。`create:<コード>` の数を理由に載せる）。`e` は例外で終わった作成と、枝を push できずに飛ばした MR も | 全部 |
| マージされた改訂が `main` に無い | 全部 |
| シナリオの筋書きに無い「止まった MR」がある | 下の通り |

シナリオの筋書き（**止まるのが正しい MR は理由にしない**）:

- `a`・`a-burst`・`e`: 作った MR はすべてマージされる。
- `a-xref`: ふつうの改訂はすべてマージされ、列車の lint で外れない。組ごとに**ちょうど 1 本だけ**マージされ、もう 1 本は `FG-CI-0032`（`reason=doc_lint_failed`）で**ちょうど 1 回**キューから外れるのが正しい（先に着地したほうが残る。原因 1 つにつき 1 件）。2 本ともマージされた（= `main` に壊れた参照が入った）、外れた理由が違う、2 回以上外れた、は理由にする。`broken_refs_on_main.commits_with_broken_refs` が 0 でなければ、それも理由にする（列車の lint が切ってある `merge_queue.train_lint_budget_ms = 0` の環境では、ここで 4 になる）。
- `b`: 重ならない MR はすべてマージされる。重なる MR（どれも手順 1 を書き換える）は**ちょうど 1 本だけ**マージされ、残りは競合で止まるのが正しい（基準の測定では 30 本のうち 16 本がマージ、14 本が停止）。0 本や 2 本以上なら理由にする。
- `c`: 仕込んだ 3 組（5/8、17/19、30/33）は、組ごとに**ちょうど 1 本だけ**マージされ、もう 1 本がキューから外れるのが正しい（基準は 40 本のうち 37 本）。それ以外の MR はすべてマージされる。`main` の木で `tests/check.sh` が失敗したら理由にする。
- `d` はマージをしないので `verdict` を持たない（push の失敗は測る対象そのもの。結果の `push_failures`）。

作成の 503（`FG-GIT-0004`）が残っている間（計画 18 S3 の前）は、再試行なしの `a`・`a-burst` は 4 になる。数字は結果に入っているので測定としては読めるが、合格ではない。

`run-all.sh` の終了コード: 0 = 全部信じてよい。2 = 引数・環境変数の誤り（ロックを取る前）。3 = `STOP`。4 = 全部最後まで動いたが、4 を返したシナリオがある（**4 のシナリオの後も次のシナリオへ進む**。プロジェクトが別で、結果は書かれているため）。129 / 130 / 143 = シグナル（HUP / INT / TERM）。それ以外 = 失敗したシナリオの終了コード（そこで止める）。
シグナルを受けたら、動いているシナリオとサンプラーを止めてから終わる（次のシナリオへは進まない）。ロックは、この実行が取り、かつ中身がこの実行の印のままのときだけ外す（他人のロックや、置き換えられたロックは残す）。
サンプラーが 30 回続けて採れないとき（約 5 分。DB が答えない等）は、空きメモリの下限を見張れないので `STOP` を書いて終わる。サンプラーが `STOP` を書かずに消えたときも、`run-all.sh` は次の段で止まる（終了コード 1）。

### 引数と環境変数の検査

- `--tag`（`run-all.sh` の `TAG`）と `sample` の `--file` は `^[A-Za-z0-9._-]+$` だけ。ファイル・ディレクトリ・プロジェクトの名前になるため、`/` や `..` を含む値は断る。`spawn-bench` が使い捨てのディレクトリを消す前には、消す先が `GS_LT_WORK` の中にあることも確かめる。
- 数の引数（`--n`・`--burst`・`--gap`・`--person-from`・`--timeout`・`--retry-busy`・`e` の全部・`spawn-bench` の `--docs`）は整数で、下限がある（`--timeout abc` や `--n 0` は断る）。シナリオに無い `--名前` も断る。
- `GS_LT_MIN_FREE_GB` は 0 以上の数、`GS_LT_PSQL_DIRECT` と `GS_LT_DOC_LINT_MISSING_FATAL` は `0` か `1`。
- `GS_LT_CONCURRENCY`（既定 32、1〜1024 の整数）: 一斉作成（`a` の波・`a-burst`・`a-xref`）で同時に開く API 呼び出しの数。負荷試験の道具は 1 台の端末なので、forge の `server.max_connections_per_peer`（既定 64）を超えた接続は、要求を読む前に断られる（503 `FG-WEB-0008`、または接続が閉じられて `TypeError`）。200 人が 200 台から出す一斉作成は 200 の端末なので、この上限には当たらない。キューから見れば、32 本ずつでも一斉に届く。上限を確かめたいときだけ、`GS_LT_CONCURRENCY` を 64 より大きくする。
- `GS_LT_PSQL_DIRECT=1` のときは **`PGHOST` と `PGPORT` を必ず明示する**。無いと `psql` はそのホストの既定のソケットがつながる PostgreSQL（別のシステムの DB かもしれない）を読むので、始める前に断る。
- 検査だけをする: `node ops/loadtest/merge-load.mjs <シナリオ> … --preflight`（合っていれば 0、誤りは 2。何も始めない。`run-all.sh` はロックを取る前に全部のシナリオでこれを通す）。

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
TAG=s1 bash ops/loadtest/run-all.sh a-xref        # 計画 18 S1: 組み合わせで壊れる参照が main に入らないこと
TAG=s0 bash ops/loadtest/run-all.sh e-rehearsal   # e の短い練習: 200 本、約 6 分 + 流し終わるまで
TAG=s0 bash ops/loadtest/run-all.sh e             # e の本番: 10,000 本、約 7 時間
```

`spawn-bench` はトークンが要らない（`GS_LT_OUT` と `GS_LT_WORK` だけ）:

```sh
GS_LT_OUT=<…>/out GS_LT_WORK=<…>/w TAG=s0 bash ops/loadtest/run-all.sh spawn-bench
# ロックを取らずに直接:
GS_LT_OUT=<…>/out GS_LT_WORK=<…>/w node ops/loadtest/merge-load.mjs spawn-bench --n 100 --tag s0
```

- プロジェクトはグループ `loadtest/` に `docs-a-<TAG>`・`docs-ab-<TAG>`（a-burst）・`docs-ax-<TAG>`（a-xref）・`docs-b-<TAG>`・`sys-c-<TAG>`・`push-d-<TAG>`・`docs-e-<TAG>` として新しく作る（同じ TAG は使えない。`run-all.sh` の `e-rehearsal` は TAG に `s` を足す）。
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
- 続けて失敗したら、時間いっぱい走らずにそこで止める（そこまでの結果を書いて終了コード 1）: MR の作成が 20 本続けて失敗、標本（滞留と DB の大きさ）が 10 回続けて採れない、流し終わりを待つ間の読み取りが 10 回続けて失敗。
- `backlog.grows`: 投入の間の標本で、最後の 1/4 の平均 > 1.5 × 2 番目の 1/4 の平均 + 1 分の投入数（`--rate`）なら `true`。標本が 8 つ未満なら `null`。練習（`e-rehearsal`）はコードの確認で、この判定は本番の長さで読む。
- 止め方: `$GS_LT_OUT/STOP` を置く（空きメモリが `GS_LT_MIN_FREE_GB` を切ると自分でも書く）。新しい MR を作るのをやめ、そこまでの結果を `e-<TAG>.json` に書いて終了コード 3 で終わる。再開はしない。
- 終わり方は上の「結論（`verdict`）と終了コード」の通り（作れなかった MR、マージされなかった MR、`main` に無い改訂、整合の失敗は 4）。
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
export PGHOST=127.0.0.1 PGPORT=55433 PGUSER=<読み取りのできるロール> PGPASSFILE=<…>/pgpass   # PGHOST と PGPORT は必須（無いと始めない）。パスワードは pgpass に。引数や環境変数に書かない
export GS_LT_STORAGE_ROOT=/var/lib/forge/repositories   # e のリポジトリの大きさ（読める利用者で動かすとき）
TAG=linux1 bash ops/loadtest/run-all.sh a a-burst c spawn-bench
TAG=linux1 bash ops/loadtest/run-all.sh e-rehearsal
TAG=linux1 bash ops/loadtest/run-all.sh e
```

- Linux では `run-all.sh` のロックの既定は、その利用者だけのディレクトリの `${XDG_RUNTIME_DIR:-$HOME/.cache}/gs-loadtest/slot.lock`（無ければ 700 で作る。だれでも書ける `/tmp` には置かない）。この既定は**同じ利用者の実行どうし**だけを順番にする。別の利用者のビルドやテストとも順番にしたいときは、両方が書ける共有の場所を `GS_LT_LOCK` で渡す。`sample` は `/proc` から forge のプロセスを読む。
- `GS_LT_PSQL_DIRECT=1` では `PGHOST` と `PGPORT` を明示する（上の「引数と環境変数の検査」）。
- `PGUSER` のロールは `forge` スキーマの表と `pg_database_size` を読めること（読み取りだけでよい。セッションは読み取り専用にする）。
- HTTPS で社内 CA のとき: `NODE_EXTRA_CA_CERTS=<ca.pem>`（API）と `GIT_SSL_CAINFO=<ca.pem>`（git）。

## ログの項目（計画 18 S0）

負荷試験の数字と突き合わせる、forge 側のログ:

| イベント | 項目 |
|---|---|
| `forge.merge_queue.batch_built`（info） | `build_ms`（列車を組む時間）、`git_spawns`（その間に起動した git の数） |
| `forge.merge_queue.batch_landed`（info） | `lock_wait_ms`（マージのロックを待った時間）、`land_ms`（ロックを取ってから着地を記録し終わるまで）、`lint_ms`（着地の前の列車の doc-lint にかかった時間。計画 18 S1 から実測。lint を切ってある・列車が文書を変えない・途中で止まった着地の引き継ぎ、では 0 かそれに近い値）、`git_spawns`（ロックを頼んでからの git の数） |
| `forge.merge_queue.entry_removed`（warn、`FG-CI-0032`） | 列車の doc-lint で外れた MR: `reason`（`doc_lint_failed` / `doc_lint_base_not_clean`）、`step`（列車の何段目か。原因の MR のときだけ）、`problems`（`rule:path:id` を 20 件まで）、`train_sha` |
| `forge.merge_queue.train_lint_unavailable`（warn、`FG-DOC-0002`） | 列車を lint できなかった: `scope=train`、`reason`（`train_budget` / `read_failed`）、`cause`、`bound`、`budget_ms` |
| `forge.jobs.succeeded` / `forge.jobs.retry_scheduled`（**debug**） | `ready_to_claim_ms`（ジョブが実行できるようになった `run_after` から claim まで。DB の時計） |

- `git_spawns` は、その処理を呼んだタスクが起動した git の数。別のタスクへ移した仕事（`tokio::spawn`・`spawn_blocking`）の git と、使い回している `cat-file --batch-command` が答えた読み取りは入らない（プロセスは起動したときに 1 回だけ数える）。git の仕事の**下限**として読む。
- `ready_to_claim_ms` は debug の行に載る。測るときは forge を `RUST_LOG=info,forge_db=debug` で動かす（既定の `info` では出ない）。
- ジョブの claim そのものの時間（キューに約 1,000 行あるとき）は Rust の計測で測る（claim は更新なので、読み取りだけの負荷試験からは測れない）。開発用の PostgreSQL を使う:

```sh
cargo nextest run -p forge-db --run-ignored only -E 'test(claim_bench_1000_queued_rows)' --no-capture
# → BENCH jobs.claim queued≈1000 claims=100 busy_keys=8  p50 … ms  p95 … ms  max … ms  mean … ms
```
