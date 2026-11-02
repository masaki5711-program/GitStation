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
| 範囲のコミットに `push_commits` の行が無い（`push_record_missing`） | 表ができる前（マイグレーション 20261111100300 より前）に push されたコミット、`update-ref` で手で動かした ref・ファイルで取り込んだリポジトリ、push の記録に失敗した push（FG-GIT-0012、runbook push-record-failed）、1 回の push が 50 000 コミットを超えた・一覧の 30 秒の時間切れ・push 前の ref の読み取り（スナップショット）や git の失敗（その push の監査の行に `push_commits_incomplete: true`、ログに FG-GIT-0016 の warn が 1 行: `reason` = `cap` / `deadline` / `snapshot_failed` / `git_failed`、`listed`、`refs`。その push の一覧に入らなかったコミットを範囲に持つ MR だけが止まる）、行の無いコミットを含む MR の squash・マージのコミット（範囲の push した人が分からないので、マージのコミットにも行を書かない。ログ `forge.review.merge_commit_unattributed`、FG-GIT-0016 `reason=merge_pushers_unknown:<理由>`） | **コミットの id を変えて** push する: 作成者が `git commit --amend`（先端だけのとき）か `git rebase --force-rebase <target>` で範囲のコミットを作り直し、`git push --force-with-lease` する（新しい id のコミットは「どの ref からも届かなかった」ので行を持つ）。**同じコミットをそのまま push し直しても行は書かれない**（もう届くコミットで、新しくない）。作り直せない（取り込んだ履歴・巨大な push）ときは下の Mitigate 2b（管理者の補填、承認が要る）。push の記録の再生（`forge admin repo record-push`）は git の一覧を持たないので、記録の行の一覧が完全な小さな push だけ行を書く |
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
2. `committers_unknown`: 行の無いコミットに **新しい id を与えて** push し直す（作成者が `git commit --amend` または
   `git rebase --force-rebase <target>` のあと `git push --force-with-lease`。新しい id のコミットはその push の行を持つ）。
   同じコミットの push し直し・別名のブランチへの push は行を書かない（もう届くコミット）。記録に失敗した push は
   runbook push-record-failed の再生（`forge admin repo record-push`）で監査の行を書く: `[MUTATING — needs approval]`
   （再生は dedup の鍵で冪等。ただし git の一覧を持たないので `push_commits` の行は書かない）。

2b. **管理者の補填（作り直せない履歴）** `[MUTATING — needs approval]`: 取り込んだリポジトリ・上限を超えた push など、
   コミットを作り直せず、**誰が push したかが別の証拠（push のログ `forge.git.push_applied`、移行の記録）で分かる** ときだけ。
   人の承認（誰が・どのコミットを・どの証拠で）を変更の記録に残してから、DB の管理者のロールで 1 トランザクションで実行する。
   監査の行が同じトランザクションに無いと `audited_change_present` が拒む（XF002）。行は変更されない（`push_commits_immutable`）ので、
   誤りは消せない: 補填は「push した人を増やす」方向にだけ使う（承認を数えにくくするだけで、数えすぎにはならない）。
   `<project_id>` `<repo_id>` `<pusher_id>` `<change_id>`（承認の記録の番号、128 文字以内）`<approver>`（承認した人）
   `<evidence>`（証拠の場所）と `<sha>` の並び（カンマ区切り）を埋める。テスト
   `the_runbook_backfill_of_push_commits_writes_rows_with_their_audit_row` がこの SQL をそのまま実行して確かめている。

   ```sql
   -- push_commits backfill (begin)
   BEGIN;
   INSERT INTO forge.audit_log (actor_kind, actor_label, action, outcome, resource_type,
       resource_id, project_id, request_id, details)
   VALUES ('system', 'runbook merge-blocked 2b', 'repository.push_commits_backfilled', 'succeeded',
       'project', <project_id>, <project_id>, '<change_id>',
       pg_catalog.jsonb_build_object('repository_id', <repo_id>, 'pusher_id', <pusher_id>,
           'commits', pg_catalog.cardinality('{<sha>}'::text[]),
           'approved_by', '<approver>', 'evidence', '<evidence>'));
   INSERT INTO forge.push_commits (repository_id, commit_sha, pusher_id, project_id, request_id)
   SELECT <repo_id>, c, <pusher_id>, <project_id>, '<change_id>'
   FROM pg_catalog.unnest('{<sha>}'::text[]) AS c
   ON CONFLICT DO NOTHING;
   COMMIT;
   -- push_commits backfill (end)
   ```

   確かめる: `SELECT commit_sha, pusher_id, request_id FROM forge.push_commits WHERE request_id = '<change_id>';` と、
   `gstn mr approvals` の `committers_unknown` が消えること。

3. 規則やルールを手で弱めない（監査つきの設定の変更として、権限のある人が判断する）。

## Verify

- `gstn mr approvals` の `blocked_by` が空になり、マージの要求が 202 になる。

## Escalate

- push の記録が多数欠けている（FG-GIT-0012 が続く）→ runbook push-record-failed。
- 監査の保存期間の設定で古い MR が恒常的に止まる → 保存期間の見直し（運用の判断、ADR-0018）。
