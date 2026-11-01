# Runbook: repository-purge（FG-REPO-0020。関連: FG-REPO-0019、FG-REPO-0001、FG-REPO-0003、FG-DB-0001、FG-DB-0002、FG-DB-0009）

削除したプロジェクトは猶予期間（`hosting.deletion_grace_days`、既定 7 日）のあいだ復元できる。`purge_after` を過ぎると、
`forge worker` の `purge` のタスク（`hosting.purge_interval_secs` ごと、既定 300 秒。1 回に最大 20 件、`purge_after` の古い順）が
完全削除する（手順 D5 MR4、ADR-0015 の Amendment D5）。1 回の持ち時間は 5 分で、過ぎたら残りは次の回
（`forge.repo.purge_cut`）。1 件ごとに:

1. **開始**（1 つのトランザクション）: プロジェクトの行、リポジトリの行の順にロックし、`deleted_at` / `purge_after` を読み直して、
   リポジトリを `purging` にする（監査 `repository.purge_started`）。これ以降は復元できない（`FG-REPO-0019 reason=purging`）。
   復元が先に確定していれば何もしない（`forge.repo.purge_skipped`、debug）。
2. リポジトリの作成のジョブが `running` なら次の回まで待つ（`forge.repo.purge_deferred`）。
3. **ディレクトリの削除**: リポジトリの ID から決まるパス（`forge admin repo-path <repo_id>`）を、同じシャードの
   `.purge-<repo_id>-<uuid>.git` に名前を変えてから消す。中断した削除の残り（`.purge-<repo_id>-*`）と作成の途中の残り
   （`.tmp-<repo_id>-*`）も消す。1 件 120 秒まで。もう無いのはエラーではない。
4. **行の削除**（1 つのトランザクション）: メンバー、保護 ref、リポジトリ、名前空間、プロジェクトの行を消し、パスを `deleted` の
   墓標で塞ぎ（`owner_at_write` = 削除したユーザー）、監査 `project.purged` とイベント `project.purged`（admin クラス、
   `dedup_key` = `project_purged:<project_id>`）を書く。古いパスの `moved` の墓標は行き先を失い（転送が終わる）、パスは塞いだまま。

成功すると worker のログに `forge.repo.purged`（`project_id`、`repo_id`、`tombstone_id`、`redirects_ended`、`storage_removed`、
`swept`、`resumed`、`overdue_s`、`duration_ms`）が 1 行出る。

## Trigger

- worker のログ（`task=purge` の実行の中）、または `forge admin project purge --now` のログに `FG-REPO-0020` RepositoryPurgeStuck（alert）
  - `step=start`: 開始のトランザクションが失敗した（DB）
  - `step=storage`: ディレクトリを消せなかった（`cause_code=FG-REPO-0001` シンボリックリンクかファイルがある、
    `FG-REPO-0003` I/O の失敗、`reason=deadline` は 120 秒で終わらなかった）
  - `step=finish`: 行の削除のトランザクションが失敗した（DB）
- 同じ `project_id` が毎回失敗する（詰まっている）。1 回だけで、その後に同じ `project_id` の `forge.repo.purged` があれば一時的なもの。
- `FG-REPO-0019 reason=purging` で復元を断られた利用者からの問い合わせ（完全削除が始まったまま終わっていない）

## Impact

- そのプロジェクトはすでに見えない（削除待ち）。完全削除が遅れるだけで、ほかのプロジェクトには影響しない。保存先の容量が
  空かない。同じパスは墓標で塞がれているので、新しいプロジェクトを同じパスに作れないのは正常（墓標の解放は別の操作）。
- 1 件なら SEV4。すべての完全削除が失敗する（DB に届かない、保存先が壊れている）なら SEV3。

## Diagnose（読み取りのみ）

1. ログの `FG-REPO-0020` の `context` を読む: `step`、`project_id`、`repo_id`、`cause_code`、`overdue_s`（`purge_after` からの秒数）。
   `source` に元のエラーの文と `context` がある。`request_id` でその回の他の行（`forge.repo.purge_deferred` など）を探す。
2. 状態を読む（権限のあるクエリ。読み取りのみ）:
   `SELECT id, deleted_at, purge_after, deleted_by FROM forge.projects WHERE id = <project_id>;`
   `SELECT id, state FROM forge.repositories WHERE project_id = <project_id>;`
   `state = 'purging'` なら開始は済んでいる（次の回で続きから再開する。`ready` には戻らない）。
