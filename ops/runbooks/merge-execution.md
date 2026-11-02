# Runbook: merge-execution（FG-REVIEW-0009。関連: FG-REVIEW-0006 / 0007 / 0008 / 0011、FG-DB-0006、FG-GIT-0002 / 0003 / 0004）

M3 R4（ADR-0024 §4）の MR のマージの実行（ジョブ `merge_request.merge`、表 `forge.merge_request_merges`）で、
**ref が動いたかどうか・DB が追いついたかどうか** を確かめる手順。マージの行（マージの試行）の読み方と照合（reconciliation）。

## Trigger

- ログに `FG-REVIEW-0009` MergeExecutionFailed（alert）が出た:
  - `reason=update_ref_failed`: `git update-ref` が失敗した（target は `expected_old` のまま、`<ref>.lock` も無い）、ref を読み直せない、
    または git の時間切れ。行は `executing` のまま。
  - `reason=record_failed`: ref は動いた（直前に `forge.review.merge_applied` が出ている）が、記録のトランザクション（tx2: `applied`・
    MR の `merged`・監査・`repository.pushed`・`merge_request.merged`）が失敗した。行は `executing` のまま。
  - `reason=ref_state_unknown`: 照合が `executing` の行を見て、target が `new_sha`（とその子孫）でも `expected_old` でもなかった。行は `failed`。
  - `reason=merge_request_not_open`: ref は動いたが、記録の時点で MR が開いていなかった（不変条件の破れ）。行は `failed`。
  - `reason=reconcile_overdue`: `forge doctor` の `review.merges` が、照合の期限を 3 分以上過ぎた生きた試行を数えた（照合が動いていないか、
    毎回失敗している）。
- `FG-SYS-0005` ServeFailed（`operation=merge_task_exited`、`task=merge_runner|merge_reconcile`）: serve のマージのタスクが止まった・panic した
  （監督のタスクが起動し直す）。
- `forge.review.merge_retry`（warn、`reason=ref_locked|lock_lost`）が同じ `merge_id` で繰り返し出る、ジョブがデッドレター（FG-DB-0006、
  `kind=merge_request.merge`）になった。
- 利用者から「マージがいつまでも終わらない」（`gstn mr merge-status` が `queued` / `prepared` / `executing` のまま）と問い合わせがあった。

## Impact

- `executing` / `failed` の行がある MR は、終わるまで次のマージ・クローズ・target の変更・ドラフトの切り替えが FG-REVIEW-0011 で止まる。
- `record_failed` のままだと、ref は動いたのに MR が `open` のまま、`repository.pushed` が出ていない（ミラー・連携が取りこぼす）。
  照合が追いつけば自動で直る。1 件なら SEV3、多くのリポジトリで続くなら SEV2（DB か git の障害を先に見る）。
- 閲覧・push・ほかの MR は影響を受けない。マージは force にも削除にもならない（新しい target は必ず古い target の子孫。ADR-0024 §4.1）。

## マージの行の読み方（状態）

| state | 意味 | ref | 次に起きること |
|---|---|---|---|
| `queued` | 要求を受け付けた。ジョブが積まれた | 動いていない | ジョブがロックを取り、再判定（tx1a）→ オブジェクト作成 → tx1b |
| `prepared` | `merge-tree` / `commit-tree` が済み、`expected_old`（試しのマージに使った target の先端）と `new_sha`（target を動かす先）が確定 | 動いていない | フェンス（`prepared → executing`）を取った worker だけが ref を動かす |
| `executing` | フェンスを取った（`executing_at`）。ref を動かしたかもしれない | **git に聞くまで分からない** | `update-ref` の CAS → tx2。失敗・停止なら照合が git に聞いて片付ける |
| `applied` | 終端。ref が動き、MR は `merged`、イベントも出た | 動いた | （`delete_source_branch` なら `source_outcome` が埋まる） |
| `superseded` | 終端。ref は動いていない（判定で止まった、衝突、CAS の負け、古い要求） | 動いていない | 利用者が新しい head でもう一度マージする |
| `failed` | 終端。人が判断する（照合で ref の状態が説明できなかった） | 不明 | この Runbook の Mitigate 3 |

`reason` と `error_code` が止まった理由（例 `superseded` + `FG-REVIEW-0008` + `target_moved`、`FG-REVIEW-0006` + `conflict`、
`FG-AUTH-0003` + `credential_revoked`、`stale`（照合が古い `queued` / `prepared` を終わらせた）、`job_failed`（ジョブが最後の試行で失敗した））。

## Diagnose（読み取りのみ）

0. `forge doctor --json` の `review.merges`（期限を過ぎた生きた試行の数と最も古いものの経過秒、直近 7 日の `failed`）。serve のログの
   debug のハートビート `forge.serve.merge_reconcile_pass`（照合の 1 回ごと、件数つき。debug を出していれば）と、
   `forge.serve.merge_runner_stopped` / `FG-SYS-0005 merge_task_exited`。

1. ログを `merge_id` と、要求の `request_id`（マージを要求した HTTP の要求の ID。ジョブのログもこの ID を持つ）で読む:
   `forge.review.merge_started` → `forge.review.merge_applied`（ref が動いた直後に **先に** 出る）→ `forge.review.merge_completed`（`outcome`）。
   `merge_applied` があれば ref は動いた。無ければ（`update-ref` の前に止まったなら）動いていない。
