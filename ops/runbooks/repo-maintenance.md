# Runbook: repo-maintenance（FG-GIT-0018 / FG-GIT-0019、リポジトリの定期の保守）

M3 O3（計画 11 §5 O3、ADR-0024「実装で決めたこと（O3）」）。`forge worker` が各リポジトリを週に 1 回、夜の窓（既定 02:00〜05:00、
`Asia/Tokyo`）の中で保守する。保守は `repository.maintain` のジョブで、**worker の専用のランナー** が 1 件ずつ走らせる（作成・版・
マージできるかの判定のジョブを待たせない）。1 回の保守の手順:

1. push の受け入れの途中（`objects/tmp_objdir-incoming-*` が 6 時間より新しい）なら **何もしない**（`skipped incoming_push`、1 時間後にまた）。
2. `git fsck`（毎回 `--connectivity-only`、`maintenance.fsck_interval_days`（既定 30 日）ごとに全体）。問題があれば FG-GIT-0019 で止まり、
   **gc も prune もしない**。
3. `git gc --cruft --prune=<猶予>`（`gc.packRefs=false`、bitmap、commit-graph は別の手順）。猶予は `maintenance.prune_grace_hours`（既定 48 時間、
   24 時間以上で、マージの照合の窓とマージのジョブの再試行の全体より長いことを worker が起動時に確かめる）。別の gc が動いていれば
   `skipped gc_running`。
4. `git pack-refs --all --prune`（マージのロック `merge:<repo_id>` を **待たずに** 取れたときだけ。マージ中なら今回は飛ばす:
   `skipped_reason = pack_refs_merge_lock_held`）。
5. `git commit-graph write --reachable`。
6. オブジェクトと ref の数を台帳に残す（ref の数は測るだけ。push は拒まない）。

各手順は `maintenance.step_timeout_secs`（既定 1 800 秒）と 1 回の保守の期限（ジョブの時間切れ 1 時間 − 60 秒）の短い方で打ち切られ、
**git の子プロセス（`repack` / `pack-objects`）ごと** 止まる（FG-GIT-0002 `step=<手順>`）。

## Trigger

- `forge doctor` の `repositories.maintenance` が FG-GIT-0018
  - `reason=failing`: 同じリポジトリの保守が **2 回続けて** 失敗した（`repositories`、`max_consecutive`、`repo_ids`、`last_error_code`）
  - `reason=overdue`: ready のリポジトリが、期日から保守の間隔（既定 7 日）以上たっても保守されていない（worker が動いていない、
    窓が来ない、ジョブが詰まっている）
- `forge doctor` の `repositories.integrity` が FG-GIT-0019（最後の fsck が失敗したリポジトリがある）
- `forge doctor` の `repositories.refs` の warn（ref が `maintenance.refs_warn`（既定 100 000）を超えたリポジトリがある）
- ログ `event=forge.git.maintenance_completed` の `outcome=failed`（warn、FG-GIT-0018 / 0002 / 0004 など）または `outcome=corrupt`（error、FG-GIT-0019）

## Impact

- FG-GIT-0018（失敗・期日超え）: clone・push・マージは動く。参照されないオブジェクト（判定の木、置き換わったマージのコミット、
  force push で外れた履歴、**秘密の除去で Purge した版のオブジェクト**）が消えず、ディスクが増え、pack がまとまらず遅くなる。SEV4（ディスクが
  逼迫していれば SEV3）。
- FG-GIT-0019（破損）: ref から届くオブジェクトが無い・読めない。そのリポジトリの clone・取得・マージ・閲覧が失敗しうる。
  **データの消失の可能性** として SEV2（複数のリポジトリなら SEV1 を検討）。
- ref の数の warn: push のたびの ref の読み取り（`ref_snapshot`）が遅くなる。今は測るだけ（push の拒否は実測の後、計画 11 Q5）。

## Diagnose（読み取りのみ）

1. `forge doctor --json` の `repositories.maintenance` / `repositories.integrity` / `repositories.refs` の `context`（`repo_ids` など）。
2. ログを `request_id`（ジョブの）と `repo_id` で読む:
   `event=forge.git.maintenance_started`（`fsck_mode`）→ `forge.git.maintenance_completed`（`outcome`、`error_code`、`step`、`context` に
   `exit_code` と git の `stderr` の先頭、破損なら `mode`・`findings`・`first_finding`）または `forge.git.maintenance_skipped`（`reason`:
   `incoming_push`、`gc_running`、`not_ready`）。古い quarantine は info の `forge.git.maintenance_stale_quarantine`。
3. 台帳（読み取りのみ）:
   ```sql
   SELECT repo_id, outcome, error_code, skipped_reason, failures, objects, size_pack_kib, refs,
          last_fsck_mode, fsck_ok, last_fsck_at, last_full_fsck_at, last_started_at, last_finished_at, next_due_at
   FROM forge.repository_maintenance WHERE repo_id = <id>;
   ```
   ジョブ: `SELECT id, state, attempts, last_error_code, created_at FROM forge.jobs WHERE kind = 'repository.maintain' ORDER BY id DESC LIMIT 20;`
