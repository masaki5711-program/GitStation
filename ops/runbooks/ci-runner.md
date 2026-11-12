# Runbook: ci-runner（FG-CI-0004、FG-CI-0005、FG-CI-0007〜0012、FG-CI-0014、FG-CI-0015、FG-CI-0020。関連: FG-CI-0006、FG-CI-0019、FG-AUTH-0001 / 0003、FG-REPO-0005）

M4 C3（計画 14、ADR-0029）の Runner の API（Runner の登録、ジョブの取得の long-poll、心拍とリース、ログ、完了、リースの回収）が
期待どおりに動かないとき。表 `forge.ci_runners`・`forge.ci_job_tokens`・`forge.ci_log_chunks`・`forge.ci_jobs`（`runner_id`・`lease_until`・
`lease_losses`）、worker のタスク `ci_lease_reaper`、doctor `ci.runners`。M4 C4 で Runner の本体 `forge-runner`（別の Linux ホスト、rootless
Podman）、ジョブの fetch の経路 `/api/v1/runner/jobs/{job_id}/repo.git`（隠した ref `refs/pipelines/<pipeline_id>/head` だけ）、
運用者のコマンド `forge admin runner register|list|disable` が入った。

## Trigger

- `forge worker` のログに `forge.ci.no_runner_available`（error、**alert**、FG-CI-0005）: 10 分を超えて `pending` のジョブがあり、
  どの Runner も取っていない。10 分に 1 回まで出る。`forge doctor` の `ci.runners` も同じコードで fail
- `forge worker` のログに `forge.ci.job_lease_lost`（warn、FG-CI-0004）: Runner の心拍が途絶え、リース（120 秒）が切れたジョブを
  回収した（`outcome=requeued` は列に戻した、`failed` は 3 回目の喪失で失敗にした、`released` は取り消し済みのジョブを放した）
- Runner のホストのログに 409 `FG-CI-0004`（その Runner はジョブを止める）、401 `FG-AUTH-0001`（`reason=runner_disabled|unknown|mismatch`）、
  403 `FG-AUTH-0003`（`reason=lease_other_runner|job_finished`）、413 `FG-CI-0006`（ログの上限）
- 利用者から「パイプラインが `pending` のまま進まない」と問い合わせがあった
- Runner のホストのログ（`journalctl -u forge-runner`、JSON）に FG-CI-0007（コンテナを起動できない・ホストが rootless でない・
  ジョブのディレクトリを消せない）、FG-CI-0008（`.forge-ci.yml` が `definition_hash` と一致しない）、FG-CI-0009（固定した
  コミットを取得できない）、FG-CI-0010（ジョブの時間切れ）、FG-CI-0011（ジョブのイメージが `image_registries` の外）、
  FG-CI-0012（ジョブの作業ディレクトリが `work_dir_max_bytes` を超えた）、FG-CI-0014（ジョブのサービスのポッドまたはコンテナを
  起動できない、計画 14 C5s）。ジョブのログの最後の行にも同じコード
- Runner のホストのログに FG-CI-0015（**alert**）: ジョブのコンテナが Runner から切り離されていることを確かめられず、Runner が
  ジョブを取らない（起動しない、または次の取得の前に止まった。ADR-0029 の追記 2026-11-12）

## Impact

- ジョブが待つだけで、失われはしない（push・閲覧・マージの判定は影響を受けない。`ci/pipeline` を必須にするのは計画 14 X）。
- FG-CI-0005 が全プロジェクトで続くなら CI 全体が止まっている: SEV3。1 つのタグのジョブだけなら SEV4。
- FG-CI-0004 が 1 件なら Runner の一時的な不調。同じ `runner_id` で続くならその Runner のホストの障害（SEV4、他の Runner があれば）。

## Diagnose（読み取りのみ）

1. Runner の一覧と最後の連絡（Web の管理画面が無いときは SQL。`last_contact_at` は Runner の要求のたびに 30 秒に 1 回まで更新）:
   `SELECT id, name, enabled, tags, last_contact_at FROM forge.ci_runners ORDER BY id;`
   - 有効な Runner が 0: 登録されていない・すべて無効にされた
   - `last_contact_at` が 5 分より古い: Runner のプロセスが止まった・ネットワークが切れた（Runner のホストで `systemctl status forge-runner`、
     C4 以降）
