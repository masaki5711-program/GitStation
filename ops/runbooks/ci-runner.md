# Runbook: ci-runner（FG-CI-0004、FG-CI-0005、FG-CI-0007〜0010。関連: FG-CI-0006、FG-AUTH-0001 / 0003、FG-REPO-0005）

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
  コミットを取得できない）、FG-CI-0010（ジョブの時間切れ）。ジョブのログの最後の行にも同じコード

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
   - FG-CI-0007 `podman_exit`（`podman_stderr`）: イメージが社内のミラーに無い・ダイジェストが違う・ストレージが一杯。
     Runner の利用者で `podman image exists <image>`、`podman system df`。`not_rootless` / `runs_as_root`: 下の「ホストの準備」と違う。
     `workspace_dirty`: ジョブのディレクトリが消せず、**Runner は次のジョブを取らずに終了した**（`ls -la <work_dir>/job`）
   - FG-CI-0008: `reason=hash_mismatch` は `expected`・`actual` を DB と比べる:
     `SELECT definition_hash, sha FROM forge.ci_pipelines WHERE id = <pipeline_id>;` と、forge のホストのリポジトリで
     `git --git-dir <repo> cat-file blob <sha>:.forge-ci.yml | sha256sum`。健全なリポジトリで一致しないなら不具合（request_id を添えて報告）
   - FG-CI-0009: `step=fetch` で `git_stderr` が 401 なら、ジョブがもう終わっていた（トークンは終わりで失効する）。404 は
     他のプロジェクト・他のコミットのジョブの経路（Runner の不具合）。接続の失敗は Runner のホストから
     `curl -sS https://<forge>/healthz`。forge の側のログは同じ `job_id` で `route=job_info_refs|job_upload_pack`
   - FG-CI-0010: ジョブの `timeout` を超えた。コンテナは `podman kill` と `podman rm --force` で消えている
     （`podman ps -a --filter name=forge-job-` で残りが無いこと）

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
   無効にする `forge admin runner disable <runner_id>`（そのトークンとジョブのトークンが次の要求から拒まれる）。
   Web の管理画面はまだ無い（API の `POST /api/v1/admin/runners` は管理者の Web のセッションだけ）。
4. トークンを Runner のホストのファイルに置く（ADR-0013）: `/etc/forge-runner/token`、所有者 `forge-runner`、モード 0600。
   環境変数 `FORGE_RUNNER_TOKEN` でもよい（ファイルより優先）。`fgr_` で始まらないものは拒む（FG-SYS-0002）。
5. 設定 `/etc/forge-runner/config.toml`:

   ```toml
   server = "https://git.example.internal"
   token_file = "/etc/forge-runner/token"
   work_dir = "/var/lib/forge-runner"      # forge-runner の利用者が書けること
   # ca_file = "/etc/forge-runner/internal-ca.pem"   # API と git の fetch（GIT_SSL_CAINFO）の両方が信頼する CA
   # allow_insecure_http = false   # http の server は true のときだけ（既定は https 必須、FG-SYS-0002 reason=insecure_http）
   # memory = "4g"   pids_limit = 4096   tmpfs_size = "1g"
   ```

6. 確かめる: `sudo -u forge-runner forge-runner --config /etc/forge-runner/config.toml check`（設定・トークン・rootless）。
7. systemd のユニット（`User=forge-runner`、`ExecStart=/usr/local/bin/forge-runner --config /etc/forge-runner/config.toml run`、
   `Restart=on-failure`）で起動する。ログは JSON（`journalctl -u forge-runner`）。

**ジョブの間で状態を残さない**: ジョブごとに `<work_dir>/job` を消してから空で作り、終わると消す（コンテナの作ったファイルは
`podman unshare rm -rf` で消す）。コンテナは `--rm` と `podman rm --force` で必ず消える。ジョブのトークンは git の環境の設定
（`http.extraHeader`）でだけ渡し、argv・コンテナの環境・ログに出さない（ログでは `[masked]`）。

**本物の Podman の試験**: ホストを用意したら、そのホストで
`FORGE_RUNNER_TEST_IMAGE=<image@sha256:…> cargo nextest run -p forge-runner --run-ignored only`
（`a_real_container_runs_without_network_and_is_removed`）。

## Mitigate（変更は人の承認のあと）

- Runner のプロセスを起こす・ホストを直す（`systemctl restart forge-runner`）。回収されたジョブは自動でやり直される（新しい `attempt`）。
- FG-CI-0007 `workspace_dirty` で止まった Runner: Runner の利用者で `podman unshare rm -rf <work_dir>/job` の後に起動する。
- FG-CI-0007 `podman_exit`: イメージを社内のミラーに入れる・`podman system prune`（Runner の利用者）の後、パイプラインを再実行する。
- 無効にした Runner を有効に戻す（管理者の Web のセッション）: `PATCH /api/v1/admin/runners/{id}` `{"enabled": true}`。
- ジョブのタグに合う Runner が無い: 管理者がタグを足す（`PATCH … {"tags": [...]}`）か、そのタグの Runner を登録する
  （`forge admin runner register …` または `POST /api/v1/admin/runners` `{"name", "tags"}`、表示された `token` を Runner の
  ホストのトークンのファイルに置く。ADR-0013）。
- トークンが漏れた・失われた: その Runner を無効にし（`forge admin runner disable <id>`）、新しい名前で登録し直す（Runner は消さない。
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

- 計画 14（M4）§4・§5 C3・C4・§6、ADR-0029、runbook `ci-pipelines`、`crates/forge-pipelines/README.md`、`crates/forge-auth/README.md`（CI principals）、
  `crates/forge-runner/README.md`、`crates/forge-web/src/routes/job_fetch.rs`。
