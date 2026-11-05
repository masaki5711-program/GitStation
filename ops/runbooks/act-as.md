# Runbook: act-as（FG-INT-0006 / FG-AUTH-0020 / 代理実行の鍵）

PM ツール（kanflow）が、**本人として** forge を操作する「代理実行（act-as）」の診断と運用の手順（M3 S2、ADR-0025）。

- kanflow は連携のサービスアカウントのトークン（スコープ `act_as`）で呼び、`Forge-Act-As: user:<id>` または `employee:<社員番号>` で本人を名指す。
- **読み取り**（`ProjectRead`・`MergeRequestRead`・`MergeRequestList`）は主張なしで、**サービスアカウントのロールと本人のロールの小さい方** で判定する。
- **承認と承認の取り消し** は、本人が forge の確認の画面で押した 1 回限りの主張（`Forge-Assertion: fga_…`）が必須。主張は MR・head の SHA・target の ref・target の先端・連携・本人に結びつき、HMAC で封じ、5 分で切れ、1 回で消える。
- マージ・マージの設定・その他すべての操作は、代理では常に `FG-AUTH-0003 reason=act_as_not_allowed`。
- 主張の秘密は claim のときに初めて作り、DB には鍵つきの MAC（`mac`）だけを置く。鍵はファイル `auth.act_as_hmac_keys_file`（秘密の名前 `act_as_hmac_keys`、ADR-0013）。

## Trigger

- kanflow の承認・読み取りが `403 FG-INT-0006`（本人を解決できない）または `403 FG-AUTH-0020`（主張が無効）で失敗する。
- kanflow の要求が `403 FG-AUTH-0003` の `reason=act_as_not_configured` / `act_as_no_integration` / `integration_disabled` / `act_as_requires_service_account` / `agent_token`、または `403 FG-AUTH-0002 required_scope=act_as` で失敗する。
- `forge doctor` の `act_as.keys` が `FAIL ... FG-SYS-0002`（鍵のファイルが読めない・形が不正・`active` が 0 か 2 つ以上、または `reason=act_as_keys_missing`: `act_as` スコープの有効なトークンがあるのに鍵が無い）。
- `forge serve` が `FG-SYS-0002 config_key=auth.act_as_hmac_keys_file` で起動しない。
- ログ `forge.auth.act_as` に `reason=ambiguous`（**データの破損**として扱う）、または `reason=mac_mismatch`（**改ざんの兆候**として扱う）。
- `FG-INT-0006` が短い時間に急増する（kanflow の誤設定か、利用者の存在の探索の兆候）。
- 鍵の定期の入れ替え、鍵の漏えいの疑い、DB の復元の後。

## Impact

- 代理が止まっても、本人が forge の画面で操作すれば承認できる（代理は便利のための経路）。データは失われない。
- 主張の検証の失敗は、何も書かずに断る。検証した後に使われなかった主張は `rejected` として消費し、使い回せない。
- `mac_mismatch` は、鍵を持たない者が DB の行を書いた・書き換えた可能性を示す（通る主張は作れないので、承認は通っていない）。
- 鍵が漏れ、かつ `forge_app` の資格情報も漏れると、主張を作れる。疑いがあれば下の「代理を即座に止める」と鍵の入れ替えを行う。

## 本番での実行の承認

次の操作は本番のデータか設定を変える。本番では **人が承認した手順としてだけ** 実行する（CLAUDE.md §3.7）。各手順に `[MUTATING — needs approval]` と書く。

- 鍵のファイルの変更と `forge serve` の再起動（入れ替え）
- サービスアカウントの無効化（`gstn user disable`）、連携の無効化（`PATCH /api/v1/admin/integrations/{id}`、管理者の Web セッション）
- 一意の索引 `users_employee_number` の作り直し、重複した社員番号の解消

## Diagnose（読み取りのみ）

1. 問い合わせ番号（`request_id`）でログを引く。代理の要求は **成功も失敗も 1 行** の `forge.auth.act_as` を出す。

   ```sh
   # JSON のログから（例）
   grep '"request_id":"<request_id>"' forge.log | grep forge.auth.act_as
   ```

   欄: `outcome`、`error_code`、`reason`、`action`、`act_as_rule`、`target_form`（`user` / `employee`）、`actor.user`（本人の ID）、`actor.via`（サービスアカウント）、`integration_id`、`credential_id`（トークンの ID）、`assertion_public_id`、`claimed_mac`（鍵つき HMAC の先頭 8 文字）。**ヘッダーの値・社員番号・主張の秘密はどこにも出ない。**

