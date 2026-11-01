# Runbook: namespace-data（FG-DB-0021）

## Trigger

- `error_code=FG-DB-0021` NamespaceDataInvalid（ログ）: パスの解決、または ID によるプロジェクトの読み込み（`forge_db::resolve`。Web の dispatcher が使う
  `NamespaceResolver` / `ProjectAccessLoader`、ADR-0015）が、スキーマと forge-db のコマンドでは書かれないはずの行を見つけた。
  - `reason=stored_path_invalid`: `moved` の墓標の行き先の今のパス（セグメントから組み立て、`forge.namespaces.full_path` との完全一致で確かめたもの）が
    `NamespacePath::parse` を通らない（名前の規則に合わないセグメント、20 段より深い、255 文字より長い。CHECK が外されている）。`rule` に parse の理由
  - `reason=moved_path_unconfirmed`: `moved` の墓標の行き先の名前空間は残っているが、セグメントから組み立てた今のパスが、同じ ID の今のパスと一致しない
    （保存された `full_path` がセグメントと食い違う。部分木の書き換えが行を取りこぼした）。`deleted` と同じに扱うのは、行き先がパージされた
    （`namespace_id` が NULL）ときだけである
  - `reason=unknown_namespace_kind` / `unknown_tombstone_kind` / `unknown_visibility`: 保存された種類・可視性が CHECK の外の値（`raw` に保存値）
  - `reason=namespace_without_project`: `kind = 'project'` の名前空間に、対応する `forge.projects` の行がない
  - `reason=project_without_namespace`: `forge.projects` の行に、`kind = 'project'` の名前空間がない
  - ID によるプロジェクトの読み込み（`GET /api/v1/projects/{id}`、M1 phase 8 MR5）だけのもの:
    - `reason=project_path_unconfirmed`: プロジェクトの名前空間の保存された `full_path` が、親をたどってつないだセグメントと一致しない
      （`moved_path_unconfirmed` と同じ食い違い。`namespace_id` に名前空間）
    - `reason=stored_path_invalid`（`project_id` 付き）: プロジェクトの `full_path` が `NamespacePath::parse` を通らない（`rule` に理由）
    - `reason=project_without_repository`: `forge.projects` の行に `forge.repositories` の行がない（1 対 1 対 1 の片側が消えている）
    - `reason=unknown_repository_kind`: `forge.repositories.kind` が CHECK の外の値（`raw` に保存値）
  - パスの解決（Smart HTTP、M1 phase 9 MR3 からリポジトリの ID と状態も同じ文で読む）でも出るもの:
    - `reason=project_without_repository`: 上と同じ（プロジェクトのパスへの clone / fetch が 500 になる）。認可の前に出るので、
      プロジェクトが見えない人にも 404 ではなく 500 になる。`projects::create` はプロジェクト・名前空間・リポジトリを 1 つの
      トランザクションで書き、リポジトリの行だけを消す処理はないので、この理由が出るのはデータが壊れたときだけである（受け入れた残り）
    - `reason=unknown_repository_state`: `forge.repositories.state` が CHECK の外の値（`raw` に保存値）
- ロールの連鎖が壊れている場合（ループ、不明なロール）は、同じ読み込みでも FG-DB-0018（Runbook `membership-data`）になる。

## Impact

- そのパス（またはそのプロジェクト）への要求が、どの利用者でも 500 で失敗する（推測でリダイレクトしたり、行の一部で判定したりしない。拒否される）。
- 壊れた行は `authorize()` より前に 500 になるので、ないパス・見えないパスの 404 と区別でき、そのパスに（壊れた）何かがあることが相手に分かる。
  コードが alert で必ず知らせるため、この露出は受け入れる（直すまでの間だけ続く）。
- データが forge-db の外で変えられている（手作業の変更、ダンプの復元、制約の削除、プロジェクト・名前空間・リポジトリの 1 対 1 対 1 の片側だけを消したパージ）。
  原因が分かるまで SEV2 として扱う。

## Diagnose（読み取りのみ）

1. ログの `context` を読む: `reason`、`namespace_id`、`tombstone_id`（`moved` の墓標のとき）、`project_id`、`unknown_*` なら `raw`、
   `stored_path_invalid` なら `rule`（パスそのものはログに出ない）。`moved_path_unconfirmed` なら、手順 2 の名前空間から親をたどって
   （Runbook `membership-data` の連鎖の SQL）セグメントをつなぎ、`full_path` と比べる。
2. 行を読む:

   ```sql
   SELECT id, parent_id, kind, owner_id, segment, full_path, depth FROM forge.namespaces WHERE id = <namespace_id>;
   SELECT id, old_path, kind, namespace_id, parent_namespace_id, cause_type, cause_id, created_at
     FROM forge.namespace_tombstones WHERE id = <tombstone_id>;
   SELECT id, visibility, deleted_at, purge_after FROM forge.projects WHERE id = <project_id>;
   SELECT id, full_path FROM forge.namespaces WHERE kind = 'project' AND owner_id = <project_id>;
   SELECT id, project_id, kind, state FROM forge.repositories WHERE project_id = <project_id>;
   ```

3. 制約が残っているか: `SELECT conname FROM pg_constraint WHERE conname IN ('namespaces_segment', 'namespaces_kind',
   'namespace_tombstones_kind', 'projects_visibility', 'repositories_kind');`
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
