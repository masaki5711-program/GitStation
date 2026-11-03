# Runbook: codeowners（FG-REVIEW-0014。関連: FG-REVIEW-0007 `codeowners_invalid` / `codeowner_approval_missing` / `changed_paths_unknown`、FG-GIT-0002 / 0003、M3 R6 の CODEOWNERS の強制の一時停止）

M3 R5（ADR-0024 §9）の CODEOWNERS。マージの条件は **target のブランチの先端** の `.forge/CODEOWNERS`（無ければ直下の
`CODEOWNERS`）から読む（MR の中のファイルは自分の MR に効かない）。読めない・解析できないファイルは **黙って無視せず**、その target への
すべてのマージを `codeowners_invalid{line, reason}` で止める（閉じる側に倒す）。

## Trigger

- ログに `forge.review.codeowners_invalid`（warn、`error_code=FG-REVIEW-0014`）が出た。`line`（1 から。0 はファイル全体）、`reason`、
  `path`（どちらのファイルか）、`target_sha`、`merge_request_id`、`request_id` を持つ。評価のたびに 1 回出る（マージの要求、ジョブの各判定、
  `GET …/approvals`）。
- マージの要求が FG-REVIEW-0007 で `reasons` に `codeowners_invalid` を含む（`details` に `codeowners_invalid:line=<n> reason=<r>`）。
- `gstn mr approvals --project <id> --iid <n>` の `blocked_by` に `codeowners_invalid` がある。

## Impact

- その target へのマージがすべて止まる（CODEOWNERS を直す MR も止まる）。ほかの target・閲覧・push は影響を受けない。1 つのプロジェクトなら SEV3。

## `reason` の読み方

| reason | 意味 | 直し方 |
|---|---|---|
| `syntax` | 行が「パターン + 所有者 1 つ以上」になっていない（所有者が無い） | 所有者を書くか、行を消す |
| `owner_syntax` | 所有者が `@username` / `@group/path` の形でない | `@` を付ける。名前は英数字と `_` `-` `.` |
| `unsupported` | セクション `[Section]`、任意の承認 `^[Section]`、メールの所有者、否定 `!pattern`（入れない文法。無視ではなく誤り） | その行を GitLab の拡張なしの形に書き直す |
| `pattern_invalid` | 空のパス要素、`.` / `..`、`[` `]` `{` `}`、`***`、`**` が要素の一部、`\#` `\ ` 以外のエスケープ、ワイルドカード 16 個超、両端の `//`、注釈の外の見えない文字（タブ以外の制御文字（VT・DEL など）、Unicode の Cf: ゼロ幅スペース・方向制御など、結合文字、ハングルの埋め字 U+115F / U+1160 / U+3164 / U+FFA0、点字の空白 U+2800、異体字セレクタ）や全角スペースなど ASCII でない空白 | glob は `*`・`?`・要素全体の `**` だけ。見えない文字は `grep -nP '[\x{200B}-\x{200F}\x{202A}-\x{202E}\x{3000}\x{FEFF}]'` で探して消す（先頭の BOM は無視される） |
| `unknown_owner` | その名前の **有効な** 利用者もグループも無い（1 要素の `@name` は利用者、無ければ最上位のグループとして探す。無効化された利用者と存在しない名前は同じ答え） | 名前を直すか、利用者を有効にする・グループを作る |
| `too_large` / `too_many_lines` / `line_too_long` / `too_many_owners` | 64 KiB / 1 000 行 / 1 行 4 KiB / 1 行の所有者 50 を超えた（R5 で決めた上限） | ファイルを分けずに短くする（ディレクトリ単位の行にまとめる） |
| `not_utf8` | UTF-8 でない | UTF-8 で保存し直す |
| `not_a_file` | シンボリックリンク・ディレクトリ・サブモジュール | 通常のファイルにする |
| `read_failed` | git がファイルを読めなかった（時間切れ・異常終了。同じ行の `git_error_code` が FG-GIT-0002 / 0003）、評価の読み取りの予算（要求 20 秒・64 回、`GET …/approvals` 10 秒・32 回、ジョブはロックの残り）が尽きた、または target のブランチが無い | runbook git-process-failed / repo-storage。target を確かめる |
| `match_budget` | 変更されたパスと CODEOWNERS の照合が 5 秒を超えた（閉じる側に倒した） | 行数とワイルドカードを減らす。パスの多すぎる MR を分ける |
| `match_failed` | 照合のタスクが失敗した（同じ行の `join_error`。プログラムの誤り） | runbook internal-error。MR はそのまま止まる |

**パターンの位置（R5 の再レビュー 2 回目で gitignore に合わせた）**: 先頭か途中に `/` があるパターン（`src/*.rs`、`/docs`）はリポジトリの直下に固定、`/` の無いパターン（`*.rs`、`docs/` のように末尾だけ）はどの深さでも一致。

## Diagnose（読み取りのみ）

1. ログの `target_sha` と `path` で、その版のファイルを読む:
   `git --git-dir <repo> show <target_sha>:<path> | sed -n '<line>p'`
2. `unknown_owner` なら、名前が利用者かグループかを確かめる:
   `SELECT id, username, state FROM forge.users WHERE username = '<name>';`
   `SELECT owner_id FROM forge.namespaces WHERE kind = 'group' AND full_path = '<path>';`
