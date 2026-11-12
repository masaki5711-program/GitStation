# Runbook: integrations（FG-INT-0012 / FG-INT-0016 / FG-INT-0017 / FG-INT-0018 / FG-INT-0019）

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
- 作業項目（M3 I1b、ADR-0016 Amendment I1b）: 結ばれたプロジェクトでは、MR のタイトルとソースブランチの名前、push されたブランチの名前から
  kanflow の `PBI-<n>` の参照を見つけて（02 §2 の規則。`PBI-430` は `PBI-43` ではない。共有のテストのベクタ `docs/api/pbi-ref-vectors.json`）、
  購読者 `work-item-sync`（`ordered`）が MR とのリンク（`forge.merge_request_work_items`、`source = auto`）とイベントの作業項目
  （`forge.event_work_items`）を書く。作業項目は (連携, 外部プロジェクト, `PBI-<n>`) で 1 つ（`forge.work_items`）。
  - タイトルから参照が消えたリンクは終わる（`unlinked_at`、`unlink_source = auto`）。**行は消えない**（履歴）。ブランチの削除・マージ・クローズでもリンクは残る。
  - 人は MR の画面・API から手でリンク・解除できる（Developer 以上、`WorkItemLink`）。**人（`human`）が** 解除した作業項目は、タイトルにあっても
    自動ではもう付かない（人がもう一度リンクすれば付く）。エージェント・サービスアカウント・ボットの解除（`unlinked_by_kind`）は検出を止めない。
    人のリンクは自動では外れない。自動で付いた作業項目を人がリンクすると人のリンクになる（自動の行は `unlink_source = converted` で終わる）。
  - 結びつけを解除すると、そのプロジェクトの開いたリンクのうち、その結びつけの作業項目のものは `unlink_source = unbound` で終わる
    （監査 `integration.unbound` の `work_item_links_ended` が数。`work_item.unlinked` のイベントは出ない）。
  - データベースが裏付ける: 人のリンク・解除には同じトランザクションの `work_item.*` の監査の行、自動のリンクには `work-item-sync` の前進が要る
    （トリガー `merge_request_work_items_audited`。違反は `FG-DB-0013` の `reason=work_item_link_unaudited` / `derived_work_item_link`、
    制約名 `merge_request_work_items_audited` / `_derived`。監査の行はその作業項目を名指す必要がある）。
  - 結ばれていないプロジェクトでは何も見つけない（手でのリンクは `FG-INT-0018`）。
  - **結びつけたとき（初めて・解除の後の結び直し・別の kanflow のプロジェクトへの結び直し・共有の結びつけを作業項目を決める結びつけに変えたとき）**、
    そのプロジェクトの既存の MR の参照を見つけ直す（ADR-0016 Amendment 2026-11-12）。結びつけの変更と同じトランザクションで
    ジョブ `work_items.relink`（`forge.jobs`、キー `work_items_relink:<結びつけの id>`）が入り、`forge worker` が実行する:
    **開いた MR**（古い順に最大 500）と **90 日以内にマージした MR**（新しい順に最大 500。マージの時刻は適用したマージの終了、無ければ
    MR の `updated_at`）ごとに内部のイベント `merge_request.relink_requested`（admin の分類。連携の feed・Webhook には出ない）を 100 件ずつの
    変更で書き（監査 `work_item.relink_requested`、プロジェクトの行、`actor_kind = system`）、`work-item-sync` がふだんと同じ規則・上限で
    リンクを付ける。**クローズした MR・90 日より前にマージした MR は見つけ直さない**（次の編集で付く）。上限で切ったときはログ
    `forge.work_items.relink_capped`（warn）。**人が解除した作業項目は結び直しても付かない**（`unlink_source = manual` の行が最後）。
    ジョブはやり直しても、もう書いた MR を飛ばす（イベントのキー `merge_request_relink_requested:<mr>:<結びつけ>`）。
    実行のときに結びつけが無くなっていれば何も書かない（ログ `forge.work_items.relink_skipped reason=binding_gone`）。
  - **リンクが変わったことの合図**: 手でのリンク・解除と、上の見つけ直しで付いた・外れたリンクは、MR ごとに 1 つの
    `merge_request.work_items_changed`（連携の分類、`merge_request.*` に入る）になる。`data` は `reason`（`manual` / `relink`）、
    `added[]`・`removed[]`（`link_id`・`work_item_id`・`work_item`・`source` / `unlink_source`）、`by`・`by_kind`。イベントの作業項目
    （feed の `work_items`）には外した作業項目も入る。kanflow は `merge_request.*` で MR を読み直すので、ふだんの同期で手のリンクも分かる
    （`work_item.*` は kanflow が読み飛ばす）。タイトルの編集で付いた・外れたリンクは、今までどおり `merge_request.updated` などが合図
    （`work_items_changed` は出ない）。
  - 自動のリンクは MR あたり 20 まで（超えた分はログ `forge.work_items.detected_capped`）、手のリンクは 50 まで（`FG-INT-0019`）、
    1 つのイベントの作業項目は 100 まで（`forge.work_items.event_items_capped`）。

