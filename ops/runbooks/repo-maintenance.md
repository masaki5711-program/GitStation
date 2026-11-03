# Runbook: repo-maintenance（FG-GIT-0018 / FG-GIT-0019 / FG-GIT-0020、リポジトリの定期の保守）

M3 O3（計画 11 §5 O3、ADR-0024「実装で決めたこと（O3）」）。`forge worker` が各リポジトリを週に 1 回、夜の窓（既定 02:00〜05:00、
`Asia/Tokyo`）の中で保守する。保守は `repository.maintain` のジョブで、**worker の専用のランナー** が 1 件ずつ走らせる（作成・版・
マージできるかの判定のジョブを待たせない）。1 回の保守の手順:

1. push の受け入れの途中（`objects/tmp_objdir-incoming-*` が 6 時間より新しい）なら **何もしない**（`skipped incoming_push`、1 時間後にまた）。
   fsck の後、gc の直前にもう一度確かめる。6 時間より古い quarantine は落ちた push の残りで、保守を止めず、gc の prune が猶予（48 時間）を
   過ぎたところで中のファイルごと消す（git 2.53 で確かめた）。
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
**git の子プロセス（`repack` / `pack-objects`）ごと** 止まる（FG-GIT-0002 `step=<手順>`）。Unix ではまずプロセスグループに `SIGTERM`
（git がロックのファイルを消して終わる）、5 秒待って `SIGKILL`（`stop=term|kill`）。Windows には穏やかな止め方が無い（`stop=kill`）。
pack-refs が **殺された**（`stop=kill`。Windows は常にこれ）ときだけ、マージのロックを持ったまま、時刻が **手順の開始から kill の時刻の間** の
`packed-refs.lock` を消す（自分のもの。`forge.git.maintenance_lock_removed`）。その外のもの（手順より古い、kill の後にできた）は残し、
FG-GIT-0020 で知らせる（`forge.git.maintenance_lock_left`）。pack-refs が自分で終わった（どの終了コードでも）か `SIGTERM` で終わったときは
git が自分のロックを消しているので、**何も消さない**（そのときあるロックは push・`update-ref` のもの。マージのロックは書く側を止めない）。
残り続ければ doctor の `repositories.stale_locks` が知らせる。残る隙: pack-refs がロックを待っている間（最大 10 秒）に他の書き手が取り、
その後に kill されたときは、そのロックも窓の中に入る（Unix では kill は `SIGTERM` を 5 秒無視した後だけ、Windows は開発機だけ）。

**pack-refs の長さ（測定、2026-11-03、Windows の開発機、git 2.53）**: loose の ref 10 000 本で 5.8 秒、100 000 本で 56〜70 秒。
そのうち `packed-refs.lock` を持つのは 100 000 本で約 6.6 秒（残りは loose の ref のファイルを 1 本ずつ消す時間で、その間は各 ref の
ロックを一瞬ずつ取るだけ）。ref を書く側（`update-ref`、receive-pack、pack-refs 自身）は `core.packedRefsTimeout=10000`（10 秒）まで
待つので、loose の ref が 100 000 本を超える規模ではこの上限に近い — `repositories.refs` の warn（既定 100 000）がその目安。マージは
マージのロックで pack-refs と重ならない。push が 10 秒待っても取れなければ、その push は失敗する（利用者がやり直せば通る）。

## Trigger

- `forge doctor` の `repositories.maintenance` が FG-GIT-0018
  - `reason=failing`: 同じリポジトリの保守が **2 回続けて** 失敗した（`repositories`、`max_consecutive`、`repo_ids`、`last_error_code`）
  - `reason=overdue`: ready のリポジトリが、期日から保守の間隔（既定 7 日）以上たっても保守されていない（worker が動いていない、
    窓が来ない、ジョブが詰まっている）
- `forge doctor` の `repositories.integrity` が FG-GIT-0019（最後の fsck が失敗したリポジトリがある）
  - `reason=stale_running`: 台帳が `running` のまま、ジョブのリース（1 時間 + 30 秒）を過ぎた（落ちた実行。スケジューラーが毎回、
    生きたジョブの無い行を `failed`・FG-DB-0007・失敗の回数 + 1 にする）
  - `reason=no_recent_success`: 間隔の 2 倍（既定 14 日）成功していない（毎晩 `skipped` になる・失敗し続けるリポジトリ）
- `forge doctor` の `repositories.stale_locks` が FG-GIT-0020（`packed-refs.lock` が 10 分より古い。`repositories`、`max_age_secs`、`repo_ids`）、
  またはログ `event=forge.git.maintenance_lock_left`（error）
- `forge doctor` の `repositories.refs` の warn（ref が `maintenance.refs_warn`（既定 100 000）を超えたリポジトリがある）
- ログ `event=forge.git.maintenance_completed` の `outcome=failed`（warn、FG-GIT-0018 / 0002 / 0004 など）または `outcome=corrupt`（error、FG-GIT-0019）

## Impact

