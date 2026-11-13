# Runbook: memory-sizing（本番サーバーのメモリの見積もりと絞り方）

GitStation の本番サーバー（`forge serve` + `forge worker` + git + PostgreSQL）を **2〜4 GB** に収めるための見積もりと設定。
clone / fetch を遅くしないことが前提。数字は `ops/bench/git-pack-memory.py` で測ったもの（下の「測定」）。

## Trigger

- サーバーを用意する・PFS と同居させる前に、必要なメモリを見積もるとき
- `forge-serve.service` / `forge-worker.service` が `MemoryMax` に当たった（`oom-kill`、`journalctl -u forge-serve` に `Killed`）、
  または大きなリポジトリの clone 中に git が `FG-GIT-0003`（`exit_code` が 137 / `signal 9`）で止まった
- swap が常に使われている（`free -m` の swap used が増え続ける、`vmstat 5` の `si` / `so` が 0 にならない）

## Impact

- メモリが足りないと、OOM killer が git（clone / push の途中）か forge 自体を止める。clone は `FG-GIT-0003`、push は
  途中で切れる（push の内容は quarantine にあるだけなので、リポジトリは壊れない。利用者はやり直せばよい）。
- swap に頼ると clone / fetch が数倍遅くなる。swap は「落ちないための保険」で、常用しない。

## 何がメモリを使うか

測った値（テスト環境）: `forge serve` と `forge worker` はそれぞれ **約 20 MB**。大きいのは次の 2 つ。

| 使う側 | 量の目安 | 決めるもの |
|---|---|---|
| git `pack-objects`（clone / fetch 1 本ごと、`upload-pack` の子） | 小さいリポジトリで数十 MB、大きいリポジトリで数百 MB〜1 GB | オブジェクト数（1 個あたり約 200 B）、差分の探索窓（`git.pack.*`）、パックの mmap |
| git `index-pack`（push 1 本ごと、`receive-pack` の子） | push の大きさ次第。差分の展開に `core.deltaBaseCacheLimit` | `git.pack.threads`、`git.pack.delta_base_cache_mib` |
| git `gc`（夜間の保守、1 度に 1 リポジトリ） | `pack-objects` と同じ。差分を探し直すときに大きい | `git.pack.*` |
| ブラウズ用の `cat-file` 常駐プロセス | 1 本あたり数 MB +（最大）`delta_base_cache_mib` | `BrowseLimits::batch_per_repo`（2）× `batch_max_repos`（32） |
| PostgreSQL | `shared_buffers` + 接続ごとに数 MB + `work_mem` × 同時のソート | `ops/linux/postgresql/README.md` |

## 見積もりの式

```
合計 ≈ serve（約 20 MB + ブラウズのキャッシュ 32 MB）
     + worker（約 20 MB）
     + N × git の 1 本（N = 同時の clone / fetch / push の数、上限 git.http.max_streams = 16）
     + 夜間の gc 1 本（worker、保守の時間帯だけ）
     + cat-file 常駐（最大 batch_per_repo × batch_max_repos 本。普段は数本）
     + PostgreSQL（shared_buffers + 接続数 × 約 5 MB）
     + OS とページキャッシュの余裕
```

git の 1 本の上限の目安（`git.pack.*` の既定値で）:

```
pack-objects 1 本 ≈ オブジェクト数 × 200 B
                  + threads × window_memory      （差分を探すときだけ。2 × 256 MiB）
                  + delta_base_cache             （64 MiB）
                  + マップしたパック ≤ packed_git_limit （1 GiB。ファイルの写像なので、足りなければ OS が捨てる）
```

例（同時 4 本の clone、リポジトリは 10 万オブジェクト・数百 MB、保守済み）:
20 + 52 + 4 ×（20 MB + 64 MB + 写像）+ PostgreSQL 約 400 MB + 余裕 ≈ **1.5〜2 GB**。
同時 16 本（`max_streams` の既定）を見込むなら 3〜4 GB。それより絞るなら `git.http.max_streams` を下げる
（超えた clone は待たずに `FG-GIT-0004 reason=streams` になり、クライアントがやり直す）。

