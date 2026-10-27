# Runbook: outbox-sequencer-lag（FG-DB-0005）

## Trigger

- `forge doctor` の `outbox.sequencer_lag` が `FG-DB-0005` OutboxSequencerLag で失敗した
- 番号の付いていない行（`forge.events` の `seq IS NULL`。監査の番号付けが有効な間は `forge.audit_log` も）があり、
  `forge.event_feed_state.sequencer_heartbeat` が 30 秒より古い、またはまだない（シーケンサーが一度も動いていない）
- 利用者から「更新の通知が遅れています」の問い合わせ（問い合わせ番号 = `request_id`）

## Impact

- 番号付けが止まっている間、購読者（通知、作業項目の同期）、連携の履歴 API、Webhook に新しいイベントが届かない。
  画面と git の操作は動き続ける。**イベントは失われない**（行は残っていて、再開すると番号が振られる）。
- 数分なら SEV3、30 分を超える、または kanflow の連携が止まって業務に影響が出ているなら SEV2。

## Diagnose（読み取りのみ）

1. `forge doctor --json` の `outbox.unsequenced_count` と `outbox.sequencer_lag` を読む。FG-DB-0005 の `context`:
   - `unsequenced_count`: 数えた行の合計。`unsequenced_events` はイベント、`unsequenced_audit` は監査の行（`off` なら監査の番号付けを意図して止めている間で、数えていない）
   - `heartbeat_age_ms`: 最後にシーケンサーが動いてからの時間。`none` はこの DB で一度も動いていない（導入の直後に `forge worker` を起動していない）
   - `lock_holder_pid`: シーケンサーの advisory lock を持っているセッション。`none` なら誰も持っていない
2. `forge worker` のプロセスが動いているかを見る（本番: `systemctl status forge-worker`、unit がなければ `ps -ef | grep 'forge worker'`、開発機: 起動したターミナル）。
3. worker のログを読む。失敗は連続の最初の 1 回だけ記録される（`context` に `task=sequencer` と `retry_in_ms`）。その `error_code` で分ける:
   - `FG-DB-0001`: DB に接続できない → Runbook `db-unavailable.md`
   - `FG-DB-0002` `query=sequencer.assign_seq` `reason=lock_timeout`: 別のセッションが advisory lock を持ったまま。`lock_holder_pid` を
     `SELECT pid, state, now() - xact_start AS in_tx, application_name FROM pg_stat_activity WHERE pid = <lock_holder_pid>;` で確かめる（`query` 列は表示しない）
   - `FG-DB-0002` `reason=permission_denied`: `forge_app` が `forge.assign_seq` を実行できない → `forge doctor` の `db.bootstrap`（`guard=function_privilege`）
   - `FG-DB-0009`: 保護のトリガーか関数の件数の確認が番号付けを拒否した。不具合か手作業の変更 → Runbook `append-only-violation.md`。**手で番号を振らない**
   - 復旧すると `forge.worker.task_recovered`（`failures` 付き）が出る
4. 番号付けの状態を読む（読み取りのみ）:
   `SELECT high_seq, audit_high_seq, audit_sequencing_enabled, now() - sequencer_heartbeat AS age FROM forge.event_feed_state;`

## Mitigate

1. `forge worker` が止まっている場合: 起動する（`systemctl start forge-worker`）。`[MUTATING — needs approval]`（ロールバック: `systemctl stop forge-worker`。番号付けは冪等で、止めても行は残る）
2. worker が失敗を繰り返している場合: ログの `error_code` の Runbook に従って原因を直してから、worker を再起動する。`[MUTATING — needs approval]`（ロールバック: なし）
3. advisory lock を長く持っているセッションがある場合: そのセッションが何かを確かめ、レビューの後に止める（`SELECT pg_cancel_backend(<pid>);`、だめなら `pg_terminate_backend`）。`[MUTATING — needs approval]`（ロールバック: なし。止めたトランザクションはロールバックされる）
4. **してはいけないこと**: `seq` や `high_seq` を手で更新しない、保護のトリガーを外さない。番号を振れるのは `forge.assign_seq` だけ。

## Verify

- `forge doctor` の `outbox.sequencer_lag` が `ok`（`heartbeat … ms ago`）になり、`outbox.unsequenced_count` のイベントの数が 0 か減り続けている。
- worker のログに `forge.worker.task_recovered` が出ている。

## Escalate

- `FG-DB-0009` が原因のとき、または 30 分で直らないときは DB の担当者と開発者（Claude のセッション）に、`forge doctor --json` の出力と worker のログ（`request_id`）を渡す。
