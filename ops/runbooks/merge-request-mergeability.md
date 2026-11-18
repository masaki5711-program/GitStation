# Runbook: merge-request-mergeability（FG-REVIEW-0003、FG-REVIEW-0022。関連: FG-DB-0006、FG-GIT-0002 / 0003 / 0004）

M3 R2（ADR-0024 §3）の「マージできるか」の判定（ジョブ `merge_request.mergeability`、表 `forge.merge_request_mergeability`）が
決まらない・遅れるとき、および判定が残す参照されないオブジェクトでディスクが増えるとき。

## Trigger

- `forge worker` のログに `FG-REVIEW-0003` MergeabilityNotChecked（warn、`forge.review.mergeability_not_checked`）が出た:
  `reason=job_failed`（ジョブが結果を保存する前に最終的に失敗した）、`reason=job_stale`（`checking` のまま、生きているジョブが無い行を
  worker の定期の見回りが終わらせた）、`reason=fanout_deferred`（1 回の target への push で、上限を超えた MR の印も付けきれなかった）
- `forge doctor` の `review.mergeability` が warn（`checking` のまま古い行、生きているジョブの無い `unchecked` の行がある）
- 利用者から「MR がいつまでも『確認中』」「マージできるはずなのに `check_failed`」と問い合わせがあった（`gstn mr mergeability`）
- リポジトリの置き場のディスクが、push の量に比べて増え続ける（下の「参照されないオブジェクト」）

## Impact

- 判定が決まらない MR は `mergeability_unknown` でマージが止まる（成功にしない設計。R4 以降）。画面と `gstn mr mergeability` は
  `unchecked` / `checking` / `check_failed` を出す。閲覧・push は影響を受けない。
- 1 件なら SEV4。多くの MR で続く、または worker が止まっているなら SEV3（worker の Runbook も見る）。
- ディスクの増加は、放っておくと置き場がいっぱいになり push が失敗する（SEV2 になりうる）。

## Diagnose（読み取りのみ）

1. ログを `forge.review.mergeability_not_checked` と、同じ `request_id` の前後の行で読む。`job_failed` ならジョブの実行器が
   原因の失敗を先にその失敗自身の `error_code` で記録している（`kind=merge_request.mergeability`、`job_id`）→ その Runbook
   （`git-process-failed`、`db-unavailable`、`repo-storage` など）。
2. 行を読む: `SELECT merge_request_id, status, reason, check_seq, checked_head_sha, checked_target_sha, computed_target_sha, paths_complete, checked_at, updated_at FROM forge.merge_request_mergeability WHERE merge_request_id = <id>;`
   `computed_target_sha` が `checked_target_sha` と違う行は **持ち越した結果**（下の「着地で持ち越した結果」）。
3. ジョブを読む: `SELECT id, state, attempts, last_error_code, last_error, created_at FROM forge.jobs WHERE kind = 'merge_request.mergeability' AND dedup_key LIKE 'mr_mergeability:<id>:%' ORDER BY id DESC LIMIT 5;`
   最新の `check_seq` の鍵（`mr_mergeability:<id>:<check_seq>`）のジョブが `queued` / `running` なら待つだけでよい。
4. `forge doctor --json` の `review.mergeability`（`checking_stale`、`unchecked_unqueued`、`decided_stale_head`（head が違う決定）、
   `decided_unverified`（1 時間確かめられていない決定）の件数）と `jobs.*`（worker が動いているか）。
5. 判定の成功は `forge.review.mergeability_checked`（info。衝突なら件数と最大 100 件のパス）、決まらない結果（`check_failed`）は
   同じイベントを warn で出す（`reason`）。

## 参照されないオブジェクト（ディスクの増加、秘密の残り）

- 判定のたびに `git merge-tree --write-tree` が **どの ref からも参照されない木と blob** をリポジトリに書く。forge のリポジトリは
  `gc.auto=0` で、これを消すのは **定期の保守（M3 O3、Runbook `repo-maintenance`）** の `git gc --cruft --prune=<猶予>` だけ。猶予
  （`maintenance.prune_grace_hours`、既定 48 時間）は 24 時間以上で、マージの照合の窓とマージのジョブの再試行の全体（今の既定で約
  51 分）より長いことを worker が起動時に確かめる（照合の途中の `new_sha` を消さない）。保守は各リポジトリに週 1 回、夜の窓の中。