## 設定

### git（`[git.pack]`、`FORGE_GIT_PACK_*`。起動時に検証し、誤りは `FG-SYS-0002`）

forge が動かす重い git（`upload-pack`・`receive-pack`・`gc`・ミラーの `push`・`cat-file`）すべてに `-c` で渡す
（`crates/forge-git/src/config.rs` の `PACK_HEAVY_SUBCOMMANDS`。子の `pack-objects` / `index-pack` にも効く）。

| キー | git の設定 | 既定 | 範囲 | 小さくすると |
|---|---|---|---|---|
| `git.pack.window_memory_mib` | `pack.windowMemory`（スレッドごと） | 256 | 1〜4096 | 大きなファイル（PDF・Excel など）の差分が見つかりにくくなり、gc 後のパックが少し大きくなる |
| `git.pack.threads` | `pack.threads` | 2 | 1〜64 | 差分を探し直すとき（保守前の clone、gc）が遅くなる。保守済みの clone は差分を使い回すので変わらない |
| `git.pack.delta_base_cache_mib` | `core.deltaBaseCacheLimit` | 64 | 1〜4096 | 差分の元を展開し直す回数が増える（push の `index-pack`、ブラウズ） |
| `git.pack.packed_git_limit_mib` | `core.packedGitLimit` | 1024 | 16〜65536 | パックの写像を作り直す回数が増える（大きなリポジトリだけ） |
| `git.pack.packed_git_window_mib` | `core.packedGitWindowSize` | 64 | 1〜1024、上の値以下 | 同上 |

git の既定（`pack.threads` = CPU の数、`pack.windowMemory` = 無制限、`core.deltaBaseCacheLimit` = 96 MiB、
`core.packedGitWindowSize` = 1 GiB）のままだと、CPU の多いサーバーで差分を探し直す clone や gc が **スレッド数 × 窓** だけ
メモリを使う。

`uploadpack.allowFilter` / `uploadpack.allowAnySHA1InWant` などプロトコルの設定は変えていない（`smart_http.rs` の固定の組）。

### 保守（reachability bitmap）

夜間の保守（`forge worker`、runbook repo-maintenance）の gc が、パック 1 つと **reachability bitmap**（ハッシュのキャッシュと
lookup table 付き）を書く。保守済みのリポジトリの clone / fetch は、オブジェクトをたどらずに bitmap から送るものを決め、
保存済みの差分をそのまま送る。保守前のリポジトリ（小さな push が積み重なり、オブジェクトがばらばら）は、clone のたびに
差分を探し直すので、遅く、メモリも使う。**保守が止まっていないこと**（`forge doctor` の `repositories.maintenance`）が、
メモリと速さの両方に効く。

### PostgreSQL

値と理由は `ops/linux/postgresql/README.md`（`forge-postgresql.conf`）に合わせる。要点:

| 設定 | 値 | |
|---|---|---|
| `shared_buffers` | 256MB | DB はメタデータだけ（リポジトリの中身はディスク） |
| `work_mem` | 4MB | ソート・ハッシュ 1 つごと |
| `maintenance_work_mem` | 64MB | |
| `max_connections` | 40 | forge の接続 =（`database.write_max_connections` + `read_max_connections`）× 2 + 3。例の 5 / 5 で 23 |

PFS のクラスターに相乗りするときは、PostgreSQL の分は PFS 側の見積もりに入る（設定は PFS チームのもの）。

### systemd と swap

- `forge-serve.service` の `MemoryHigh` / `MemoryMax` には、clone の git も入る（git は serve の cgroup で動く）。
  上の式の「serve + N × git」を `MemoryMax` にする。`MemoryHigh` はその 8 割程度（超えると回収が始まり、遅くなる）。
- swap は **保険**（物理メモリの 25〜50 %、`vm.swappiness=10` 程度）。常に swap が使われるなら、メモリを足すか
  `git.http.max_streams` / `git.pack.*` を下げる。

