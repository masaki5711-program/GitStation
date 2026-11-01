# Runbook: push-record-failed（FG-GIT-0012。関連: FG-DB-0001、FG-DB-0002、FG-DB-0010）

push の ref の更新は git が行い、DB のトランザクションには入らない（ADR-0008）。git が終わった後、forge-web は
まずログに `event=forge.git.push_applied`（info）を 1 行出し、それから `forge_db::repositories::record_push` で
監査の行 `repository.pushed` とイベント `repository.pushed`（分類 `integration`）を 1 つの変更で書く（M1 段階 9）。
この記録が再試行（3 回 / 30 秒、MR4）の後も書けなかったときに `FG-GIT-0012` PushRecordFailed（alert）が出る。

`forge.git.push_applied` の行は、その push の要求の `request_id`、`repo_id`、`project_id`、`user_id`、`refs`（更新の数）と、
`push`（`forge_core::PushApplied` の JSON の文字列。ref ごとに名前・旧・新・種類 `create` / `fast_forward` / `force` / `delete`・
新しいコミット最大 20 件と総数）を持つ。これが再投入の入力になる。IP・User-Agent・名前・メールは入らない。

## Trigger

- ログに `error_code=FG-GIT-0012` が出た
  - `reason=record_failed`: `record_push` がすべての試行で失敗した。`repo_id`、`refs`、`attempts`、`cause_code`（最後の失敗のコード）、
    `request_id`（push の要求のもの）
  - `reason=push_log_invalid`（`field`、`line` または `push`。`field=project_id` / `user_id` はログと DB の不一致）/ `push_log_empty` / `push_log_unreadable` / `push_log_too_large`:
    下の Mitigate 2 の `forge admin repo record-push` が、渡されたファイルを使えなかった（push はまだ記録されていない）

## Impact

- **リポジトリ自体は正しい**（ref は更新済み。clone / fetch には影響しない）。
- 欠けるのは監査の記録と、連携の履歴・Webhook（kanflow など）への `repository.pushed`。再投入するまで、連携先はこの push を知らない。
- 1 件なら SEV3。DB が落ちていて多くの push が記録されていないなら、DB の障害（Runbook `db-unavailable`）が本体で SEV2。

## Diagnose（読み取りのみ）

1. `FG-GIT-0012` の行の `context` を読む: `cause_code` が原因の系統を示す。
   - `FG-DB-0001` DbUnavailable: DB に届かない → Runbook `db-unavailable`。`forge doctor` を回す。
   - `FG-DB-0002` DbQueryFailed（`reason=change_timeout` / `commit_outcome_unknown` など）: DB が遅い、またはコミットの結果が不明。
     `commit_outcome_unknown` のときは、実は記録できている可能性がある（下の 3 で確かめる。再投入しても二重にはならない）。
   - `FG-DB-0010` ChangeRejected `reason=invalid_push_record`（`rule`）: 記録の中身が矛盾している（forge-web の不具合）。
     再投入しても同じく拒否される。不具合として扱い、Escalate へ。
2. 同じ `request_id` の `event=forge.git.push_applied` の行を探す（push の要求ごとに 1 行）:

   ```sh
   # JSON のログ（本番の形式）から、request_id で絞る
   grep '"request_id":"<request_id>"' <log files> | grep '"event":"forge.git.push_applied"'
   ```

3. 記録が本当に無いか確かめる（読み取り。書き込み用ロールで）:

   ```sql
   SELECT id, request_id, occurred_at FROM forge.events
    WHERE event_type = 'repository.pushed' AND request_id = '<request_id>';
   SELECT id, request_id, created_at FROM forge.audit_log
    WHERE action = 'repository.pushed' AND request_id = '<request_id>';
   ```

## Mitigate