2. 待っているジョブと、そのタグ:
   `SELECT id, pipeline_id, project_id, tags, updated_at FROM forge.ci_jobs WHERE status = 'pending' ORDER BY id LIMIT 20;`
   ジョブは **ジョブのタグをすべて持つ** Runner にだけ渡る（タグの無いジョブはどの Runner でも）。機密のリポジトリ用にタグを分けた
   Runner が止まると、そのタグのジョブだけが待つ。
3. 持たれているジョブとリース:
   `SELECT id, runner_id, attempt, lease_until, lease_losses, status FROM forge.ci_jobs WHERE runner_id IS NOT NULL ORDER BY lease_until;`
   `lease_until` が過ぎたものは `ci_lease_reaper`（10 秒ごと）が回収する。過ぎたまま残るなら worker が止まっている（`forge doctor`、
   ログの `task=ci_lease_reaper`）。
4. 1 つのジョブの経過: ログの `job_id` で `forge.ci.job_claimed` → `forge.ci.job_lease_lost` / `forge.ci.job_completed` を追う
   （`request_id`・`runner_id`・`attempt`）。
5. ジョブのトークン（平文は残らない。ハッシュも読み取り用の接続からは見えない）:
   `SELECT id, job_id, attempt, runner_id, expires_at, revoked_at, revoked_reason FROM forge.ci_job_tokens WHERE job_id = <id>;`
   `revoked_reason` は `job_finished` / `job_canceled` / `lease_lost` / `project_deleted` / `project_moved`。期限は DB の `now()` で判定する。
6. 401 `FG-AUTH-0001` の Runner: ログの `credential=runner`・`reason`。`runner_disabled` は管理者が無効にした、`unknown` / `mismatch` は
   トークンのファイルが古い・壊れた（再登録が要る。トークンは登録のときに 1 回しか表示されない）。