2. `reason` で原因を決める。

   | コード | `reason` | 原因 | 確かめ方 |
   |---|---|---|---|
   | FG-INT-0006 | `missing_value` / `malformed` | 空、文法の外（`user:<数字>` か `employee:<英数字 . _ ->`）、前後の空白、`,` を含む | kanflow の送る値の作り方 |
   | FG-INT-0006 | `multiple_headers` | `Forge-Act-As` が 2 つ | kanflow の HTTP クライアント・プロキシ |
   | FG-INT-0006 | `email_form` | `user:` にメールを入れた（メールでは指さない、ADR-0021） | kanflow を `sub.user_id` か社員番号に直す |
   | FG-INT-0006 | `unknown` | その ID・社員番号の利用者がいない（forge に一度もサインインしていない人を含む） | `SELECT id, state FROM forge.users WHERE id = <id>`（社員番号は書き込みのプールで、本人の確認の後に） |
   | FG-INT-0006 | `not_human` | 相手がサービスアカウント（自分自身を含む）かボット | kanflow の対応表 |
   | FG-INT-0006 | `disabled` | 相手が無効化されている | `SELECT state FROM forge.users WHERE id = <id>` |
   | FG-INT-0006 | `ambiguous` | 社員番号が 2 行に当たった（一意の索引が壊れた・外された）。**閉じる側に倒し、誰としても扱わない** | `SELECT indexdef FROM pg_indexes WHERE indexname = 'users_employee_number'`。下の「社員番号の重複」 |
   | FG-WEB-0010 | `act_as_resolution_failures` | 1 つのサービスアカウントの解決の失敗が 1 分に 60 回を超えた | kanflow の誤設定か探索。`Retry-After` の後に再開する |
   | FG-AUTH-0003 | `act_as_not_allowed` | 代理で許していない操作（マージ・設定・コメント・チェック・連携の履歴…） | 本人が forge の画面で行う |
   | FG-AUTH-0003 | `act_as_requires_service_account` | セッション・人のトークン・ボットにヘッダー | kanflow は連携のサービスアカウントのトークンで呼ぶ |
   | FG-AUTH-0003 | `agent_token` | エージェントと宣言したトークン | `act_as` はエージェントのトークンに付かない（発行で `FG-AUTH-0008 act_as_agent_token`） |
   | FG-AUTH-0003 | `act_as_no_integration` / `integration_disabled` | サービスアカウントの連携が無い・無効 | `gstn admin integration list`（管理者） |
   | FG-AUTH-0003 | `act_as_not_configured` | 鍵のファイルが無い | `forge doctor` の `act_as.keys` |
   | FG-AUTH-0002 | `required_scope=act_as`（または `merge_approve`） | トークンのスコープが足りない | `gstn admin service-account tokens <id> --json` |
   | FG-REPO-0005 | `act_as_unbound` / `act_as_not_visible` | 連携に結びついていないプロジェクト、または本人かサービスアカウントの一方に見えない | `GET /api/v1/projects/{id}/integration-bindings` |
   | FG-AUTH-0020 | `missing` | 承認・取り消しに主張が無い | kanflow は確認の画面を開いてから送る |
   | FG-AUTH-0020 | `unexpected` | 読み取りに主張、または `Forge-Act-As` の無い `Forge-Assertion` | kanflow の送り方 |
   | FG-AUTH-0020 | `malformed` / `multiple_headers` | `fga_<16 桁>_<64 桁>` でない、2 つ | kanflow の送り方 |
   | FG-AUTH-0020 | `unknown` | その public id の claim 済みの主張が無い | 下の SQL |
   | FG-AUTH-0020 | `key_retired` | 引退した鍵で封じた主張 | 入れ替えの直後なら、確認の画面を開き直す |
   | FG-AUTH-0020 | `mac_mismatch` | 行が鍵なしで書かれた・書き換えられた。**改ざんの兆候** | 行を残し、下の「改ざんの疑い」 |
   | FG-AUTH-0020 | `expired` / `consumed` | 5 分を過ぎた / 使用済み・拒否済み（同時の 2 回目を含む） | 確認の画面を開き直す |
   | FG-AUTH-0020 | `wrong_integration` / `wrong_user` | 別の連携の主張 / `Forge-Act-As` の本人と押した人が違う | kanflow の `sub` の照合（下の「kanflow の `sub`」） |
   | FG-AUTH-0020 | `wrong_action` / `wrong_resource` | 承認の主張で取り消し、別の MR | kanflow の対応 |
   | FG-AUTH-0020 | `sha_mismatch` / `head_changed` / `target_retargeted` / `target_changed` | 本文の sha が違う / push された / target が変わった / target が進んだ | 確認の画面を開き直す（新しい head を本人が見る） |
   | FG-AUTH-0020 | `claim_unknown` / `claim_expired` / `claim_repeated` | claim の state が無い・期限切れ・2 回目（2 回目は何も返さない） | kanflow の state の保存。応答を失ったなら押し直し |
   | FG-REVIEW-0013 | `self_approval` | 本人が MR の作成者 | 本人は承認できない（代理でも同じ） |

