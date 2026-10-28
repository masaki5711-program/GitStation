# Runbook: subscriber-lag（FG-DB-0008）

## Trigger

- `forge doctor` の `subscribers.lag` または `subscribers.dead_letters` が `FG-DB-0008` SubscriberLag で失敗した
- worker のログに `FG-DB-0008`（`task=subscribers`）が出た。`reason` で分ける:
  - `stuck`: `ordered` の購読者が同じイベントで 5 回続けて失敗し、そこで止まっている（同じイベントを再試行し続け、自分では飛ばさない）
  - `fanout_limit`: そのイベントの処理が、根のイベントあたり 100 件を超える派生イベント、または深さ 8 を超える派生を作ろうとした（購読者のループ）。
    `ordered` は止まり、`independent` はそのイベントを隔離した
  - `quarantined`: `independent` の購読者が 5 回失敗し、イベントを `forge.subscriber_dead_letters` に隔離して先に進んだ（その購読者にとってそのイベントは飛ばされた）
  - `behind`: 購読者を待つイベントがあり、次のイベントの番号付けから 60 秒を超えた（worker が動いていない、DB に届かない、購読者が遅い）
  - `retention_overrun`: 保持期間の上限を過ぎて未処理のイベントが消された（手順 C7 で入る。今はまだ出ない）
- 利用者から「更新の処理が遅れています」の問い合わせ（問い合わせ番号 = `request_id`）

## Impact

- その購読者が担うもの（通知、Webhook の投入、kanflow の作業項目の同期など）が遅れる。画面・git の操作・イベントの履歴 API は動き続ける。
- `stuck` / `behind` ではイベントは失われない（再開すると順に処理される）。`quarantined` / `independent` の `fanout_limit` では、
  その購読者にとってそのイベントは処理されない（隔離の行は 30 日残る）。
- `ordered` の購読者の停止が 30 分を超える、または kanflow の同期が止まって業務に影響が出ているなら SEV2、それ以外は SEV3。

## Diagnose（読み取りのみ）

1. `forge doctor --json` の `subscribers.lag` と `subscribers.dead_letters` を読む。FG-DB-0008 の `context`:
   - `subscriber`: 購読者の名前、`seq`: 止まっている（隔離した）元のイベントの番号、`event_type`
   - `lag_events`: `high_seq - last_seq`（その購読者を待つイベントの数）
   - `failures`、`last_error_code`: 失敗の回数と、ハンドラーが最後に返したエラーコード（その FG のコードの Runbook も読む）
   - `oldest_pending_age_ms`: 次のイベントが番号付けされてからの時間（`behind` の判定）
   - `limit`（`fanout_limit` のとき）: `per_root`（根あたり 100 件）か `depth`（深さ 8）
   - `count_24h`、`newest_seq`（`subscribers.dead_letters`）: 直近 24 時間の隔離の数と最新の隔離
2. ログの行の `request_id` は **元のイベントを書いた要求の `request_id`**。それで HTTP 要求 → イベント → 購読者を辿る。
3. 購読者と隔離の状態を読む（読み取りのプール、`forge_ro`）:
   ```sql
   SELECT name, ordering, enabled, last_seq, failures, failing_since, last_error_code, updated_at
     FROM forge.event_subscribers ORDER BY name;
   SELECT subscriber, seq, error_code, attempts, quarantined_at
     FROM forge.subscriber_dead_letters ORDER BY quarantined_at DESC LIMIT 20;
   SELECT high_seq FROM forge.event_feed_state;
   ```
4. 止まっているイベントを読む（`payload` は表示名を含むので、必要な項目だけを見る）:
   `SELECT seq, event_type, aggregate_type, aggregate_id, project_id, request_id, root_seq, causation_depth FROM forge.events WHERE seq = <seq>;`
5. worker のログでハンドラー自身の失敗を見る（`task=subscribers`、`subscriber=…`、`source_seq=…`、ハンドラーの `error_code`）。
   失敗の行は購読者ごとの再試行の間隔（200 ms から倍々、最大 60 秒）ごとに 1 回までしか出ない。
   ハンドラーの外の失敗もイベントに数える: 復号できないイベント（`FG-SYS-0001 reason=decode`、`column`）、セーブポイントの RELEASE の失敗
   （`query=subscribers.release`。ハンドラーが SQL のエラーを握りつぶした）、COMMIT で派生イベントの検査に拒まれた（`FG-DB-0013 reason=derived_*`）。
   `source_event_id` も付く。イベントに数えない失敗（`subscriber_not_registered`、`ordering_mismatch`、`event_missing`、`cursor_moved`、DB の障害）は
   `source_seq` なしで出て、`failures` は増えない。