7. Runner のホストでの失敗（M4 C4）。ログの `context` の `reason`・`step` で分ける:
   - FG-CI-0007 `podman_exit`（`step=create|start`、`podman_stderr`）: イメージが社内のミラーに無い・ダイジェストが違う・ストレージが一杯。
     スクリプトが自分で 125 で終わったときは FG-CI-0007 ではなく、終了コード 125 の失敗（`podman inspect` で `exited` を確かめて分ける）。
     Runner の利用者で `podman image exists <image>`、`podman system df`。`not_rootless` / `runs_as_root`: 下の「ホストの準備」と違う。
     `workspace_dirty`: ジョブのディレクトリが消せず、**Runner は次のジョブを取らずに終了した**（`ls -la <work_dir>/job`）
   - FG-CI-0008: `reason=hash_mismatch` は `expected`・`actual` を DB と比べる:
     `SELECT definition_hash, sha FROM forge.ci_pipelines WHERE id = <pipeline_id>;` と、forge のホストのリポジトリで
     `git --git-dir <repo> cat-file blob <sha>:.forge-ci.yml | sha256sum`。健全なリポジトリで一致しないなら不具合（request_id を添えて報告）
   - FG-CI-0009: `step=fetch` で `git_stderr` が 401 なら、ジョブがもう終わっていた（トークンは終わりで失効する）。404 は
     他のプロジェクト・他のコミットのジョブの経路（Runner の不具合）。接続の失敗は Runner のホストから
     `curl -sS https://<forge>/healthz`。forge の側のログは同じ `job_id` で `route=job_info_refs|job_upload_pack`
   - FG-CI-0011: `context` の `image` が設定の `image_registries` のどの接頭辞でも始まらない。git も pull もコンテナも動いていない。
     イメージを社内のミラーに入れて `.forge-ci.yml` の `image` をミラーのもの（ダイジェストで固定）に直す。レジストリを足すのは
     レビューの後（設定を変えて `systemctl restart forge-runner`）
   - FG-CI-0012: `context` の `bytes`・`limit_bytes`。ジョブの終わりに `<work_dir>/job/work` がそれより大きかった（ジョブは失敗、
     ディレクトリは消えている）。`forge.runner.workspace_unmeasured`（warn）は測れなかっただけでジョブは失敗にしない
   - FG-CI-0010: ジョブの `timeout` を超えた。コンテナは `podman kill` と `podman rm --force` で消えている
     （`podman ps -a --filter name=forge-job-` で残りが無いこと。サービスのあるジョブは `podman pod ps --filter name=forge-job-` も）
   - FG-CI-0014: `context` の `reason`。`pod_create` はポッド（`podman pod create --network=none`）を作れなかった
     （pause のイメージを作れない、user namespace の設定）。`service_start` は `service` のコンテナの `step`（`create` / `start`）が
     失敗した: `podman_stderr` を読む（イメージを取れない・容量・`tmpfs` や `user` がイメージに合わない）。ジョブのコンテナは作られていない。
     ポッドとその中のコンテナは `podman pod rm --force` で消えている。サービスのイメージが `image_registries` の外なら FG-CI-0011
     （`service` 付き、コンテナも pull も無い）
   - FG-CI-0015 `reason=unexpected_config`: `key`（最初の違い）と `deviations`（すべて。`Env` は変数の名前だけ）。Podman が Runner 自身の
     containers.conf 以外の何かで確かめのコンテナを変えた。`cat <work_dir>/runner/podman/containers.conf`、`podman --version`、
     `type podman`（包みでないか）。`reason=probe_failed`: 確かめのコンテナを作れない・読めない・消せない（`step`、`podman_stderr`、
     `remove_failed`。直し方は FG-CI-0007 と同じ）。`conf_unwritable`: `<work_dir>/runner` に書けない。上の「ジョブの切り離し」
   - サービスが起動した後に落ちた・応答しない: FG-CI-0014 ではない。ジョブのスクリプトの待ち（`until pg_isready -h 127.0.0.1; do sleep 1; done`）
     が失敗または時間切れ（FG-CI-0010）になり、失敗したジョブのログの終わりに各サービスのログの最後の 50 行が `[service <名前>]` 付きで入る
     （ジョブの出力と同じマスク）。読めなかったら `the log of service <名前> could not be read` と `forge.runner.service_log_unread`（warn）

## Runner のホストの準備（M4 C4、forge-runner）

前提: forge とは別の Linux ホスト（計画 14 §10.1）、git 2.38 以上、Podman 4 以上。**Runner は root で動かない**（起動を拒む）。

1. 専用の利用者を作り、rootless Podman を使えるようにする（変更は人の承認のあと）:
   `useradd --create-home --shell /usr/sbin/nologin forge-runner`、`/etc/subuid`・`/etc/subgid` に範囲
   （例 `forge-runner:200000:65536`）、`loginctl enable-linger forge-runner`。確かめる:
   `sudo -u forge-runner podman info --format '{{.Host.Security.Rootless}}'` が `true`。
2. ジョブのイメージは社内のレジストリのミラーだけ（ダイジェストで固定。計画 14 §9.3）。ジョブのコンテナはネットワークを持たない
   （`--network=none`）。イメージの取得はホストの Podman がする（`--pull=missing`）。
3. forge の管理者が forge のホストで登録し、表示されたトークンを 1 回だけ控える（再表示は無い）:
   `forge admin runner register --name linux-1 --tags linux,podman --admin <管理者のユーザー名>`
   （`--admin` は記録される登録者。有効な人の管理者でなければ FG-AUTH-0003）。一覧 `forge admin runner list`、
   無効にする `forge admin runner disable <runner_id> --admin <管理者のユーザー名>`（監査の記録の操作者。そのトークンとジョブのトークンが次の要求から拒まれる）。
   Web の管理画面はまだ無い（API の `POST /api/v1/admin/runners` は管理者の Web のセッションだけ）。
4. トークンを Runner のホストのファイルに置く（ADR-0013）: `/etc/forge-runner/token`、所有者 `forge-runner`、モード 0600。
   環境変数 `FORGE_RUNNER_TOKEN` でもよい（ファイルより優先）。`fgr_` で始まらないものは拒む（FG-SYS-0002）。