3. 主張の行を見る（`forge_ro` は `mac` と `state_hash` を読めない）。

   ```sql
   SELECT id, state, intent, integration_id, user_id, project_id, merge_request_id, head_sha,
          target_ref, issued_at, expires_at, claimed_at, finished_at, reject_reason,
          issued_request_id, claimed_request_id, finished_request_id
   FROM forge.act_as_assertions WHERE public_id = '<assertion_public_id>';
   ```

   `pending`（押された、claim の前）→ `claimed`（kanflow が受け取った）→ `consumed`（操作に使った）/ `rejected`（`reject_reason`: `expired` は期限切れの片づけ、`action_refused` / `action_failed` / `not_used` は検証の後に使われなかった）。

4. 監査を本人ごとに見る（代理の変更は `actor_kind = user_via_service_account`、`actor_user_id` が本人、`actor_via_id` がサービスアカウント）。

   ```sql
   SELECT occurred_at, action, resource_type, resource_id, project_id, via_assertion_id, request_id
   FROM forge.audit_log
   WHERE actor_user_id = <本人の id>
     AND (action LIKE 'act_as_assertion.%' OR actor_kind = 'user_via_service_account')
   ORDER BY id DESC LIMIT 50;
   ```

5. 監査の `act_as_claimed`（主張された相手の鍵つき HMAC）で行を探すには、ホストの操作者が値を HMAC にする（読み取りだけ。DB に書かない）。

   ```sh
   forge admin act-as claimed-mac --value 'employee:<社員番号>'          # 今の active の鍵
   forge admin act-as claimed-mac --value 'user:<id>' --kid <act_as_key_id>  # 古い行はその鍵で
   ```

   ```sql
   SELECT occurred_at, action, actor_user_id, request_id FROM forge.audit_log
   WHERE act_as_claimed = '<出力の act_as_claimed>' AND act_as_key_id = <kid>;
   ```

6. 鍵の状態: `forge doctor` の `act_as.keys`（`N active, N verifying, N retired`）。

## Mitigate

### 代理を即座に止める `[MUTATING — needs approval]`

- 連携ごと止める: 連携を無効化する（管理者の Web セッションで `PATCH /api/v1/admin/integrations/{id}` に `{"enabled": false}`）。代理の要求は `integration_disabled` になる。
- トークンの漏えい: そのトークンを失効させる（runbook service-accounts）。
- 今すぐすべて: サービスアカウントを無効化する（`gstn user disable <id>`、`admin` スコープ）。

### 鍵の入れ替え `[MUTATING — needs approval]`

1. 32 バイトの乱数を作る（`openssl rand -base64 32`）。鍵のファイルに新しい kid の行を足し、**新しい行に `active`、古い行の印を外す**（古い行は印なし = まだ検証に使う）。

   ```
   1:<古い鍵の base64>
   2:<新しい鍵の base64> active
   ```

