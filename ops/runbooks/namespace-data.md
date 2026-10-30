# Runbook: namespace-data（FG-DB-0021）

## Trigger

- `error_code=FG-DB-0021` NamespaceDataInvalid（ログ）: パスの解決、または ID によるプロジェクトの読み込み（`forge_db::resolve`。Web の dispatcher が使う
  `NamespaceResolver` / `ProjectAccessLoader`、ADR-0015）が、スキーマと forge-db のコマンドでは書かれないはずの行を見つけた。
  - `reason=stored_path_invalid`: `moved` の墓標の行き先の今のパス（セグメントから組み立て、`forge.namespaces.full_path` との完全一致で確かめたもの）が
    `NamespacePath::parse` を通らない（名前の規則に合わないセグメント、20 段より深い、255 文字より長い。CHECK が外されている）。`rule` に parse の理由
  - `reason=unknown_namespace_kind` / `unknown_tombstone_kind` / `unknown_visibility`: 保存された種類・可視性が CHECK の外の値（`raw` に保存値）
  - `reason=namespace_without_project`: `kind = 'project'` の名前空間に、対応する `forge.projects` の行がない
  - `reason=project_without_namespace`: `forge.projects` の行に、`kind = 'project'` の名前空間がない
- ロールの連鎖が壊れている場合（ループ、不明なロール）は、同じ読み込みでも FG-DB-0018（Runbook `membership-data`）になる。

## Impact

- そのパス（またはそのプロジェクト）への要求が、どの利用者でも 500 で失敗する（推測でリダイレクトしたり、行の一部で判定したりしない。拒否される）。
- データが forge-db の外で変えられている（手作業の変更、ダンプの復元、制約の削除、プロジェクト・名前空間・リポジトリの 1 対 1 対 1 の片側だけを消したパージ）。
  原因が分かるまで SEV2 として扱う。

## Diagnose（読み取りのみ）

1. ログの `context` を読む: `reason`、`namespace_id`、`tombstone_id`（`moved` の墓標のとき）、`project_id`、`unknown_*` なら `raw`、
   `stored_path_invalid` なら `rule`（パスそのものはログに出ない）。
2. 行を読む:

   ```sql
   SELECT id, parent_id, kind, owner_id, segment, full_path, depth FROM forge.namespaces WHERE id = <namespace_id>;
   SELECT id, old_path, kind, namespace_id, parent_namespace_id, cause_type, cause_id, created_at
     FROM forge.namespace_tombstones WHERE id = <tombstone_id>;
   SELECT id, visibility, deleted_at, purge_after FROM forge.projects WHERE id = <project_id>;
   SELECT id, full_path FROM forge.namespaces WHERE kind = 'project' AND owner_id = <project_id>;
   ```

3. 制約が残っているか: `SELECT conname FROM pg_constraint WHERE conname IN ('namespaces_segment', 'namespaces_kind',
   'namespace_tombstones_kind', 'projects_visibility');`
4. いつ誰が変えたか: `forge_audit.ddl_audit`（制約・トリガーの変更）と、`forge.audit_log` の `user.*` / `group.*` / `project.*` の行（正しい履歴）を見る。
   監査の行のない変更は、トリガーが無効だった時間がある — Runbook `append-only-violation` も確かめる。

## Mitigate

1. 壊れた行を直す承認済みの手段は **まだない**（D5 の名前の変更・移動・削除・パージのコマンドが入るまで）。手で UPDATE しない、監査の行を手で書かない、
   トリガーを止めない。`/incident` で事案にし、正しい値（`forge.audit_log` の行、またはバックアップ）を障害記録に残したうえで、修復の手段
   （forge-db の型付きコマンドか、レビューを通した 1 回限りの手順）を通常の開発の流れで作る。それを実行する段階は `[MUTATING — needs approval]`
   （ロールバック: 修復の手段と一緒に、戻す前の値と戻し方を障害記録に残す）
2. 外された制約を、マイグレーションのファイルの定義どおりに戻す。`[MUTATING — needs approval]`（ロールバック: 不要。出荷した状態に揃えるだけ）

## Verify

- 同じパス（または `GET /api/v1/projects/{id}`）を再び要求し、`FG-DB-0021` が出ないこと。
- 手順 2・3 の SQL が、不正な値・欠けた行を返さないこと。

## Escalate

- 誰がいつ変えたか分からない、または監査の行のない変更がある: セキュリティの事案として扱い（`/incident`）、DB の管理者に連絡する。
- 事後: データを変えた経路（手作業・復元・パージの手順）を特定し、その手順の Runbook か `forge doctor` の検査に戻す（障害の振り返りの対策）。
