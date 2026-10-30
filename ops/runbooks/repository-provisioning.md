# Runbook: repository-provisioning（FG-REPO-0012。関連: FG-REPO-0002、FG-REPO-0003、FG-REPO-0004、FG-DB-0006）

プロジェクトを作ると、その 1 つのリポジトリは `provisioning` の状態で記録され、同じトランザクションで
ジョブ `repository.provision`（dedup_key `repository_provision:<repo_id>`）が投入される（手順 D4 案 A）。
`forge worker` のハンドラーが保存先にベアリポジトリを作り（`init_bare_or_adopt`）、`mark_ready` で `ready` にする
（監査 `repository.created` とイベント `repository.created`）。成功すると worker のログに `forge.repo.provisioned`
（`repo_id`、`project_id`、`outcome` = `created` / `adopted` / `already_ready`、`duration_ms`）が 1 行出る。

## Trigger

- `forge doctor` の `repositories.provisioning` が `FG-REPO-0012` RepositoryProvisioningStuck（alert）で失敗した
  - `reason=job_not_live`: そのリポジトリの作成のジョブが `queued` / `running` でない（`dead`、無い、または `succeeded` なのに `ready` でない）
  - `reason=too_old`: ジョブは生きているが、10 分を超えて `provisioning` のまま
- worker のログに `kind=repository.provision` の `FG-DB-0006` JobDeadLettered が出た
- `forge doctor` の `repositories.storage` が `FG-REPO-0002`（`ready` なのにディレクトリが無い）で失敗した → Runbook `repo-storage.md`
  （このときは作り直さない。下の「してはいけないこと」）。**`ready` のリポジトリのディレクトリが消えたことに気づけるのは
  この doctor の検査だけ**（worker は `ready` のリポジトリを見に行かない）。定期的に `forge doctor` を回す。
  `other`（シンボリックリンク、ファイル、I/O の失敗）の件数も一緒に出る。`FG-REPO-0003 reason=check_timeout` は、
  保存先が遅い・止まっていて 60 秒で見終わらなかった（`checked` まで見た）→ `repo-storage.md`

## Impact

- そのプロジェクトは存在するが、リポジトリが無いので clone / push できない。ほかのプロジェクトには影響しない。
- 1 件なら SEV3。新しいプロジェクトがどれも `provisioning` のまま（worker が動いていない、保存先が壊れている）なら SEV2。

## Diagnose（読み取りのみ）

1. `forge doctor --json` の `repositories.provisioning` を読む。`context`:
   `reason`、`stuck`（詰まっている件数）、理由ごとの件数 `job_not_live` / `too_old`（`provisioning` の古い 10 000 件を分類する。
   それより多ければ `truncated`）、先頭のものの `repo_id`、`project_id`、`age_ms`、`job_state`、`threshold_ms`。
   先頭は `job_not_live` が `too_old` より先、それぞれ古い順。
   - ジョブはリポジトリの**鍵の連なり**の最後のもの: `repository_provision:<repo_id>`、そのジョブが `succeeded` なのに
     `ready` でないときに入れ直したものは `repository_provision:<repo_id>:after:<前のジョブの id>`。
   - `job_state`: `queued` / `running`（生きている）、`none`（`dead` か、ジョブが無い）、`succeeded`。
   - **`job_state=succeeded`** は「ジョブは成功したのにリポジトリが `ready` でない」状態で、通常は起きない。
     DB を復元した（リポジトリの行は復元前、ジョブの行は後の状態）か、行を手で書き換えたときに起きる。`detail` もそう言う。
     重複除去の鍵は `succeeded` のジョブが持ち続けるので、そのままでは同じ鍵で入れ直せない。
     `forge admin repo reprovision` はこれを扱う（Mitigate 3）。復元が原因なら、ほかのリポジトリも同じ状態でないか
     `job_not_live` の件数で確かめる。
2. `reason=too_old` で `job_state=queued`: worker が動いていない可能性が高い。`forge doctor` の `outbox.sequencer_lag` が
   「worker not observed」と言っていないか、`jobs.queue_depth` が `reason=queue_wait` を出していないかを見る → `job-queue-backlog.md`。
   worker の起動の失敗は、`storage.root` が無い・開けない（`FG-SYS-0002` `config_key=storage.root`、`FG-REPO-0001`）でも起きる
   （D4 MR3b から worker は保存先のルートが無いと起動しない）。
3. ジョブの行を読む（権限のあるクエリ。読み取りのみ）:
   `SELECT id, state, attempts, max_attempts, dedup_key, last_error_code, last_error, origin_request_id, created_at, finished_at FROM forge.jobs WHERE kind = 'repository.provision' AND (dedup_key = 'repository_provision:<repo_id>' OR dedup_key LIKE 'repository_provision:<repo_id>:after:%') ORDER BY id DESC LIMIT 5;`
