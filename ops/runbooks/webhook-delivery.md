# Runbook: webhook-delivery（FG-INT-0004 / FG-INT-0010 / FG-INT-0011 / FG-INT-0020）

プロジェクトの Webhook（M3 I2、ADR-0016 §8 と Amendment I2、`docs/api/webhooks.md`）が届かない・送り先が拒否された・
自動で無効になった、のときの手順。Webhook は「読みに来て」の合図で、**正はイベントの履歴 API**（`integration-feed-lag.md`）。
受信先は配信 ID（`Forge-Delivery`）とイベント ID（`event.id`）で重複を除く（少なくとも 1 回の配信）。

## Trigger

- ログに **FG-INT-0020** WebhookAttemptFailed（info）: 1 回の試行が失敗し、次の試行を待っている（`attempt`、`status`、`reason`、`retry_in_s`）。
- ログに **FG-INT-0004** WebhookDeadLettered（warn）: 1 つの配信が最後の試行（10 回、約 16 時間）まで失敗した。配信は `failed` で、もう自動では送らない。
- ログに **FG-INT-0011** WebhookDisabled（warn）: Webhook が 50 回続けて失敗した（`reason=failures`）か、24 時間失敗し続けた（`reason=failing_24h`）ので、
  forge が自動で無効にした。監査に `webhook.disabled`（`actor_kind = system`）、イベント `webhook.disabled`（maintainer の分類）が残る。
- 作成・変更で **400 / FG-INT-0010** WebhookTargetForbidden（「この送り先は許可されていません」）。配信の時点でもログに FG-INT-0010（warn）。
- `forge doctor` の `webhook.allowed_targets`（空なら info: どの Webhook も配れない）、`webhook.allow_http`（`true` なら警告）、
  `webhook.secret_keys`（鍵がない・読めない）、`webhook.deliveries`（直近 24 時間の `failed` と無効化）。

## Impact

- forge の画面・git・イベントの履歴は止まらない。止まるのはその Webhook の受信先への通知だけ。
- kanflow など履歴 API を読む連携は、Webhook が止まっても取りこぼさない（定期取得で埋まる）。即時の反映だけが遅れる。
- 重大度: 業務が Webhook の即時性に依存して止まっているなら SEV3、それ以外は SEV4。

## Diagnose（読み取りのみ）

1. ログを `error_code` と `request_id`（イベントを書いた要求の ID。配信のジョブはそれを引き継ぐ）で探す。どの行も **URL のパスとクエリを出さない**
   （`origin` = scheme://host:port だけ）。受信先の応答の本文は保存しない。ログに出るのは最大 128 文字の `response_head`（表示できない文字は `?`、
   20 文字以上のトークンらしい並びは `[redacted]`）だけ。
2. 配信の履歴（Maintainer 以上）:
   ```
   gstn webhook list --project <id>
   gstn webhook view <webhook_id> --project <id>
   gstn webhook deliveries <webhook_id> --project <id> [--limit 50]
   ```
   `state`（`pending` / `succeeded` / `failed` / `skipped`）、`attempts`、`last_status`、`last_reason`、`last_error_code`。
   `last_reason` の意味:
   | reason | 意味 |
   |---|---|
   | `status` | 受信先が 2xx 以外を返した（`last_status`） |
   | `redirect` | 3xx を返した。**リダイレクトは追わない**（URL を直す） |
   | `connect` / `connect_timeout` | 接続できない・3 秒で接続できない |
   | `timeout` | 全体 10 秒を超えた |
   | `tls` | 証明書を検証できない（`webhook.ca_file` の CA、名前の不一致） |
   | `dns` | 名前を解決できない |
   | `target_forbidden` / `resolved_forbidden` | 送り先の方針で拒否（下の FG-INT-0010） |
   | `webhook_disabled` / `owner_lost` / `not_admitted` / `event_gone` | 送らずに終えた（`skipped`）。無効、作成者の権限の喪失、許可リストから外れた、イベントが保持で消えた |
3. DB（読み取り）:
   ```sql
   SELECT id, project_id, enabled, disabled_reason, consecutive_failures, failing_since, last_delivery_at
     FROM forge.webhooks WHERE id = <webhook_id>;
   SELECT id, delivery_uid, event_type, state, attempts, last_status, last_reason, last_error_code,
          last_attempt_at, finished_at FROM forge.webhook_deliveries
    WHERE webhook_id = <webhook_id> ORDER BY id DESC LIMIT 20;
   SELECT state, attempts, run_after, last_error_code FROM forge.jobs
    WHERE kind = 'webhook.deliver' AND state IN ('queued', 'running', 'dead') ORDER BY run_after LIMIT 20;
   SELECT occurred_at, action, details FROM forge.audit_log
    WHERE action LIKE 'webhook.%' AND resource_id = <project_id> ORDER BY id DESC LIMIT 20;
   ```
