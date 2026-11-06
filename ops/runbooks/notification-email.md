# Runbook: notification-email（FG-INT-0015）

メールの通知（M3 N1、ADR-0016 Amendment N1）が送れない・遅れている・「メールが来ない」と言われた、のときの手順。
通知は MR の承認・マージ・コメントを **MR の作者**に伝える。配信は `forge worker` の専用のランナーが行い、社内の SMTP の中継（`notify.smtp_host`）へ渡す。
メールの失敗が MR の操作を止めることはない。

## Trigger

- `forge doctor` の `notify.queue` が **FG-INT-0015**（warn）で失敗した。`reason`:
  - `backlog`: 10 分より古い通知が待っている（ワーカーが動いていない、または中継が落ちていて試行が尽きていない）
  - `failed_recently`: 24 時間以内に `failed` になった通知がある（中継が 5xx で拒んだ、または試行が尽きた）
  - `outcome_unknown`: 30 日以内に `unknown` になった通知がある（確認済みの印は無いので、30 日たつと数えない。最も古いものの経過秒が `oldest_unknown_secs`）（本文を渡した後に応答が無かった。**再送しない**）
- `forge doctor` の `notify.relay` が失敗した（このホストから中継の `host:port` へ TCP でつながらない。何も送らない確認）
- ワーカーのログに **FG-INT-0015**（warn）。`reason`: `connect_failed`、`connection_lost`、`tls_failed`、`auth_failed`、`timeout`、
  `rejected_temporary`（4xx）、`rejected_permanent`（5xx）、`tries_used_up`、`outcome_unknown`。`stage`: `connect` / `tls` / `auth` / `mail_from` /
  `rcpt_to` / `data` / `end_of_data`。`smtp_code` は中継が返した 3 桁の番号。**宛先・本文・中継の返事の文はログに出ない**
- 利用者から「承認・マージされたのにメールが来ない」
- `forge worker` が `FG-SYS-0002 config_key=notify.…` で起動しない（設定の誤り。下の「設定」）

## Impact

- メールが届かないだけ。MR・承認・マージ・Webhook は止まらない。`failed` / `unknown` の通知は**自動では再送しない**（二重に送らないため）。
- `retrying` / `pending` は中継が答えれば送られる（試行は `notify.max_attempts` 回、待ちは 10 秒から倍々で最大 1 時間）。試行が尽きたら `failed`。
- 重大度: 通知がないことで承認待ちが滞る・業務に影響が出ているなら SEV3、それ以外は SEV4。

## 設定（`notify.*`、ADR-0013 の鍵。環境変数は `FORGE_NOTIFY_…`）

| 鍵 | 既定 | 内容 |
|---|---|---|
| `notify.enabled` | `false` | `true` でだけ通知を記録しメールのランナーが動く。`false` の間も購読者は登録されたまま全イベントを読んで通す（記録しない・遅れない）ので、`false` の間のイベントは通知にならず、有効に戻しても遡らない |
| `notify.smtp_host` / `notify.smtp_port` | — / 587（starttls）・465（tls）・25（none） | 中継 |
| `notify.smtp_tls` | `starttls` | `none` / `starttls`（必須。出さない中継とは続けない）/ `tls`（最初から） |
| `notify.smtp_ca_file` | —（`starttls` / `tls` では必須） | 中継の証明書を検証する根の PEM。**これだけを信頼する**（OS の保管庫は使わない）。`none` とは組めない |
| `notify.smtp_username` + `notify.smtp_password_file` | なし | AUTH PLAIN。`tls = none` とは組めない。ファイルは 0600 |
| `notify.from_address` / `notify.from_name` | — / `GitStation` | 送信元（`local@domain` の形だけ） |
| `notify.default_language` | `ja` | 言語を選んでいない人向け（`ja` / `en` / `id`） |
| `notify.connect_timeout_ms` / `notify.send_timeout_ms` | 5000 / 30000 | 接続 / 会話全体 |
| `notify.max_attempts` | 5 | 1 通あたりの試行回数（1〜25） |

設定の誤りは起動時に `FG-SYS-0002`（`config_key` と `reason`）。中継の情報が分かったら鍵を足して `forge worker` を再起動する（CA とパスワードは起動時に読む）。
**試験の受け口**: 試験環境は `127.0.0.1:28025` の SMTP の受け口（例: mailpit）に向ける（`notify.smtp_tls = none`、認証なし）。本番の中継には試験で触れない。

## Diagnose（読み取りのみ）

1. `forge doctor --json` の `notify.queue` と `notify.relay`、`jobs.dead`、`subscribers.lag`（`notify` の遅れ）を読む。
2. 通知の状態（派生の表。本文は保存されない）:
   ```sql
   SELECT state, count(*), min(created_at) FROM forge.notifications GROUP BY state ORDER BY state;
   SELECT id, user_id, kind, state, reason, attempts, created_at, updated_at
     FROM forge.notifications WHERE state IN ('failed', 'unknown') ORDER BY updated_at DESC LIMIT 20;
   ```
   - `reason`（終わった理由）: 送らなかった `skipped`（`muted` / `opted_out`）、`suppressed`（`recipient_inactive` / `recipient_not_person` / `no_address` /
     `address_invalid` / `not_visible` / `merge_request_gone`）、`failed`（`rejected_permanent` / `tries_used_up`）、`unknown`（`outcome_unknown` / `interrupted`）。
