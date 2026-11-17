# Runbook: portal-widgets（FG-AUTH-0026 / FG-AUTH-0027 / FG-WEB-0010 / FG-WEB-0011 / FG-INT-0022 / FG-INT-0023 / FG-INT-0024）

PFS のポータル（iotpf-portal、レルム `iotpf`）のホームに出す GitStation のウィジェットと、ポータルへ送る新着（ADR-0036、
`docs/portal/widgets.md`）の手順。ポータルは **サーバー側から利用者本人のアクセストークン（`azp = portal`）** で
`/api/portal-widgets/v1/**` を呼ぶ。新着は GitStation の `forge worker` が `POST {portal.registry_url}/api/v1/news` で送る。

## Trigger

- ポータルのウィジェットの枠が「権限がありません」「取得できません」になる。ログに **FG-AUTH-0026** PortalTokenInvalid（warn、401）、
  **FG-AUTH-0027** PortalUserNotLinked（info、403）、**FG-AUTH-0003** で `credential=portal`（403）、**FG-WEB-0011**（422）。
- ログに **FG-INT-0022** PortalNewsAttemptFailed（info）: 新着の 1 回の送信が失敗し、次の試行を待っている。
- ウィジェットの枠が一時的に「取得できません」になり、ログに **FG-WEB-0010** RateLimited で `route=portal_widgets`（info、429）。
- ログに **FG-INT-0023** PortalNewsUndeliverable（**alert**）: 新着を諦めた（試行が尽きた / ポータルが内容を拒否した /
  ジョブが結果を書かずに終わり、1 時間ごとの掃除が `pending` の行を終わらせた = `detail=job_ended`）。
- ログに **FG-INT-0024** PortalServiceTokenUnavailable（warn）: ポータルへ送るトークンを Keycloak から取れない。
- `forge doctor` の `portal.news` が失敗（24 時間以内に諦めた新着がある、1 時間より古い送信待ちがある）。

## Impact

- GitStation の画面・git・CI は止まらない。止まるのはポータルのホームの GitStation の枠と、ポータルの新着の一覧だけ。
- 新着が届かなくても、出来事（配備の失敗、自動マージの停止、仕様の公開）は GitStation の画面に残る。
- 重大度: ポータルの新着を業務の連絡に使っていて届いていないなら SEV3、それ以外は SEV4。

## 設定（`[portal]`、環境変数は `FORGE_PORTAL_…`）

| 鍵 | 既定 | 内容 |
|---|---|---|
| `portal.enabled` | `false` | `true` でだけウィジェットの経路がポータルのトークンを受ける（`auth.oidc` も要る）。`false` なら 401 `FG-AUTH-0026 reason=portal_disabled` |
| `portal.news_enabled` | `false` | `true`（`portal.enabled` も）でだけ新着を outbox に書き、送る。**off のあいだは何も書かない**（on にしても過去は流れない） |
| `portal.registry_url` | — | 新着の宛先の origin（`https://host[:port]`、ループバックだけ `http`）。新着が on なら必須 |
| `portal.client_id` | `portal` | ポータルの Keycloak のクライアント。受け付ける `azp` |
| `portal.ca_file` | —（`https` では必須） | 宛先の証明書を検証する根の PEM。**これだけを信頼する** |
| `portal.timeout_ms` | 5000 | 1 回の送信（接続と応答）。500〜30000 |
| `portal.max_attempts` | 8 | 1 件あたりの試行（10 秒から倍々、最大 1 時間）。1〜20 |

トークンの取得には `auth.oidc.*`（`client_id`、`client_secret_file`、`ca_file`）をそのまま使う。Keycloak の `gitstation` クライアントで
**service account を有効** にする（PFS 側の MR）。ポータル側は `NEWS_SENDER_CLIENTS` に `gitstation` を入れる（PFS の CI/CD 変数）。
設定の誤りは起動時に `FG-SYS-0002`（`config_key`、`reason`。例 `portal.news_enabled reason=needs_portal_enabled`、
`needs_auth_oidc`、`portal.ca_file reason=missing`）。

## Diagnose（読み取りのみ）

