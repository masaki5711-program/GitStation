# Runbook: edge-agent（FG-EDGE-0004。関連: FG-EDGE-0003）

計画 21（ADR-0041）のエッジのリリースの署名・要約・blob が合わないとき。**改ざんの疑い** として扱う。

このコードを出すのは `gstn edge release verify` だけ。2026-11-25 の範囲の変更（ADR-0041 Amendment、計画 21 §0）で、機器・展開・機器の
エージェントは利用者の設定のツールが持つ。GitStation は `forge-edge-agent` を作らず、機器の側の手順（機器のログ、機器の復旧）もこの runbook には
足さない（設定のツールの runbook が持つ）。runbook の名前 `edge-agent` は FG-EDGE-0004 が参照しているので変えない。

## Trigger

- `FG-EDGE-0004` EdgeSignatureInvalid。`reason`:
  - `digest_mismatch`: マニフェストから作り直した要約が封筒の `digest` と違う（マニフェストが書き換えられた）。
  - `bad_signature`: その鍵のその要約へのエッジの領域の署名ではない（要約まで作り直した、鍵が違う、**部品の署名など他の領域の署名を流用した**）。
  - `signature_malformed`: 署名が base64 の 64 バイトでない。
  - `key_revoked`: 失効した鍵の署名。
  - `blob_mismatch` / `blob_missing` / `blob_not_a_regular_file`（`--blobs`）: blob の大きさ・SHA-256 がマニフェストと違う、無い、普通のファイルでない。

## Impact

- 署名の通らないリリースは使わない（設定のツールに渡さない）。
- 原因が改ざんなら、署名の鍵・blob の置き場・ファイルを運んだ経路のどこかが信用できない。SEV2 から（`key_revoked` 以外で説明がつかないとき）。

## Diagnose（読み取りのみ）

1. `gstn edge release verify --release <file> [--blobs <dir>] --json` の `problems[]`（`reason`・`subject` = kid か blob のパス）、
   `release_id`・`version`・`key_id`。
2. ファイルをどこから得たか（CI の成果物の取得、手での写し）。同じ成果物を取り直して比べる
   （`GET /api/v1/projects/{id}/pipelines/{pid}/jobs/{jid}/artifacts`、応答の sha256 のヘッダー）。
3. `blob_mismatch`: `sha256sum <dir>/<path>` とマニフェストの `sha256`・`size` を比べる。ダウンロードの途中で切れたもの（大きさが違う）か、
   中身が違うものか。
4. `bad_signature` で要約は合っている: 誰が要約を作り直したか。鍵の一覧に部品の鍵が混じっていないか（ADR-0041 §4: 領域が違うので通らない）。
5. `key_revoked`: その鍵がいつ・なぜ失効したか（鍵の管理者に確かめる）。

## Mitigate（変更は人の承認のあと）

1. そのリリースを使わない（設定のツールに渡さない。渡したあとなら、設定のツールの側で止めるよう利用者に伝える）。
2. CI の成果物からリリースと blob を取り直して確かめ直す。通れば、元のファイルの経路（写し）を調べる。
3. 取り直しても通らない: 改ざんとして扱う。blob の置き場と鍵ファイルに書けた人・プロセスを調べ、鍵の漏えいの疑いがあれば Runbook
   `edge-signing-key`。
4. `key_revoked`: 新しい鍵で署名し直したリリースを使う（署名するのは任意の E4 を作ったときだけ）。

## Verify

- 取り直したリリースで `gstn edge release verify --blobs` が問題なく終わる（`verified: true`）。

## 関連

- ADR-0041 §2〜§5・§10、計画 21 §5.2・§5.8・§9、Runbook `edge-signing-key`。