## Trigger

- kanflow と新しくつなぐ（連携の登録）。
- kanflow のプロジェクトとリポジトリを対応させる・やめる（結びつけ・解除）。
- 結びつけが `FG-INT-0016`（409、BindingConflict）で断られた: そのプロジェクトはもう別の連携、または同じ連携の別の外部プロジェクトに結ばれている。
- 結びつけが `FG-INT-0012`（409、IntegrationLimitReached）で断られた: その連携の結びつけが 200 に達した。
- 結びつけが `FG-INT-0017`（409、IntegrationDisabled）で断られた: 連携が無効（`reason=integration_disabled`）、またはその
  サービスアカウントが無効（`reason=service_account_disabled`）。
- 連携をすぐ止めたい（トークンの漏えい、kanflow 側の暴走）。
- 手でのリンクが `FG-INT-0018`（409、WorkItemProjectNotBound）で断られた: そのプロジェクトは連携に結ばれていない。
- 手でのリンクが `FG-INT-0019`（409、WorkItemLinkLimitReached）で断られた: その MR の手のリンクが 50 に達した。
- 「MR が kanflow の PBI に出ない」「リンクが付かない・外れない」という問い合わせ。
- 「結び直したのに、開いている MR が kanflow の PBI に出ない」（`dev-status` の `work_items: []`）という問い合わせ。

## Impact

- `FG-INT-0016` / `FG-INT-0012`: その結びつけが作られないだけ。ほかの結びつけ・連携は動く。何も書かれていない。
- 連携の無効化: その連携のイベントの履歴（I1c）が止まる。結びつけとメンバーはそのまま。
- `FG-INT-0018` / `FG-INT-0019`: そのリンクが作られないだけ。何も書かれていない。
- `work-item-sync` が止まると（Runbook subscriber-lag、`FG-DB-0008 reason=stuck`）、リンクとイベントの作業項目の更新が遅れる（失われはしない）。

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

7. MR の作業項目のリンク（そのプロジェクトの Reporter 以上）。`source`（`auto` / `manual`）、`unlinked_at`（`null` = 付いている）、`unlink_source`。

   ```sh
   gstn mr work-items --project <project_id> --iid <iid> --json
   ```

   `linked_by_kind` / `unlinked_by_kind` は `human` / `agent` / `service_account` / `bot`。`unlink_source` は `auto`（タイトルから消えた）、
   `manual`（手）、`converted`（人のリンクに切り替わった）、`unbound`（結びつけの解除）。

   リンクが付かないとき: プロジェクトが結ばれているか（2）、参照が規則どおりか（`PBI-43`、`pbi-43`、`PBI #43`、`PBI_43`。`PBI--43` や前に英数字が付くと拾わない。
   ベクタ `docs/api/pbi-ref-vectors.json`）、人が一度解除していないか（`unlink_source = manual` の行があれば自動では付かない）、
   `work-item-sync` が止まっていないか（`forge doctor` の `subscribers.lag`）。

   ```sql
   SELECT l.id, w.ref, l.source, l.linked_by, l.linked_seq, l.linked_at, l.unlinked_at, l.unlink_source, l.unlinked_by
     FROM forge.merge_request_work_items AS l JOIN forge.work_items AS w ON w.id = l.work_item_id
    WHERE l.merge_request_id = (SELECT id FROM forge.merge_requests WHERE project_id = <project_id> AND iid = <iid>)
    ORDER BY l.id;
   SELECT e.seq, e.event_type, w.ref FROM forge.event_work_items AS x
     JOIN forge.events AS e ON e.seq = x.seq JOIN forge.work_items AS w ON w.id = x.work_item_id
    WHERE e.project_id = <project_id> ORDER BY e.seq DESC LIMIT 20;
   ```

