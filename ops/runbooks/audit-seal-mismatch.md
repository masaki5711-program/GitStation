# Runbook: audit-seal-mismatch（FG-DB-0011）

## Trigger

- `forge doctor` の `audit.seal_verify` が `FG-DB-0011` AuditSealMismatch で失敗した（直近 24 時間の封印の再計算）
- `forge worker` の封印のタスク（`task=audit_seal`）のログに `FG-DB-0011` が出た: `reason=gap`（封印しようとした範囲に欠番がある）、`reason=chain` の `feed_state_missing` / `audit_high_below_seal`（下記）。いずれも何も封印していない。失敗の続く間、ログは 1 回（同じ `request_id`、1 分ごとに再掲）
- `forge doctor` の `audit.seal_verify` が `warn … HMAC not checked`: そのホストに鍵ファイルがなく、HMAC を照合していない。**監視のホスト（doctor を定期実行するホスト）には `audit.seal_keys_file` を置く**（worker と同じ鍵ファイル、所有者だけが読める）。不一致ではないが、そのままでは封印の値と鎖しか確かめていない
- C9 以降: 送出先との突き合わせ（`reason=sink_mismatch` / `sink_missing` / `db_behind_sink`）。この Runbook の C8 の版はそれらの手順をまだ持たない

## Impact

- 監査ログ（`forge.audit_log`）が封印（`forge.audit_seal`）と一致しない。**改ざんの可能性**として扱う（ADR-0018 §4。ガードを越えて行を変えられるのは DB の所有者（`forge_owner`）か superuser だけ）。
- `reason=gap` を封印のジョブが出している間は新しい封印が作られず、5 分後に `FG-DB-0015`（Runbook `audit-seal-backlog.md`）も出る。
- forge の画面と git の操作は動き続ける。原則 SEV2（セキュリティの事象の可能性）。原因が説明できるまで下げない。

## Diagnose（読み取りのみ）

1. **証拠を残す**: `forge doctor --json` の出力と、worker のログの該当の行（`request_id`）を保存する。DB には何もしない。
2. `context` を読む:
   - `reason` と `detail`:
     - `recompute` / `rows_sha256`: 封印済みの行の **封印の対象の列**（`seq`、`occurred_at`、`actor_*`、`credential_id`、`act_as_*`、`via_assertion_id`、`effective_scope`、`action`、`outcome`、`resource_*`、`project_id`、`request_id`、`error_code`、`occurrences`、`details`）が変わった。個人情報の列（`actor_label`・`source_ip`・`user_agent`）の伏せ字では起きない
     - `recompute` / `seal_sha256`: 封印の行そのものの値（`rows_sha256`、範囲、`events_high_seq`、`kind`、`key_id` など）が、その `seal_sha256` と合わない（封印の行の書き換え）
     - `recompute` / `hash_version`: このバイナリの知らないハッシュの版。`recompute` / `kind`: 知らない `kind`
     - `chain` / `prev_seal_sha256`: 前の封印の `seal_sha256` とつながらない。`chain` / `first_seq`: 直前の範囲の封印がない
     - `chain` / `feed_state_missing`（封印のタスク）: `forge.event_feed_state` の行がないのに、番号付きの監査の行か封印がある（行の削除はガードが拒む: 所有者か superuser の操作、または誤った復元）
     - `chain` / `audit_high_below_seal`（封印のタスク、`last_sealed_seq` / `audit_high_seq` 付き）: `audit_high_seq` が最後の封印の `last_seq` より小さい（`audit_high_seq` は減らない: 状態の書き換えか、封印より古い時点への巻き戻し。C9 の `db_behind_sink` の DB の中だけの版）
     - `hmac` / `mismatch`: 鍵 `key_id` で HMAC が合わない（封印の値を書き換えて計算し直した、または鍵ファイルの鍵の値が違う）
     - `hmac` / `unknown_key_id`: 鍵ファイル（`audit.seal_keys_file`）にその `key_id` がない（鍵を消した・別の鍵ファイルを使っている）
     - `gap` / `missing_rows`: 範囲の行が DB にない。`missing_seq` が最初の欠けた番号。`seal_id` があれば封印済みの行が消えた、なければ封印前の範囲の番号が飛んでいる（シーケンサーの不具合か、`forge_seq` の権限での手作業）
   - `seal_id`、`first_seq`、`last_seq`: 対象の封印と範囲
