# Runbook: audit-seal-backlog（FG-DB-0015）

## Trigger

- `forge doctor` の `audit.seal_backlog` が `FG-DB-0015` AuditSealBacklog で失敗した
- 封印（`forge.audit_seal`）のない監査の行のうち最も古いものが 5 分より古い（最後の封印の後に番号を振られた行、またはまだ番号のない行）

## Impact

- 新しい監査の行が封印されていない間、その行の改ざんは封印では検知できない（ADR-0018 §4）。画面と git の操作は動き続ける。
- 数十分なら SEV3。1 時間を超える、または `FG-DB-0011` と同時なら SEV2（Runbook `audit-seal-mismatch.md` を先に）。
- **想定どおりの場合**: 監査の番号付けが止まっている間（`audit_sequencing=off`）。導入の直後（C9 の送出先との最初の比較の前）と、巻き戻しを検出して封印の再同期を待っている間（C9）はこの警報が出続ける。

## Diagnose（読み取りのみ）

1. `forge doctor --json` の `audit.seal_backlog` の `context` を読む:
   - `oldest_age_s`: 最も古い未封印の行の経過秒
   - `last_sealed_seq`: 最後の封印の `last_seq`（`none` はまだ何も封印していない）
   - `audit_high_seq`: 番号を振った最後の監査の番号
   - `unsealed_numbered`: 最後の封印の後に番号を振られた行の数（100000 を超えると `100000+`）
   - `unsequenced`: まだ番号のない監査の行の数
   - `audit_sequencing`: `on` / `off`（`forge.event_feed_state.audit_sequencing_enabled`）
2. `unsequenced > 0` で `audit_sequencing=off`: 監査の番号付けが止まっている。導入の直後か、再同期の待ちかを確かめる（C9 の `feed.rollback_check`）。封印のジョブの問題ではない。
3. `unsequenced > 0` で `audit_sequencing=on`: シーケンサーが止まっている → Runbook `outbox-sequencer-lag.md`（`FG-DB-0005` も出ているはず）。
4. `unsealed_numbered > 0`: 封印のタスク（`forge worker` の `task=audit_seal`。10 秒ごとに見て、前の封印から 1 分たつか 1000 行を超えたら封印する）が動いていない・失敗している。`forge worker` が動いているか（`systemctl status forge-worker`）、worker のログの `task=audit_seal` の失敗の `error_code` を読む（失敗の続く間は 1 回、同じ `request_id` で 1 分ごとに再掲）:
   - `FG-DB-0011` `reason=gap` / `reason=chain`: 欠番・状態の不一致 → Runbook `audit-seal-mismatch.md`
   - `FG-SYS-0002` `config_key=audit.seal_keys_file`: worker の鍵ファイルがない（`reason=missing`: **worker は起動しない**。起動のログに出る）・読めない・形式が違う（`reason`、`line`）
   - `FG-SYS-0001` `operation=seal.v1` `reason=details_unreadable`（`seq` 付き）: 版 1 で表せない行。`details` の jsonb の入れ子が 128 段を超える（serde_json の上限。jsonb はそれ以上を許すが、アプリは書かない。生の SQL でしか書けない）。その行で封印が止まる。行は変えず、開発者（Claude のセッション）と対処を決める
   - `FG-DB-0001` / `FG-DB-0002`: DB の接続・文の失敗 → Runbook `db-unavailable.md` など
5. 封印の状態を読む（読み取りのみ）:
   `SELECT max(last_seq), max(sealed_at) FROM forge.audit_seal;` と
   `SELECT audit_high_seq, audit_sequencing_enabled FROM forge.event_feed_state;`

### 受け入れているリスク

- 封印のジョブは `pg_try_advisory_xact_lock(1718579815, 8)` を取れなければ何もしない。別のセッションがその lock を持ち続けると封印が止まる（`pg_locks` の `classid = 1718579815 AND objid = 8 AND objsubid = 2` で持ち主の pid が分かる）。
- 1 回の封印は最大 5000 行。溜まった行は複数回に分けて、間を空けずに封印される。
- 封印は C6 のジョブの待ち行列ではなく `forge worker` の常駐のタスク（ADR-0018 の C8 の Amendment）。worker が止まると封印も止まる。

## Mitigate

1. worker が止まっている: 起動する（`systemctl start forge-worker`）。`[MUTATING — needs approval]`（ロールバック: `systemctl stop forge-worker`）
2. 鍵ファイルの問題: `audit.seal_keys_file`（`FORGE_AUDIT_SEAL_KEYS_FILE`）を正しい鍵ファイル（1 行に `<key_id> <16 進の鍵>`、所有者だけが読める）にして worker を再起動する。`[MUTATING — needs approval]`（ロールバック: 前の設定に戻す）
3. advisory lock を長く持つセッション: 何かを確かめ、レビューの後に止める（`pg_cancel_backend`）。`[MUTATING — needs approval]`（ロールバック: なし）
4. **してはいけないこと**: 封印の行を手で入れない、番号付けを手で有効にしない（`audit_sequencing_enabled` は C9 の比較の関数だけが変える）。

## Verify

- `forge doctor` の `audit.seal_backlog` が `ok … sealed to <seq>` になり、`audit.seal_verify` も `ok`。

## Escalate

- 1 時間で直らない、または原因が `FG-DB-0011` のときは、DB の担当者と開発者（Claude のセッション）に `forge doctor --json` の出力と worker のログ（`request_id`）を渡す。
