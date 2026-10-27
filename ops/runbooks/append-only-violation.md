# Runbook: append-only-violation（FG-DB-0009）

## Trigger

- `error_code=FG-DB-0009` AppendOnlyViolation（ログ、または `forge doctor` の `db.bootstrap`）
- `forge doctor` が `FAIL  db.bootstrap  FG-DB-0009 …` を出した

## Impact

- `reason=rejected`: 追記のみの表への変更が拒否された。データは守られている。呼び出し元の不具合か、手作業の変更の試み（SEV3。手作業なら SEV2 として扱い、誰が何をしたかを確かめる）。
- `reason=guard_missing` / `role_membership_changed`: 保護の仕組みが弱まっている。直ちに利用者への影響はないが、監査の記録の信頼性に関わる（SEV2）。

## Diagnose（読み取りのみ）

1. ログの `context` を読む（`forge doctor` の出力には、ロールの名前と `unexpected` / `missing` は出さない。ログで読む）。
   - `reason=rejected`: `query`（文の名前）、`sqlstate=XF001`、`db_message`（操作と表の名前。例 `UPDATE on forge_audit.ddl_audit is not allowed (append-only)`）。
     その `query` を出したコードを探す。追記のみの表は、その表を持つ関数だけが変える（ADR-0018 §3）。
   - `reason=guard_missing` の `guard`:
     - `bootstrap_version`: 導入の手順の版が違う（`found` と `expected`）、または未導入（`found=none`） → Runbook `db-bootstrap`
     - `role_attributes`: ロールがない、LOGIN の有無が違う、または SUPERUSER・CREATEROLE・CREATEDB・BYPASSRLS・REPLICATION のどれかが付いている（ログに `role`）
     - `role_separation`: `forge_app`・`forge_maint`・`forge_migrator` のどれかが、決められたもの以外のロール（`pg_write_all_data` などの組み込みのロールを含む）のメンバー、
       または `forge_app` / `forge_maint` が `forge_owner`・`forge_seq`・`forge_purge` に届く（ログに `role` と `member_of`）
     - `object_owner`: スキーマ `forge_audit` か、記録の表の所有者が `forge_audit` でない、または表にルールや行のポリシーがある（`object`）
     - `log_triggers`: 記録の表の保護のトリガーがない、別の関数を呼んでいる、種類が違う、`ENABLE ALWAYS` でない、または余分なトリガーがある（`trigger`）
     - `event_triggers`: DDL の記録のイベントトリガーがない、無効、別の関数を呼んでいる、所有者が superuser でない（`trigger`）
     - `function_owner` / `function_body`: 保護の関数の所有者（`forge_audit`、`cluster_identity` は superuser）か本文が出荷したものと違う（`function`）
     - `function_privilege`: 保護の関数の EXECUTE が出荷した状態と違う（`cluster_identity()` は `forge_app`・`forge_seq` だけ、ほかの 2 つはどの forge のロールにもない。`function`・`role`）
     - `object_owner` で `object=database` / `schema public`: DB か `public` の所有者が superuser でない（`forge_owner` が DB を持つと、superuser のセッションに関数を仕掛けられる）。拡張の名前なら `forge_audit` に拡張がある
     - `privileges`: DB・スキーマ（`public`・`forge`・`forge_audit`）・記録の表・`forge._sqlx_migrations` の権限、または `forge_owner`・`forge_seq`・`forge_purge` の既定の権限が出荷した状態と違う（`object`。見つかった権限の一覧はログの `grants`）。GRANT / REVOKE は DDL の記録に対象が残らないので、これが唯一の検出
     - `db_settings`: DB の既定のセッションの設定（`ALTER DATABASE … SET`、`role=*`）か、ログインの設定が導入の手順のもの（`search_path` の 3 行）と違う（`role`）
   - `guard=migration_records`（`version`）: `forge.audit_log` の `db.migrate` の行が、このバイナリのマイグレーションと合わない（存在しない版、同じ版の 2 行目、`guarded_changes` の食い違い）。マイグレーションの外で書かれた行は、その取引の DDL を想定内に見せかける。`SELECT id, txid, occurred_at, details FROM ONLY forge.audit_log WHERE action = 'db.migrate' ORDER BY id;` で一覧にし、同じ `txid` の `forge_audit.ddl_audit` の行を確かめる。セキュリティの事案として扱う
   - `reason=unexpected_ddl`: 導入の手順でもマイグレーションでもない DDL が、確認されないまま残っている（ログに `count` と `first_id`）。
     `SELECT id, occurred_at, session_user_name, application_name, command_tag, object_identity FROM forge_audit.ddl_audit_status() WHERE NOT expected AND NOT acknowledged ORDER BY id;` で一覧にする
   - `reason=role_membership_changed`: ログの `unexpected`（決まったものでも `database.privileged_members` にもないメンバーの関係、または ADMIN OPTION 付き。`member->role`）と
     `missing`（あるはずのメンバーの関係）。