## Diagnosis

1. 誰が使っているか: `systemd-cgtop -m`（cgroup ごと）、`ps -eo rss,etime,args --sort=-rss | head`（git の行の `args` で
   `pack-objects` / `index-pack` / `cat-file` を見分ける）。
2. clone が重いリポジトリ: その時間の `forge.git.served` のログ（`repo_id`、`bytes_out`、`duration_ms`）。
3. そのリポジトリが保守済みか: `forge doctor` の `repositories.maintenance`、または
   `ls <storage>/<id>.git/objects/pack/*.bitmap`。bitmap が無い・パックが多い → runbook repo-maintenance。
4. 同時の本数: `FG-GIT-0004 reason=streams` が出ていれば `max_streams` に当たっている（メモリではなく本数で止まっている）。

## Mitigation（変更は承認を得てから）

1. 保守が遅れているリポジトリを先に保守する（runbook repo-maintenance。clone の負荷とメモリが同時に下がる）。
2. `git.http.max_streams` を下げる（同時の git の本数 = メモリの上限）。再起動が必要。
3. `git.pack.threads` / `git.pack.window_memory_mib` を下げる（再起動が必要）。
4. それでも足りなければメモリを足す。swap を増やすのは一時しのぎ。

## 測定（`ops/bench/git-pack-memory.py`）

```sh
python3 ops/bench/git-pack-memory.py --commits 3000 --runs 3
```

決まった種で合成リポジトリ（3000 コミット、テキスト 400 ファイル、4 MiB のバイナリ文書 8 個を 25 コミットごとに編集、
512 KiB の圧縮できないファイルを 100 コミットごと）を作り、次を測る。

- `loose`: 差分なしのパック（保守前に小さな push が積み重なった状態の近似）
- `gc-main`: この変更の前の保守（bitmap あり、上限なし）
- `gc-lean`: この変更の保守（bitmap + lookup table、`git.pack.*` の既定の上限つき）

それぞれで、`git clone --bare --no-local` の時間と、サーバー側の `pack-objects` の時間・ピークメモリを、
git の既定（`git-defaults`）と forge の上限（`forge-caps`）で比べる。ピークは Windows ではジョブオブジェクトの
`PeakProcessMemoryUsed`（1 プロセスのコミット量の最大）、Linux では `ru_maxrss`（RSS。パックの写像を含む）。
OS が違う数字は比べない。

### 結果（2026-11-13、Windows 11・32 CPU・git 2.53.0.windows.3、3 回の中央値。ピークは 3 回の最大）

合成リポジトリ: 3000 コミット、ばらばらのオブジェクトで 689 MiB、保守後 58 MiB。

| 場面 | git の既定 | forge の上限 | |
|---|---|---|---|
| 夜間の gc（ばらばら → 1 パック + bitmap） | 11.5 秒 / **809 MiB** | 12.0 秒 / **199 MiB** | メモリ −75 %、時間 +4 % |
| 保守前の clone の `pack-objects`（差分を探し直す） | 7.3 秒 / **866 MiB** | 8.1 秒 / **298 MiB** | メモリ −66 %、時間 +10 % |
| 保守前の clone 全体 | 15.8 秒 | 16.3 秒 | +3 % |
| 保守済みの clone 全体 | 1.5 秒 | 1.2〜1.6 秒 | 差なし（ばらつきの範囲） |
| 保守済みの clone の `pack-objects` | 0.15 秒 / 測定の下限以下 | 0.17 秒 / 測定の下限以下 | bitmap から送る。差分を探さない |

- 測定の下限: 計測用のラッパー（Python）自体が 33.8 MiB なので、それ以下は区別できない（`baseline` の行）。
- **速さに一番効くのは保守**: 保守済みの clone は保守前の約 10 分の 1 の時間（15.8 秒 → 1.5 秒）。上限は保守済みの clone を遅くしない。
- 上限で遅くなるのは差分を探し直すときだけ（スレッドが 32 → 2）。それでも clone 全体では +3 %。
