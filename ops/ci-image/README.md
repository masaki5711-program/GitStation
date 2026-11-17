# GitStation 自身の CI のイメージ

GitStation の `.forge-ci.yml`（リポジトリの直下、計画 14 X）を `forge-runner` で動かすためのジョブのイメージと、
PostgreSQL のサービスのイメージ。**ジョブにはネットワークが無い**（`--network=none`、ADR-0029）ので、
ビルドとテストに要るものはすべてイメージに入れておく。

| ファイル | 役割 |
|---|---|
| `Dockerfile` | ジョブのイメージ `ci/rust`: Rust（`rust-toolchain.toml` と同じ版、rustfmt・clippy 付き）、cargo-nextest、git、`Cargo.lock` のクレートすべて |
| `postgres.Containerfile` | サービスのイメージ `ci/postgres:16-forge`: 試験用レジストリの `postgres:16` に `max_connections=200` とポート 55432（`ops/dev/compose.yml` と同じ。一部の DB のテストが `127.0.0.1:55432` を確かめる） |
| `build-and-push.mjs` | 2 つを作って試験用レジストリに入れ、`.forge-ci.yml` のダイジェストを書き替え、`image.toml` を書く |
| `image.toml` | **スクリプトが書く。手で直さない。** 今 `.forge-ci.yml` が指すイメージが何から作られたか（ツールチェーン、2 つのダイジェスト、`Cargo.lock` のクレートの数と指紋） |

## 中身

