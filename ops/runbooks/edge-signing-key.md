# Runbook: edge-signing-key（FG-EDGE-0003。関連: FG-EDGE-0004）

計画 21（ADR-0041）のエッジのリリースの署名を確かめる鍵が無い・読めない・リリースの `key_id` を持っていないとき。

E1 の時点で鍵を使うのは `gstn edge release verify`（手元の鍵の一覧 `edge-keys.json`）だけ。サーバーの鍵ファイル
`edge.signing_keys_file`・`GET /api/v1/edge/keys`・`forge admin edge-key`・doctor `edge.signing_key` は E4 で足し、この runbook も E4 で広げる。
機器のエージェント（E6）は導入のときの鍵を固定して持つ。エッジの鍵は **部品の鍵（`agent-keys.json`）とは別**（計画 21 §11 問い 7）。

## Trigger

- `gstn edge release verify` が `FG-EDGE-0003` EdgeSigningKeyUnavailable で終わった。`reason`:
  - `keys_missing`: `--keys` が無く、gstn の設定のディレクトリ（`GSTN_CONFIG_DIR`、既定は OS の設定の場所）に `edge-keys.json` が無い。
  - `keys_unreadable`: ファイルが読めない（権限、1 MiB 超）。
  - `keys_invalid`: 形が違う（JSON でない、`kid` が空・重複、`public_key` が base64 の 32 バイトでない、知らない鍵、64 個超）。
  - `unknown_key`: リリースの `key_id` が一覧に無い。手元の一覧が鍵の入れ替えより古いか、**誰も発行していない kid**（改ざんの印）。

## Impact

- その人はリリースの署名を確かめられない（確かめが通らないだけで、何も書き換えない）。機器・サーバーには影響しない。SEV4。
- `unknown_key` で、その kid を管理者が発行していないなら改ざんの疑い: Runbook `edge-agent` に移る（SEV2 から）。

## Diagnose（読み取りのみ）

1. `gstn edge release verify --release <file> --json` の `problems[]`（`reason`・`subject`）と、リリースの `key_id`。
2. 手元の一覧: `edge-keys.json` の `keys[].kid`。
3. 管理者に、今のエッジの kid と過去の kid（退役・失効を含む）の一覧を確かめる。リリースの kid がそのどれにも無ければ改ざんの疑い。
4. 部品の鍵の一覧（`agent-keys.json`）を `--keys` に渡していないか。エッジのリリースは部品の鍵では決して通らない（ADR-0041 §4）。

## Mitigate（変更は人の承認のあと）

1. 管理者から今のエッジの公開鍵の一覧を受け取り、`edge-keys.json` を置き換える（公開鍵なので秘密ではない。E4 以降は
   `GET /api/v1/edge/keys` の応答と同じ形）。
2. 形は `{"keys":[{"kid":"k1","public_key":"<base64>","created_at":"…","retired_at":null,"revoked_at":null}]}`。
3. `gstn edge release verify` をもう一度。

## Verify

- `gstn edge release verify` が FG-EDGE-0003 を出さない（他の問題があれば、それぞれのコードの Runbook へ）。

## 関連

- ADR-0041 §4（署名の領域と鍵）、ADR-0013 Amendment（鍵ファイルの kid と入れ替え）、計画 21 §5.2・§7、Runbook `edge-agent`。