5. 設定 `/etc/forge-runner/config.toml`:

   ```toml
   server = "https://git.example.internal"
   token_file = "/etc/forge-runner/token"
   work_dir = "/var/lib/forge-runner"      # forge-runner の利用者が書けること（`:` と `,` は使えない）
   image_registries = ["registry.internal/"]   # 必須。ジョブのイメージはこのどれかで始まる（`/` で終わる接頭辞）
   # ca_file = "/etc/forge-runner/internal-ca.pem"   # API と git の fetch（GIT_SSL_CAINFO）の両方が信頼する CA
   # allow_insecure_http = false   # http の server は true のときだけ（既定は https 必須、FG-SYS-0002 reason=insecure_http）
   # memory = "4g"   pids_limit = 4096   tmpfs_size = "1g"
   # file_size_limit = "4g"   nofile_limit = 4096     # --ulimit=fsize / nofile
   # work_dir_max_bytes = 21474836480                 # ジョブの終わりに確かめる（超えたら FG-CI-0012）
   ```

   **`work_dir` は容量の上限（クォータ）のあるファイルシステムに置く（必須）**。ジョブのコンテナは `/workspace`（= `<work_dir>/job/work`）に
   書けるので、上限が無いとジョブがホストのディスクを使い切れる。コンテナには `--memory`・`--memory-swap`（同じ値、スワップを足さない）・
   `--pids-limit`・`--ulimit=fsize`（1 ファイル）・`--ulimit=nofile` が付くが、ファイルの合計はファイルシステムでしか止められない。
   どちらか（変更は人の承認のあと）:
   - XFS のプロジェクトのクォータ: `work_dir` を `prjquota` 付きでマウントした XFS に置き、
     `xfs_quota -x -c 'project -s -p /var/lib/forge-runner 42' /var/lib`、`xfs_quota -x -c 'limit -p bhard=20g 42' /var/lib`
   - ループバックのファイルシステム: `fallocate -l 20G /var/lib/forge-runner.img`、`mkfs.ext4 /var/lib/forge-runner.img`、
     `/etc/fstab` に `/var/lib/forge-runner.img /var/lib/forge-runner ext4 loop,nodev,nosuid 0 2`
   `work_dir_max_bytes` はクォータより小さくする（ジョブの後の確かめ。クォータが無い・大きすぎるホストの網）。

6. 確かめる: `sudo -u forge-runner forge-runner --config /etc/forge-runner/config.toml check`（設定・トークン・rootless・
   ジョブの切り離し。下の「ジョブの切り離し」）。
7. systemd のユニット（`User=forge-runner`、`ExecStart=/usr/local/bin/forge-runner --config /etc/forge-runner/config.toml run`、
   `Restart=on-failure`）で起動する。ログは JSON（`journalctl -u forge-runner`）。

**ジョブの間で状態を残さない**: ジョブごとに `<work_dir>/job` を消してから空で作り、終わると消す（コンテナの作ったファイルは
`podman unshare rm -rf` で消す）。**止めるとき**（`systemctl stop forge-runner` の SIGTERM、Ctrl-C）は、実行中のジョブを取り消しと同じように止め、
コンテナを `podman kill` と `podman rm --force` で消し、ディレクトリを消して結果（失敗）を報告してから終わる（`forge.runner.shutdown_requested`
→ `forge.runner.stopped`）。ユニットの `TimeoutStopSec` は 90 秒以上にする。**起動のとき**は、前の実行が残した `forge-job-*` のコンテナを
すべて `podman rm --force` で消してから（`forge.runner.stale_container_removed`、warn）ジョブのディレクトリを消す。消せなければ
FG-CI-0007 `reason=stale_containers` で起動しない。コンテナは `podman create` → `podman start --attach` で動かし、終わると `podman rm --force` で必ず消える。ジョブのトークンは git の環境の設定
（`http.extraHeader`）でだけ渡し、argv・コンテナの環境・ログに出さない（ログでは `[masked]`）。

### ジョブの切り離し（ADR-0029 の追記 2026-11-12、FG-CI-0015）

containers.conf はジョブのコンテナにフラグで外せないもの（`volumes`・`env`・`devices`・`default_sysctls`・`annotations`・`mounts.conf`・
OCI hooks）を足せる。そこで:

