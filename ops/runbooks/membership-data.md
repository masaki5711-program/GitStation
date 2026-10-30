# Runbook: membership-data（FG-DB-0018）

## Trigger

- `error_code=FG-DB-0018` MembershipDataInvalid（ログ）: ロールの読み込み（`forge_db::roles`）が、スキーマと forge-db のコマンドでは書かれないはずの行を見つけた。
  - `reason=namespace_chain_too_deep`: 名前空間の親をたどると 20 段を超える（`namespaces.parent_id` のループ、または CHECK が拒むはずの深さ）
  - `reason=unknown_role`: `group_members.role` / `project_members.role`（D4 MR4、`resource=project`）が 5 つのロール以外の値

## Impact

- そのグループ（とその子孫）でロールを使う操作が、その利用者に対して失敗する（ロールを途中まで読んで判定することはしない。拒否される）。
- データが forge-db の外で変えられている（手作業の変更、ダンプの復元、トリガーや制約の無効化）。原因が分かるまで SEV2 として扱う。

## Diagnose（読み取りのみ）

1. ログの `context` を読む: `reason`、`resource`（group / namespace）、`id`（読み込みの起点）、`user`（ロールを読んだ利用者）、`unknown_role` なら `raw`（保存されていた値）。
2. `namespace_chain_too_deep`: 親の連鎖を読む（同じ `id` が 2 度出たらループ）。

   ```sql
   WITH RECURSIVE c(id, parent_id, step) AS (
     SELECT id, parent_id, 1 FROM forge.namespaces WHERE id = <id>
     UNION ALL
     SELECT n.id, n.parent_id, c.step + 1 FROM forge.namespaces n JOIN c ON n.id = c.parent_id
     WHERE c.step < 25)
   SELECT * FROM c;
   ```

   子グループのページで起点が `resource=group` のときは、`SELECT id FROM forge.namespaces WHERE kind = 'group' AND owner_id = <id>;` で名前空間の id を得る。
3. `unknown_role`: `SELECT group_id, user_id, role FROM forge.group_members WHERE role NOT IN ('guest','reporter','developer','maintainer','owner');`
   と、制約が残っているか `SELECT conname FROM pg_constraint WHERE conname = 'group_members_role';` を確かめる。
   `resource=project` なら同じことを `forge.project_members`（`project_id`）と制約 `project_members_role` で行う。
4. いつ誰が変えたか: `forge_audit.ddl_audit`（制約・トリガーの変更）と、`forge.audit_log` の `group.*` / `member.*` の行（正しい履歴）を見る。
   監査の行のない変更は、トリガーが無効だった時間がある — Runbook `append-only-violation` も確かめる。

## Mitigate

1. 壊れた行を直す承認済みの手段は **まだない**（forge のコマンドにも、この Runbook にもない）。手で UPDATE しない、監査の行を手で書かない、トリガーを止めない
   （状態の表は監査のトリガー `audited_change_present` が守っていて、手で書いた監査の行は封印の連鎖を壊しうる）。`/incident` で事案にし、
   正しい値（`forge.audit_log` の `group.*` / `member.*` の行、またはバックアップ）を障害記録に残したうえで、修復の手段（forge-db の型付きコマンドか、レビューを通した 1 回限りの手順）を
   通常の開発の流れで作る。それを実行する段階は `[MUTATING — needs approval]`（ロールバック: 修復の手段と一緒に、戻す前の値と戻し方を障害記録に残す）
2. 外された制約・トリガーを、マイグレーションのファイルの定義どおりに戻す。`[MUTATING — needs approval]`（ロールバック: 不要。出荷した状態に揃えるだけ）

## Verify

- 同じ利用者で同じ操作を再実行し、`FG-DB-0018` が出ないこと。
- 手順 2・3 の SQL が、ループ・不正な値を返さないこと。

## Escalate

- 誰がいつ変えたか分からない、または監査の行のない変更がある: セキュリティの事案として扱い（`/incident`）、DB の管理者に連絡する。
- 事後: データを変えた経路（手作業・復元の手順）を特定し、その手順の Runbook か `forge doctor` の検査に戻す（障害の振り返りの対策）。