- 確かめ方（読み取りのみ）: `git --git-dir=<repo> count-objects -v`（`count` と `size` の増え方）と、台帳
  `SELECT objects, size_pack_kib, last_finished_at, outcome FROM forge.repository_maintenance WHERE repo_id = <id>`。
- 急いで消したいとき（ディスクの逼迫）: `[MUTATING — needs approval]` `forge admin repo maintain <repo_id>`（保守のジョブを 1 件積む。
  窓の外でもすぐ動き、猶予より新しいオブジェクトは残す。戻し: 不要）。手での `git prune` は使わない（猶予と push の途中の確かめを
  通らない）。
- **承認のコミッターの規則への影響**: push の記録の先端（一覧の切れた push の `new`）のコミットが保守の prune で消えると、そのコミットに
  どの ref も向いていなければ「範囲に届かない」とみなす（`cat-file --batch-check` で確かめる）。どの ref も向いていない先端は、猶予より
  古いときだけ消える。
- 保守の pack-refs と push・`update-ref` は `packed-refs.lock` を取り合う（書く側は 10 秒まで待つ）。残ったロック（FG-GIT-0020）は
  Runbook `repo-maintenance`。
- **秘密の除去（`secret-removal`）との関係**: 版の ref を Purge しても、その前に判定が書いた木・blob（秘密の中身を含みうる）は
  参照されないまま残る。秘密の除去の手順の prune（Runbook `secret-removal`）は、この判定の残りも同じく消す。閲覧の API は
  参照されないオブジェクトを ID で読ませない（ADR-0024 §2 の到達可能性）。

## Mitigate

1. 原因（ジョブの失敗のコード）を、そのコードの Runbook で直す。
2. 判定のやり直しは、その MR の次の push・target の変更・再オープンで起きる。worker の定期の見回り（30 秒ごと）が、生きている
   ジョブの無い `unchecked` の行を最大 50 件ずつ確かめ直し、決まっている行も最大 50 件ずつ target の生きた先端と比べて、
   動いていれば確かめ直す（`confirmed_at` が古い順）。`check_failed reason=job_failed|job_stale` の行を `unchecked` に戻すには
   `[MUTATING — needs approval]` 利用者がその MR の target を一度変えて戻すか、ソースに push する（戻し: 不要。判定が書き直す）。
3. 表を手で書き換えない（承認された手順の外では行わない）。

## Verify

- `gstn mr mergeability --project <id> --iid <iid>` が `can_be_merged` / `cannot_be_merged` になり、`checked_head_sha` が MR の `head_sha` と同じ。
- `forge doctor` の `review.mergeability` が ok。

## MR 同士の衝突の予測（FG-REVIEW-0022、M3 X6）

衝突の予測（計画 04 C-2、ADR-0024 Amendment X6）は、MR の新しい head・target の変更・再オープンのたびにジョブ
`merge_request.conflict_scan` が、同じ target に向けて開いている他の MR（下書きを含む）と比べる。まず変更ファイルの重なりで絞り、
重なる組だけ `merge-tree` を試す。予測は **助言** で、マージを止めない（マージを止めるのは C-1 の判定）。

- **Trigger**: ログの `forge.review.conflict_scan_incomplete`（`reason=peers_capped|pairs_capped|budget_exhausted|pair_failed`）、
  `forge.review.conflict_scan_not_finished`（`reason=job_failed|job_stale`）。`gstn conflicts --project <id> --iid <iid>` の
  scan が `incomplete` / `failed`。
- **Diagnose（読み取りのみ）**:
  - `SELECT merge_request_id, status, reason, scan_seq, scanned_head_sha, peers, pairs_tried, pairs_skipped, updated_at
    FROM forge.merge_request_conflict_scans WHERE merge_request_id = <id>;`
  - `SELECT low_mr_id, high_mr_id, outcome, reason, prediction_seq, checked_at FROM forge.merge_request_conflict_pairs
    WHERE <id> IN (low_mr_id, high_mr_id);`
  - `pair_failed` は同じ `request_id` で git のコード（FG-GIT-0002 / 0003 / 0004）が出ている。そのコードの手順で直す（ジョブは再試行する）。
  - `peers_capped`（50 件）・`pairs_capped`（20 組）・`budget_exhausted`（120 秒）は上限による。target に開いている MR が多いと起きる。
    試さなかった組は「衝突なし」とは表示しない（予測が無いだけ）。