- **Podman は Runner 自身の containers.conf だけを読む**: `<work_dir>/runner/podman/containers.conf`（起動のたびに書き直す。
  横に空の `mounts.conf` と空の `hooks.d`）を、すべての Podman の呼び出しに `CONTAINERS_CONF` で渡す。これがあると Podman は
  `/usr/share/containers`・`/etc/containers`・`~/.config/containers` の containers.conf も drop-in（`containers.conf.d`）も読まない
  （Podman 5.3 で確かめた）。`storage.conf`・`registries.conf` は今までどおりホストのものを読む。`[engine]` の設定（`cgroup_manager`・
  `runtime`・`events_logger` など）も Podman の既定に戻る（下の注意）。Podman の呼び出しの環境は `PATH` と `CONTAINERS_CONF` だけ。
- **フラグ**: ジョブとサービスのコンテナに `--pid/--ipc/--uts/--cgroupns=private`・`--env-host=false`・`--http-proxy=false`・
  `--init=false`・`--shm-size=64m`。ポッドは `--share=net`。user namespace は rootless Podman のもの（Runner の利用者の subuid）:
  `--userns=private` は uid の対応の指定が要り、`auto` は入れ子で動かないので付けない。`/etc/hosts` は Podman が作る
  （`base_hosts_file = "none"`・`host_containers_internal_ip = "none"` でホストの名前と IP を入れない。`--no-hosts` は
  `/etc/hosts` の無いイメージ（ci/rust・ci/postgres）で `localhost` が引けなくなるので使わない）。
- **確かめ**: **ジョブを取る前に毎回**（と起動のとき・`check`）、`--volume` の無いサービスと同じ作りの確かめのコンテナ
  `forge-isolation-probe-<pid>` を `<work_dir>/runner/empty-rootfs` から作り（起動しない・pull しない）、`podman inspect` を読んで消す。
  マウント・デバイス・能力・sysctl・注釈・namespace・`/dev/shm`・想定外の環境変数のどれかが違えば FG-CI-0015 で**ジョブを取らない**
  （起動のときは終了、動いている間は次の取得の前に止まる）。起動のときは前の実行が残した `forge-isolation-probe-*` を消す。

- **本番の Runner のホスト**（準備のとき）: 確かめるのは `podman --version`（4 以上）と、`podman` が `CONTAINERS_CONF` を無視する
  包み（ラッパーのスクリプト）でないこと。ホストの containers.conf は Runner のジョブには効かない。**注意**: `[engine]` を
  ホストの containers.conf で変えていた（`cgroup_manager`・`runtime` など）なら、Runner のジョブでは Podman の既定になる。
  `forge-runner check` が通り、`FORGE_RUNNER_TEST_IMAGE=… cargo nextest run -p forge-runner --run-ignored only` が通ることを確かめる
  （`real_podman_*` の 2 つは確かめのコンテナが drop-in を無視し、書き換えた Runner の containers.conf を断ることを試す）。
- **試験環境の Runner**（`ops/runner/compose.yml`）: イメージ quay.io/podman/stable の `volumes = ["/proc:/proc"]` と host の
  ipc/uts/cgroup は、Runner 自身の containers.conf を使うのでジョブに届かない。compose が入れる drop-in `ops/runner/containers.conf`
  （`volumes = []` ほか）は、同じ利用者で手で動かす Podman 用に残している（Runner には効かない）。
- `unexpected_config` の `key`・`deviations` で何が違うかが分かる（`Env` は名前だけ）。起動し直すと containers.conf は書き直される。
  直らなければ Podman の版と包みを疑う。確かめのコンテナが残っていたら次の起動が消す。手で消すなら
  `podman rm --force forge-isolation-probe-<pid>`（Runner の利用者、人の承認のあと）。

### ジョブのサービス（計画 14 C5s、ADR-0029 の追記 2026-11-11）

