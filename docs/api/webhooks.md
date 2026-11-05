# Webhook の契約（`gstn webhook`、配信の本文 schema_version 1）

M3 I2（ADR-0016 §8 と末尾の Amendment I2、計画 02 §5.5・§11.1、計画 11 §5 I2）。プロジェクトのイベントを、運用者が許可した
社内の受信先へ HTTP の POST で押し出す。**Webhook は「読みに来て」の合図で、正はイベントの履歴 API**（`integration-feed.md`）。
少なくとも 1 回の配信なので、受信先は `Forge-Delivery`（配信 ID）と `event.id` で重複を除く。順序は保証しない。

## 約束

- 配信の本文は `schema_version` が同じ間、欄を **足すことはあっても、名前を変えたり消したり意味を変えたりしない**。受信先は知らない欄を無視する。
- 送り先は **運用者の許可リスト（`webhook.allowed_targets`）にある `host:port` / `cidr:port` だけ**。既定は空で、空の間はどの Webhook も作れない。
  ループバック・リンクローカル（`169.254.169.254` のメタデータを含む）・未指定・マルチキャスト・ブロードキャスト・予約・IPv4 互換・NAT64・6to4 は
  **許可リストに関係なく拒否**する（IPv4 射影の IPv6 は中の IPv4 で判定）。`https` だけ（`webhook.allow_http = true` の開発環境だけ `http`）。
  URL の userinfo（`user:pass@`）とフラグメントは拒否。違反は `400 FG-INT-0010`「この送り先は許可されていません」だけを返し、理由はサーバーのログだけ。
- 配信のたびに名前を引き直して検査し、**検査したアドレスに接続を固定する**（DNS rebinding の対策）。TLS は元の名前（SNI）で
  `webhook.ca_file` の CA だけで検証する。**リダイレクトは追わない**（3xx は失敗）。プロキシの環境変数は使わない。接続 3 秒・全体 10 秒。
  応答は最大 4 KiB だけ読み、本文は保存しない。2xx が成功。
- 配る種類は Webhook の `event_types`（1〜64 個）で選ぶ: `*`（連携の分類のすべて。既定）、`<家族>.*`（連携の分類の家族。例 `merge_request.*`）、
  種類の名前。maintainer の分類（`member.*`・`protected_ref.*`・`project.visibility_changed`・`merge_settings.*`・`webhook.disabled`）と
  deployment の分類は **名前を明示したときだけ**（家族の `*` では入らない）。admin の分類（`token.*`・インスタンス全体のイベント）は選べない。
  プロジェクトのないイベントは決して配らない。
- **作成者の権限で判定し直す**: 配る記録を作るときと、各試行の直前に、Webhook を作った人（`owner_id`）がそのプロジェクトの Maintainer 以上で、
  有効で、プロジェクトが削除待ちでないことを `authorize()` で確かめる。失えば配らない（配信は `skipped`、`last_reason=owner_lost`、
  ログに FG-INT-0010 `reason=owner_lost`）。
- 秘密は作成と入れ替えのときに **1 回だけ** 返す（`whsec_…`）。サーバーは AES-256-GCM で封じて保存し（鍵は DB の外の
  `webhook.secret_keys_file`）、読み取りの答えには決して出さない。

## 経路（Maintainer 以上。作成・変更・削除・入れ替え・再送は人だけ）

| 経路 | Action | トークン | 答え |
|---|---|---|---|
| `GET /api/v1/projects/{id}/webhooks` | `WebhookRead` | `read_api` | `200` `{items: [Webhook]}`（最大 20） |
| `POST /api/v1/projects/{id}/webhooks` `{url, event_types?}` | `WebhookManage` | `api` | `201` `{webhook, secret}`。21 個目は `409 FG-INT-0012`（`reason=webhook_limit`） |
| `GET …/webhooks/{webhook_id}` | `WebhookRead` | `read_api` | `200` Webhook |
| `PATCH …/webhooks/{webhook_id}` `{url?, event_types?, enabled?}` | `WebhookManage` | `api` | `200` `{outcome: updated|unchanged, fields, webhook}`。`enabled: true` は連続の失敗の数を 0 に戻す |
| `DELETE …/webhooks/{webhook_id}` | `WebhookManage` | `api` | `204`（配信の履歴も消える） |
| `POST …/webhooks/{webhook_id}/secret` | `WebhookManage` | `api` | `200` `{secret, secret_version, previous_secret_valid_until}` |
| `GET …/webhooks/{webhook_id}/deliveries?before=&limit=` | `WebhookDeliveryRead` | `read_api` | `200` `{items: [Delivery], next_before}`（新しい順、`limit` 1〜100、既定 20） |
| `POST …/deliveries/{delivery_id}/redeliver` | `WebhookRedeliver` | `api` | `202` `{outcome: queued}`。配信中（`pending`）と最後の試行から 60 秒は `429 FG-WEB-0010`（`Retry-After`）。無効の Webhook は `400 FG-WEB-0006 rule=webhook_disabled` |

