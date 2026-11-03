# Runbook: integration-feed-lag（FG-INT-0007 / FG-INT-0008 / FG-INT-0009 / FG-DB-0014）

連携（kanflow）のイベントの履歴 API（`GET /api/v1/integrations/{id}/events`、M3 I1c、ADR-0016 §7 と Amendment I1c、`docs/api/integration-feed.md`）
が続きを返せない・連携が読みに来ない・履歴の世代を作り直した、のときの手順。

## Trigger

- `forge doctor` の `integrations.feed_lag` が **FG-INT-0009** FeedConsumerLag（warn）で失敗した。`reason`:
  - `not_reading`: 有効な連携が、結びつけたプロジェクトを持つのに 24 時間読みに来ていない（一度も読んでいないものは、最初の結びつけから 24 時間後に数える）
  - `behind`: まだ読んでいない一番古いイベントが、保持期間の半分より古い（保持で消えると 410 `retention` になり、取り直しが要る）
- `forge serve` のログに **FG-INT-0009**（warn、`reason=behind`）が出た（読み取りのたびに判定し、連携ごとに 1 分に 1 回まで）
- kanflow のログに **410 / FG-INT-0007** FeedCursorExpired が出た。本文の `reason`:
  `retention`（保持で消えた・カーソルが保持期間の 1.5 倍より古い）、`epoch_mismatch`（世代を作り直した）、`ahead_of_head`（DB を前の時点に戻した）、
  `visible_set_changed`（結びつけ・解除・**同じプロジェクトの解除と結び直し**・サービスアカウントのロール・プロジェクトの削除・`event_types` の変更）
- kanflow のログに **400 / FG-INT-0008** FeedCursorInvalid が出た（`rule` はサーバーのログだけ）
- **FG-DB-0014** FeedEpochChanged（warn）: `forge admin events new-epoch` を実行した（復元の手順の一部）
- `forge doctor` の `subscribers.work-item-sync` が警告（作業項目の同期が止まると、履歴はその手前までしか返さない）
- `forge doctor` の `feed.sink_compare` が警告（M3 では常に。下の「受け入れているリスク」）

## Impact

- 履歴 API は止まらない。FG-INT-0007 / 0008 はその連携の 1 回の要求の失敗で、kanflow は手順どおり取り直す（下の「kanflow の復旧」）。
- FG-INT-0009 `not_reading` / `behind` を放っておくと、保持（`events.retention`、既定 90 日、上限 1.5 倍 = 135 日）で未読のイベントが消え、
  kanflow は 410 `retention` から全体を取り直す（取りこぼしはしないが、PBI の「開発」タブの更新が取り直しまで止まる）。
- 重大度: kanflow の更新が 1 営業日以上止まって業務に影響が出ているなら SEV3、それ以外は SEV4。履歴のデータが消えたわけではない。

## Diagnose（読み取りのみ）

1. `forge doctor --json` の `integrations.feed_lag`、`subscribers.work-item-sync`、`events.retention`、`feed.sink_compare` を読む。
   FG-INT-0009 の `context`: `integration_id`、`reason`、`last_read_at`（UTC、または `never`）、`behind_seconds`、`retention_days`。
2. 読み取りの記録（`forge.integration_feed_reads`、連携ごとに 1 分に 1 回まで書く派生の表）:
   ```sql
   SELECT integration_id, last_read_at, cursor_seq, read_bound_seq, oldest_unread_at
     FROM forge.integration_feed_reads ORDER BY last_read_at;
   SELECT epoch, high_seq, retained_from, epoch_rotated_at FROM forge.event_feed_state;
   SELECT name, enabled, last_seq, failures, last_error_code FROM forge.event_subscribers
    WHERE name = 'work-item-sync';
   SELECT events_days FROM forge.retention_policy;
   ```
   - `read_bound_seq` は読み取りの上限 = `min(high_seq, work-item-sync の last_seq)`。`work-item-sync` が止まると上限も止まる
     （作業項目を決める前のイベントは返さない）→ Runbook `subscriber-lag.md`。
3. 410 / 400 の要求をサーバーのログで探す（kanflow のログの `request_id` = 問い合わせ番号）:
   `error_code=FG-INT-0007` の `reason`・`cursor_seq`・`retained_from`・`high_seq`、`error_code=FG-INT-0008` の `rule`
   （`tag` = 値の改変か別の一覧のカーソル、`kid` = 鍵の入れ替えで古い鍵を早く消した、`other_integration` = 別の連携のカーソル）。
   **カーソルの値はログに出ない**（出ていたら不具合）。
