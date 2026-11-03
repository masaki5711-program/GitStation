# Runbook: integrations（FG-INT-0012 / FG-INT-0016 / FG-INT-0017）

連携（kanflow）の登録・有効と無効の切り替え・プロジェクトの結びつけと解除の手順（M3 I1a、計画 11 §5 I1、02 §2、ADR-0016 Amendment I1a）。

- 連携（`forge.integrations`）は、つなぐ先の種類（今は `kanflow` だけ）と、連携が使う **サービスアカウント**（1 つの連携に 1 つ。後から変えられない）、
  有効か、イベントの種類の許可リスト（`event_types`。既定は `*` = 連携向けのすべて）でできている。秘密の値はまだ持たない（受信の署名の鍵は後の MR）。
- 結びつけ（`forge.integration_bindings`）は、forge のプロジェクト 1 つと、連携先のプロジェクト（`external_project_id`）の組。
  - **1 つのプロジェクトは 1 つの結びつけだけ**（`integration_bindings_one_per_project`。2 つの kanflow のプロジェクトに結ぶと `PBI-123` が
    どちらのものか決まらないため。02 §2）。逆に 1 つの kanflow のプロジェクトに forge のプロジェクトを複数結ぶのはよい。
  - **1 つの連携に最大 200**（`FG-INT-0012`）。
  - 結びつけると、連携のサービスアカウントが **そのプロジェクトの Reporter** になる（まだメンバーでないとき。`granted_membership = true`）。
    解除すると、その結びつけが付けたメンバーで、まだ Reporter のままなら外す。手で付けた・手でロールを変えたメンバーは残す。
  - 削除待ちのプロジェクトには結べない（`FG-REPO-0006`）。完全削除（purge）で結びつけも消える。
  - **無効の連携、またはサービスアカウントが無効の連携には結べない**（`FG-INT-0017`）。解除はできる。
  - 結びつけが付けたメンバーを人が外すと、付けた印（`granted_membership`）が消える。その後に人が付け直したメンバーは、解除しても残る。

## Trigger

- kanflow と新しくつなぐ（連携の登録）。
- kanflow のプロジェクトとリポジトリを対応させる・やめる（結びつけ・解除）。
- 結びつけが `FG-INT-0016`（409、BindingConflict）で断られた: そのプロジェクトはもう別の連携、または同じ連携の別の外部プロジェクトに結ばれている。
- 結びつけが `FG-INT-0012`（409、IntegrationLimitReached）で断られた: その連携の結びつけが 200 に達した。
- 結びつけが `FG-INT-0017`（409、IntegrationDisabled）で断られた: 連携が無効（`reason=integration_disabled`）、またはその
  サービスアカウントが無効（`reason=service_account_disabled`）。
- 連携をすぐ止めたい（トークンの漏えい、kanflow 側の暴走）。

## Impact

- `FG-INT-0016` / `FG-INT-0012`: その結びつけが作られないだけ。ほかの結びつけ・連携は動く。何も書かれていない。
- 連携の無効化: その連携のイベントの履歴（I1c）が止まる。結びつけとメンバーはそのまま。

## 本番での実行の承認

次の操作は本番のデータを変える。本番では **人が承認した手順としてだけ** 実行する（CLAUDE.md §3.7）。各手順に `[MUTATING — needs approval]` と書く。

- 連携の登録・変更（`POST /api/v1/admin/integrations`、`PATCH /api/v1/admin/integrations/{id}`）
- `gstn integration-binding set` / `remove`、`gstn user disable`

**連携の登録と変更（有効・無効、`event_types`）は管理者の Web セッションだけ**（トークンからは `FG-AUTH-0002 reason=session_required` で
必ず拒否。連携が何を読めるかは人が決める）。管理画面はまだ無い。それまでは管理者がブラウザでサインインし、同じタブの開発者ツールの
コンソールから API を呼ぶ（service-accounts の Runbook と同じ。CSRF の値は `GET /api/v1/session` の `csrf_token` を `X-GitStation-CSRF` で送る）。
結びつけと解除はプロジェクトの **Maintainer 以上**（人だけ。エージェントのトークン・サービスアカウントは不可）で、`api` スコープのトークンでもよい。

## Diagnose（読み取りのみ）

1. 連携の一覧と、それぞれのサービスアカウント・有効か・結びつけの数（管理者の `admin` スコープのトークン）。

   ```sh
   gstn admin integration list --json
   gstn admin integration view <integration_id> --json
   gstn admin integration bindings <integration_id> --json
   ```

2. あるプロジェクトの結びつけ（そのプロジェクトの Reporter 以上）。

   ```sh
   gstn integration-binding list --project <project_id> --json
   ```

3. `FG-INT-0016` のログ: `reason`（`bound_to_other_integration` / `bound_to_other_external_project`）、`project_id`、`integration_id`
   （頼んだ方）、`bound_integration_id`（今の結びつけ）。外部プロジェクトの id はログに出さない。