3. `gstn mr approvals --project <id> --iid <n> --json` で、ほかの止める理由（`codeowner_approval_missing` など）も見る。

## Mitigate

1. **CODEOWNERS を直すのが正しい手順**: 直したファイルを MR で target に入れる（止まっていなければ通常の MR。本番の設定の変更ではない）。
2. `[MUTATING — needs approval]` CODEOWNERS が壊れていると、それを直す MR 自体も止まる（行き詰まり）。M3 R6 からの逃げ道は
   **CODEOWNERS の強制の期限つきの一時停止**（`CodeownersEnforcementToggle`、ADR-0024 §9）。承認された手順としてだけ行う:
   1. 実行する人: そのプロジェクトの **人間の** Maintainer 以上で、ロールがプロジェクトのすべての保護ブランチの `push_min_role` 以上
      （Owner の水準で保護されたブランチがあれば Owner。足りなければ FG-AUTH-0003 `reason=above_own_role`）。エージェントのトークン・
      サービスアカウントは拒まれる（`agent_token` / `service_account`）。トークンなら `api` スコープ。
   2. 止める（期限は必須、1〜1440 分。直す MR のレビューとマージに足りる最短にする）:
      `gstn codeowners-enforcement suspend --project <id> --for <分>`
      （= `PUT /api/v1/projects/<id>/merge-settings/codeowners-enforcement` 本文 `{"enforced": false, "for_minutes": <分>}`）。
      期限なし・0・1441 以上は FG-AUTH-0003（`expiry_missing` / `expiry_out_of_range`）。
      **一時停止は延ばせない**: 止まっている間にもう一度止めても何も変わらない（答えは今の期限のまま）。足りなければ、期限が過ぎるか
      戻してから、改めて承認を得て止め直す（期限の過ぎた一時停止を止め直すと、期限の記録が同じ変更に先に残る）。
   3. 一時停止の間は、CODEOWNERS の所有者の承認（`codeowner_approval_missing`）と読めないファイル（`codeowners_invalid`）だけが止める
      理由から外れる。**設定のファイルの変更は Maintainer の承認が要るまま**（`config_change_needs_maintainer_approval`。CODEOWNERS を直す
      MR はこれに当たる）。承認のルール・保護ブランチのマージの条件もそのまま効く。
   4. 直す MR をマージする。
   5. すぐに戻す: `gstn codeowners-enforcement enforce --project <id>`（戻し忘れても **期限で構造的に戻る**: 判定は「強制中、または期限を
      過ぎた」を強制とみなす。worker の記録を待たない）。
   記録: 監査の行とイベント `merge_settings.codeowners_enforcement_changed`（`enforced`、`until`）、ログ
   `forge.review.codeowners_enforcement_changed`（`request_id`、`project_id`、`user_id`、`until`）が残る。誰が・なぜ・いつまでを
   インシデント記録か MR の説明に書く。戻し方: 上の 5（または期限を待つ）。
   保護ブランチに直接の push の禁止（merge rule の `direct_push_forbidden`、FG-GIT-0017）があると、直接の push の逃げ道は無い。
   R6 より前の「target に push できる人の直接の push」は、禁止の無いブランチでだけ使える（承認された手順。force push はしない）。
3. 直した後、止まっていた MR はもう一度マージを要求すれば評価し直される（CODEOWNERS はキャッシュしない）。

## 期限が来たとき（知らせ）

- worker のタスク `codeowners_restore`（60 秒ごと）が、期限を過ぎた一時停止を 1 回記録する: 監査の行とイベント
  `merge_settings.codeowners_enforcement_restored`（actor `system`、`expired_at`、dedup は `codeowners_enforcement_restored:<project_id>:<期限の epoch>`）、
  ログ `forge.worker.codeowners_enforcement_restored`（info、`restored` の件数、`request_id`）。
- 記録に失敗した行は `forge.worker.codeowners_restore_failed`（warn、`error_code`、`project_id`）を 1 回出し、次の回の後ろへ回る
  （ほかのプロジェクトの記録は続く）。
- **記録のためだけ**: worker が止まっていても、判定は期限を過ぎた時点で強制に戻っている（`GET /api/v1/projects/<id>/merge-settings` の
  `codeowners.enforced` は期限の後 `true`）。記録が来ない・遅れるのは worker の障害（runbook worker）。

## Verify

- `gstn mr approvals --project <id> --iid <n>` の `blocked_by` から `codeowners_invalid` が消え、`codeowners` に必要な所有者の行が出る。
- 新しい `forge.review.codeowners_invalid` がその target で出ない。
- 一時停止を使ったなら `gstn merge-settings show --project <id>` が `CODEOWNERS enforced` を示す（戻した、または期限が過ぎた）。
- 一時停止を使ったなら `gstn merge-settings show --project <id>` が `CODEOWNERS enforced` を示す（戻した、または期限が過ぎた）。

## Escalate

- `read_failed` が続く（git の障害）→ runbook git-process-failed / repo-storage。
- 所有者の名前が正しいのに `unknown_owner` が出る（`forge.namespaces.full_path` と食い違う）→ runbook namespace-data。