3. `forge admin project purge --dry-run` で、いま対象になっているプロジェクトの一覧（`state`、`purge_after`、`overdue_s`）を見る。
   何も変えない。
4. `step=storage` のとき、ディレクトリを見る（**消さない**）:
   - `forge admin repo-path <repo_id>` でパスと有無。シャードの中の残り: `ls -la <shard>`（`.purge-<repo_id>-*`、`.tmp-<repo_id>-*`）。
   - `cause_code=FG-REPO-0001`（`reason=symlink` / `not_dir`、`leftover` があればそのパス）: forge はそこにシンボリックリンクやファイルを
     作らない。手を加えられた可能性がある。`ls -la`、`readlink -f <path>` で何を指しているか確かめる → `repo-storage.md`。
   - `cause_code=FG-REPO-0003`: `df -h <root>`、ルートの所有者と権限、カーネルのログ。`reason=deadline` は削除が 120 秒を超えた
     （大きなリポジトリ、遅い保存先）。裏で削除が続いていることがあり、次の回で残りを消す。
5. `step=start` / `step=finish` のとき: `cause_code` の Runbook（`FG-DB-0001` / `FG-DB-0002` → `db-unavailable.md`）。
   `cause_code=FG-DB-0009 reason=rejected`（`step=start`）は DB のガードが `purging` を断った: プロジェクトが削除待ちでない、または
   猶予がまだ終わっていない（時計のずれ、間に確定した復元）。次の回は `purge_after` を読み直して飛ばすので、繰り返すなら時計を見る。
   `cause_code=FG-SYS-0001`（`not_purging`、`purging_without_deletion`、`project_without_namespace` など）はデータの破損 →
   `namespace-data.md`、`internal-error.md`。
6. `forge.repo.purge_deferred` が同じリポジトリで続く: そのリポジトリの作成のジョブが `running` のまま
   （`repository-provisioning.md` の Diagnose 3 のクエリ）。リースが切れれば reaper が戻し、次の作成の実行は `purging` を見て何もしない。

## Mitigate

1. 原因を直す: 保存先の空き・権限（`repo-storage.md`）、DB への接続（`db-unavailable.md`）。
2. `FG-REPO-0001` のとき: **リンクやファイルを消さない**。中身を確かめ、必要なら保存先の `repos/` の外へ移して保管する
   （`mv <path> <root>/quarantine-<repo_id>-<日付>`）。`[MUTATING — needs approval]`（ロールバック: 元の場所へ戻す）
3. 次の worker の回を待つか、すぐ実行する: `forge admin project purge --now`。`[MUTATING — needs approval]`
   - 1 回分（最大 20 件）を worker と同じ手順で実行する。猶予期間は短くならない（`purge_after` を過ぎたものだけ）。
   - 何度実行してもよい: 消えたディレクトリはエラーにならず、行とイベントは 1 回だけ書かれる。止まったところから再開する。
   - 出力: プロジェクトごとに 1 行（`purged` / `skipped` / `deferred` / `failed` とその理由・コード、`request_id`）と
     `purged=<n> failed=<n> other=<n>`。失敗があれば終了コードは 0 以外（`FG-REPO-0020 reason=batch_failed`）。

## Verify

- `forge admin project purge --dry-run` にそのプロジェクトが出ない（`due=0` か、一覧にない）。
- ログに `forge.repo.purged`（その `project_id`）が 1 行。`forge admin repo-path <repo_id>` が `missing`。
- 権限のあるクエリで `SELECT count(*) FROM forge.events WHERE dedup_key = 'project_purged:<project_id>'` が 1、
  `SELECT kind FROM forge.namespace_tombstones WHERE cause_type = 'project' AND cause_id = <project_id>` に `deleted` がある。

## Escalate

- 保存先の上に手を加えられた形跡（`FG-REPO-0001`）があるとき: セキュリティの担当へ（インシデントとして記録）。
- `FG-SYS-0001` のデータの破損: DB の担当と開発（Claude）へ。破損の原因が分かるまで、行を手で消さない。

## してはいけないこと

- `forge.repositories.state` や `forge.projects` を手で書き換えて復元しようとしない（`purging` から戻る道はない。DB のガードが断る）。
  リポジトリが必要なら、バックアップから別のプロジェクトとして戻す。
- ディレクトリを `rm -rf` で先に消してから行を消す、のような手作業をしない。`--now` が同じことを順序どおり、記録付きで行う。