1. DB を直す（`cause_code` の Runbook。`db-unavailable` など）。`forge doctor` が緑になるまで再投入しない。
2. **再投入**: Diagnose 2 の行（複数の push なら複数行でよい。ほかのイベントの行や JSON でない行は読み飛ばされる）を
   ファイルに書き出し、次を実行する。`[MUTATING — needs approval]`（ロールバック: 不要。記録は追記のみで、取り消す操作はない）

   ```sh
   forge admin repo record-push --from-log /tmp/push-<request_id>.log
   ```

   - 出力は push ごとに 1 行: `push <request_id> (repository <id>, <n> refs): recorded`、または
     `already recorded (nothing changed)`（既に記録されていた。`commit_outcome_unknown` の後など）。
   - **冪等**: 重複除去の鍵は `repository_pushed:<repo_id>:<sha256(request_id ‖ 名前順の ref の更新)>` で、ログに残った
     request_id から作る。同じファイルを何度実行しても 1 件しか記録されない。途中で失敗したら、原因を直して同じファイルで
     もう一度実行してよい（記録済みのものは `already recorded` になる）。
   - **行者は再投入した運用者**: 監査の行・イベントの行者は `system`、`actor_label` は `forge-cli:<OS のアカウント>`。
     ファイルの中身は信用しないので、ログに書かれた push した利用者は行者にせず、詳細とペイロードの
     `replayed_from: {user_id, credential_id}` に入れる。`request_id` は push の要求のもの（ログの行と結び付く）。
     CLI 自身の `request_id` は CLI のログ `forge.admin.repo.push_recorded`（`push_request_id` も持つ）にある。
   - **書く前にすべて確かめる**: まずファイルの中の push の行をすべて読んで確かめ（1 行でも読めなければ
     `FG-GIT-0012 reason=push_log_invalid`、`field` と `line`）、次にすべての push を DB と照合する
     （リポジトリが存在する — 無ければ `FG-REPO-0005`、リポジトリのプロジェクトがログの `project_id` と同じ — 違えば
     `push_log_invalid field=project_id`、ログの `user_id` の利用者が存在する — 無ければ `field=user_id`。`push` = 何番目の push か）。
     ここまでで 1 件でも失敗すれば何も書かない。
   - **書き込みは push ごとに 1 つのトランザクション**: 照合の後、push を 1 件ずつ記録する。途中で DB が落ちるなどして
     失敗すると、それより前の push は記録済み、後のものは未記録のまま残る（ファイル全体の 1 トランザクションではない）。
     そのまま同じファイルでもう一度実行すれば、記録済みのものは `already recorded` になり、残りだけが記録される。
3. 手で `forge.audit_log` / `forge.events` に INSERT しない（封印と連携の順序が壊れる）。ログの行を手で書き換えない
   （`request_id` や ref を変えると別の push として記録される）。

## Verify

- Diagnose 3 の SQL が、その `request_id` の行を監査・イベントともに 1 件ずつ返す。
- 同じファイルで `forge admin repo record-push` をもう一度実行すると `already recorded (nothing changed)` になる。
- 連携の履歴にその push の `repository.pushed` が出る（分類 `integration`。プロジェクトの Reporter 以上に見える）。

## Escalate

- `push_log_invalid field=project_id` / `user_id`: ログの行が別のリポジトリ・利用者を指している（行の取り違え、
  手で書き換えたファイル、ID の巻き戻った復元）。そのまま記録しない。正しい行かを確かめ、分からなければセキュリティの
  事案として扱う。
- `cause_code=FG-DB-0010 reason=invalid_push_record`、または `push_log_invalid` が続く: forge-web の不具合。
  ログの行（ref の名前を含む。社外に出さない）と `rule` / `field` を添えて開発者に渡す。
- `forge.git.push_applied` の行が見つからない（ログの保持期間を過ぎた、ログが欠けた）: 再投入できない。どの push が
  記録されていないかを git の reflog（`git -C <repo path> reflog --date=iso`、パスは `forge admin repo-path <repo_id>`）で
  確かめ、記録を残せないことを事案の記録に書く（ログの保持期間の見直しを事後の対策にする）。
- 事後: 原因（DB の停止など）の Runbook と、必要ならアラートの閾値に反映する。
