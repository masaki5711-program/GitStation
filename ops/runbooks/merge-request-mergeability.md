# Runbook: merge-request-mergeability（FG-REVIEW-0003。関連: FG-DB-0006、FG-GIT-0002 / 0003 / 0004）

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
2. 行を読む: `SELECT merge_request_id, status, reason, check_seq, checked_head_sha, checked_target_sha, checked_at, updated_at FROM forge.merge_request_mergeability WHERE merge_request_id = <id>;`
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

## Escalate

- worker が動いていない、または同じ MR で `job_failed` が繰り返す → 開発者（Claude）に `request_id` と行の内容を渡す。
- ディスクの空きが 20% を切った → インフラの担当に連絡し、上の `forge admin repo maintain` を承認つきで行う（保守の失敗・期日超えは
  Runbook `repo-maintenance`）。