- **Mitigate**: 上限は仕様（M3 では設定にしていない）。試さなかった組は、どちらかの MR の次の push で試される。
  `job_failed` / `job_stale` は原因のコードを直す。次の push・target の変更・再オープンで予測し直す。
  `[MUTATING — needs approval]` 予測し直させたいときは、利用者がその MR の target を一度変えて戻す（戻し: 不要）。表を手で書き換えない。
- **Verify**: `gstn conflicts` の scan が `scanned`、`stale: false`、`scanned_head_sha` が MR の `head_sha` と同じ。

## 着地で持ち越した結果（計画 18 S4、ADR-0037 (c)。関連: FG-REVIEW-0006）

target に MR が着地するたびに、開いている MR を全部確かめ直す代わりに、**着地が変えたパスと触れ合いうる MR だけ** を確かめ直す。
残りは結果（`can_be_merged` / `cannot_be_merged`）をそのまま新しい先端に付け替える（持ち越し。1 つの UPDATE、git もジョブも無い）。

- **仕組み**
  - 判定のジョブは、結果と同じ UPDATE で head の変更パスの集合を行に書く（`head_files`、`head_dirs`、`head_deleted_dirs`、
    `paths_complete`）。だから保存された集合は必ず `checked_head_sha` のもの。新しい head が来ると行は `unchecked` に戻り、
    古い集合は使われない（持ち越すのは `checked_head_sha` が MR の今の head と同じ、決まった行だけ）。
  - 着地（キューの列車、通常のマージ）は、動かした範囲のパスを 1 回だけ読み（キューはバッチごとに 1 回）、`repository.pushed` の
    `landed`（`ref`、`old`、`new`、`paths_complete`、`changed_files`、`changed_dirs`、`deleted_parent_dirs`）に書く。
    target への直接の push には `landed` が無く、全件を確かめ直す（S4 の前と同じ）。
  - 購読側 `merge-request-refresh` は、`checked_target_sha` が着地の `old` と同じ行のうち、触れ合いえない行を `new` に付け替える。
    `computed_target_sha` は動かさない（**実際に計算した先端**）。それ以外の MR は今までどおり確かめ直す。
- **「触れ合いうる」（確かめ直す）の規則**: どちらかの集合が無い・不完全。同じファイル。片方のファイルがもう片方のディレクトリ
  （`a` と `a/b`）。片方が何かを消したディレクトリの下を、もう片方が変えた（ディレクトリの削除・改名と、その下への追加。両向き）。
  パスは大文字と小文字を区別せずに比べる（大文字小文字だけ違う名前は触れ合う）。**不完全** になるのは: 上限（`review.paths_max`、
  既定 5000）を超えた、UTF-8 でないパス、シンボリックリンクかサブモジュール、**全体に効くパス**（どこかの `.gitattributes` /
  `.gitmodules`、`.forge/` の下、ルートの `.forge-ci.yml` と `CODEOWNERS`）を含む、パスを読めなかった。不完全な着地は全件を
  確かめ直し、不完全な MR は毎回確かめ直す。
- **持ち越しは表示とキューに入れる条件のためだけ**。マージそのものは必ずもう一度マージする: キューの列車は各 MR を `merge-tree` で
  積み直し、直接のマージはロックの下で試しのマージをする。そこで衝突した MR は `FG-REVIEW-0006`（キューでは `reason=conflict` で
  外れる）になり、**何も着地しない**。持ち越した結果が外れたときは、同じ `request_id` で info
  `forge.review.mergeability_carried_disproved`（`merge_request_id`、`check_seq`）が出て、計算し直しを積む。
- **Trigger**: `FG-REVIEW-0006` が、MR のページで「マージできる」と出ていた MR に出た。利用者から「触れていないはずの変更で
  結果が古い」と問い合わせがあった。