4. `FG-INT-0012` のログ: `reason=binding_limit`、`limit=200`、`integration_id`、`project_id`。数は

   ```sql
   SELECT count(*) FROM forge.integration_bindings WHERE integration_id = <integration_id>;
   ```

5. `FG-INT-0017` のログ: `reason`、`integration_id`、`service_account_id`、`project_id`。`gstn admin integration view <id>` の
   `enabled` と、`gstn admin service-account view <service_account_id>` の `state` を見る。止めたのが意図どおりなら何もしない。
   そうでなければ、止めた理由を確かめてから管理者が有効に戻す（C の逆。`{"enabled": true}`）。

6. 変更の履歴（監査）: 連携は `resource_type = 'integration'`、結びつけはプロジェクトの行。

   ```sql
   SELECT id, action, actor_user_id, occurred_at, details FROM forge.audit_log
   WHERE (resource_type = 'integration' AND resource_id = <integration_id>)
      OR (resource_type = 'project' AND resource_id = <project_id> AND action LIKE 'integration.%')
   ORDER BY id DESC LIMIT 20;
   ```

   結びつけがメンバーを付けた・外したときは、同じトランザクションに `member.added` / `member.removed` の行とイベントがある。

## Mitigate

### A. 連携を登録する `[MUTATING — needs approval]`

1. 連携用のサービスアカウントを作り、トークンを発行する（Runbook service-accounts の A・B。目的ごとに 1 つ）。
2. 管理者のブラウザのコンソールから:

   ```js
   const s = await (await fetch('/api/v1/session')).json();
   await (await fetch('/api/v1/admin/integrations', {method: 'POST',
     headers: {'Content-Type': 'application/json', 'X-GitStation-CSRF': s.csrf_token},
     body: JSON.stringify({kind: 'kanflow', service_account_id: <service_account_id>})})).json();
   ```

   `event_types` を省くと `["*"]`（連携向けのすべての種類）。絞るときは `["merge_request.*", "repository.pushed"]` のように
   連携向けの種類か `<家族>.*` を 1〜64 個。ほかの分類（`member.*`、`token.*` など）は `FG-WEB-0006 rule=unknown_event_type` で断られる。
   サービスアカウントでない id は `rule=not_service_account`、別の連携が使っているアカウントは `rule=service_account_in_use`。

### B. プロジェクトを結ぶ・解除する `[MUTATING — needs approval]`

```sh
gstn integration-binding set --project <project_id> --integration <integration_id> --external-project <kanflow_project_id>
gstn integration-binding remove --project <project_id> --integration <integration_id> --yes
```

- 同じ外部プロジェクトへもう一度結ぶのは `unchanged`（エラーではない）。
- **別の kanflow のプロジェクトへ付け替える** とき（`FG-INT-0016 bound_to_other_external_project`）: 先に今の結びつけを解除してから結ぶ。
  解除と結びつけの間、その連携のイベントの履歴にこのプロジェクトは出ない（I1c のカーソルは 410 `visible_set_changed` で取り直しになる）。
- **別の連携に結ばれている** とき（`bound_to_other_integration`）: どちらの連携が正しいかを決めてから、`bound_integration_id` の結びつけを解除する。
- **200 に達した** とき（`FG-INT-0012`）: `gstn admin integration bindings <id>` で使っていない結びつけを探して解除する。足りなければ、
  目的を分けて 2 つ目の連携（別のサービスアカウント）を作る。上限はコードの定数（forge-db `integrations::MAX_BINDINGS`）で、設定では変えない。

### C. 連携をすぐ止める

- 管理者のセッションで無効にする `[MUTATING — needs approval]`: コンソールから
  `PATCH /api/v1/admin/integrations/<id>` に `{"enabled": false}`（A と同じ形）。無効の間は新しい結びつけができない
  （`FG-INT-0017`。既にある結びつけとメンバーは残るので、外すなら B で解除する）。
- トークンしか手元に無いとき（漏えいへの対応）: 連携のサービスアカウントを無効化する。トークンはすべて同じ変更で失効する
  `[MUTATING — needs approval]`:

  ```sh
  gstn user disable <service_account_id>
  ```

## Verify

- `gstn admin integration view <id> --json` の `enabled`・`event_types`・`bindings` が期待どおり。
- `gstn integration-binding list --project <id> --json` の `items` が期待どおり（解除後は空）。
- 結んだプロジェクトのメンバーにサービスアカウントが Reporter でいる（`gstn member list --project <id>`）。解除後は（結びつけが付けた
  Reporter なら）いない。

## 後片付け

- 監査の行（`integration.created` / `updated` / `bound` / `unbound`）が残っていることを確かめる（Diagnose 6）。
- 連携をやめたときは、サービスアカウントのトークンも失効させる（Runbook service-accounts）。連携の行は消さない（無効にするだけ）。