4. worker のログを `origin_request_id` で検索する（プロジェクトを作った要求と同じ `request_id`）。`last_error_code` ごとに:
   - `FG-REPO-0004` RepoPathOccupied（すぐ `dead`、やり直さない）: 保存先のそのパスに**自分のものではない**ものがある。
     `adopt_refused` が理由を言う:
     - `unmarked`: 目印 `forge.provisionToken` の無いリポジトリ（DB を復元して ID が巻き戻った、手で置いた）
     - `token_mismatch`: 別の行のトークンの目印（復元前の別プロジェクトのリポジトリの可能性が高い）
     - `not_dir` / `symlink` / `config_not_a_file` / `not_a_repository` / `not_bare` / `includes` / `duplicate_marker`:
       リポジトリではない、または手を加えられたもの
     - `unreadable` / `unwritable`: forge の利用者が読めない・書けない（権限、壊れた config）
     パスは `forge admin repo-path <repo_id>`。中身を見る: `ls -la <path>`、`git -C <path> log -1 --format=%H%n%cI`、
     `git config --file <path>/config --no-includes --list`（トークンの値を記録やチャットに貼らない）。
   - `FG-REPO-0003` StorageIoFailed（やり直す）: 空き容量（`df -h <root>`）、保存先のルートの所有者と権限、カーネルのログ → `repo-storage.md`。
     `reason=deadline`（`operation` 付き）は、ジョブの持ち時間（ジョブの時間切れの半分、30 秒）の中でそのファイル操作が
     終わらなかった（保存先が遅い・止まっている）
   - `FG-REPO-0001` StoragePathRejected（すぐ `dead`）: ルートの下にシンボリックリンクやファイルがある → `repo-storage.md`
   - `FG-GIT-0001` / `FG-GIT-0002` / `FG-GIT-0004`（やり直す）: git が無い・遅い・混んでいる → `git-unavailable.md`。
     `FG-GIT-0002` は 1 回の git が 20 秒か、ジョブの持ち時間の残りを超えた（どちらか短い方）。
     試行を使い切ると `dead` になる（`attempts = max_attempts`）
   - `FG-GIT-0003`（作る途中の git の失敗、やり直す）→ `git-process-failed.md`
   - `FG-DB-0001` / `FG-DB-0002`（`mark_ready` の失敗、やり直す）→ `db-unavailable.md`。ディレクトリはできているので、
     次の試行は自分の目印を見て引き取る（`outcome=adopted`）
   - `FG-SYS-0002` `config_key=git.hooks_path`（すぐ `dead`）: フックの場所の設定の誤り（M1 phase 10 以降）
5. 作りかけの一時ディレクトリ: 名前を変える前に止まった試みは、シャードの下に `.tmp-<repo_id>-<uuid>.git` を残すことがある。
   試みごとに名前が違うので、やり直しの妨げにはならない（自動では消さない）。探す:
   `find <root>/repos -maxdepth 2 -name '.tmp-*' -mmin +60`

## Mitigate

1. 原因を直す（Diagnose 4 の各 Runbook）。worker が動いていなければ起動する（`too_old` で `job_state=queued` なら、これだけで直る）。
2. `FG-REPO-0004` のとき: **ディレクトリを消さない**。どのプロジェクトのものか確かめ（DB の復元の記録、`git log`）、
   必要なものなら別の場所へ移して保管する（`mv <path> <root>/quarantine-<repo_id>-<日付>.git` など、ルートの `repos/` の外）。
   ID の巻き戻りが原因なら、リポジトリの ID の連番を既存の最大の ID より先に進める（`repo-storage.md`）。
   `[MUTATING — needs approval]`（ロールバック: 移したディレクトリを元の場所へ戻す）
3. ジョブを投入し直す: `forge admin repo reprovision <repo_id>`。`[MUTATING — needs approval]`
   - `provisioning` のリポジトリにだけ働く。`ready` のリポジトリは `FG-REPO-0014 reason=repository_ready` で断られる（何も書かない）。
   - 鍵の連なりの最後のジョブで分かれる（行の鍵を取ってから読むので、同時に 2 回実行しても 1 件しか入らない）:
     - `queued` / `running`: 何もしない（「provisioning job <id> is already queued / running」）。worker が動いているか確かめる。
     - `dead` / 無い: 同じ鍵で新しいジョブを入れる。
     - `succeeded`（復元・手の変更の後）: `repository_provision:<repo_id>:after:<そのジョブの id>` の鍵で新しいジョブを入れる
       （「queued after job <id>」）。
   - 投入すると監査ログに `repository.reprovision_requested`（`repository_id`、`job_id`、`holder_job_id` / `holder_state` =
     鍵を持っていた `succeeded` のジョブ、無ければ `none`、操作した OS の利用者）が残る。
   - ロールバック: 不要（新しいジョブは前と同じ手順をもう一度試すだけ。自分の目印のディレクトリなら引き取り、無ければ作る）。
4. 一時ディレクトリ `.tmp-*`: そのリポジトリが `ready` になった後で、1 時間以上前のものだけを消してよい
   （実行中の試みの一時ディレクトリを消さないため）。`[MUTATING — needs approval]`（ロールバック: なし。中身は作りかけで、使われていない）
5. **してはいけないこと**: `forge.repositories.state` を手で `ready` にしない（監査とイベントが残らず、ディレクトリも無い）。
   `forge.jobs` の `dead` の行を `queued` に戻さない（手順 3 で新しいジョブを作る）。`ready` のリポジトリのディレクトリが無いときに
   作り直さない（中身が失われている。バックアップから戻す → `repo-storage.md`）。

## Verify

- worker のログに `forge.repo.provisioned`（その `repo_id`、`outcome=created` または `adopted`）が出た。
- `forge doctor` の `repositories.provisioning` が `ok`、`repositories.storage` が `ok`。
- `forge admin repo-path <repo_id>` が `exists`。
- `jobs.dead` は元の `dead` の行が 24 時間を過ぎるまで失敗のままなので、`dead_recent` が増えていないことを見る（`job-dead-lettered.md`）。

## Escalate

- `FG-REPO-0004` で、ディレクトリがどのプロジェクトのものか分からない、または DB の復元が絡むときは、操作を止めて開発者
  （Claude のセッション）に `forge doctor --json`、ジョブの行、`adopt_refused`、`ls -la <path>` の結果を渡す（トークンの値は渡さない）。
- 同じ原因で繰り返し詰まる、または 1 時間で直らないときも同様。