- **Diagnose（読み取りのみ）**
  1. 上の Diagnose 2 の行を読む。`computed_target_sha ≠ checked_target_sha` なら持ち越した結果。MR のページと
     `gstn mr status --json` の `carried_from_target_sha` も同じことを言う。
  2. `FG-REVIEW-0006` の MR がキューで外れたなら、まず **同じ列車の前の MR** と衝突したのかを見る（それは絞り込みの誤りではない。
     キューに並んだ 2 つの変更が初めて出会うのが列車）: `SELECT id, batch_id, state, reason, error_code FROM forge.merge_queue_entries
     WHERE merge_request_id = <id> ORDER BY id DESC LIMIT 3;` と同じ `batch_id` の他のエントリー。
  3. 前の MR が無いのに衝突したなら、絞り込みが「触れ合わない」と誤った。持ち越した着地を探す: worker のログの
     `forge.review.mergeability_carried`（info。`source_seq`、`carried`、`settled`（先の着地の記録で付け替え済みの数）、
     `rechecked`）。その `source_seq` のイベントの `landed` と、MR の行の `head_files` / `head_dirs` / `head_deleted_dirs` を
     比べる: `SELECT payload->'landed' FROM forge.events WHERE seq = <source_seq>;`
     `SELECT head_files, head_dirs, head_deleted_dirs, paths_complete FROM forge.merge_request_mergeability WHERE merge_request_id = <id>;`
     （パスの名前が出る。インシデントの記録には件数と該当のパスだけを書く。）この組を開発者（Claude）に渡す — 規則の穴は
     試験（`may_interact_matches_its_sql_twin` と同じ場所）にしてから直す。
  4. 絞り込みが効いていないとき（着地のたびに全件を確かめ直している）: `forge.review.mergeability_paths_unused`（info。`paths` =
     `incomplete`（着地が不完全: 上限・全体に効くパスなど）/ `rule` / `malformed` / `ref_not_moved`。`landed` が無い記録 —
     直接の push、S4 より前の記録 — は `absent` で debug）、着地の側の warn
     `forge.review.merge_paths_unlisted`（着地のパスを読めなかった。`error_code` つき）と info `forge.review.merge_paths_incomplete`
     （`reason`）、判定の側の info `forge.review.mergeability_paths_incomplete`（`reason`）と warn
     `forge.review.mergeability_paths_unread`（`error_code` つき。結果は保存され、その MR は毎回確かめ直す）。どれも結果を
     間違えない側（確かめ直す）に倒れているので、急ぐ必要はない。
- **知っておくこと**
  - 持ち越した `can_be_merged` には、新しい先端のマージ結果の ref（`refs/merge-requests/<iid>/merge`）が無い。worker の見回り
    （`sweep_missing_merge_results`）は、着地が止まって 300 秒たった行を 1 回計算し直す（着地が続く間は持ち越しのたびに時計が戻る）。
    必須の `ci/pipeline` があるリポジトリでは、それまで直接のマージは FG-CI-0013（マージキューは影響を受けない: 列車が試す）。
  - 持ち越した `cannot_be_merged` は、触れ合う着地か新しい head が来るまで残る（衝突したファイルに触れない着地では直らない、で正しい）。
  - 文字の正規化（NFC / NFD）はそろえない。同じに見える名前でも別のパスとして比べる（列車が見つける）。
- **Mitigate — 絞り込みを切る**: `[MUTATING — needs approval]` サーバーの設定 `review.path_filter = false`
  （`FORGE_REVIEW_PATH_FILTER=false` か設定ファイルの `[review] path_filter = false`）を **`forge serve` と `forge worker` の両方** に
  入れて再起動する（片方だけでも持ち越しは止まる: worker は集合を書かず持ち越さない、serve は `landed` を書かない）。
  **戻し**: 設定を消して（または `true`）再起動。切った後の行は次の着地で普通に確かめ直される。表は書き換えない。
  - **切る代償**: S4 の前と同じに戻る。着地のたびに target の開いている MR を全部（`review.mergeability_fanout_max` まで。超えた分は
    `unchecked` で見回り待ち）確かめ直す。開いている MR が多いリポジトリでは、着地 1 回ごとに MR の数だけ判定のジョブと git の
    起動が走り、キューの後ろで「確認中」が長くなる（2026-11-17 の負荷試験の形）。正しさは変わらない。
  - 上限だけを変えるなら `review.paths_max`（1〜20000、既定 5000）。大きくすると行と `landed` が大きくなる（`landed` は 16 KiB を
    超えると不完全として送る）。
- **Verify**: 切った後は `forge.review.mergeability_carried` が出なくなり、着地の後に各 MR の `check_seq` が進む。戻した後は、
  触れ合わない着地で `forge.review.mergeability_carried` の `carried` が 0 より大きい。

## Escalate

- worker が動いていない、または同じ MR で `job_failed` が繰り返す → 開発者（Claude）に `request_id` と行の内容を渡す。
- ディスクの空きが 20% を切った → インフラの担当に連絡し、上の `forge admin repo maintain` を承認つきで行う（保守の失敗・期日超えは
  Runbook `repo-maintenance`）。