- FG-GIT-0018（失敗・期日超え）: clone・push・マージは動く。参照されないオブジェクト（判定の木、置き換わったマージのコミット、
  force push で外れた履歴、**秘密の除去で Purge した版のオブジェクト**）が消えず、ディスクが増え、pack がまとまらず遅くなる。SEV4（ディスクが
  逼迫していれば SEV3）。
- FG-GIT-0019（破損）: ref から届くオブジェクトが無い・読めない。そのリポジトリの clone・取得・マージ・閲覧が失敗しうる。
  **データの消失の可能性** として SEV2（複数のリポジトリなら SEV1 を検討）。
- ref の数の warn: push のたびの ref の読み取り（`ref_snapshot`）が遅くなる。今は測るだけ（push の拒否は実測の後、計画 11 Q5）。
- FG-GIT-0020（残った `packed-refs.lock`）: 消えるまで、packed の ref を消す push（ブランチ・タグの削除、版の ref の Purge）と、それを要る
  `update-ref` がすべて 10 秒待って失敗する。保守の pack-refs も失敗する。SEV3（多くのリポジトリなら SEV2）。

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
2. **残ったロック（FG-GIT-0020）**: そのリポジトリで git が動いていない（`pgrep -af '<repo_id>.git'` / タスクマネージャー）ことと、
   ロックの時刻が最も長い push（`git.http.timeout_ms`、既定 1 時間）より古いことを確かめてから、`[MUTATING — needs approval]`
   `mv <path>/packed-refs.lock <path>/packed-refs.lock.stale-<日付>`（戻し: 元の名前へ戻す）。その後 `git --git-dir=<path> show-ref | head`
   と `forge admin repo maintain <repo_id>`。動いている git があるうちは消さない。
3. **破損（FG-GIT-0019）**: **gc・prune・`git repack -d` をしない**（取り戻せたかもしれないオブジェクトを消す）。保守のジョブ自身も、
   fsck が通るまで gc をしない。
   1. `/incident` で障害として扱い、findings の ID を記録する（中身は記録しない）。
   2. 失われたオブジェクトを持つ写しを探す: 開発者の clone（`git cat-file -t <id>`）、GitLab のミラー（O2）、最後のバックアップ（O1）。
   3. `[MUTATING — needs approval]` 写しからオブジェクトを戻す（例: 写しで `git pack-objects --stdout <<< <id の一覧>` を作り、forge の
      リポジトリで `git --git-dir=<path> index-pack --stdin --fix-thin` に渡す）。戻し: 追加した pack を別名へ移す（追加しただけで既存は変えない）。
   4. 戻せないとき（写しが無い）は、そのリポジトリの所有者と相談し、失われた範囲を記録する。
   5. `[MUTATING — needs approval]` `forge admin repo maintain <repo_id>` で fsck をやり直す（全体の fsck になる）。
   **古いオブジェクトの形の問題（legacy）**: `--full` の fsck は、昔の git や他の道具が作った形の崩れたオブジェクト（例 `badTimezone`、
   `zeroPaddedFilemode`、`missingSpaceBeforeDate`）を error として数える。中身は失われていないので取り戻す必要はない。`stderr` の
   `error in <type> <id>: <msg-id>: …` を確かめ、移行してきたリポジトリの既知の古い履歴なら `[MUTATING — needs approval]`
   `git --git-dir=<path> config fsck.<msg-id> ignore`（例 `fsck.zeroPaddedFilemode ignore`。戻し: `git config --unset fsck.<msg-id>`）。
   push の受け入れ（`receive.fsckObjects`）は別の設定で、新しい崩れたオブジェクトは今まで通り断る。その後 `forge admin repo maintain`。
   fsck が exit 128 で「not a git repository」「dubious ownership」なら破損ではなく FG-GIT-0018 `reason=fsck_unusable`（パス・所有者・
   権限を直す。Runbook `repo-storage`）。
4. **期日超え**: worker を動かす / 窓の設定を直す（`[MUTATING — needs approval]` 設定の変更と再起動。戻し: 元の設定）。急ぐリポジトリは
   `forge admin repo maintain <repo_id>`。
   **`no_recent_success`**: 台帳の `skipped_reason` を見る。`incoming_push` が続くなら、その時刻に長い push か落ちた push の残り
   （6 時間より新しい quarantine）がある。`gc_running` が続くなら `gc.pid` の pid とホストを確かめる（12 時間より古いものは git が無視する）。
   **`stale_running`**: worker のログで同じ `repo_id` の `forge.git.maintenance_run_lost`（FG-DB-0007。2 回続けば error）を探す（落ちた実行。
   次の夜に再試行）。**一時的な誤報**: キューの reaper がリースの切れたジョブを戻し、再試行がまだ始まっていない間（最大でバックオフの時間）は、
   生きたジョブがあっても doctor はこの理由を出す。次の `forge doctor` で消えていれば対処は要らない。
5. **ref の数の warn**: 何もしなくてよい（測るだけ）。急に増えたリポジトリは、版の ref（`refs/merge-requests/<iid>/v<n>`）の増え方（force push
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