`.forge-ci.yml` のジョブに `services`（2 つまで）があると、Runner はジョブごとにポッド `forge-job-<job>-<attempt>` を
`--network=none` で作り、各サービスをその中のコンテナ `forge-job-<job>-<attempt>-svc-<名前>` として起動し（読み取り専用のルート、
`--cap-drop=all`、`no-new-privileges`、ジョブと同じメモリ・プロセス数・ulimit）、最後にジョブのコンテナを同じポッドに入れる。
ポッドのネットワークはループバックだけ: サービスとジョブは `127.0.0.1` で届き合い、外にも Runner のホストにも届かない。
サービスのイメージもジョブと同じく `image_registries` の中でダイジェストで固定。`env` の値は平文（**シークレットを書かない**。C6 の対象外）。
Runner は準備ができるのを待たない（ジョブのスクリプトが待つ）。ジョブの `timeout` は**コンテナの段の全体に 1 つの期限**で、ポッドの作成・
各サービスの作成と起動（イメージの pull を含む）・ジョブのコンテナの作成と実行が残りの時間を分け合う（超えたら FG-CI-0010）。
**上限**: ポッドの cgroup にジョブの `memory`（`--memory`・`--memory-swap` を同じ値）を付けるので、ジョブとサービスを合わせてもジョブ 1 つ分の
メモリを超えない（各コンテナにも同じ上限が付く）。Podman のポッドには `--pids-limit` が無いので、プロセス数の上限はコンテナごと
（サービス 2 つなら合計で `pids_limit` の 3 倍まで）。ポッドの infra コンテナは rootless Podman がその場で作る pause のイメージ
（`localhost/podman-pause:<版>`）で、取得（pull）はしない（本物の Podman の試験で確かめる）。
**どの終わり方でも**（成功・失敗・時間切れ・取り消し・停止・起動の失敗）`podman pod rm --force --ignore --time=0` でポッドとその中の
コンテナを消す。失敗したら 1 回だけやり直し、それでも消せなければ FG-CI-0007 `reason=pod_remove`（`pod`、`podman_stderr`）を記録し、
作業ディレクトリが消せないとき（`workspace_dirty`）と同じく **Runner は次のジョブを取らずに止まる**。消してから起動し直す（Mitigate）。
サービスのログの末尾は、失敗したジョブ（取り消し・停止は除く）のときだけ足す（各 64 KiB まで、終わりの側を残す）。
**起動のとき**は残ったコンテナに続けて `forge-job-<job>-<attempt>` の名前のポッドもすべて消す（`forge.runner.stale_pod_removed`、warn。
消せなければ FG-CI-0007 `reason=stale_containers`）。手で確かめる（Runner の利用者で、読み取りのみ）: `podman pod ps --filter name=forge-job-`。

**本物の Podman の試験**: ホストを用意したら、そのホストで
`FORGE_RUNNER_TEST_IMAGE=<image@sha256:…> cargo nextest run -p forge-runner --run-ignored only`
（`a_real_container_runs_without_network_and_is_removed`）。サービスの試験は
`FORGE_RUNNER_TEST_PG_IMAGE=<postgres:16 のミラー@sha256:…>` も付ける（`a_real_postgres_service_is_reached_over_loopback_and_nothing_leaks`）。

**試験環境**では、Runner を Docker の上のコンテナとして動かせる（`ops/runner/README.md`。本番では使わない: seccomp を外し、ジョブごとのメモリとプロセス数の上限が効かず、外側のコンテナの上限だけになる）。

## 成果物（M4 C6、FG-CI-0019 / FG-CI-0020）

ジョブが `.forge-ci.yml` の `artifacts: { paths: [...] }` で宣言したファイルは、スクリプトが成功した後に Runner が **ジョブ自身のトークン**
（`fgj_…`）で 1 ファイルずつ `PUT /api/v1/runner/jobs/{job_id}/artifacts?path=` に送る。シンボリックリンクはたどらず送らない（ログに
`artifact path skipped`）、無いパスはログに `artifact path not found`。`needs` のあるジョブは、スクリプトの前に必要なジョブの成果物を
`GET …/needs/artifacts[/{id}]` で受け取り、作業ツリーのそのパスに置く（既にあるファイルの上には書かない）。