4. **FG-INT-0010** の `reason`（ログだけ）:
   - `not_allowlisted` / `resolved_not_allowlisted`: `webhook.allowed_targets` に `host:port`（または `cidr:port`）がない。**既定は空**。
   - `loopback` / `link_local`（`169.254.169.254` のメタデータを含む）/ `unspecified` / `multicast` / `broadcast` / `reserved` /
     `ipv4_compatible` / `nat64` / `six_to_four`: 許可リストに関係なく**常に拒否**する範囲。IPv4 射影の IPv6 は中の IPv4 で判定する。
   - `resolved_denied`: 名前の解決結果のどれかが常に拒否の範囲（DNS rebinding の疑い）。`address` が解決したアドレス。
   - `userinfo` / `fragment` / `scheme` / `invalid_url`: URL の形。`http` は `webhook.allow_http = true`（開発だけ）のときだけ。
   - `owner_lost`: Webhook を作った人（`owner_id`）がもうそのプロジェクトの Maintainer でない・無効・プロジェクトが削除待ち。
     `decision` は `authorize()` の答えのコード。
   サーバーで `nslookup <host>` を引き、解決したアドレスが許可リストの範囲にあるか見る。
5. **FG-INT-0011**: `failures` と `failing_since`。配信の履歴の直近の `last_reason` で、受信先の故障か、URL・証明書・許可リストの問題かを分ける。

## Mitigate（変更は承認が要る）

- **受信先を直す**（受信先の担当）: URL に 10 秒以内に 2xx を返す。3xx を返さない（リダイレクトは追わない）。`Forge-Signature` を検証し、
  `t` が 5 分より古いものは拒否する（`docs/api/webhooks.md` の検証の手順）。
- **許可リストに足す**（運用者、承認が要る）: 社内の正当な受信先だけを `webhook.allowed_targets` に `host:port` で足して `forge serve` と
  `forge worker` を再起動する。ループバック・リンクローカル・メタデータは**足しても拒否される**（足さない）。
- **再送**（Maintainer）: 受信先を直した後、必要な配信だけ:
  ```
  gstn webhook redeliver <webhook_id> <delivery_id> --project <id>
  ```
  同じ `Forge-Delivery` で新しい回を始める（試行は 1 からやり直し）。`pending` の間と、最後の試行から 60 秒以内は `429 FG-WEB-0010`。
  無効の Webhook には再送できない（`FG-WEB-0006 rule=webhook_disabled`）。多くを取りこぼした受信先は、再送より履歴 API で取り直す。
- **自動で無効になった Webhook を戻す**（Maintainer、受信先を直してから）:
  ```
  gstn webhook update <webhook_id> --project <id> --enable
  ```
  有効にすると連続の失敗の数と `failing_since` を 0 に戻す。無効の間のイベントは記録されていない（再送できない）。履歴 API で埋める。
- **作成者の権限の喪失（`owner_lost`）**: 今の Maintainer が自分の名前で作り直す（新しい秘密を受信先に渡す）か、作成者のロールを戻す。
- **秘密の漏えい・入れ替え**（Maintainer）:
  ```
  gstn webhook rotate-secret <webhook_id> --project <id>
  ```
  新しい秘密は 1 回だけ表示する。前の秘密は 24 時間だけ署名に使う（`Forge-Signature` に `v1` が 2 つ、`Forge-Signature-Kid` が `新,旧`）。
  受信先は 24 時間のうちに新しい秘密に切り替える。漏えいなら、入れ替えた後に受信先で古い秘密を即座に捨てる。
- **`webhook.secret_keys_file` の鍵の入れ替え**（運用者、承認が要る）: 新しい鍵を大きい `key_id` で足す（古い鍵は残す。消すと
  `FG-SYS-0002 reason=key_missing` で配れない）→ 再起動。新しい鍵で封じるのは、作成・入れ替えのときだけ。

## Verify

- `gstn webhook deliveries <webhook_id> --project <id>` の新しい配信が `succeeded`。
- `gstn webhook view` の `consecutive_failures` が 0、`enabled` が true。
- `forge doctor` の `webhook.deliveries` が ok（直近 24 時間に `failed` も無効化もない）。

## Prevent / 受け入れているリスク

- 試行は 10 回まで（10 秒 × 4^(n−1)、最大 4 時間、合計約 16 時間）。Webhook ごとに 50 回続けて失敗するか 24 時間失敗し続けたら無効にする。
- 配信は `forge worker` の **Webhook 専用の実行の列**（`webhook.deliver` だけ）で 1 件ずつ行う。遅い受信先（最大 13 秒）はほかの Webhook の
  配信を遅らせるが、マージやプロビジョニングのジョブは遅らせない。Webhook ごとの同時実行は worker の数まで（M3 は 1 つ）。
- 接続は検査したアドレスに固定する（名前を 2 度引かない）。プロキシの環境変数は使わない。TLS は `webhook.ca_file` の CA だけを信頼する（公開の CA は使わない）。
- 配信の本文はイベントの履歴と同じ形で、email と社員番号を入れない。配信済みの本文は取り消せない（利用者の匿名化の Runbook に書いた制約と同じ）。
- 作業項目（`event.work_items`）は配信の時点で `work-item-sync` が記録したものだけ（まだなら空）。正は履歴 API。
- 配信の履歴は `webhook.delivery_retention_days`（既定 30 日）で消す（終わったものだけ）。
