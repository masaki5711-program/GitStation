# 試験用の Runner（Docker の上のコンテナ）

`forge-runner` を、Docker（Rancher Desktop など）のあるマシンでコンテナとして動かすための一式。
**試験環境だけで使う。** 本番の Runner は専用の Linux ホストに置く（`ops/runbooks/ci-runner.md`、計画 14 §10.1）。

| ファイル | 役割 |
|---|---|
| `Dockerfile` | `forge-runner` を Linux 向けにビルドし、Podman 入りのイメージに載せる |
| `compose.yml` | Runner と、ジョブのイメージを置く試験用レジストリ（`registry:5000`） |
| `registries.conf` | 試験用レジストリを http で使う設定（Runner の中の Podman 用） |
| `config.example.toml` | Runner の設定の見本 |
| `.local/` | トークンと設定の実物（git に入れない） |

## 仕組み

- Runner のコンテナの中で rootless Podman が動き、ジョブごとにコンテナを作る。Docker のソケットは渡さない。特権も付けない。
- ジョブのコンテナはネットワークを持たない。イメージは `registry:5000/` で始まるものだけ（ダイジェストで固定）。
- Runner は外へ要求を出すだけで、ポートを公開しない。
- GitStation へは `host.docker.internal` で届く（Rancher Desktop が名前を解決する）。
  `extra_hosts: host-gateway` は足さない。Windows のホストに届かない別のアドレスになる。

## 始め方

作業ディレクトリは `ops/runner`。試験用の GitStation（`ops/test-env`、:28080）が動いていること。

1. Runner を登録し、トークンを置く（トークンは 1 回だけ表示される。画面やログに残さない）。

   ```sh
   mkdir -p .local
   forge --config <試験環境の設定> admin runner register --name docker-test-1 --tags linux,podman --admin <管理者> > .local/token
   cp config.example.toml .local/config.toml
   ```

   `.local/token` はトークンの 1 行だけにする。

2. ビルドして起動する。

   ```sh
   docker compose up -d --build
   ```

3. ジョブのイメージをレジストリに入れる（外から取るのはこのときだけ）。出てきたダイジェストを `.forge-ci.yml` に書く。

   ```sh
   docker compose exec -T runner sh -c 'podman pull -q docker.io/library/alpine:3.20 \
     && podman push -q --digestfile /tmp/d docker.io/library/alpine:3.20 registry:5000/ci/alpine:3.20 \
     && cat /tmp/d && podman rmi --all --force'
   ```

   `.forge-ci.yml` では `image: registry:5000/ci/alpine@sha256:<ダイジェスト>`、`tags: [linux]`。

4. 確かめる。

   ```sh
   docker compose run --rm runner check      # ok: configuration, token, rootless podman
   docker compose logs --no-log-prefix runner | tail
   forge --config <試験環境の設定> admin runner list   # last_seen が進む
   ```

   プロジェクトに `.forge-ci.yml` を push すると、パイプラインができ、コミットに `ci/pipeline` のチェックが付く。

止める: `docker compose stop runner`（動いているジョブは止められ、失敗として報告される。コンテナとジョブのディレクトリは残らない）。
消す: `docker compose down`（`-v` を付けるとレジストリの中身も消える）。

## 本番と違うところ（試験だから許していること）

| 項目 | ここでの状態 | 本番 |
|---|---|---|
| seccomp | `seccomp=unconfined`（入れ子の rootless Podman に要る） | 専用ホストで Podman の既定 |
| GitStation との通信 | http（`allow_insecure_http = true`） | https |
| レジストリ | http、認証なし | 社内のミラー |
| メモリとプロセス数の上限 | **外側のコンテナの上限（6 GB、4096）だけが効く。** ジョブごとの `--memory` は効かず、ジョブの中からは外側の値が見える | ジョブごとに効く |
| 届く範囲 | Runner は Windows のホストの 127.0.0.1 に届く（ジョブは届かない） | Runner のホストから GitStation だけ |

上限が外側だけなので、Runner 1 つにつきジョブは 1 つ（今の作り）という前提で使う。

## うまくいかないとき

| 症状 | 原因と直し方 |
|---|---|
| ログに `forge.runner.claim_failed`、`FG-CLI-0003`、`detail` が `reason=connect` | GitStation に届いていない。`server` のアドレス、GitStation が動いているか、`extra_hosts` を足していないかを見る |
| 同じく `FG-AUTH-0001` | トークンが違う、または Runner が無効。`admin runner list` で確かめ、登録し直す |
| `check` が FG-CI-0007 | Podman が rootless で動けない。`security_opt` と `/dev/fuse` が compose にあるか見る |
| Git Bash で `docker run --device /dev/fuse` が失敗する | パスが書き換えられている。`MSYS_NO_PATHCONV=1` を付ける（compose 経由なら要らない） |
| ジョブが FG-CI-0011 | イメージが `registry:5000/` で始まっていない |

コードの意味と手順は `ops/runbooks/ci-runner.md`。
