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
4. `forge doctor --json` の `review.mergeability`（`checking_stale`、`unchecked_unqueued` の件数）と `jobs.*`（worker が動いているか）。
5. 判定の成功は `forge.review.mergeability_checked`（info。衝突なら件数と最大 100 件のパス）、決まらない結果（`check_failed`）は
   同じイベントを warn で出す（`reason`）。

## 参照されないオブジェクト（ディスクの増加、秘密の残り）

- 判定のたびに `git merge-tree --write-tree` が **どの ref からも参照されない木と blob** をリポジトリに書く。forge のリポジトリは
  `gc.auto=0` で、到達できないオブジェクトを猶予つきで消す定期の保守は **計画 11 の O3 で入れる予定で、まだ無い**。それまで、
  判定の回数に比例してオブジェクトが増える。
- 確かめ方（読み取りのみ）: `git --git-dir=<repo> count-objects -v`（`count` と `size` の増え方）。
- `[MUTATING — needs approval]` 手での削除: `git --git-dir=<repo> prune --expire=<猶予>`（猶予は 24 時間以上。照合中のジョブの
  オブジェクトを消さないため。例 `--expire=2.days.ago`）。戻し: できない（消したオブジェクトは戻らない。判定は次のジョブが作り直す）。
  本番では利用者の承認のうえで、push の少ない時間に 1 リポジトリずつ行う。
- **秘密の除去（`secret-removal`）との関係**: 版の ref を Purge しても、その前に判定が書いた木・blob（秘密の中身を含みうる）は
  参照されないまま残る。秘密の除去の手順の prune（Runbook `secret-removal`）は、この判定の残りも同じく消す。閲覧の API は
  参照されないオブジェクトを ID で読ませない（ADR-0024 §2 の到達可能性）。

## Mitigate

1. 原因（ジョブの失敗のコード）を、そのコードの Runbook で直す。
2. 判定のやり直しは、その MR の次の push・target の変更・再オープンで起きる。worker の定期の見回り（30 秒ごと）が、生きている
   ジョブの無い `unchecked` の行を最大 50 件ずつ確かめ直す。`check_failed reason=job_failed|job_stale` の行を `unchecked` に戻すには
   `[MUTATING — needs approval]` 利用者がその MR の target を一度変えて戻すか、ソースに push する（戻し: 不要。判定が書き直す）。
3. 表を手で書き換えない（承認された手順の外では行わない）。

## Verify

- `gstn mr mergeability --project <id> --iid <iid>` が `can_be_merged` / `cannot_be_merged` になり、`checked_head_sha` が MR の `head_sha` と同じ。
- `forge doctor` の `review.mergeability` が ok。

## Escalate

- worker が動いていない、または同じ MR で `job_failed` が繰り返す → 開発者（Claude）に `request_id` と行の内容を渡す。
- ディスクの空きが 20% を切った → インフラの担当に連絡し、上の prune を承認つきで行う。
