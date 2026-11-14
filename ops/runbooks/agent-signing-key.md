# Runbook: agent-signing-key（FG-AGENT-0003。関連: FG-AGENT-0002）

計画 16（ADR-0032）のエージェントの部品の署名を確かめる鍵が無い・読めない・部品の `key_id` を持っていないとき。

A1 の時点では、鍵を使うのは開発者の PC の `gstn agent verify` だけ（手元の鍵の一覧 `agent-keys.json`）。サーバーの鍵ファイル
（`agent.signing_keys_file`、ADR-0013 の形）と `GET /api/v1/agent-keys` は A3・A5 で足す。そのときこの Runbook にサーバー側の手順を足す。

## Trigger

- `gstn agent verify` が `FG-AGENT-0003` SigningKeyUnavailable で終わった。`reason`:
  - `keys_missing`: `--keys` が無く、gstn の設定のディレクトリ（`GSTN_CONFIG_DIR`、既定は OS の設定の場所）に `agent-keys.json` が無い。
  - `keys_unreadable`: ファイルが読めない（権限、4 MiB 超）。
  - `keys_invalid`: 形が違う（JSON でない、`kid` が空・重複、`public_key` が base64 の 32 バイトでない、知らない鍵）。
  - `unknown_key`: lock の `key_id` が一覧に無い。手元の一覧が鍵の入れ替えより古いか、**誰も発行していない kid**（改ざんの印）。

## Impact

- その開発者は部品の署名を確かめられない（確かめが通らないだけで、何も書き換えない）。他の人には影響しない。SEV4。
- `unknown_key` で、その kid を管理者が発行していないなら改ざんの疑い: Runbook `agent-supply-chain` に移る（SEV2 から）。

## Diagnose（読み取りのみ）

1. `gstn agent verify --json` の `problems[]`（`reason`・`subject`）。
2. 手元の一覧: `agent-keys.json` の `keys[].kid`。lock の `parts[].key_id` と比べる。
3. 管理者に、今の kid と過去の kid（退役・失効を含む）の一覧を確かめる。lock の kid がそのどれにも無ければ改ざんの疑い。

## Mitigate（変更は人の承認のあと）

1. 管理者から今の公開鍵の一覧を受け取り、`agent-keys.json` を置き換える（公開鍵なので秘密ではない。A5 以降は `GET /api/v1/agent-keys` の応答と同じ形）。
2. 形は `{"keys":[{"kid":"k1","public_key":"<base64>","created_at":"…","retired_at":null,"revoked_at":null}]}`。
3. `gstn agent verify` をもう一度。

## Verify

- `gstn agent verify` が FG-AGENT-0003 を出さない（他の問題があれば、それぞれのコードの Runbook へ）。

## 関連

- ADR-0032 §4（署名と鍵）、ADR-0013 Amendment（鍵ファイルの kid と入れ替え）、Runbook `agent-supply-chain`。
