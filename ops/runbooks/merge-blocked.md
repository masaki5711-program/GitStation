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
| `committer` | 承認者が **MR の範囲（`head ^<target の先端>`: マージが持ち込むコミット）のコミットを、push で初めて持ち込んだ** ことがある（`forge.push_commits` にその (コミット, push した人) の行がある。別名のブランチへの push、MR の作成前の push も含む。既にあるコミットにブランチやタグを作っただけの push は行を書かない）。`committer_push_audit_id` がその push の監査の行 | 別のレビュアーの承認。push の記録を確かめる（下の Diagnose 2） |
| `committers_unknown` | **誰がコミットを push したかを示せない**（閉じる側に倒す。誰の承認も数えない） | 下の表 |
| `not_in_rule` | 有効だが、どのルール・CODEOWNERS の行・設定のファイルの条件にも入っていない（数えても足しにならない） | ルールの承認できる人・CODEOWNERS の所有者に承認を頼む |

### `committers_unknown` の内訳（ログ `forge.review.approval_input_unread` と評価の入力）

R5 の 4 回目から、コミッターは push を受けた時点で記録する（表 `forge.push_commits`: push がそれまでどの ref からも届かなかったコミットを
持ち込んだら、(コミット, push した人) ごとに 1 行）。MR の範囲のコミットに 1 つでも行の無いものがあれば、誰の承認も数えない（閉じる側）。

| 原因 | 典型 | 対処 |
|---|---|---|
| 範囲のコミットに `push_commits` の行が無い（`push_record_missing`） | 表ができる前（マイグレーション 20261111100300 より前）に push されたコミット、`update-ref` で手で動かした ref・ファイルで取り込んだリポジトリ、push の記録に失敗した push（FG-GIT-0012、runbook push-record-failed）、1 回の push が 1 000 000 コミットを超えた・一覧の 30 秒の時間切れ・git の失敗（その push の監査の行に `push_commits_incomplete: true`。その push の一覧に入らなかったコミットを範囲に持つ MR だけが止まる） | 作成者がブランチを rebase して push し直す（新しいコミットは行を持つ）。push の記録の再生（`forge admin repo record-push`）は git の一覧を持たないので、記録の行の一覧が完全な小さな push だけ行を書く |
| 範囲が 1 000 コミットを超える（`range_too_long`）、範囲を読む git の失敗（`git_failed`）・時間切れ（`timeout`） | 長く分岐したブランチ、遅いディスク | ブランチを分ける。もう一度要求する |

タグの作成・削除、既にあるコミットへのブランチ、ほかの無関係な大きな push、prune（オブジェクトの削除）は、この判定に影響しない
（行を読むだけで履歴をたどらない）。

## Diagnose（読み取りのみ）

1. `gstn mr approvals --project <id> --iid <n> --json` で `approvals`・`rules`・`codeowners`・`blocked_by` を見る。
2. `committer` の証拠: `SELECT commit_sha, pusher_id, request_id, first_pushed_at FROM forge.push_commits WHERE repository_id = <repo_id> AND pusher_id = <user_id> AND commit_sha = ANY('{<範囲のコミット>}');`
   と、その push の監査の行 `SELECT id, occurred_at, actor_user_id FROM forge.audit_log WHERE id = <committer_push_audit_id>;`。
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
