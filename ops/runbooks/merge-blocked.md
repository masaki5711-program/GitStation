# Runbook: merge-blocked（FG-REVIEW-0007 の承認とマージの条件の理由。関連: FG-REVIEW-0010 / 0013 / 0014 / 0015 / 0016、FG-GIT-0017）

M3 R5（ADR-0024 §7・§8）で、承認があるのにマージが止まるときの読み方。とくに **コミッターの規則** で承認が数えられない理由
`committer` / `committers_unknown`（ADR-0024 §8 の 3。再レビュー #40 で「副作用は理由の値で見せる」と決めた）。

## Trigger

- マージの要求が FG-REVIEW-0007 MergeBlocked で、`reasons` に `approvals_missing`・`codeowner_approval_missing`・
  `config_change_needs_maintainer_approval`・`changed_paths_unknown` を含む（`details` に内訳）。
- 利用者から「承認したのに数えられていない」「承認が消えた」と問い合わせがあった。
- M3 R6: `reasons` に `required_check_missing`・`required_check_not_successful`・`unresolved_discussions` を含む、または
  `approvals_missing` の内訳が `rule=@protected_branch`（保護ブランチのマージの条件の最低の承認数）。
- M3 R6: キューにあったマージが `superseded`（`gstn mr merge-status` の `error_code`）で FG-REVIEW-0007 / FG-REVIEW-0010 — 要求の後に
  マージの条件・設定が変わり、ジョブが判定し直した（ADR-0024 §4.2 の 3・5）。

**まず全部の理由を見る**: `gstn mr status --project <id> --iid <n> --json`（安定した契約、`docs/api/merge-status.md`）。`reasons` は
ADR-0024 §7 の固定の順で、最初の 1 つが「止まっている理由の 1 行」。

## Impact

- その MR がマージできない（ほかは影響を受けない）。条件を満たせば自動では動かない: 利用者がもう一度マージを要求する。SEV4 相当。

## マージの条件の理由（M3 R6、保護ブランチのマージの条件 `protected_ref_merge_rules`）

target に一致する保護のルールのマージの条件は **いちばん厳しいものを合わせて** 効く（最低の承認数は最大、必須のチェックは和、
スレッドと直接の push の禁止はどれかが on なら on）。条件は `GET /api/v1/projects/<id>/protected-refs` の `merge_rule` で見る。

| reason（detail） | 意味 | 対処 |
|---|---|---|
| `approvals_missing`（`rule=@protected_branch required=<n> counted=<m>`） | 保護ブランチの最低の承認数。数えられる承認（下の表の理由が無い承認）なら誰のものでも数える（承認のルールとは別に効く） | 承認を集める。数えられない理由は下の表 |
| `required_check_missing`（`name=<check>`） | 必須のチェックの実行が今の head に **報告されていない**。報告が無いことは成功ではない（ADR-0024 §10）。C1（Checks API）が入るまでは報告の経路が無いので、必須のチェックを設定した保護ブランチには **マージできない** | C1 の前は必須のチェックを外す（`gstn merge-rule set` で `--check` を付けずに設定し直す）。C1 の後は提供者（`ci/` のサービスアカウントなど）がその head に報告する |
| `required_check_not_successful`（`name=… status=… conclusion=…`） | 実行はあるが `completed` かつ `success` でない（`neutral` も満たさない、実行中も満たさない） | チェックを直して再実行する |
| `unresolved_discussions`（`count=<n>`） | マージの条件がスレッドの解決を求め、未解決のスレッドが n 件ある | スレッドを解決する（MR の作成者は他人のスレッドを解決できない。ADR-0024 §12） |

関連する拒否:
- 必須のチェックに `ext/` の名前を設定しようとした → FG-REVIEW-0016（必須にできるのは `ci/`・`kanflow/`・`forge/` だけ）。
- 作業項目のリンクの条件（`require_work_item_link`）は I1 まで設定できない（FG-WEB-0006 `rule=unavailable_until_i1`）。
- 保護ブランチへの直接の push が FG-GIT-0017 で拒まれた → マージの条件の `direct_push_forbidden`。MR を作ってマージする（マージは禁止の
  影響を受けない）。CODEOWNERS が壊れて MR も通らないときは runbook codeowners の一時停止。