- エージェントのトークン・サービスアカウント・ボットは作成・変更・削除・入れ替え・再送を拒否（`403 FG-AUTH-0003`）。読み取りは同じロールなら可。
- 見えないプロジェクトは `404 FG-REPO-0005`、削除待ちは `FG-REPO-0006`。**別のプロジェクトの Webhook・別の Webhook の配信**は、判定の後に
  `404 FG-REPO-0005`（`resource=webhook` / `webhook_delivery`）。
- `gstn webhook list|view|create|update|delete|rotate-secret|deliveries|redeliver`（`gstn help webhook`）。

## 配信の要求

```
POST <url>
Content-Type: application/json
User-Agent: gitstation-webhook/<version>
Forge-Event: merge_request.merged
Forge-Delivery: 0b9c1f0e-…（配信 ID。再試行・再送で変わらない）
Forge-Webhook-Id: 3
Forge-Signature: t=1700000000,v1=<hex>[,v1=<hex>]
Forge-Signature-Kid: 2[,1]
```

```json
{
  "schema_version": 1,
  "webhook_id": 3,
  "delivery_id": "0b9c1f0e-…",
  "event": {
    "id": "evt_0192f3c4a1b27c3d8e9f00112233aabb",
    "type": "merge_request.merged",
    "schema_version": 1,
    "occurred_at": "2026-11-03T01:15:00.123Z",
    "project_id": 12,
    "actor": { "kind": "user", "user": { "id": 5, "username": "alice", "display_name": "Alice Example" }, "via": null },
    "work_items": [{ "provider": "kanflow", "project_id": "kf-1", "ref": "PBI-123" }],
    "data": { "merge_request_id": 87, "iid": 4 }
  }
}
```

- `event` はイベントの履歴（`integration-feed.md`）の 1 件と同じ形（`cursor` はない）。**email と社員番号は入れない**。
  `work_items` は配信の時点で `work-item-sync` が記録したもので、まだなら空（履歴 API が正）。
- `Forge-Signature` の `v1` は `HMAC-SHA256(secret, t + "." + delivery_id + "." + 本文のバイト列)` の 16 進。`t` は試行ごとに作り直す。
  秘密の入れ替えから 24 時間は、新しい秘密と前の秘密の 2 つの `v1` を付ける。`Forge-Signature-Kid` は同じ順の秘密の版（`secret_version`）。

### 受信先の検証の手順

1. `Forge-Signature` を `,` で分け、`t` と `v1` の値を取る。
2. `abs(今 - t) > 300` 秒なら拒否する（再送攻撃の対策）。
3. 持っている秘密で `HMAC-SHA256(secret, t + "." + Forge-Delivery + "." + 生の本文)` を計算し、`v1` の **どれか** と定数時間で比べて一致すれば受理。
4. `Forge-Delivery`（または `event.id`）で重複を除き、すぐ 2xx を返す（重い処理は後で）。10 秒を超えると失敗に数える。

## 再試行と自動の無効化

- 1 回の配信は最大 10 回試す。失敗した試行の後は 10 秒 × 4^(n−1)（最大 4 時間）待つ（合計約 16 時間）。各試行の失敗はログの
  FG-INT-0020（info）、最後の試行の失敗は FG-INT-0004（warn）で、配信は `failed`。
- Webhook ごとに **50 回続けて失敗する** か **24 時間失敗し続ける** と、forge が自動で無効にする（`disabled_reason = failures` /
  `failing_24h`、FG-INT-0011 warn、監査 `webhook.disabled`、イベント `webhook.disabled`）。成功すると数は 0 に戻る。
  Maintainer が受信先を直して `enabled: true` にすると戻る。無効の間のイベントは記録しない（履歴 API で埋める）。

## 配信の履歴（Delivery）

| 欄 | 意味 |
|---|---|
| `id` | 履歴の id（`redeliver` と `before` に使う） |
| `delivery_id` | `Forge-Delivery` の値 |
| `event_seq` / `event_id` / `event_type` | 元のイベント |
| `state` | `pending`（試行を待つ・試行中）、`succeeded`、`failed`、`skipped`（送らずに終えた） |
| `attempts` / `redeliveries` | 試行の数 / 再送の回数 |
| `last_status` / `last_reason` / `last_error_code` / `last_duration_ms` | 最後の試行。`last_reason`: `status`、`redirect`、`connect`、`connect_timeout`、`timeout`、`tls`、`body`、`request`、`dns`、`target_forbidden`、`resolved_forbidden`、`webhook_disabled`、`owner_lost`、`not_admitted`、`event_gone` |

本文と応答の本文は保存しない。履歴は終わったものを `webhook.delivery_retention_days`（既定 30 日）で消す。
Runbook: `ops/runbooks/webhook-delivery.md`。
