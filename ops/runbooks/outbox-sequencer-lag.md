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
3. worker のログを読む。失敗は連続の始まりに 1 回、`error_code` か `context` の `operation` / `query` / `reason` が変わったとき、
   同じ失敗が続く間は 60 秒ごとに記録される（`context` に `task=sequencer`、`failures`（連続の回数）、`retry_in_ms`。同じ連続の行は同じ `request_id`）。
   `forge worker` が終了コード 1 で止まり `FG-SYS-0001` `reason=shutdown_channel_closed` または `reason=task_panicked` を記録していたら、worker 自体の不具合 → Runbook `internal-error.md`。
   その `error_code` で分ける:
   - `FG-DB-0001`: DB に接続できない → Runbook `db-unavailable.md`
   - `FG-DB-0002` `query=sequencer.assign_seq` `reason=lock_timeout`: 別のセッションが advisory lock を持ったまま。`lock_holder_pid` を
     `SELECT pid, state, now() - xact_start AS in_tx, application_name FROM pg_stat_activity WHERE pid = <lock_holder_pid>;` で確かめる（`query` 列は表示しない）
   - `FG-DB-0002` `reason=permission_denied`: `forge_app` が `forge.assign_seq` を実行できない → `forge doctor` の `db.bootstrap`（`guard=function_privilege`）
   - `FG-DB-0009`: 保護のトリガーか関数の件数の確認が番号付けを拒否した。不具合か手作業の変更 → Runbook `append-only-violation.md`。**手で番号を振らない**
   - 復旧すると `forge.worker.task_recovered`（`failures` 付き）が出る
4. 番号付けの状態を読む（読み取りのみ）:
   `SELECT high_seq, audit_high_seq, audit_sequencing_enabled, now() - sequencer_heartbeat AS age FROM forge.event_feed_state;`

### 受け入れているリスク（ADR-0016 C4 の Amendment）

- advisory lock（`pg_advisory_xact_lock(1718579815, 1)`）は `forge_app` のどのセッションからも取れる。worker 以外がそれを持ち続けると番号付けが止まる。
  そのセッションは `lock_holder_pid` に出るので、手順 3 の `pg_stat_activity` で確かめる。
- `include_audit`（監査の行も番号付けするか）は呼び出し側が決める（ADR-0018 §5）。C9 までは worker が常に `false` を渡す。
- heartbeat は番号を振ったとき、または 5 秒より古いときだけ書かれる（migration `20261027110100`）。動いている worker の heartbeat は最大で約 5 秒古く、
  `heartbeat … ms ago` が数秒でも異常ではない（遅れの判定は 30 秒）。
- `outbox.sequencer_lag` が `ok … worker not observed` のとき、番号を待つ行はないが worker は 30 秒以上動いていない（または一度も動いていない）。
  worker を常駐させる環境なら手順 2 を確かめる。

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