- 保存先: `<storage.root>/ci-artifacts/<project_id>/<artifact_id>`（名前はサーバーが ID から付ける）、表 `forge.ci_artifacts`
  （大きさ・sha256・期限 30 日・`keep`）。worker のタスク `ci_artifact_expiry`（10 分ごと）が期限の過ぎたもの（`keep` 以外）のファイル、
  次に行を消し、消えたプロジェクトのディレクトリを消す（`forge.ci.artifacts_expired`、`forge.ci.artifacts_of_purged_project_removed`）。
- 上限: 1 ファイル 100 MiB、1 回の実行で 512 MiB・1000 ファイル（FG-CI-0019、Runner は送る前に数え、サーバーも保存しながら確かめる）。
- 読む: 人とエージェントは `GET /api/v1/projects/{id}/pipelines/{pid}/jobs/{jid}/artifacts[/{aid}]`（ログと同じく、AI には両方の印が
  `open` のときだけ。それ以外は FG-AUTH-0022 `route=ci_artifact`）、`gstn pipeline artifacts` / `artifact-download`。

Diagnose（読み取りのみ）:

1. ジョブのログの最後の行と Runner のホストのログの FG-CI-0020 の `reason`（`send` / `receive` / `collect` / `place` /
   `path_exists` / `path_invalid` / `size_mismatch`）と `server_code`。`server_code=FG-CI-0004` は送る前にジョブが終わった
   （取り消し・リースの喪失）、`FG-REPO-0003` はサーバーの保存先の失敗。
2. `SELECT id, attempt, path, size_bytes, expires_at, keep FROM forge.ci_artifacts WHERE job_id = <job_id> ORDER BY id;`
3. サーバーの保存先: `df -h <storage.root>`、`ls <storage.root>/ci-artifacts/<project_id> | wc -l`。`*.part` は途中で失敗した送信
   （`forge.ci.artifact_remove_failed` が出たときだけ残る）。

Mitigate（承認のあと）: 保存先の空きを作る（期限の過ぎた成果物は `ci_artifact_expiry` が消す。`keep` の行はデプロイした版なので消さない）、
Runner と forge の間のネットワークを直す、リポジトリのファイルと同じパスを成果物にしない。その後パイプラインをやり直す
（`gstn pipeline retry`）。

## Mitigate（変更は人の承認のあと）

- Runner のプロセスを起こす・ホストを直す（`systemctl restart forge-runner`）。回収されたジョブは自動でやり直される（新しい `attempt`）。
- FG-CI-0007 `workspace_dirty` で止まった Runner: Runner の利用者で `podman unshare rm -rf <work_dir>/job` の後に起動する。
- FG-CI-0007 `podman_exit`: イメージを社内のミラーに入れる・`podman system prune`（Runner の利用者）の後、パイプラインを再実行する。
- FG-CI-0014: サービスのイメージをミラーに入れる、または `.forge-ci.yml` のサービスの `user`・`tmpfs`・`env` を直す（PostgreSQL は
  `user: "999:999"`、`tmpfs: [/var/lib/postgresql/data, /var/run/postgresql]`、`PGDATA` をデータの tmpfs の下の新しいディレクトリに）。
  その後パイプラインを再実行する。消え残ったポッドは Runner の利用者で `podman pod rm --force <名前>`（人の承認のあと）。
- FG-CI-0007 `reason=pod_remove` で止まった Runner: Runner の利用者で `podman pod ps --filter name=forge-job-` を見て
  `podman pod rm --force <名前>` の後に起動する（起動のときも残ったポッドを消す）。消せない理由は `podman_stderr`。
- FG-CI-0015 で止まった Runner: `unexpected_config` は原因（Runner のディレクトリを書き換えるもの、CONTAINERS_CONF を無視する
  Podman や包み）を取り除いてから起動し直す（containers.conf は書き直される）。`probe_failed` は FG-CI-0007 と同じく Podman を直す。
  確かめを外して動かす回避は無い（ジョブが Runner のプロセスや環境を見られるおそれがある）。
- 無効にした Runner を有効に戻す（管理者の Web のセッション）: `PATCH /api/v1/admin/runners/{id}` `{"enabled": true}`。
- ジョブのタグに合う Runner が無い: 管理者がタグを足す（`PATCH … {"tags": [...]}`）か、そのタグの Runner を登録する
  （`forge admin runner register …` または `POST /api/v1/admin/runners` `{"name", "tags"}`、表示された `token` を Runner の
  ホストのトークンのファイルに置く。ADR-0013）。