4. パス: `forge admin repo-path <repo_id> --storage-root <保存先>`。そのうえで（読み取りのみ）:
   - `git --git-dir=<path> count-objects -v`（`count`・`size`（loose）、`in-pack`、`size-pack`、`garbage`）
   - `git --git-dir=<path> fsck --connectivity-only --no-dangling`（破損の確認。`--full` で中身も）
   - `git --git-dir=<path> cat-file -t <id>`（findings の各 ID）
   - `ls -la <path>` で `gc.pid`、`packed-refs.lock`、`refs/**/*.lock`、`objects/tmp_objdir-incoming-*` の有無と時刻
5. 期日超え（`overdue`）: worker が動いているか（`forge doctor` の `jobs.*`、ログ `forge.worker.maintenance_jobs` が起動時に出ているか）、
   窓の設定（`maintenance.window_start` / `window_end` / `window_time_zone`）が意図どおりか、`repository.maintain` のジョブが `queued` の
   まま溜まっていないか（同時に生きている保守のジョブは `maintenance.batch_max` 件まで）。

## Mitigate

1. **失敗（FG-GIT-0018）**: `stderr` に従って原因を直す。
   - 空き容量・権限 → Runbook `repo-storage`。
   - 時間切れ（FG-GIT-0002 `step=gc` など）が続く大きなリポジトリ → `[MUTATING — needs approval]` `maintenance.step_timeout_secs` を上げる
     （上限 3 000 秒。設定を変えて worker を再起動。戻し: 元の値に戻して再起動）。
   - 残ったロックのファイル: `gc.pid` は 12 時間たてば git が自分で無視する（消さない）。`packed-refs.lock` / `<ref>.lock` が **最も長い push
     （`git.http.timeout_ms`、既定 1 時間）より古く**、その時刻に動いている git が無い（`pgrep -af git` / タスクマネージャー）ときだけ、
     `[MUTATING — needs approval]` 別名へ移す（`mv <lock> <lock>.stale-<日付>`。戻し: 元の名前へ戻す）。
   - 直したら `[MUTATING — needs approval]` `forge admin repo maintain <repo_id>`（保守のジョブを 1 件積むだけ。窓の外でもすぐ動く。
     戻し: 不要。保守は何度動かしても同じ結果になる）。
2. **破損（FG-GIT-0019）**: **gc・prune・`git repack -d` をしない**（取り戻せたかもしれないオブジェクトを消す）。保守のジョブ自身も、
   fsck が通るまで gc をしない。
   1. `/incident` で障害として扱い、findings の ID を記録する（中身は記録しない）。
   2. 失われたオブジェクトを持つ写しを探す: 開発者の clone（`git cat-file -t <id>`）、GitLab のミラー（O2）、最後のバックアップ（O1）。
   3. `[MUTATING — needs approval]` 写しからオブジェクトを戻す（例: 写しで `git pack-objects --stdout <<< <id の一覧>` を作り、forge の
      リポジトリで `git --git-dir=<path> index-pack --stdin --fix-thin` に渡す）。戻し: 追加した pack を別名へ移す（追加しただけで既存は変えない）。
   4. 戻せないとき（写しが無い）は、そのリポジトリの所有者と相談し、失われた範囲を記録する。
   5. `[MUTATING — needs approval]` `forge admin repo maintain <repo_id>` で fsck をやり直す（全体の fsck になる）。
3. **期日超え**: worker を動かす / 窓の設定を直す（`[MUTATING — needs approval]` 設定の変更と再起動。戻し: 元の設定）。急ぐリポジトリは
   `forge admin repo maintain <repo_id>`。
4. **ref の数の warn**: 何もしなくてよい（測るだけ）。急に増えたリポジトリは、版の ref（`refs/merge-requests/<iid>/v<n>`）の増え方（force push
   の多い MR）か、自動化が tag を作り続けていないかを所有者に確かめる。

## Verify

- 台帳の `outcome = 'succeeded'`、`failures = 0`（破損の後は `fsck_ok = true`、`last_fsck_mode = 'full'`）。
- `forge doctor` の `repositories.maintenance` / `repositories.integrity` が ok。
- ログに同じ `repo_id` の `forge.git.maintenance_completed outcome=succeeded`（`objects`、`refs`、`duration_ms`）。

## Escalate

- 破損が複数のリポジトリで見つかった、またはストレージの I/O エラーがある → インフラの担当と `/incident`（SEV1 を検討）。
- 同じリポジトリで `failed` が続き、`stderr` から原因が分からない → 開発者（Claude）に `request_id`、台帳の行、`count-objects -v` を渡す。

## 関連

- ロックのキー `repo_maint:<repo_id>`（`hashtextextended('repo_maint:' || <repo_id>, 0)`）は **O1（バックアップ）のために予約** している。
  O3 は取らない（保守と同じリポジトリのマージを分けるのは `merge:<repo_id>`）。
- 参照されないオブジェクトと猶予: Runbook `merge-request-mergeability`。秘密の除去: Runbook `secret-removal`。