2. 行を読む: `SELECT id, merge_request_id, state, method, reason, error_code, head_sha, expected_old, new_sha, executing_at, source_outcome, created_at, updated_at, finished_at FROM forge.merge_request_merges WHERE id = <merge_id>;`
   MR ごとの最新: `... WHERE merge_request_id = <mr_id> ORDER BY id DESC LIMIT 3;`（`gstn mr merge-status --project <id> --iid <iid> --json` でも同じ値）。
3. ジョブを読む: `SELECT id, state, attempts, last_error_code, last_error FROM forge.jobs WHERE kind = 'merge_request.merge' AND dedup_key = 'mr_merge:<merge_id>';`
4. target の先端を読む（読み取り）: `git --git-dir=<storage>/<repo の置き場> rev-parse --verify <target_branch>`。
   祖先かどうか: `git --git-dir=<…> merge-base --is-ancestor <new_sha> <tip>`（終了コード 0 = `new_sha` は祖先）。
5. ロックのファイル: `<repo>/refs/heads/<…>.lock` があるか（`update-ref` が `ref_locked` で再試行している）。

## 照合（reconciliation）の動き

`forge serve` が 60 秒ごとに（マージのジョブと同じプロセスの、ジョブとは別のタスクで。1 回 50 秒まで）古い行を片付ける
（`Merger::reconcile`）。どの行もリポジトリのマージのロック（`merge:<repo_id>`）の下で、行を読み直してから決める。ロックが
別のマージに使われていれば（FG-DB-0002 `merge_lock_busy`）その行は次の回へ回し、ほかの失敗は `error_code` つきで 1 回ログに
出す。どちらも、その行を次の走査の後ろへ回す（`reconcile_seen_at`）:

- `queued` / `prepared` で 10 分更新が無い → `superseded`（`reason=stale`）。フェンス（`prepared → executing`）を通っていないので、
  この行の worker は ref を動かしていない（まだ生きていても、フェンスの比較して交換に負けて止まる）。
- `executing` で `executing_at` から 10 分 30 秒（照合の余裕 10 分 + `update-ref` の時間切れ 30 秒）を過ぎた → **git に聞く**:
  - target の先端が `new_sha`、または `new_sha` を祖先に持つ → tx2 をやり直して `applied`（イベントは `merge_id` で dedup。2 回目は書かない）。
  - 先端が `expected_old` のまま → ref は動いていない → `superseded`（`reason=not_applied`）。
  - どちらでもない → `failed`（FG-REVIEW-0009 `reason=ref_state_unknown`、alert）。人が判断する（Mitigate 3）。
- worker は `update-ref` を起動する直前に `now − executing_at` を見て、余裕から時間切れを引いた値（9 分 30 秒）を過ぎていれば起動しない
  （照合が触りうる時刻に ref を動かさない）。

## Mitigate

1. `update_ref_failed` / `record_failed`: **待つ**。ジョブの再試行と照合が、上のとおり git に聞いて片付ける。原因（DB の障害なら
   `db-unavailable`、git なら `git-process-failed`、置き場なら `repo-storage`）を先に直す。行を手で書き換えない。
2. `ref_locked` が続く: 他の git の処理（手作業、壊れた push）が `<ref>.lock` を残している。`[MUTATING — needs approval]` その ref を
   書いているプロセスが無いことを確かめてから、ロックのファイルを消す（戻し: 不要。消すのは中身の無いロックのファイルだけ）。
3. `ref_state_unknown`（`failed`）: 誰かがマージの最中に target を別の値に動かした（force push、手作業）。target の履歴（`git log <target>`）と
   `new_sha` を比べ、利用者（MR の作成者とマージした人）と相談する。`[MUTATING — needs approval]` 必要なら target を戻すか、
   改めて MR をマージし直す（新しい試行）。行は `failed` のまま残す（記録）。
4. デッドレター（FG-DB-0006）: 最後の試行で失敗したジョブは、フェンスを通っていない行を `superseded`（`reason=job_failed`）にする。
   残った `queued` / `prepared` の行は照合が 10 分後に `superseded` にする。利用者はもう一度マージを要求する。
5. `merge_request_not_open`（`failed`）: ref は `new_sha` に動いている。MR がどう閉じられたか（監査の `merge_request.*` の行）を確かめ、
   利用者と相談する。`[MUTATING — needs approval]` 必要なら target を戻す（`git update-ref <target> <expected_old> <new_sha>`）。
   戻し: もう一度 `new_sha` に動かす。
6. `FG-REVIEW-0012`（作成者のメールが使えない）は alert ではない。利用者のメールを直してから、もう一度マージする。

## Verify

- `gstn mr merge-status --project <id> --iid <iid>` が `applied` で、`gstn mr view` が `merged`。
- `git rev-parse <target>` が行の `new_sha`（またはその子孫）。
- `SELECT count(*) FROM forge.merge_request_merges WHERE state IN ('queued','prepared','executing') AND updated_at < now() - interval '15 minutes';` が 0。

## Escalate

- `failed`（`ref_state_unknown`）は必ず人が判断する（Maintainer とマージを要求した人）。
- 照合が動かない（`forge doctor` の `review.merges` が `reconcile_overdue`、serve のログに `FG-SYS-0005 merge_task_exited` や
  `operation=merge_reconcile` の失敗が続く。照合が何かを片付けた回は info の `forge.review.merge_reconciled` も出る）・同じ行が残り
  続ける: Claude（保守担当）へ、
  `merge_id`・`request_id`・行の値・target の先端を添えて。