3. 封印の行を読む（読み取りのみ。superuser の手順は `SET search_path = pg_catalog, pg_temp` で始める）:
   `SELECT id, kind, hash_version, key_id, first_seq, last_seq, sealed_at, shipped_at FROM forge.audit_seal WHERE id = <seal_id>;`
4. 範囲の行の数を確かめる（欠番の確認。値は表示しない）:
   `SELECT count(*), min(seq), max(seq) FROM forge.audit_log WHERE seq BETWEEN <first_seq> AND <last_seq>;`
5. 変更の跡を探す: `forge_audit.ddl_audit` のその時刻の行（`ALTER TABLE … DISABLE TRIGGER` など）、`forge doctor` の `db.bootstrap`（`unexpected_ddl`、`log_triggers`）、PostgreSQL のサーバーのログ（`log_statement` の設定による）。
6. `hmac` / `unknown_key_id` なら、worker と doctor のホストの `audit.seal_keys_file` を比べる（鍵の値は表示しない。行の数と key_id の一覧だけ: `awk '!/^#/ && NF {print $1}' <file>`）。

### 受け入れているリスク（ADR-0018 Context の決定 1）

- 封印は **防止ではなく検知**。所有者か superuser は行も封印も変えられる。HMAC の鍵は DB の外にあるので、封印を作り直すには鍵も要る。
- 送出先（`forge seal-sink`、C9）がまだないので、封印の行を **すべて消して作り直す** 改ざんは、C8 の範囲では鍵を持つ者にしかできないが、検知は C9 の突き合わせからになる。
- `audit.seal_verify` は直近 24 時間（`sealed_at`）の封印だけを再計算する。古い範囲の再計算（7 日で一巡）は C9 の `audit.seal_reverify`。
- `sealed_at` は `seal_sha256` にも HMAC にも含まれない（ガードが決める値）。ガードを外せる者は `sealed_at` を古くして、その封印を 24 時間の窓から外せる。その範囲は C9 の全件の再計算で見つかる（新しい封印の鎖はその封印の `seal_sha256` に結び付いたまま）。
- 1 回の検査は最大 5000 個の封印（`warn … only the oldest 5000 checked`）。

## Mitigate

1. **封印を作り直さない・封印の行を消さない・欠番を手で埋めない**（証拠が消える）。
2. 改ざんの疑いがある場合: セキュリティの担当者に連絡し、DB の所有者（`forge_migrator`）と superuser の資格情報を **ローテーション** する。`[MUTATING — needs approval]`（ロールバック: なし）
3. 行を戻す必要がある場合: バックアップ（PITR）から該当の範囲の行を **別の DB に** 復元して比べ、元の値を確かめてから、承認済みの手順で戻す。`[MUTATING — needs approval]`（ロールバック: 戻す前の値を記録しておく）
4. `hmac` / `unknown_key_id` で鍵ファイルから鍵が消えていた場合: 退役した鍵を鍵ファイルに戻す（鍵は追加するだけで、消さない）。worker を再起動する。`[MUTATING — needs approval]`（ロールバック: 戻した行を消す）
5. 封印のジョブの `reason=gap`（封印前の欠番）: シーケンサーの不具合か手作業かを確かめる。欠番は埋めない。対処の方針（その範囲を封印せずに進めるなど）は開発者（Claude のセッション）と決める（C8 には欠番を飛ばす手順はない）。

## Verify

- `forge doctor` の `audit.seal_verify` が `ok … HMAC checked` になる（鍵のある環境で実行する）。
- worker のログに `FG-DB-0011` が出なくなり、`audit.seal_backlog` が `ok`。

## Escalate

- すべての `FG-DB-0011` はセキュリティの担当者と DB の担当者に、`forge doctor --json` の出力、worker のログ（`request_id`）、手順 3〜5 の結果を渡す。原因が説明できるまで事象を閉じない（postmortem 必須）。
