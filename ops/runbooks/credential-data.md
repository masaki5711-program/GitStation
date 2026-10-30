# Runbook: credential-data（FG-DB-0019）

## Trigger

- `error_code=FG-DB-0019` CredentialDataInvalid（ログ）: トークンの認証（`forge_db::tokens::authenticate`）が、スキーマと forge-db のコマンドでは書かれないはずの `forge.tokens` の行を見つけた。
  - `reason=unknown_scope`: `scopes` にこのビルドが知らないスコープ名がある（`raw` に保存値）
  - `reason=bad_hash`: `secret_sha256` が 32 バイトでない

## Impact

- その 1 つのトークンでの認証が失敗する（スコープを途中まで読んで通すことはしない。拒否される）。
- データが forge-db の外で変えられている（手作業の変更、ダンプの復元、制約の無効化）か、新しいビルドが書いた行を古いビルドが読んでいる。原因が分かるまで SEV2 として扱う。

## Diagnose（読み取りのみ）

1. ログの `context` を読む: `reason`、`token_id`、`public_id`（秘密やハッシュは出ない）、`unknown_scope` なら `raw`。
2. 行を読む（書き込み用ロールで、ハッシュ列は選ばない）:

   ```sql
   SELECT id, user_id, public_id, scopes, created_at, revoked_at FROM forge.tokens WHERE id = <token_id>;
   SELECT conname FROM pg_constraint WHERE conname = 'tokens_scopes_known';
   ```

3. いつ誰が変えたか: `forge_audit.ddl_audit`（制約の変更）と `forge.audit_log` の `token.*` の行（正しい履歴）を見る。
   デプロイ直後で `unknown_scope` なら、新しいビルドが書いたスコープを古いビルドが動いているだけの可能性がある（ビルドの版を確かめる）。

## Mitigate

1. 該当トークンを失効する: `forge admin token revoke <token_id>`、必要なら新しいトークンを発行する。`[MUTATING — needs approval]`
   （ロールバック: 失効は戻せない。同じスコープで新しいトークンを発行し直す）
2. 外された制約を、マイグレーションのファイルの定義どおりに戻す。`[MUTATING — needs approval]`（ロールバック: 不要）
3. 手で UPDATE しない、監査の行を手で書かない、トリガーを止めない。

## Verify

- 同じトークン（または再発行したもの）で認証し、`FG-DB-0019` が出ないこと。
- 手順 2 の SQL が、不正な値を返さないこと。

## Escalate

- 誰がいつ変えたか分からない、または監査の行のない変更がある: セキュリティの事案として扱い（`/incident`）、DB の管理者に連絡する。
- 事後: データを変えた経路を特定し、その手順の Runbook か `forge doctor` の検査に戻す。