- キューにあったマージが FG-REVIEW-0010 で `superseded` → 要求の後にプロジェクトの設定（`gstn merge-settings show`）がその方式を
  許さなくなった。許された方式でもう一度要求する。

### 承認の引き継ぎ（`reset_approvals_on_push = false`、ADR-0024 §8.2）

プロジェクトの設定で「新しいコミットで承認を消す」を切ると、版の記録が承認を新しい head へ写すことがある（新しい行の `carried_from`、
イベント `merge_request.approved` の `carried_from` と `uncounted_reason=carried`。古い行は `revoked_reason=carried`）。
**写さない**（従来どおり `new_commits` で消す）のは: target が保護ブランチ、前の head からの変更が設定のファイル（`.forge/**` など）か
target の CODEOWNERS の行が覆うパスに触れる、変更を読み切れない（ログ `forge.review.approvals_carry_decided` の `reason` =
`target_protected` / `config_path` / `owned_path` / `changed_paths_unknown` / `codeowners_unknown`、読めなかった入力は
`forge.review.approvals_carry_unread` の warn）。写した承認も数えるかは評価の時点で決まる。

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
| 範囲のコミットに `push_commits` の行が無い（`push_record_missing`） | 表ができる前（マイグレーション 20261111100300 より前）に push されたコミット、`update-ref` で手で動かした ref・ファイルで取り込んだリポジトリ、push の記録に失敗した push（FG-GIT-0012、runbook push-record-failed）、1 回の push が 20 000 コミットを超えた・一覧の 30 秒の時間切れ・push 前の ref の読み取り（スナップショット）や git の失敗（その push の監査の行に `push_commits_incomplete: true`、ログに FG-GIT-0016 の warn が 1 行: `reason` = `cap` / `deadline` / `snapshot_failed` / `git_failed` / `record_fallback`（DB が遅く、行を付けた記録の試行が失敗し、最後の試行が行なしで監査の行だけを書いた）、`listed`、`refs`。その push の一覧に入らなかったコミットを範囲に持つ MR だけが止まる）、行の無いコミットを含む MR の squash・マージのコミット（範囲の push した人が分からないので、マージのコミットにも行を書かない。ログ `forge.review.merge_commit_unattributed`、FG-GIT-0016 `reason=merge_pushers_unknown:<理由>`） | **コミットの id を変えて** push する: 作成者が `git commit --amend`（先端だけのとき）か `git rebase --force-rebase <target>` で範囲のコミットを作り直し、`git push --force-with-lease` する（新しい id のコミットは「どの ref からも届かなかった」ので行を持つ）。**同じコミットをそのまま push し直しても行は書かれない**（もう届くコミットで、新しくない）。作り直せない（取り込んだ履歴・巨大な push）ときは下の Mitigate 2b（管理者の補填、承認が要る）。push の記録の再生（`forge admin repo record-push`）は git の一覧を持たないので、記録の行の一覧が完全な小さな push だけ行を書く |
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

1. `committer`: そのコミットを push していない別のレビュアーに承認を頼む（規則は既定で有効。M3 R6 からプロジェクトの設定
   `committer_approvals_excluded` で切れる: `gstn merge-settings set --project <id> --exclude-committer-approvals false`、Maintainer。
   切るとコミッターの承認も数え、`committers_unknown` でも止まらない — レビューの独立性を落とす判断なので、プロジェクトの決定として残す）。
2. `committers_unknown`: 行の無いコミットに **新しい id を与えて** push し直す（作成者が `git commit --amend` または
   `git rebase --force-rebase <target>` のあと `git push --force-with-lease`。新しい id のコミットはその push の行を持つ）。
   同じコミットの push し直し・別名のブランチへの push は行を書かない（もう届くコミット）。記録に失敗した push は
   runbook push-record-failed の再生（`forge admin repo record-push`）で監査の行を書く: `[MUTATING — needs approval]`
   （再生は dedup の鍵で冪等。ただし git の一覧を持たないので `push_commits` の行は書かない）。