3. 「メールが来ない」は、その人の `user_id` で:
   ```sql
   SELECT id, kind, state, reason, attempts, created_at FROM forge.notifications
    WHERE user_id = <id> ORDER BY id DESC LIMIT 10;
   SELECT email_enabled, language, muted_kinds FROM forge.notification_preferences WHERE user_id = <id>;
   ```
   - 行が無い: その時 `notify.enabled` が `false` だった、または作者本人の操作だった、またはイベントの種類が通知の対象ではない（承認・マージ・コメントのみ。スレッドの解決は対象外）。
   - `skipped`: 本人が止めている（`gstn notifications get`）。`suppressed` / `not_visible`: 送る時にそのプロジェクトを見られなかった（メンバーから外れた・削除待ち）。
   - `sent`: 中継は受け取った。迷惑メールの振り分け・中継側のログ（`Message-ID` = `<notification-{id}@送信元のドメイン>`）を調べる。
4. ジョブの側: `SELECT id, state, attempts, last_error_code FROM forge.jobs WHERE kind = 'notification.send' ORDER BY id DESC LIMIT 20;`
   （メールの失敗はジョブを `dead` にしない。`dead` があれば別の不具合 → Runbook `job-dead-lettered.md`）。
5. ワーカーのログを `error_code=FG-INT-0015` で探し、`reason` と `stage` から原因を絞る:
   | reason / stage | 見るもの |
   |---|---|
   | `connect_failed` / connect | 中継の名前・ポート、このホストからの経路（`notify.relay`）、ファイアウォール |
   | `tls_failed` / tls | 中継が STARTTLS を出すか、証明書が `notify.smtp_ca_file` の根で検証できるか、証明書の名前が `notify.smtp_host` と合うか |
   | `auth_failed` / auth | ユーザー名・パスワード（`smtp_code=535`）、中継が AUTH PLAIN を出すか（TLS の後に出す中継が多い） |
   | `rejected_temporary`（4xx） | グレーリスト・中継の満杯。待てば再試行で通る |
   | `rejected_permanent` / rcpt_to | 宛先の拒否（`smtp_code=550` ほか）。その人のメールアドレス（`forge.users.email`）が正しいか |
   | `rejected_permanent` / mail_from | 送信元 `notify.from_address` を中継が許さない |
   | `timeout` | 中継が遅い。`notify.send_timeout_ms` を伸ばす前に中継を調べる |
   | `outcome_unknown` | 中継のログで `Message-ID` を探す。届いていれば何もしない。無ければ下の「再送」 |

## Mitigate（変更は承認が要る）

- 設定の誤りは `notify.*` を直してワーカーを再起動する。中継が戻れば `pending` / `retrying` は自動で送られる。
- **試行が尽きた（`failed`）通知・`unknown` の通知の再送**: 自動では送らない。必要なら承認を得て、中継を直した後に新しい通知として出し直す手段は無いので、
  対象の人に直接伝える（件数が多いときは `forge.notifications` の行を CSV にして連絡に使う）。**行を `pending` に戻す手書きの UPDATE はしない**
  （`unknown` は中継が受け取った可能性があり、二重に送ることになる）。
- メールを止める必要があるとき（誤送信・中継の事故）: `notify.enabled=false` にしてワーカーを再起動する（購読者もランナーも止まる）。止めている間のイベントは通知にならない。
- 本人の設定を変えるのは本人だけ（管理者でも不可）。`gstn notifications set --email off` / `--mute <種類>` / `--language ja|en|id|default`。

## Verify

- `forge doctor` の `notify.queue` と `notify.relay` が ok。
- 試験の通知（自分の MR を別の人に承認してもらう）で `forge.notifications` の `state` が `sent` になり、受け口（試験環境は mailpit）に 1 通だけ届く。
- ワーカーのログに `forge.notify.sent`（`notification_id`・`kind`・`attempt`。宛先は出ない）。

## Prevent / 受け入れているリスク

- **at-most-once**: 本文を渡した後の応答の欠落と、送信中のワーカーの死は `unknown` にして再送しない。「届かないかもしれない」を選び「二重に届く」を避けた。
- 通知の行と本人の設定は、プロジェクトの purge でも利用者の無効化でも消えない（id・種類・状態・理由だけで、本文・宛先は無い）。保持期間はアプリ内の通知（U4）と一緒に決める。
- 1 通知 = 1 通。ダイジェストはない。送るのは 1 件ずつ（専用のランナー 1 つ）。
- TLS の握手の通し試験は自動試験にない（設定・CA の読み込み・格下げの拒否・段階の分類は試験済み）。本番の中継につなぐ前に、試験環境の
  mailpit（`tls = none`）と、実際の中継の `tls` / `starttls` の両方で 1 通ずつ確かめる。
