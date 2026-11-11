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
   サービスに使うイメージ（`postgres:16` など）も同じ手順で入れる。

4. 確かめる。

   ```sh
   docker compose run --rm runner check      # ok: configuration, token, rootless podman
   docker compose logs --no-log-prefix runner | tail
   forge --config <試験環境の設定> admin runner list   # last_seen が進む
   ```

   プロジェクトに `.forge-ci.yml` を push すると、パイプラインができ、コミットに `ci/pipeline` のチェックが付く。

止める: `docker compose stop runner`（動いているジョブは止められ、失敗として報告される。コンテナとジョブのディレクトリは残らない）。
消す: `docker compose down`（`-v` を付けるとレジストリの中身も消える）。

## ジョブのサービス（PostgreSQL の例）

ジョブは `services` で横にコンテナを 2 つまで動かせる（計画 14 C5s）。ジョブとサービスは 1 つのポッドに入り、ネットワークは
ループバックだけ（`--network=none`）。サービスには `127.0.0.1` で届く。Runner は準備ができるのを待たないので、スクリプトで待つ。

```yaml
version: 1
stages: [test]
jobs:
  test:
    stage: test
    image: registry:5000/ci/rust@sha256:<ダイジェスト>
    timeout: 30m
    tags: [linux]
    services:
      - name: db
        image: registry:5000/ci/postgres:16@sha256:<ダイジェスト>
        user: "999:999"                 # postgres の利用者。root にしない
        env:
          POSTGRES_PASSWORD: test       # 平文。シークレットは書かない
          PGDATA: /var/lib/postgresql/data/pgdata
        tmpfs: [/var/lib/postgresql/data, /var/run/postgresql]   # 読み取り専用のルートの上の書ける場所
    script:
      - until pg_isready -h 127.0.0.1; do sleep 1; done
      - FORGE_TEST_DATABASE_URL=postgres://postgres:test@127.0.0.1:5432/postgres cargo test
```

`pg_isready` の無いイメージでは、`timeout 60 sh -c 'until (echo > /dev/tcp/127.0.0.1/5432) 2>/dev/null; do sleep 1; done'`（bash）
などで待つ。ジョブが失敗すると、ジョブのログの終わりに各サービスのログの最後の 50 行（`[service db] …`）が入る。
キーは `name`（`[a-z][a-z0-9-]{0,30}`）・`image`（必須）と `env`（16 まで）・`user`・`tmpfs`（4 つまで、絶対パス、`/tmp` は常にある）だけ。
`user` が `root`・`0`・`0:<gid>` だと `gstn ci lint` が警告を出す（`service_user_root`、エラーではない）。
ジョブの `timeout` はサービスの起動（pull を含む）とジョブの実行の合計に掛かる。メモリの上限はポッド全体で 1 ジョブ分（ジョブとサービスの合計）。
プロセス数の上限はコンテナごと。
`command`・`ports`・`volumes`・特権は無い（`gstn ci lint` が `key_unknown`）。

## 本番と違うところ（試験だから許していること）

| 項目 | ここでの状態 | 本番 |
|---|---|---|
| seccomp | `seccomp=unconfined`（入れ子の rootless Podman に要る） | 専用ホストで Podman の既定 |
| GitStation との通信 | http（`allow_insecure_http = true`） | https |
| レジストリ | http、認証なし | 社内のミラー |
| メモリとプロセス数の上限 | **外側のコンテナの上限（6 GB、4096）だけが効く。** ジョブごと・ポッドごとの `--memory` は効かず（cgroups が無効）、ジョブの中からは外側の値が見える | ジョブごとに効く（サービスのあるジョブはポッドの合計にも） |
| イメージの置き場 | Podman のイメージ（`podman-store`）はジョブの間で残る（コンテナとジョブのディレクトリは残らない） | 同じ。ジョブごとに消すのは作業ディレクトリとコンテナ |
| 届く範囲 | Runner は Windows のホストの 127.0.0.1 に届く（ジョブは届かない） | Runner のホストから GitStation だけ |

seccomp を外しているので、**信頼できないプロジェクトのジョブはこの Runner で動かさない。**
上限が外側だけなので、Runner 1 つにつきジョブは 1 つ（今の作り）という前提で使う。

## うまくいかないとき

| 症状 | 原因と直し方 |
|---|---|
| ログに `forge.runner.claim_failed`、`FG-CLI-0003`、`detail` が `reason=connect` | GitStation に届いていない。`server` のアドレス、GitStation が動いているか、`extra_hosts` を足していないかを見る |
| 同じく `FG-AUTH-0001` | トークンが違う、または Runner が無効。`admin runner list` で確かめ、登録し直す |
| `check` が FG-CI-0007 | Podman が rootless で動けない。`security_opt` と `/dev/fuse` が compose にあるか見る |
| `check` が FG-CI-0015 `default_volumes` | `containers.conf`（`volumes = []` の drop-in）が compose で入っていない。イメージの既定の `/proc:/proc` がジョブに入る |
| Git Bash で `docker run --device /dev/fuse` が失敗する | パスが書き換えられている。`MSYS_NO_PATHCONV=1` を付ける（compose 経由なら要らない） |
| ジョブが FG-CI-0011 | イメージ（サービスのイメージも、`context` の `service`）が `registry:5000/` で始まっていない |
| ジョブが FG-CI-0014 | サービスのポッドかコンテナを起動できない。`podman_stderr` を見る（イメージをレジストリに入れたか、`tmpfs`・`user`） |
| サービスのあるジョブが待ちで時間切れ | ジョブのログの `[service <名前>]` の行を見る（データのディレクトリに書けない、など） |

コードの意味と手順は `ops/runbooks/ci-runner.md`。