2b. **管理者の補填（作り直せない履歴）** `[MUTATING — needs approval]`: 取り込んだリポジトリ・上限を超えた push など、
   コミットを作り直せず、**誰が push したかが別の証拠（push のログ `forge.git.push_applied`、移行の記録）で分かる** ときだけ。
   - **補填は承認を数えさせる**: 範囲のコミットに行が揃うと `committers_unknown` が消え、**補填した P 以外の人の承認が数えられる
     ようになる**。P のほかにも push した人がいたのに P だけを書くと、その人（本当のコミッター）の承認が数えられてしまう。
     「数えすぎにはならない」わけではない。だから **手順の承認者が証拠を自分で確かめる**（各コミットについて、push した人を
     すべて証拠と突き合わせる。分からない人がいるコミットは補填しない — 止めたままにする）。
   - 行は変更も削除もできない（`push_commits_immutable`、削除は purge だけ）ので、誤りは消せない。
   - 人の承認（誰が・どのコミットを・どの証拠で）を変更の記録に残してから、DB の管理者のロールで psql から 1 トランザクションで
     実行する。監査の行が同じトランザクションに無いと `audited_change_present` が拒む（XF002）。
   - 値は psql の変数で渡す（手で SQL に埋め込まない。psql が `:'var'` を引用つきの文字列に、`:var` をそのままの数に展開する）。
     `operator` は実行する人（OS のアカウント名など。`actor_label` に入る。全体で 64 文字以内）、`change_id` は承認の記録の番号
     （128 文字以内、push の request_id と別の名前）、`shas` はコミットの id の並び `{<id>,<id>,…}`。

   ```sql
   \set project_id 12
   \set repo_id 34
   \set pusher_id 56
   \set operator 'ops-alice'
   \set change_id 'chg-2026-11-03-001'
   \set approver 'Approver Name'
   \set evidence 'push log of request req-…'
   \set shas '{<40 or 64 hex>,<40 or 64 hex>}'
   -- push_commits backfill (begin)
   BEGIN;
   INSERT INTO forge.audit_log (actor_kind, actor_label, action, outcome, resource_type,
       resource_id, project_id, request_id, details)
   VALUES ('system', 'merge-blocked 2b by ' || :'operator', 'repository.push_commits_backfilled',
       'succeeded', 'project', :project_id, :project_id, :'change_id',
       pg_catalog.jsonb_build_object('repository_id', :repo_id, 'pusher_id', :pusher_id,
           'commits', pg_catalog.cardinality(:'shas'::text[]),
           'operator', :'operator', 'approved_by', :'approver', 'evidence', :'evidence'));
   INSERT INTO forge.push_commits (repository_id, commit_sha, pusher_id, project_id, request_id)
   SELECT :repo_id, c, :pusher_id, :project_id, :'change_id'
   FROM pg_catalog.unnest(:'shas'::text[]) AS c
   ON CONFLICT DO NOTHING;
   COMMIT;
   -- push_commits backfill (end)
   ```

   テスト `the_runbook_backfill_of_push_commits_writes_rows_with_their_audit_row` が begin〜end の SQL を psql と同じ規則で
   変数を展開して実行し、監査の行の無い挿入が拒まれることも確かめている。

   確かめる: `SELECT commit_sha, pusher_id, request_id FROM forge.push_commits WHERE request_id = :'change_id';` と、
   `gstn mr approvals` の `committers_unknown` が消えること（消えたら、残りの承認がどう数えられるかも確かめる）。

3. 規則やルールを手で弱めない（監査つきの設定の変更として、権限のある人が判断する）。

## Verify

- `gstn mr approvals` の `blocked_by` が空になり、マージの要求が 202 になる。

## Escalate

- push の記録が多数欠けている（FG-GIT-0012 が続く）→ runbook push-record-failed。
- 監査の保存期間の設定で古い MR が恒常的に止まる → 保存期間の見直し（運用の判断、ADR-0018）。