2. `forge serve` を再起動する（鍵は起動のときに読む）。`forge doctor` で `act_as.keys` が `1 active, 1 verifying, 0 retired`。
3. **10 分**（主張の寿命 5 分 + 時計のずれ）待ってから、古い行に `retired` を付け、もう一度再起動する。`retired` の鍵の主張は `key_retired` で断るが、鍵は監査の `act_as_claimed` の突き合わせのためにファイルに残す（消すと、その鍵の行は突き合わせられなくなるだけで、判定には影響しない）。
4. 鍵の漏えいの疑いなら、1 の後すぐに古い鍵を `retired` にしてよい（使いかけの主張は押し直しになる）。

### 復元の後 `[MUTATING — needs approval]`

DB を復元したら、代理の鍵を入れ替える（上の手順）。復元の前の主張は寿命の 5 分で既に使えないが、入れ替えで確実にする。

### 社員番号の重複（`reason=ambiguous`） `[MUTATING — needs approval]`

1. 重複を見る: `SELECT employee_number, array_agg(id) FROM forge.users WHERE employee_number IS NOT NULL GROUP BY 1 HAVING count(*) > 1;`
2. どちらが本人かを Keycloak の管理者と確かめ、もう一方の行の扱いを決める（無効化された利用者の社員番号は、トリガー `users_employee_number_kept_when_disabled` で消せない・変えられない。必要なら承認された手順でトリガーを外さずに、正しい側を残す方針を決める）。
3. 一意の索引を作り直す: `CREATE UNIQUE INDEX users_employee_number ON forge.users (employee_number) WHERE employee_number IS NOT NULL;`（マイグレーションの所有者で）。直るまで、その番号の代理は誰としても扱われない（閉じる側）。

### 改ざんの疑い（`reason=mac_mismatch`）

行を残す（消さない、変えない）。`issued_request_id` / `claimed_request_id` と監査の行を突き合わせ、`forge_app` の資格情報で DB に書ける者を確かめる。鍵を入れ替え（上）、`forge_app` のパスワードを入れ替える（runbook db-bootstrap）。承認は通っていない（MAC が合わない主張は使えない）。

### kanflow の `sub`

claim の応答の `sub.user_id` を、kanflow は state を結びつけた kanflow の利用者と照らし、違えば捨てる（共用の PC で別の人の forge のセッションで押された場合）。forge の側は `Forge-Act-As` の本人 = 主張の本人を必ず確かめる（`wrong_user`）ので、他人として **書く** ことはできない。`wrong_user` が続くなら、kanflow の照合と対応表を確かめる。

## Verify

- `forge doctor` の `act_as.keys` が ok。
- kanflow から承認して、`forge.act_as_assertions` の行が `consumed`、監査の `merge_request.approved` が `actor_kind = user_via_service_account` で本人とサービスアカウントの両方を持つ。
- 同じ主張の 2 回目が `FG-AUTH-0020 reason=consumed`。

## 注記: 受け入れた 2 つの残る危険（ADR-0025 §9）

1. **侵害された kanflow は、本人に別の MR の確認の画面を開かせられる**（ポップアップの URL は kanflow が作る）。守りは、画面が forge のデータだけ（MR の番号・タイトル・SHA・target・受け取る連携）を表示し、kanflow の文字を出さないこと。本人が見ずに押せば防げない。疑わしいときは、本人ごとの `act_as_assertion.issued` の監査（Diagnose 4）を kanflow の画面の操作の記録と突き合わせる。
2. **`FG-INT-0006` と、相手が決まった後のコード（`FG-AUTH-0003` など）の違いで、サービスアカウントは利用者の存在を知れる**。守りは、解決の失敗の流量の制限（1 分に 60 回、`FG-WEB-0010`）とログ。`FG-INT-0006` の急増は kanflow の誤設定か探索の兆候として扱い、Diagnose 1 で `actor.via` ごとに数え、必要なら連携を止める。

## 関連

- ADR-0025（代理実行）、ADR-0017 §4.2（確認の画面）、ADR-0016 §9（監査の列）
- runbook service-accounts（`act_as` スコープは連携のアカウントだけ）、integrations（代理を止める）、user-offboarding（無効化しても社員番号は残る）、config-invalid（`FG-SYS-0002`）
