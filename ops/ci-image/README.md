# GitStation 自身の CI のイメージ

GitStation の `.forge-ci.yml`（リポジトリの直下、計画 14 X）を `forge-runner` で動かすためのジョブのイメージと、
PostgreSQL のサービスのイメージ。**ジョブにはネットワークが無い**（`--network=none`、ADR-0029）ので、
ビルドとテストに要るものはすべてイメージに入れておく。

| ファイル | 役割 |
|---|---|
| `Dockerfile` | ジョブのイメージ `ci/rust`: Rust（`rust-toolchain.toml` と同じ版、rustfmt・clippy 付き）、cargo-nextest、git、`Cargo.lock` のクレートすべて |
| `postgres.Containerfile` | サービスのイメージ `ci/postgres:16-forge`: 試験用レジストリの `postgres:16` に `max_connections=200` とポート 55432（`ops/dev/compose.yml` と同じ。一部の DB のテストが `127.0.0.1:55432` を確かめる） |
| `build-and-push.mjs` | 2 つを作って試験用レジストリに入れ、`.forge-ci.yml` に書くダイジェストを出す |

## 中身

| 項目 | 値 |
|---|---|
| 土台 | `rust:1.99.1-slim-bookworm@sha256:f2c004e3…`（ダイジェストで固定） |
| Rust | 1.99.1、rustfmt・clippy（rustup がジョブの中でネットワークを要らないように、既定のツールチェーンに入れてある） |
| cargo-nextest | 0.9.150（crates.io から `--locked` でビルド。ホストの版と同じ） |
| git | 2.39.5（Debian bookworm。forge-git の最低は 2.38、ADR-0006） |
| クレート | `Cargo.toml`・`Cargo.lock`・各クレートの `Cargo.toml`（中身の無い `src`）で `cargo fetch --locked`。全ターゲット・全プラットフォーム分 |
| 環境変数 | `CARGO_NET_OFFLINE=true`、`CARGO_INCREMENTAL=0`、`CARGO_TERM_COLOR=always`、`RUST_BACKTRACE=1` |
| ラベル | `io.gitstation.rust.version`、`io.gitstation.rust.components`、`io.gitstation.nextest.version` ほか |
| 大きさ（2026-11-11） | 展開して **1.67 GB**（Podman）、圧縮して 0.51 GB。うちクレートの層が 661 MB |

- `registry/cache`（`.crate` のファイル）は消さない。`registry/src` だけだとオフラインのビルドが
  `failed to download … --offline was specified` で失敗する（試して確かめた）。
- 依存のビルド済みの成果物は**入れていない**。ジョブの `target/` はイメージの外（`/workspace`）にでき、読み取り専用のイメージの
  中のものは使えない。入れるならジョブの始めに数 GB を写すことになり、作業ディレクトリの問題（下）が悪くなる。全体のビルドは
  32 コアで clippy 51 秒、テストのビルド 2 分なので、要らない。
- シークレットは入れない。

### 利用者が root である理由

ジョブは**コンテナの中では root** で動く。`forge-runner` はジョブのコンテナに `--user` を渡さず、作業ディレクトリを Runner の利用者の
持ち物（0755）で作る。rootless Podman ではそれがコンテナの root に当たるので、イメージの `USER` を root 以外にすると
`/workspace` に書けない（2026-11-11 に試した: `touch: cannot touch '/workspace/x': Permission denied`、`drwxr-xr-x root root /workspace`）。
コンテナの root はホストでは Runner の特権の無い利用者で、`--cap-drop=all`・`no-new-privileges`・読み取り専用のルート・
ネットワーク無しは変わらない。

**非 root で動かすには**（別の MR、Runner のコード）: ジョブのコンテナに `--userns=keep-id:uid=1000,gid=1000` を付ける（作業
ディレクトリの持ち主がコンテナの uid 1000 に見える）。イメージには uid 1000 の `ci` を作ってあるので、そのときは `USER ci` に替える。

## 作り方

試験用の Runner（`ops/runner/README.md`、compose のプロジェクト `gs-runner-test`）が動いていること。リポジトリの直下で:

```sh
node ops/ci-image/build-and-push.mjs            # 作る・入れる・ダイジェストを出す
node ops/ci-image/build-and-push.mjs --skip-postgres --jobs 4
```

- 試験用レジストリはポートを公開していない（compose のネットワークの中の `registry:5000` だけ）。そこでスクリプトは
  `docker save` → Runner のコンテナの `podman load` → `podman push registry:5000/ci/rust:<版>` の順に運ぶ（`ops/runner/README.md` の 3 と同じ道）。
  載せた後のコピーは Runner の Podman から消す（ジョブはダイジェストでレジストリから取る）。
- `postgres.Containerfile` は Runner の Podman の中で、レジストリの `ci/postgres@sha256:2addf63b…` を土台に作る。
- `rust-toolchain.toml` と `Dockerfile` の `RUST_VERSION` が違うと止まる。ツールチェーンを上げるときは `RUST_VERSION` と土台の
  ダイジェストを一緒に替える。**`Cargo.lock` を変えたら作り直す**（無いクレートがあるとジョブはオフラインで失敗する）。
- docker build の中の cargo（cargo-nextest のビルド）は `--jobs`（既定 4、`CARGO_BUILD_JOBS`）で並列を抑える。

2026-11-11 の結果:

| イメージ | ダイジェスト |
|---|---|
| `registry:5000/ci/rust`（1.99.1） | `sha256:e96601afbc8533373dd783de037b22dba07f5a4fd5a932cedcd2a4a94e5a634f` |
| `registry:5000/ci/postgres`（16-forge） | `sha256:dcc37320cb7b82f77b6eac3a5d58440d67d8b03790dcd3d63c43fc5605176206` |

## 確かめたこと（2026-11-11、Runner の Podman で、ジョブと同じ制限で）

`--network=none --read-only --tmpfs=/tmp:…,size=1g --cap-drop=all --security-opt=no-new-privileges`、イメージの既定の利用者で:

| 手順 | 結果 |
|---|---|
| `cargo fmt --all -- --check` | 通った |
| `cargo tree -p forge-cli` / `-p forge-client`（`test-support` が無いこと） | 通った |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 通った（51 秒） |
| `cargo nextest run --locked --workspace --profile ci --no-run` | 通った（2 分） |
| `gstn ci lint .forge-ci.yml` | `No problem in .forge-ci.yml.` |
| `cargo nextest run -p forge-db --profile ci -j 4`（Runner と同じポッド: `--network=none`、サービス `db` を `999:999`・tmpfs・下書きの `env` で） | **965 個すべて通った**（178 秒）。`max_connections` は 200 |

サービスで分かったこと（下書きに入れた）:

- ポートは 55432 にする。`pool::tests::connect_checks_the_server_version` が `target=127.0.0.1:55432/forge_dev` を確かめる。
- `POSTGRES_INITDB_ARGS: --auth-host=scram-sha-256` が要る。ポッドではジョブが `127.0.0.1` から繋ぐので、そのままでは initdb が
  ループバックを trust にし、`pool::tests::a_wrong_password_is_auth_failed_and_never_logged` が落ちる（GitHub では Docker の
  ブリッジ越しなので scram になっていた）。

ネットワーク無しで最後まで通った（`CARGO_HOME` は読み取り専用のままでよい）。

## 作業ディレクトリの大きさの問題（X の前に直す）

ジョブは毎回 `target/` を空から作る。今の試験用 Runner の作業ディレクトリは **tmpfs 4 GB**（`ops/runner/compose.yml`）、
`work_dir_max_bytes` は **3 GiB**（`config.example.toml`）で、ワークスペースのテストのビルドが入らない。

測った値（Linux、上と同じジョブのコンテナ、`CARGO_INCREMENTAL=0`、`/workspace/target`）:

| ビルド | `target/` の大きさ |
|---|---|
| clippy だけ（既定のデバッグ情報） | 0.79 GB |
| clippy ＋ テストのビルド ＋ `gstn`（既定のデバッグ情報、`debug = 2`） | **29.9 GB**（`deps` 28 GB、テストのバイナリ 128 個で 27.0 GB、1 つ最大 430 MB） |
| テストのビルド（`debug = "line-tables-only"`） | 11.6 GB |
| clippy ＋ テストのビルド（`debug = 0`） | **6.2 GB**（clippy だけなら 0.60 GB） |

参考: Windows の開発用の作業ツリー `D:/APP/GitStation-main/target/debug` は 53 GB（`deps` 32 GB ＋ `incremental` 21 GB、何度もの
ビルドが積もったもの。Windows はデバッグ情報が `.pdb` で形も違うので、Runner の見積もりには使わない）。

提案（Runner のコードは変えない。設定と `.forge-ci.yml` だけ）:

1. **`.forge-ci.yml` でデバッグ情報を切る**: `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0`（下書きに入れた）。ジョブの
   ピークは約 6.2 GB になる。バックトレースは関数名まで（行番号は出ない）。
2. **試験用 Runner**（`ops/runner/compose.yml`、`.local/config.toml`）:
   - `/var/lib/forge-runner` を tmpfs から名前付きのボリューム（ディスク）に替える。tmpfs はメモリで、コンテナの `mem_limit: 6g` に
     数えられるので 6 GB 台の `target/` は置けない。
   - `work_dir_max_bytes = 10737418240`（10 GiB。6.2 GB に余裕を持たせる）。ボリュームには容量の上限が無いので、この確かめが網になる。
   - `memory` は今の `"2g"` から `"4g"` へ（未計測: 4 並列のリンクで 430 MB 級のバイナリを作る。`CARGO_BUILD_JOBS=4` のまま）。
3. **本番の Runner**（runbook `ci-runner`）: XFS のプロジェクトのクォータの例 20 GB はデバッグ情報を切れば足りる。切らないなら
   40 GB 以上が要る。`work_dir_max_bytes` はクォータより小さく（例 16 GiB）。
4. ジョブの `/tmp` は既定の 1 GB（`tmpfs_size`）。テストの一時ディレクトリが入る。全体のテストを Runner で回して足りるかを確かめる
   （未確認）。

## まだ確かめていないこと

- Runner のジョブとして（`git push` からパイプラインまで）は動かしていない。作業ディレクトリの大きさの問題を直してから。
- ワークスペース全体の `cargo nextest run`（PostgreSQL 付き）は回していない。確かめたのは `forge-db` だけ。
- `.github/workflows/ci.yml` の PostgreSQL は `timescale/timescaledb:2.28.1-pg16`、ここは試験用レジストリの `postgres:16`。今の
  マイグレーションとテストは TimescaleDB を使わない（`forge-db` のテストは素の PostgreSQL 16 で計画を確かめている）。