| 項目 | 値 |
|---|---|
| 土台 | `rust:1.99.1-slim-bookworm@sha256:f2c004e3…`（ダイジェストで固定） |
| Rust | 1.99.1、rustfmt・clippy（rustup がジョブの中でネットワークを要らないように、既定のツールチェーンに入れてある） |
| cargo-nextest | 0.9.150（crates.io から `--locked` でビルド。ホストの版と同じ） |
| git | 2.39.5（Debian bookworm。forge-git の最低は 2.38、ADR-0006） |
| クレート | `Cargo.toml`・`Cargo.lock`・各クレートの `Cargo.toml`（中身の無い `src`）で `cargo fetch --locked`。全ターゲット・全プラットフォーム分 |
| 環境変数 | `CARGO_NET_OFFLINE=true`、`CARGO_INCREMENTAL=0`、`CARGO_TERM_COLOR=always`、`RUST_BACKTRACE=1` |
| ラベル | `io.gitstation.rust.version`、`io.gitstation.rust.components`、`io.gitstation.nextest.version`、`io.gitstation.crates.sha256`・`io.gitstation.crates.count`（入れたクレートの指紋と数。スクリプトが `--label` で付ける）ほか |
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
node ops/ci-image/build-and-push.mjs            # 作る・入れる・.forge-ci.yml と image.toml を書き替える
node ops/ci-image/build-and-push.mjs --skip-postgres --jobs 4
```

- 試験用レジストリはポートを公開していない（compose のネットワークの中の `registry:5000` だけ）。そこでスクリプトは
  `docker save` → Runner のコンテナの `podman load` → `podman push registry:5000/ci/rust:<版>` の順に運ぶ（`ops/runner/README.md` の 3 と同じ道）。
  載せた後のコピーは Runner の Podman から消す（ジョブはダイジェストでレジストリから取る）。
- `postgres.Containerfile` は Runner の Podman の中で、レジストリの `ci/postgres@sha256:2addf63b…` を土台に作る。
- `rust-toolchain.toml` と `Dockerfile` の `RUST_VERSION` が違うと止まる。ツールチェーンを上げるときは `RUST_VERSION` と土台の
  ダイジェストを一緒に替える。**`Cargo.lock` のクレートが変わったら作り直す**（無いクレートがあるとジョブはオフラインで失敗する。
  下の「作り直しの見張り」のテストが落ちて知らせる）。
- スクリプトは終わりに `.forge-ci.yml` の `ci/rust@sha256:…`（と `ci/postgres@sha256:…`）を新しいダイジェストに書き替え、
  `ops/ci-image/image.toml` を書く。**この 2 つを同じコミットに入れる。** `--skip-build` は、手元のイメージが今の `Cargo.lock` から
  作ったもの（ラベル `io.gitstation.crates.sha256` が同じ）のときだけ通る。
- docker build の中の cargo（cargo-nextest のビルド）は `--jobs`（既定 4、`CARGO_BUILD_JOBS`）で並列を抑える。

## 作り直しの見張り（2026-11-17）

2026-11-17 に、`Cargo.lock` に `russh` が入った後もイメージを作り直していなかったので、GitStation 自身のパイプラインの 3 つの
ジョブがすべて依存の解決で落ちた（``error: no matching package named `russh` found``、36 秒）。ジョブにはネットワークが無いので、
イメージに無いクレートは取れない。これが黙って起きないように、テスト `crates/forge-runner/tests/own_ci_image.rs` が次を確かめる
（`cargo nextest run -p forge-runner --test own_ci_image`。ファイルを読むだけで、Docker もレジストリも要らない）。

| テスト | 落ちるとき |
|---|---|
| `the_pinned_image_holds_every_crate_of_cargo_lock` | `Cargo.lock` のクレートの指紋が `image.toml` の `crates_sha256` と違う（クレートを足した・版を上げた・消した） |
| `the_pinned_image_has_the_toolchain_of_rust_toolchain_toml` | `rust-toolchain.toml` の `channel` が `image.toml` の `rust_channel` と違う |
| `forge_ci_yml_pins_exactly_the_recorded_images` | `.forge-ci.yml` の `image:` が `image.toml` の 2 つのダイジェスト以外を指す（手で書き替えた・片方だけコミットした） |
| `the_fingerprint_covers_registry_packages_only` | 指紋の決まり（下）が変わった |

- **指紋**: `Cargo.lock` の `[[package]]` のうち `source` のあるもの（ワークスペースのクレートには無い）を
  `name version source checksum` の 1 行にし、並べ替え、行ごとに LF を付けた文字列の SHA-256。`build-and-push.mjs` の
  `lockedCrates` とテストの `locked_crates` が同じ決まりで計算する。ワークスペースのクレートを足す・既にあるクレートへの依存を足す
  だけなら指紋は変わらず、作り直しは要らない。クレートを消したときも変わる（作り直す。イメージに要らないクレートを残さない）。
- **落ちたら**: 試験用の Runner（`gs-runner-test`）を動かして `node ops/ci-image/build-and-push.mjs` を走らせ、`.forge-ci.yml` と
  `ops/ci-image/image.toml` をその MR に入れる。`image.toml` だけを手で直してテストを通さない（イメージは古いままで、ジョブが落ちる）。
- **見張れないこと**: レジストリに実際にそのダイジェストがあるか（レジストリのボリュームを消すと無くなる。ジョブは FG-CI-0007 で
  落ちる）、`Dockerfile` を変えたのに作り直していないこと。

2026-11-11 の結果:

| イメージ | ダイジェスト |
|---|---|
| `registry:5000/ci/rust`（1.99.1） | `sha256:e96601afbc8533373dd783de037b22dba07f5a4fd5a932cedcd2a4a94e5a634f` |
| `registry:5000/ci/postgres`（16-forge） | `sha256:dcc37320cb7b82f77b6eac3a5d58440d67d8b03790dcd3d63c43fc5605176206` |

2026-11-17 の結果（`Cargo.lock` の今のクレート 461 個で作り直した。指紋 `30dcb36d…`。cargo-nextest の層はキャッシュが効き、
作る・入れるで 1 分 44 秒）:

| イメージ | ダイジェスト |
|---|---|
| `registry:5000/ci/rust`（1.99.1） | `sha256:8f7c368ae318e2519233de79aa8754438d427a5dac8331ce02c7432eaef264fd` |
| `registry:5000/ci/postgres`（16-forge） | `sha256:b778617c9388e61cf971b9e493680425f8d0f02498cf1977a3def211c066fbd2` |

2026-11-17 の 2 回目（OpenSSH のクライアントを足した。作る・入れるで 2 分 16 秒）。今の `.forge-ci.yml` と `image.toml` は
これを指す:

| イメージ | ダイジェスト |
|---|---|
| `registry:5000/ci/rust`（1.99.1） | `sha256:31e8984ac0cc924a8c9078ff3f8b9b740bfee5e4cc3f2670000e1d62781bc8b3` |
| `registry:5000/ci/postgres`（16-forge） | `sha256:9b7429f1f88dcfeb3d58a9f29510fb16fbf94795c4ac70151b245aaf26d79697` |

1 回目のイメージでビルドは通ったが、`forge-cli` の `tests/ssh_e2e.rs` が落ちた（`CI: the OpenSSH client (ssh, ssh-keygen)
must be installed for this test`）。このテストは本物の git を SSH で動かし、`CI` が立っていると `ssh` が無いことを飛ばさずに
失敗にする。イメージに入っているのは `git`・`hostname`・`openssh-client`（`ssh -V` は OpenSSH 9.2p1）。**テストが新しい
コマンドを使うようになったら Dockerfile に足して作り直す**（クレートと違い、これを見張るテストは無い。落ちるのは
パイプラインの `test` のジョブ）。

`postgres.Containerfile` は変えていないが、作り直すとダイジェストは変わる（Podman のビルドは時刻を含む）。前のダイジェストも
レジストリに残る。

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
2. **試験用 Runner**（`ops/runner/compose.yml`、`.local/config.toml`）:（**済み**: `chore/ops-runner-workdir-disk`。`mem_limit` も 6g → 8g）
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