1. ログを `error_code` と `request_id` で探す。トークン、クレーム、社員番号、本文は **ログに出ない**。
2. ウィジェットが失敗する:
   | コード | `reason` | 意味と確認 |
   |---|---|---|
   | FG-AUTH-0026 | `portal_disabled` | `portal.enabled` が off、または `auth.oidc` が無い（`forge serve` の起動ログ `forge.serve.portal_widgets_disabled`） |
   | FG-AUTH-0026 | `bearer_required` | Bearer が無い（Cookie / Basic）。ポータルの呼び方の問題 |
   | FG-AUTH-0026 | `expired` / `not_yet_valid` | 時計のずれ、またはポータルのトークンの更新の遅れ（`forge doctor` の `auth.oidc` の時計） |
   | FG-AUTH-0026 | `issuer` / `unknown_kid` / `signature` / `alg_not_allowed` | ポータルと GitStation のレルムが違う、鍵の更新 |
   | FG-AUTH-0026 | `missing_claim` / `typ` / `subject` | ID トークンや別の種類のトークンが来た |
   | FG-AUTH-0026 | `azp` | `azp` が `portal.client_id` でない（別のアプリのトークン。DB を読む前に落ちる） |
   | FG-AUTH-0003 | `portal_wrong_party` | 同じ判断の二重の守り（`authorize()`）。本番の認証器からは出ない。出たら認証器の不具合 |
   | FG-AUTH-0003 | `portal_no_access_role` | 利用者に GitStation の `access` ロールが無い（PFS の利用者管理） |
   | FG-AUTH-0004 | `owner_disabled` | GitStation の利用者が無効 |
   | FG-AUTH-0027 | `unlinked` | 社員番号の利用者がいない。本人が一度 GitStation にログインする |
   | FG-AUTH-0027 | `employee_number_ambiguous` / `employee_number_linked` / `privileged_account` / `auto_link_disabled` | runbook oidc-identity-link（リンクの規則はサインインと同じ） |
   | FG-AUTH-0027 | `auth_source_not_linkable` | アカウントの種類（local / ldap）が `auth.oidc.link_auth_sources` に無い。運用者がリンクする（`forge admin user link-identity`） |
   | FG-AUTH-0027 | `employee_number_format` / `employee_number_missing` | トークンの `staff_id` が無い、または社員番号の形でない（PFS の利用者の属性） |
   | FG-AUTH-0027 | `sign_in_required` | `auth.oidc.claims_source = id_token` の構成。社員番号では探さない。本人が一度ログインする |
   | FG-WEB-0010 | `route=portal_widgets`（`reason=in_flight` は同時 16 件、無ければ 1 分 6000 件） | 門（全員で 1 つ）。ポータルの呼ぶ頻度を `http.request` の件数（パス `/api/portal-widgets/v1/`）で確かめる。更新の間隔（60 / 120 秒）で説明できない量なら PFS に報告。正当に増えたなら `WIDGET_LIMIT` / `WIDGET_MAX_IN_FLIGHT` を変更として上げる |
   | FG-WEB-0011 | `param`、`rule` | 宣言にない値。`docs/portal/widgets.md` と PFS の台帳の宣言を比べる |
3. 新着（DB、読み取り）:
   ```sql
   SELECT state, count(*) FROM forge.portal_news GROUP BY state;
   SELECT id, source_id, topic, attempts, last_status, last_error_code, reason, updated_at
     FROM forge.portal_news WHERE state IN ('pending', 'failed') ORDER BY id DESC LIMIT 20;
   SELECT id, state, attempts, last_error_code FROM forge.jobs
    WHERE kind = 'portal.news_send' ORDER BY id DESC LIMIT 20;
   ```
   | `last_status` / `reason` | 意味 |
   |---|---|
   | 401 / 403 | ポータルが gitstation のトークンを受けない（`NEWS_SENDER_CLIENTS`、service account）。キャッシュのトークンは捨てて再試行する |
   | 400 / 404 / 409 / 413 / 422（`reason=refused`） | 内容を拒否した、宛先の URL が違う。待っても直らないのですぐ `failed` |
   | 5xx、`timeout`、`connect`、`tls` | ポータルが落ちている、証明書（`portal.ca_file`）、ネットワーク |
   | `service_token`（FG-INT-0024 が先に出る） | Keycloak がトークンを出さない（`client_rejected` = secret か service account） |
   | `dropped` / `no_audience` | 宛先の全員が無効・社員番号なし・プロジェクトを読めない。正常 |
   | `failed` / `tries_used_up` でログが `detail=job_ended` | ジョブが結果を書けずに終わった（最後の試行で DB が失敗、リースの期限切れ）。同じ行の request_id と、その前の `portal.news_send` のジョブの失敗（FG-DB-0006 / FG-DB-0007、`forge.jobs` の `last_error_code`）を見る |

   `pending` の行は、`queued` か `running` のジョブがある間だけ残る。ジョブの無い `pending` は `forge worker` の 1 時間ごとの掃除が
   `failed` にする（ログ `forge.portal.news_housekept` に `swept` と `purged`）。1 時間より古い `pending` が残るなら worker が動いていない。
4. `forge doctor` の `portal.news` と `auth.oidc`。

## Mitigate（承認された手順だけ）

1. 原因を直す（設定、CA、PFS 側の `NEWS_SENDER_CLIENTS`、service account）。設定の変更は `forge worker` の再起動で効く。
2. **新着の再送（ユーザーの承認が要る）**: 諦めた行を送信待ちに戻し、ジョブを入れ直す。`source_id` で冪等なので二重にはならない。
   ```sql
   -- 承認を得てから。対象の id を確かめて限定する。
   UPDATE forge.portal_news SET state = 'pending', reason = NULL, updated_at = now()
    WHERE state = 'failed' AND id IN (...);
   ```
   その後 `forge worker` が拾うよう、同じ id のジョブを `portal.news_send`（payload `{"news_id": <id>}`）で入れる手順は
   job-dead-lettered の「再投入」に従う。
   **行を `pending` に戻すのとジョブを入れるのは同じトランザクションで行う**（ジョブの無い `pending` は 1 時間ごとの掃除が
   また `failed` にして FG-INT-0023 を出す）。
3. ウィジェットを止める: `portal.enabled = false`（全ウィジェットが 401 になる。ポータルは枠を「取得できません」と出す）。

## Verify

- ウィジェット: ポータルのホームで枠が表示される。ログに FG-AUTH-0026 / 0027 が増えない。
- 新着: `SELECT state, count(*) FROM forge.portal_news GROUP BY state;` で `pending` が減り `sent` が増える。`forge doctor` の
  `portal.news` が ok。

## Feedback

- 原因がポータル側（契約の変更、トークンの形）なら PFS に報告し、`docs/portal/widgets.md` と契約のテストを直す。
- 新しい失敗の形は errors.toml の診断と、このランブックの表に足す。