6. `behind` のとき: `forge worker` が動いているか（`systemctl status forge-worker`）、`outbox.sequencer_lag` も失敗していないか（番号付けが止まっていれば購読者も止まる → Runbook `outbox-sequencer-lag.md`）。
   worker のログに `FG-DB-0001`（DB に届かない）や LISTEN の失敗（`query=subscribers.listen*`）があれば、購読者は 5 秒ごとの確認と再接続で続く。

### 受け入れているリスク（ADR-0016 C5 の Amendment）

- 派生イベントの検査（系譜・深さ・根あたりの件数・同じトランザクションでの購読者の前進）は事故の水準の制御で、アプリの資格情報を持つ者は購読者の行も書ける。
- 根あたり 100 件の数え方は 1 つの文の中だけ。同じ根から 2 つのトランザクションが同時に派生すると、合計で 100 件をわずかに超えうる。
- ハンドラーが 20 秒を超えると打ち切って失敗に数える（`reason=handler_timeout`）。外部への呼び出しはハンドラーではなくジョブにする。

## Mitigate

1. **原因を直して再試行させる（推奨）**: `last_error_code` の原因（ハンドラーの不具合、依存するデータ、外部の障害）を直す。`stuck` の購読者は同じイベントを再試行し続けているので、
   ハンドラーが成功すれば自分で再開し、`failures` が 0 に戻る。不具合の修正はリリースの手順で。`[MUTATING — needs approval]`（ロールバック: 前の版に戻す）
2. `behind`: worker を起動・再起動する（`systemctl restart forge-worker`）。`[MUTATING — needs approval]`（ロールバック: なし。処理は 1 件ごとのトランザクションで、途中で止めても失われない）
3. `fanout_limit`: 派生イベントを作りすぎる購読者（ループ）を直す。`independent` はそのイベントを隔離済み。`ordered` は直すまで止まったまま。
4. **`ordered` の購読者がどうしても処理できないイベントを飛ばす**（最後の手段。承認済みの手順だけ）:
   `forge admin subscriber skip <name> --seq <seq> --reason <理由>` `[MUTATING — needs approval]`（ロールバック: なし。飛ばしたイベントは
   その購読者にとって処理されない。後から必要なら、そのイベントの効果を手作業か専用の修復の手順で補う）
   - **このコマンドは C5 の時点では未実装**（設計だけ）。実装するまでは、開発者（Claude のセッション）に修正のリリースを依頼する。手で `last_seq` を更新しない。
   - 設計: `forge_maint`（`database.maint_url`）で接続し、`forge_owner` が所有する `SECURITY DEFINER` の関数 `forge.subscriber_skip(name, seq, reason)`
     （`forge_maint` だけが EXECUTE）を呼ぶ。関数は購読者の行を `FOR UPDATE` で取り、`ordering = 'ordered'` かつ `last_seq + 1 = seq` のときだけ、
     `subscriber_dead_letters` に `(name, seq, last_error_code, failures)` を記録し、`last_seq = seq`・`failures = 0` にし、同じトランザクションで
     システムの監査の行（`action = 'subscriber.skipped'`、`resource_type = 'subscriber'`、`details` に名前・`seq`・理由・`session_user`）を書く。
     `forge_app` には実行させない（アプリの資格情報で飛ばせないように）。doctor の権限の検査と `guarded_objects` に関数を加える。
5. **してはいけないこと**: `last_seq` や `failures` を SQL で直接書き換えない、派生イベントの検査のトリガーを外さない、`enabled` を `false` にして止めたことを隠さない
   （`enabled` は `forge_app` から変えられない。運用者が DB の管理者として変えたら、Runbook の記録に残す）。

## Verify

- `forge doctor` の `subscribers.lag` が `ok`（`… with pending events (max N)` の N が 0 か減り続けている）、`subscribers.dead_letters` が `ok`。
- worker のログで、止まっていた購読者の `FG-DB-0008` が出なくなり、`event_subscribers.failures = 0` になっている。

## Escalate

- 30 分で直らないとき、`fanout_limit` が繰り返すとき、隔離が増え続けるときは、開発者（Claude のセッション）に `forge doctor --json` の出力、
  worker のログ（`request_id`、`subscriber`、`source_seq`）、手順 3 の結果を渡す。
