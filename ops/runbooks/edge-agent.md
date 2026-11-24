# Runbook: edge-agent（FG-EDGE-0004。関連: FG-EDGE-0003）

計画 21（ADR-0041）のエッジのリリースの署名・要約・blob が合わないとき。**改ざんの疑い** として扱う。

E1 の時点でこのコードを出すのは `gstn edge release verify` だけ。E6 で機器のエージェント `forge-edge-agent` が、入れる前に同じ確かめをして
このコードを機器のログ（`device_id`・`rollout_id`・`release_digest` 付き）に出し、サーバーに報告する。機器の側の手順（ログの見方、
`rollback_failed` の機器の手での復旧、ディスク、時計のずれ）は E6 でこの runbook に足す。

## Trigger

- `FG-EDGE-0004` EdgeSignatureInvalid。`reason`:
  - `digest_mismatch`: マニフェストから作り直した要約が封筒の `digest` と違う（マニフェストが書き換えられた）。
  - `bad_signature`: その鍵のその要約へのエッジの領域の署名ではない（要約まで作り直した、鍵が違う、**部品の署名など他の領域の署名を流用した**）。
  - `signature_malformed`: 署名が base64 の 64 バイトでない。
  - `key_revoked`: 失効した鍵の署名。
  - `blob_mismatch` / `blob_missing` / `blob_not_a_regular_file`（`--blobs`）: blob の大きさ・SHA-256 がマニフェストと違う、無い、普通のファイルでない。

## Impact

- 署名の通らないリリースは入れない（機器は入れずに報告する。E6）。展開の対象の機器がそのリリースを入れられない。
- 原因が改ざんなら、署名の鍵・blob の置き場・中継（E8）・機器のどこかが信用できない。SEV2 から（`key_revoked` 以外で説明がつかないとき）。

## Diagnose（読み取りのみ）

1. `gstn edge release verify --release <file> [--blobs <dir>] --json` の `problems[]`（`reason`・`subject` = kid か blob のパス）、
   `release_id`・`version`・`key_id`。
2. ファイルをどこから得たか（サーバーの応答の保存、手での写し、中継）。同じリリースをサーバーから取り直して比べる（E5: `GET /api/v1/edge/desired`）。
3. `blob_mismatch`: `sha256sum <dir>/<path>` とマニフェストの `sha256`・`size` を比べる。ダウンロードの途中で切れたもの（大きさが違う）か、
   中身が違うものか。
4. `bad_signature` で要約は合っている: 誰が要約を作り直したか。鍵の一覧に部品の鍵が混じっていないか（ADR-0041 §4: 領域が違うので通らない）。
5. `key_revoked`: その鍵がいつ・なぜ失効したか（E4 の鍵の表）。

## Mitigate（変更は人の承認のあと）

1. そのリリースを入れない。展開中なら一時停止する（E5 の `EdgeRolloutPause`。止めるのは安全側）。
2. サーバーからリリースと blob を取り直して確かめ直す。通れば、元のファイルの経路（中継・写し）を調べる。
3. 取り直しても通らない: 改ざんとして扱う。blob の置き場と鍵ファイルに書けた人・プロセスを調べ、鍵の漏えいの疑いがあれば Runbook
   `edge-signing-key`（E4 で鍵の失効と入れ替え）。
4. `key_revoked`: 新しい鍵で署名し直したリリースで展開し直す（E4 / E5）。

## Verify

- 取り直したリリースで `gstn edge release verify --blobs` が問題なく終わる（`verified: true`）。

## 関連

- ADR-0041 §2〜§5・§10、計画 21 §5.2・§5.8・§9、Runbook `edge-signing-key`。
