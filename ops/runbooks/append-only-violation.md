# Runbook: append-only-violation（FG-DB-0009）

## Trigger

- `error_code=FG-DB-0009` AppendOnlyViolation（ログ、または `forge doctor` の `db.bootstrap`）
- `forge doctor` が `FAIL  db.bootstrap  FG-DB-0009 …` を出した

## Impact

- `reason=rejected`: 追記のみの表への変更が拒否された。データは守られている。呼び出し元の不具合か、手作業の変更の試み（SEV3。手作業なら SEV2 として扱い、誰が何をしたかを確かめる）。
- `reason=guard_missing` / `unexpected_ddl` / `role_membership_changed`: 保護の仕組みが弱まっている。直ちに利用者への影響はないが、監査の記録の信頼性に関わる（SEV2）。

## Diagnose（読み取りのみ）

1. ログの `context` を読む。
   - `reason=rejected`: `query`（文の名前）、`sqlstate=XF001`、`db_message`（操作と表の名前。例 `UPDATE on forge.ddl_audit is not allowed (append-only)`）。
     その `query` を出したコードを探す。追記のみの表は、その表を持つ関数だけが変える（ADR-0018 §3）。
   - `reason=guard_missing` の `guard`:
     - `bootstrap_version`: 導入の手順の版が違う、または未導入 → Runbook `db-bootstrap`
     - `role_attributes`: ロールがない、LOGIN の有無が違う、または SUPERUSER・CREATEROLE・CREATEDB・BYPASSRLS・REPLICATION のどれかが付いている（`role` が付く）
     - `role_separation`: `forge_app` か `forge_maint` が `forge_owner`・`forge_seq`・`forge_purge` に届く（`role` と `member_of` が付く）
     - `log_triggers` / `event_triggers`: トリガーがない、または `ENABLE ALWAYS` でない（`trigger` が付く）
     - `function_owner` / `function_body`: 導入の手順の関数の所有者が superuser でない、または本文が出荷したものと違う（`function` が付く）
     - `function_privilege`: `forge.cluster_identity()` の EXECUTE が `forge_app`・`forge_seq` 以外にある、または足りない（`role` が付く）
     - `app_login`: `database.url` のログインが superuser（すべての保護を迂回できる）
   - `reason=role_membership_changed`: ログの `unexpected`（設定にないメンバー）と `missing`（あるはずのメンバー）。`forge doctor` の出力にはロールの名前を出さない。
2. 状態を確かめる（読み取りのみ。`forge_app` で接続してよい）:
   - トリガー: `SELECT c.relname, t.tgname, t.tgenabled FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = 'forge' AND NOT t.tgisinternal;`（`tgenabled` が `A` であること）
   - イベントトリガー: `SELECT evtname, evtenabled, evtowner::regrole FROM pg_event_trigger;`
   - メンバー: `SELECT r.rolname AS role, m.rolname AS member FROM pg_auth_members am JOIN pg_roles r ON r.oid = am.roleid JOIN pg_roles m ON m.oid = am.member WHERE r.rolname IN ('forge_owner', 'forge_seq', 'forge_purge');`
3. いつ誰が変えたかを DDL の記録で確かめる:
   `SELECT occurred_at, session_user_name, application_name, command_tag, object_type, object_identity FROM forge.ddl_audit ORDER BY id DESC LIMIT 50;`
   同じ `txid` に `forge.bootstrap_log` の行があれば導入の手順の変更。`forge-migrate` で `forge_migrator` ならマイグレーション。それ以外は想定外の変更。

## Mitigate

1. `reason=rejected` で呼び出し元の不具合: 修正を通常の開発の流れ（failing test から）で入れる。保護を外さない。
2. `guard_missing`（トリガー・関数・権限）: 導入の手順の `03-database-objects.sql` を再実行して戻す（Runbook `db-bootstrap` の手順 4）。`[MUTATING — needs approval]`（ロールバック: 不要。正しい状態に揃えるだけ）
3. `role_attributes` / `role_separation`: `01-cluster-roles.sql` を再実行して属性を戻し、余分なメンバーを外す（`REVOKE forge_owner FROM <role>;`）。`[MUTATING — needs approval]`（ロールバック: 外したメンバーを `GRANT` で戻す。外す前の一覧を障害記録に残す）
4. `role_membership_changed` で正当な管理者のロール: レビューの後、`database.privileged_members` に追加する。正当でなければ 3 と同じく外す。`[MUTATING — needs approval]`
5. `app_login`: `database.url` を `forge_app` のログインに直して再起動する。`[MUTATING — needs approval]`（ロールバック: 前の設定に戻す）

## Verify

- `forge doctor` の `db.bootstrap` が `ok`
- 新しい FG-DB-0009 が出ていない

## Escalate

- 誰も説明できない DDL や、メンバーの変更があった: セキュリティの事案として `/incident` で扱う（SEV2）。DB の superuser の操作の記録を DB の担当者に依頼する。
