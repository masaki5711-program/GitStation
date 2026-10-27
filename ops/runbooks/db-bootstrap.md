# Runbook: db-bootstrap（DB のロールと保護の仕組みの導入）

ADR-0018 §1 の導入の手順です。新しい DB のサーバーに GitStation を入れるとき、または `forge doctor` の `db.bootstrap` が
`FG-DB-0009 reason=guard_missing guard=bootstrap_version` を出したとき（新しい版の導入ファイルがある）に使います。

## Trigger

- 新しい環境の構築（DB のサーバーに GitStation の DB がまだない）
- `forge doctor` の `db.bootstrap` が `guard=bootstrap_version`（`found=none` は未導入、数字は古い版）
- `forge db migrate` が `FG-SYS-0002 config_key=database.migrate_url reason=wrong_role role_check=owner_missing`（ロールがない）

## Impact

- 導入の前は `forge serve` も `forge db migrate` も動かない（新しい環境なので利用者への影響はない）。
- 版が古いだけの場合は、`forge doctor` が警報を出すが、`forge serve` は動き続ける。

## Diagnose（読み取りのみ）

1. `forge doctor --json` を実行し、`database`、`db.bootstrap`、`db.connections` の行を読む。
2. 版を確かめる（`forge_app` で読める）:
   `SELECT bootstrap_version, applied_at, applied_by FROM forge_audit.bootstrap_log ORDER BY id;`
3. ロールがあるか確かめる: `SELECT rolname, rolcanlogin, rolsuper, shobj_description(oid, 'pg_authid') FROM pg_roles WHERE rolname LIKE 'forge\_%';`
   `forge_owner`・`forge_migrator`・`forge_seq`・`forge_purge`・`forge_app`・`forge_ro`・`forge_maint`・`forge_audit` の 8 つがあり、
   LOGIN は `forge_migrator`・`forge_app`・`forge_maint` だけ、`rolsuper` はすべて false、コメントが `forge bootstrap role` であること。

## Mitigate

導入のファイルは `Git/ops/db/bootstrap/` にあり、**superuser** で `psql` から順に実行する（DB の所有者もその superuser になる。`forge_owner` は DB を所有しない）。
手作業で superuser のセッションを開くときは、最初に `SET search_path = pg_catalog, pg_temp;` を実行する。各ファイルは最初の誤りで止まる
（`\set ON_ERROR_STOP on`）。01 と 03 は何度実行してもよく、03 は保護の仕組みを出荷した状態に作り直す。

1. クラスターのロールを作る。`[MUTATING — needs approval]`
   `psql -d postgres -f 01-cluster-roles.sql`
   - 同じ名前のロールが既にあり、このファイルが作ったものでない（コメントがない）場合は止まる（ロールはクラスター全体のもので、別の DB のものかもしれない）。
     そのロールの用途を DB の担当者と確かめ、GitStation のものにしてよいときだけ `COMMENT ON ROLE <name> IS 'forge bootstrap role';` を実行してから再実行する。
   - （ロールバック: 新しい環境なら `DROP ROLE` で作ったロールを消す。既存の環境では不要。属性を正しい値に揃えるだけ）
2. ログインのパスワードを設定する（値は記録に残さない。`\password` は入力を表示しない）。`[MUTATING — needs approval]`
   `psql -d postgres -c '\password forge_app'`、同様に `forge_migrator`、`forge_maint`。
   （ロールバック: 前のパスワードに戻す。前のパスワードの値はこの手順の外で保管する）
3. DB を作る（新しい環境だけ。既にあれば止まる）。`[MUTATING — needs approval]`
   `psql -d postgres -v dbname=forge -f 02-create-database.sql`
   （ロールバック: 新しい環境なら `DROP DATABASE forge`。データが入った後は行わない）
4. DB の中のオブジェクトを作る、または新しい版に更新する・保護の仕組みを直す。`forge db migrate` の実行中は行わない（ロックの待ちで片方が失敗する）。`[MUTATING — needs approval]`
   `psql -d forge -f 03-database-objects.sql`
   （1 つのトランザクション。途中で失敗したら何も変わらない。03 はすべての権限（受け手を問わない。付与権を通した付与も CASCADE で外す）・既定の権限・セッションの既定の設定を出荷した状態に作り直す。外した余分なトリガー・関数・拡張・ルール・権限と、消したセッションの設定（運用者が設定した `log_min_duration_statement` なども消える）は警告に出るので、障害記録に残し、必要な設定は別の方法（`postgresql.conf`）で戻す。ロールバック: 前の版のファイルを同じように実行する）

   **版 2 からの更新**（版 2 では DB の所有者が `forge_owner`、マイグレーションの記録が `public._sqlx_migrations`）: superuser で
   `SET search_path = pg_catalog, pg_temp; ALTER DATABASE forge OWNER TO <superuser>; ALTER TABLE public._sqlx_migrations SET SCHEMA forge;` を実行してから 03 を実行する。`[MUTATING — needs approval]`
   （ロールバック: `ALTER TABLE forge._sqlx_migrations SET SCHEMA public; ALTER DATABASE forge OWNER TO forge_owner;`）
5. 設定を **ログインごとに別のファイル** に書く（`forge serve` は移行と保守のパスワードのファイルを持つ設定では起動しない — `reason=not_for_this_command`）。
   - `forge serve` / `forge doctor`: `database.url = postgres://forge_app@<host>:<port>/forge`、`database.password_file`
   - `forge db migrate`: 上に加えて `database.migrate_url = postgres://forge_migrator@<host>:<port>/forge`、`database.migrate_password_file`
   - `forge admin …`: `database.maint_url = postgres://forge_maint@<host>:<port>/forge`、`database.maint_password_file`
   - パスワードのファイルは絶対パス、所有者だけが読める（0600）。`forge serve` のサービスのユーザーは移行と保守のパスワードのファイルを読めないようにする。
   - DB の管理者のロールを `forge_owner` などのメンバーにする場合は、`database.privileged_members` に `forge_owner:<role>` の形で書く。
6. 開発機（`ops/dev/compose.yml` の DB）では承認は不要で、`Git/` で次のように実行する:
   `docker compose -f ops/dev/compose.yml exec -T postgres psql -U forge_dev -d forge_dev -f - < ops/db/bootstrap/01-cluster-roles.sql`
   （02 は `-v dbname=forge`、03 は `-d forge` で同様に）。テストは同じファイルを自分で実行する（ループバックの DB だけ。パスワードはソースにある合成の値）。

## Verify

- `forge doctor` の `db.bootstrap` が `ok … version N, guards intact`、`db.connections` が `ok`
- `forge db migrate` が `up to date` か、保留のマイグレーションを適用する

## Escalate

- 03 が `database … must be owned by a superuser (create it with 02)` か `schema forge_audit exists and is owned by …` で止まる: 02 で作っていない DB か、別のものが同じ名前を使っている。DB の担当者と確認する（版 2 の DB は下の「版 2 からの更新」）。
- 03 が `public._sqlx_migrations exists` で止まる: 版 2 の DB。下の「版 2 からの更新」。
- superuser で実行できない環境（マネージドの DB など）: ADR-0018 の前提が崩れるので、設計の見直しとしてエスカレーションする。
