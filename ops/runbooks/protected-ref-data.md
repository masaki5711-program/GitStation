# Runbook: protected-ref-data（FG-DB-0020）

## Trigger

- `error_code=FG-DB-0020` ProtectedRefDataInvalid（ログ）: 保護の規則の読み込み（`forge_db::protected_refs`。push の判定と
  ポリシーのスナップショットに使う `levels_for`、規則の変更が既存の規則を読むとき）が、forge-db のコマンドでは書かれないはずの
  `forge.protected_refs` の行を見つけた（ADR-0008 Amendment D4 MR1・MR5）。
  - `reason=too_many_stored_rules`（`project_id`）: 1 つのプロジェクトに 100 を超える規則がある（上限はアプリケーションだけが守る）
  - `reason=stored_pattern_invalid`（`protected_ref_id`）: パターンが `RefPattern::parse` を通らない（DB の CHECK は接頭辞と長さだけ）
  - `reason=stored_push_min_role_invalid`（`protected_ref_id`）: `push_min_role` が developer / maintainer / owner 以外

## Impact

- そのプロジェクトへの push（と規則の変更）が拒否される。規則を飛ばして判定すると保護が緩むので、途中までの規則では判定しない。
- データが forge-db の外で変えられている（手作業の変更、ダンプの復元、制約の無効化）。原因が分かるまで SEV2 として扱う。

## Diagnose（読み取りのみ）

1. ログの `context` を読む: `reason`、`project_id` または `protected_ref_id`（パターンそのものはログに出ない）。
2. 行を読む（書き込み用ロールで）:

   ```sql
   SELECT id, project_id, pattern, push_min_role, created_at, updated_at
   FROM forge.protected_refs WHERE project_id = <project_id> ORDER BY id;
   -- protected_ref_id のとき
   SELECT id, project_id, pattern, push_min_role FROM forge.protected_refs WHERE id = <protected_ref_id>;
   SELECT conname FROM pg_constraint WHERE conrelid = 'forge.protected_refs'::regclass;
   ```

3. いつ誰が変えたか: `forge_audit.ddl_audit`（制約の変更）と、`forge.audit_log` の `protected_ref.*` の行（正しい履歴、`project_id` で絞る）を見る。
   監査の行のない変更は、トリガーが無効だった時間がある — Runbook `append-only-violation` も確かめる。

## Mitigate

1. 規則に反する行を、監査の残る経路で消す（`forge_db::protected_refs::delete`。HTTP の経路・CLI はまだない — 承認の記録に行の id を残し、
   経路ができるまでは DB の管理者と相談する）。`[MUTATING — needs approval]`（ロールバック: 消した規則は `protected_refs::create` で作り直す。
   監査の記録の `details` に、元のパターンと `push_min_role` がある）
   `delete` は行を解釈しないので、読めない行（パターン・ロール）も消せる。その監査の行は `stored_invalid: true` と、保存されていた文字列
   （パターン 255 バイト、ロール 32 文字まで）を持つ。消した後、`levels_for` は FG-DB-0020 を出さなくなる。
2. 外された制約を、マイグレーション `20261031100000` の定義どおりに戻す。`[MUTATING — needs approval]`（ロールバック: 不要）
3. 手で UPDATE しない、監査の行を手で書かない、トリガーを止めない。

## Verify

- 同じプロジェクトへの push が FG-DB-0020 を出さないこと（保護の判定は FG-GIT-* の拒否か成功になる）。
- 手順 2 の SQL が、100 以下の行と、規則に合う値だけを返すこと。

## Escalate

- 誰がいつ変えたか分からない、または監査の行のない変更がある: セキュリティの事案として扱い（`/incident`）、DB の管理者に連絡する。
- 事後: データを変えた経路を特定し、その手順の Runbook か `forge doctor` の検査に戻す。