4. `visible_set_changed` の原因を監査ログで見る:
   ```sql
   SELECT occurred_at, action, project_id, details FROM forge.audit_log
    WHERE action IN ('integration.bound', 'integration.unbound', 'integration.updated',
                     'member.added', 'member.role_changed', 'member.removed', 'project.deleted')
    ORDER BY id DESC LIMIT 20;
   ```
5. kanflow が読みに来ない（`not_reading`）: kanflow の poller が動いているか、サービスアカウントのトークンが失効していないか
   （`FG-AUTH-0001` がサーバーのログに出ていないか、`forge doctor` の `service_accounts.tokens_expiring` → Runbook `service-accounts.md`）、
   連携が無効にされていないか（`gstn admin integration view <id> --json` の `enabled`。無効なら 403 `reason=integration_disabled`）。

## Mitigate（変更は承認が要る）

- **kanflow の復旧（410 のとき。kanflow 側の動作で、サーバーの変更ではない）**: 先に `GET /api/v1/integrations/{id}/events/cursor` で今の位置を取る →
  開発状況 API（`state=all`）で全体を取り直す → 取っておいた位置から履歴を読む。順番を変えると、取り直しの間の変化を取りこぼす。
- **`work-item-sync` が止まっている**: Runbook `subscriber-lag.md`（飛ばすのは最後の手段）。再開すれば履歴も先へ進む。
- **鍵を早く消した（FG-INT-0008 `rule=kid`）**: 承認を得て、消した鍵を `api.cursor_keys_file` に戻す（復号にだけ使われる）→ `forge serve` を再起動。
  戻せないなら kanflow に取り直しを頼む（上の復旧手順）。
- **DB を復元・作り直した（同じクラスター）**: 承認を得て、復元の手順の中で 1 回だけ実行する:
  ```
  forge admin events new-epoch
  ```
  - `database.maint_url` / `database.maint_password_file`（`forge_maint`）が要る。`forge.feed_new_epoch` が `epoch` を作り直し、
    監査に `feed.epoch_rotated`（`old_epoch`、`new_epoch`、`reason=manual`）を残し、ログに FG-DB-0014（warn）を出す。
  - 以後、すべての連携のカーソルは 410 `epoch_mismatch` になり、kanflow は取り直す。同じ原因で 2 回実行しない（2 回目も取り直しが起きるだけで害はない）。
- 保持期間の変更（`forge.retention_policy.events_days`、最小 7 日）は追記のマイグレーションで行う（`forge_app` は書けない）。短くすると未読の連携が 410 になる。

## Verify

- `forge doctor` の `integrations.feed_lag` が ok（全連携が 24 時間以内に読み、遅れが保持期間の半分未満）。
- `SELECT integration_id, last_read_at FROM forge.integration_feed_reads` の `last_read_at` が進む（1 分に 1 回まで更新）。
- kanflow のログに 410 / 400 が続かない（取り直しの後の 1 回は正常）。

## Prevent / 受け入れているリスク

- **`feed.require_sink_compare` は M3 では `false` だけ**（計画 11 Q1）。監査の封印の送出先（`forge seal-sink`、C9）がまだないため、
  送出先との比較なしで履歴を返す。DB の巻き戻しの自動検出（`system_identifier`・タイムライン・送出先）は C9 で入る。それまでは、復元の後に
  手で `forge admin events new-epoch` を実行する（上）。`true` は起動時に `FG-SYS-0002 reason=unsupported` で拒む。doctor の `feed.sink_compare` が毎回警告する。
- 結びつけの解除は作業項目のリンクを `work_item.unlinked` なしで終わらせる（ADR-0016 Amendment I1b §5）。履歴は結びつけの世代
  （`integration_bindings.generation`）をカーソルの `VisibleSet` に含めるので、解除と結び直し（同じ kanflow のプロジェクトでも）の後は必ず 410
  `visible_set_changed` になり、kanflow は取り直す。
- 保持（`events.retention`、`forge worker` のタスク）は購読者（`retention_blocking`）の位置より先を消さないが、保持期間の 1.5 倍を過ぎた行は消す。
- 流量制限（連携ごとの 429）は I1c ではまだない（1 回の要求は最大 500 件・2 MiB、カーソルの範囲の索引で読む）。