2. 状態を確かめる（読み取りのみ。`forge_app` で接続してよい。superuser で接続するときは、最初に `SET search_path = pg_catalog, pg_temp;` を実行する）:
   - トリガー: `SELECT c.relname, t.tgname, t.tgenabled, t.tgtype, t.tgfoid::regprocedure, t.tgattr, t.tgqual IS NOT NULL AS has_when FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname IN ('forge', 'forge_audit') AND NOT t.tgisinternal;`
   - 表の権限（移行後の表を含む）: `SELECT c.relname, c.relacl FROM pg_class c WHERE c.relnamespace IN ('forge'::regnamespace, 'forge_audit'::regnamespace) AND c.relkind = 'r';`、列の権限は `SELECT attrelid::regclass, attname, attacl FROM pg_attribute WHERE attacl IS NOT NULL AND attrelid IN (SELECT oid FROM pg_class WHERE relnamespace = 'forge'::regnamespace);`
   - ルールと行のポリシー: `SELECT ev_class::regclass, rulename FROM pg_rewrite WHERE ev_class IN (SELECT oid FROM pg_class WHERE relnamespace = 'forge'::regnamespace) AND rulename <> '_RETURN';`
   - 権限: `SELECT nspname, nspacl FROM pg_namespace WHERE nspname IN ('public', 'forge', 'forge_audit');`、`SELECT datacl FROM pg_database WHERE datname = current_database();`、`SELECT defaclrole::regrole, defaclnamespace::regnamespace, defaclobjtype, defaclacl FROM pg_default_acl;`
   - イベントトリガー: `SELECT evtname, evtevent, evtenabled, evtowner::regrole, evtfoid::regprocedure FROM pg_event_trigger;`
   - メンバーの関係: `SELECT m.rolname AS member, r.rolname AS role, am.admin_option FROM pg_auth_members am JOIN pg_roles r ON r.oid = am.roleid JOIN pg_roles m ON m.oid = am.member WHERE r.rolname LIKE 'forge\_%' OR m.rolname LIKE 'forge\_%';`
3. いつ誰が変えたかを DDL の記録で確かめる:
   `SELECT occurred_at, session_user_name, application_name, command_tag, object_type, object_identity FROM forge_audit.ddl_audit ORDER BY id DESC LIMIT 50;`
   同じ `txid` に `forge_audit.bootstrap_log` の行があれば導入の手順の変更。`forge-migrate` で `forge_migrator` ならマイグレーション。それ以外は想定外の変更。

## Mitigate

1. `reason=rejected` で呼び出し元の不具合: 修正を通常の開発の流れ（failing test から）で入れる。保護を外さない。
2. `guard_missing`（トリガー・イベントトリガー・関数・権限・所有者）: 導入の手順の `03-database-objects.sql` を再実行して戻す（Runbook `db-bootstrap` の手順 4）。
   03 は保護のトリガーとイベントトリガーを作り直し、余分なトリガーを外し、関数と権限と所有者を揃える。`[MUTATING — needs approval]`（ロールバック: 不要。出荷した状態に揃えるだけ。外したトリガーの名前は警告に出るので障害記録に残す）
3. `role_attributes`: `01-cluster-roles.sql` を再実行して属性を戻す。`[MUTATING — needs approval]`（ロールバック: 不要）
4. `role_separation` / `role_membership_changed` で正当でないメンバーの関係: 外す（`REVOKE <role> FROM <member>;`）。`[MUTATING — needs approval]`（ロールバック: 外したメンバーの関係を `GRANT` で戻す。外す前の一覧を障害記録に残す）
5. `unexpected_ddl`: 変更を誰が・なぜ行ったかを確かめる。正当なら確認の行を残す（`forge_maint` で `INSERT INTO forge_audit.ddl_audit_ack (ddl_audit_id, note) VALUES (<id>, '<理由>');`。`forge admin ddl-audit ack` ができるまでの手順）。`[MUTATING — needs approval]`（ロールバック: 不可。確認の行は追記のみ。誤って確認した場合は、その旨を新しい確認の行の `note` に残す）。正当でない変更は戻し、セキュリティの事案として扱う
6. マイグレーションで作った表（`forge.events` など）の `privileges` / `log_triggers`: 03 では直らない。出荷した状態はマイグレーションのファイルにあるので、余分な権限を `REVOKE` し、トリガーを `ALTER TABLE … ENABLE ALWAYS TRIGGER …` で戻す（superuser、`SET search_path = pg_catalog, pg_temp;` から）。`[MUTATING — needs approval]`（ロールバック: 外した権限を `GRANT` で戻す。前の状態をログの `grants` から障害記録に残す）
7. `role_membership_changed` で正当な管理者のロール: レビューの後、`database.privileged_members` に `role:member` を追加する。`[MUTATING — needs approval]`（ロールバック: 設定から外す）

## Verify

- `forge doctor` の `db.bootstrap` が `ok`
- 新しい FG-DB-0009 が出ていない

## Escalate

- 誰も説明できない DDL や、メンバーの関係の変更があった: セキュリティの事案として `/incident` で扱う（SEV2）。DB の superuser の操作の記録を DB の担当者に依頼する。