- トークンが漏れた・失われた: その Runner を無効にし（`forge admin runner disable <id> --admin <管理者>`）、新しい名前で登録し直す（Runner は消さない。
  ジョブの記録が `runner_id` を持つ）。
- FG-CI-0004 で `failed` になったジョブ: Runner が健全になってから、パイプラインを再実行する（`gstn pipeline retry --project <id> <pipeline_id>`）。
- **行を直接書き換えない**。`lease_until` や `status` を手で変えると、Runner と DB の状態が食い違い、同じジョブが 2 回動く。

## Verify

- `forge doctor` の `ci.runners` が ok（有効な Runner と生きている Runner が 1 以上、10 分を超えて待つジョブが無い）。
- 待っていたジョブに `forge.ci.job_claimed` が出て、`gstn pipeline show` で `running` → `succeeded` / `failed` に進む。

## 知っておくこと

- **2 つの Runner が同じジョブを取ることは無い**: 取得は `FOR UPDATE OF job, pipeline SKIP LOCKED` の 1 文。
- **リースの喪失はジョブを 1 回だけ列に戻す**: 回収はパイプライン → ジョブの順にロックし、リースが切れていることを同じ文で確かめる。
  3 回目の喪失で FG-CI-0004 の `failed`。古い `attempt` の Runner の呼び出しは何も変えない（409）。
- **取り消し**: 人がパイプラインを取り消すと、実行中のジョブは Runner を持ったまま `canceled` になり、トークンは同じトランザクションで失効する。
  Runner は次の心拍で `{"state": "canceled"}` を受け取って止め、完了の呼び出しで放す（来なければ回収が放す）。
- **シークレット**: 渡してよいかは `StoredPipeline::secrets_allowed()`（作成時に保護されたブランチへの push）だけで決める。応答の
  `secrets_allowed` が偽なら `secret_names` は空。値は C6 まで存在しない。
- **ログ**: 追記だけ（`seq` の再送は何もしない）。1 チャンク 64 KiB（JSON で送るので文字は 32 KiB まで）、1 試行 4096 チャンク・16 MiB まで
  （FG-CI-0006）。**C3 にはログを読む経路が無い**。読み取りは C5 で、出力の機密の印を継いでエージェントに FG-AUTH-0022 を返す（ADR-0029 決定 5）。
- **Runner のトークンはリポジトリも API も読めない**（FG-AUTH-0003）。ジョブのトークンは自分のプロジェクトの固定した SHA の複製
  （`PinnedCopy`）だけで、それ以外の API は見えないプロジェクトと同じ FG-REPO-0005。ジョブの fetch の経路（C4）は
  `refs/pipelines/<pipeline_id>/head` だけを広告し（初めての fetch で固定した SHA に作る。人の clone・push からは隠す）、want は
  固定したコミットから届くものだけ（それ以外は帯域内の FG-GIT-0014）。終わったジョブのトークンは FG-AUTH-0001。
- **`refs/pipelines/<id>/head` は動いている間だけ**: パイプラインを終わらせた完了の呼び出しが消す（失敗は forge のログの
  `forge.ci.pipeline_ref_delete_failed`、warn、そのコード。完了そのものは成り立つ）。取り消し・回収で終わったものや消し損ねたものは、
  リポジトリの保守（`repository.maintain`、runbook `repo-maintenance`）が `pending` / `running` でないパイプラインの ref をまとめて消す
  （`forge.git.pipeline_refs_pruned`、`deleted`・`kept`。失敗は保守の `step=pipeline_refs`）。再実行したパイプラインは次のジョブの fetch で作り直す。

## 関連

- 計画 14（M4）§4・§5 C3・C4・C5s・§6、ADR-0029、runbook `ci-pipelines`、`crates/forge-pipelines/README.md`、`crates/forge-auth/README.md`（CI principals）、
  `crates/forge-runner/README.md`、`crates/forge-web/src/routes/job_fetch.rs`。
