# Runbook: merge-blocked（FG-REVIEW-0007 の承認の理由。関連: FG-REVIEW-0013 / 0014 / 0015）

M3 R5（ADR-0024 §7・§8）で、承認があるのにマージが止まるときの読み方。とくに **コミッターの規則** で承認が数えられない理由
`committer` / `committers_unknown`（ADR-0024 §8 の 3。再レビュー #40 で「副作用は理由の値で見せる」と決めた）。

## Trigger

- マージの要求が FG-REVIEW-0007 MergeBlocked で、`reasons` に `approvals_missing`・`codeowner_approval_missing`・
  `config_change_needs_maintainer_approval`・`changed_paths_unknown` を含む（`details` に内訳）。
- 利用者から「承認したのに数えられていない」「承認が消えた」と問い合わせがあった。

## Impact

- その MR がマージできない（ほかは影響を受けない）。条件を満たせば自動では動かない: 利用者がもう一度マージを要求する。SEV4 相当。

## 承認が数えられない理由（`gstn mr approvals --json` の `approvals[].uncounted_reason`）

数えるかは **評価の時点**（マージの要求・ジョブの各判定・状態の表示のたび）に決める。承認した時点の事実は使わない。

| uncounted_reason | 意味 | 対処 |
|---|---|---|
| `stale` | 承認が今の head / target のものでない（新しいコミット・target の変更の後。通常は版の記録・target の変更で承認そのものが消える） | 今の head を見てもう一度承認する |
| `inactive` | 承認者が無効化・ロックされた | 別の人の承認 |
| `not_human` | 承認者がサービスアカウント・ボット | 人の承認（承認の Action 自体が人だけ） |
| `role_insufficient` | 承認者のプロジェクトでの今のロールが Developer 未満（所属の削除・引き下げ） | 別の人の承認、またはロールを戻す |
| `author` | MR の作成者（承認の Action が拒むので通常は出ない） | — |
| `committer` | 承認者が **MR の範囲（`head ^<target の先端>`: マージが持ち込むコミット）のコミットを運んだ push** をしたことがある。別名のブランチへの push、MR の作成前の push も含む。一覧が切れた（21 件以上の）push は、その `new` が範囲に届くだけで運んだとみなす（範囲のコミットが別の記録に載っていても）。`committer_push_audit_id` がその push の監査の行 | 別のレビュアーの承認。push の記録を確かめる（下の Diagnose 2） |
| `committers_unknown` | **誰がコミットを push したかを示せない**（閉じる側に倒す。誰の承認も数えない） | 下の表 |
| `not_in_rule` | 有効だが、どのルール・CODEOWNERS の行・設定のファイルの条件にも入っていない（数えても足しにならない） | ルールの承認できる人・CODEOWNERS の所有者に承認を頼む |

### `committers_unknown` の内訳（ログ `forge.review.approval_input_unread` と評価の入力）

| 原因 | 典型 |
|---|---|
| push の記録が無いコミットがある（`push_record_missing`） | 監査の保存期間を過ぎた古いコミット、push の記録に失敗した push（FG-GIT-0012、runbook push-record-failed）、push の記録が始まる前・バックアップから戻したリポジトリ、`update-ref` で手で動かした ref |
| ref の一覧が切れた push で、一覧に無い ref を残していない記録（`push_record_truncated`。表 `push_unlisted_refs` ができる前（マイグレーション 20261111100200 より前）に 100 を超える ref を 1 回で push した）| 古い大きな取り込み。新しい push は一覧に無い ref を `push_unlisted_refs` に残すので、この理由にならない |
| 一覧の切れた push の先端から先の履歴が 50 000 コミットを超える（`push_graph_too_large`）、先端のコミットがリポジトリに無い（`git_failed`） | 長い別の履歴の一括の push |
| 範囲に届く一覧の切れた push が 16 件を超える（`too_many_partial_pushes`）、届くかを確かめる git が失敗した・予算が尽きた（`push_reach_unknown`） | 長い履歴の一括の push。リポジトリ全体の一覧の切れた push のうち、範囲に届かないものは数えない |
| 評価の読み取りの予算が尽きた（`timeout`。要求 20 秒・64 回、`GET …/approvals` 10 秒・32 回、ジョブはロックの残り。一覧の切れた push の数によらず、届くかの確認は git 1 回） | 大きなリポジトリ・遅いディスク。もう一度要求する |
| 範囲が 1 000 コミットを超える（`range_too_long`）、git の失敗（`git_failed`） | 長く分岐したブランチ |

## Diagnose（読み取りのみ）

1. `gstn mr approvals --project <id> --iid <n> --json` で `approvals`・`rules`・`codeowners`・`blocked_by` を見る。
2. `committer` の証拠: `SELECT id, occurred_at, actor_user_id, details -> 'refs' FROM forge.audit_log WHERE id = <committer_push_audit_id>;`
   （その push の `new` か `commits` に MR の範囲のコミットがある）。
3. `committers_unknown`: MR の範囲を git で読み、push の記録と突き合わせる:
   `git --git-dir <repo> rev-list <head> ^<target の先端>` と
   `SELECT id, actor_user_id, details -> 'refs' FROM forge.audit_log WHERE project_id = <id> AND action = 'repository.pushed' ORDER BY id DESC LIMIT 50;`
4. 承認が消えた: `SELECT id, approver_id, head_sha, target_branch, revoked_at, revoked_reason FROM forge.merge_request_approvals WHERE merge_request_id = <mr_id> ORDER BY id;`
   （`new_commits` = 新しい版、`target_changed` = target の変更、`unapproved` = 本人の取り消し。イベント `merge_request.unapproved` も同じ）。

## Mitigate

1. `committer`: そのコミットを push していない別のレビュアーに承認を頼む（規則は既定で有効。プロジェクトの設定で切る口は R6）。
2. `committers_unknown`: push の記録が無いコミットを、**記録の残る push で運び直す**（作成者がブランチを rebase・amend して push し直すと、
  新しいコミットは新しい push の記録に載る）。記録に失敗した push は runbook push-record-failed の再生（`forge admin repo record-push`）で
  記録を書く: `[MUTATING — needs approval]`（runbook push-record-failed の手順。戻し方は不要: 再生は dedup の鍵で冪等、
  書かれるのは監査の行とイベントだけ）。
3. 規則やルールを手で弱めない（監査つきの設定の変更として、権限のある人が判断する）。

## Verify

- `gstn mr approvals` の `blocked_by` が空になり、マージの要求が 202 になる。

## Escalate

- push の記録が多数欠けている（FG-GIT-0012 が続く）→ runbook push-record-failed。
- 監査の保存期間の設定で古い MR が恒常的に止まる → 保存期間の見直し（運用の判断、ADR-0018）。