8. 結び直しの後にリンクが戻らないとき: 見つけ直しのジョブと、そのイベント・監査を見る（読み取りのみ）。

   ```sql
   SELECT id, state, attempts, last_error_code, payload, created_at FROM forge.jobs
    WHERE kind = 'work_items.relink' AND payload->>'project_id' = '<project_id>' ORDER BY id DESC LIMIT 5;
   SELECT count(*) FROM forge.events
    WHERE event_type = 'merge_request.relink_requested' AND project_id = <project_id>;
   SELECT seq, payload->>'iid' AS iid, payload->>'reason' AS reason, payload->'added' AS added
     FROM forge.events WHERE event_type = 'merge_request.work_items_changed' AND project_id = <project_id>
    ORDER BY seq DESC LIMIT 20;
   ```

   ジョブが `dead` なら `last_error_code` の Runbook（多くは `FG-DB-0002` / `FG-DB-0006`）。`queued` のままなら `forge worker` が動いているか
   （`forge doctor` の `jobs.*`）。イベントがあって `work_items_changed` が無いなら、`work-item-sync` が止まっていないか（`subscribers.lag`）、
   その MR を人が解除していないか（7 の SQL）。対象は開いた MR と 90 日以内のマージだけで、クローズした MR は付かない（仕様）。
   ジョブを入れ直す手段（CLI）はまだ無い。急ぐときは、その MR のタイトルを編集すれば次のイベントでリンクされる。

9. `FG-INT-0018` のログ: `reason=project_not_bound`、`project_id`。`FG-INT-0019` のログ: `reason=manual_link_limit`、`limit=50`、`project_id`、
   `merge_request_id`。人のリンクと解除の監査は `work_item.linked` / `work_item.unlinked`（プロジェクトの行、`actor_kind = user`）。
   自動のリンクは派生イベント（`actor_kind = system`、`caused_by_seq` が元の MR のイベント）で、監査の行は無い。

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

### D. 作業項目を手でリンクする・解除する `[MUTATING — needs approval]`

```sh
gstn mr link-work-item --project <project_id> --iid <iid> --ref PBI-43
gstn mr unlink-work-item --project <project_id> --iid <iid> --ref PBI-43
```

- `FG-INT-0018`: 先に B でプロジェクトを結ぶ。
- `FG-INT-0019`: 要らない手のリンクを解除する。上限はコードの定数（forge-db `work_items::MAX_MANUAL_LINKS`）。
- 自動のリンクを人が解除すると、そのタイトルのままでも自動では付き直さない（意図した解除を上書きしないため）。戻すときは手でリンクする。
- 前の結びつけの作業項目へのリンクが残っているとき（通常は解除で終わる）は、作業項目の id で解除する:
  `gstn mr unlink-work-item --project <project_id> --iid <iid> --ref <work_item_id>`（id は `gstn mr work-items` の `work_item_id`）。

## Verify

- `gstn admin integration view <id> --json` の `enabled`・`event_types`・`bindings` が期待どおり。
- `gstn integration-binding list --project <id> --json` の `items` が期待どおり（解除後は空）。
- 結んだプロジェクトのメンバーにサービスアカウントが Reporter でいる（`gstn member list --project <id>`）。解除後は（結びつけが付けた
  Reporter なら）いない。
- 作業項目: `gstn mr work-items --project <id> --iid <iid>` に期待どおりのリンクがある（解除した行は `unlinked_at` 付きで残る）。

## 後片付け

- 監査の行（`integration.created` / `updated` / `bound` / `unbound`）が残っていることを確かめる（Diagnose 6）。
- 連携をやめたときは、サービスアカウントのトークンも失効させる（Runbook service-accounts）。連携の行は消さない（無効にするだけ）。

## 代理実行を止める（M3 S2、ADR-0025）

kanflow が本人として承認する経路（代理実行）は、連携を無効化すると止まる（代理の要求は `FG-AUTH-0003 reason=integration_disabled`）。今すぐすべてを止めるにはサービスアカウントを無効化する。手順・診断・鍵の入れ替えは runbook act-as。
