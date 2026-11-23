# Runbook: agent-signing-key（FG-AGENT-0003・FG-AGENT-0014。関連: FG-AGENT-0002）

計画 16（ADR-0032）のエージェントの部品の署名を確かめる鍵が無い・読めない・部品の `key_id` を持っていないとき。

鍵を使うのは開発者の PC の `gstn agent verify`（手元の鍵の一覧 `agent-keys.json`）と、サーバー（A3 から: 鍵ファイル
`agent.signing_keys_file` と表 `forge.agent_signing_keys`。公開の署名は A5、`GET /api/v1/agent-keys` も A5）。

## Trigger

- `gstn agent verify` が `FG-AGENT-0003` SigningKeyUnavailable で終わった。`reason`:
  - `keys_missing`: `--keys` が無く、gstn の設定のディレクトリ（`GSTN_CONFIG_DIR`、既定は OS の設定の場所）に `agent-keys.json` が無い。
  - `keys_unreadable`: ファイルが読めない（権限、4 MiB 超）。
  - `keys_invalid`: 形が違う（JSON でない、`kid` が空・重複、`public_key` が base64 の 32 バイトでない、知らない鍵）。
  - `unknown_key`: lock の `key_id` が一覧に無い。手元の一覧が鍵の入れ替えより古いか、**誰も発行していない kid**（改ざんの印）。

- サーバー（A3）: `forge doctor` の `agent.signing_keys` が `FG-AGENT-0014` SigningKeyFileInvalid（`config_key=agent.signing_keys_file`）。`reason`:
  - `insecure_permissions`（`mode` 付き）: 鍵ファイルを group か others が読める（Unix）。`rule=owner`: 別のユーザーの持ち物（Linux）。秘密の鍵の種なので断る。
  - Windows では権限を確かめられないので、合っていても `warn`（ACL を forge のサービスのアカウントだけにする）。
  - `keys_unreadable`（`config_reason`: `not_absolute`・`unreadable`・`invalid` = 普通のファイルでない・64 KiB 超）。
  - `keys_invalid`（`rule`・`line`）: 行の形 `<kid> <64 桁の 16 進数>`（kid は 1〜65535）でない、kid の重複、鍵が無い。
  - `key_not_registered`・`key_mismatch`・`key_revoked`・`signing_key_retired`（`kid` 付き）: ファイルと DB の公開鍵が合わない。

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

### サーバー（A3。変更は人の承認のあと）

1. 読み取りで確かめる: `ls -l <agent.signing_keys_file>`（所有者は forge のサービスのユーザー、モード 600）、
   `SELECT kid, public_key, retired_at, revoked_at FROM forge.agent_signing_keys ORDER BY kid::integer`（読み取りのプール）。
2. `insecure_permissions`: `chmod 600` と `chown`。ファイルが他の人に読まれていた期間があれば、鍵の盗難として扱い、
   新しい kid を足して古い kid を失効する判断を Runbook `agent-supply-chain` で行う。
3. `keys_invalid`: 行を直す（鍵の中身はログに出ない。`line` で場所が分かる）。
4. `key_not_registered`: 新しい kid の公開鍵を登録してから署名に使う（A5 の `forge admin agent-key`。保守の接続 `database.maint_url` の
   forge_maint だけが `forge.agent_signing_key_manage` を実行できる。forge_app は鍵の表を読むだけ）。
5. `key_mismatch`: ファイルの種が登録した公開鍵と合わない。取り違えか偽のファイル。説明がつかなければ改ざんの疑い:
   Runbook `agent-supply-chain`。
6. `key_revoked`: 失効した鍵の行をファイルから消す。`signing_key_retired`: 新しい kid を足す（いちばん大きい kid が署名する）。

## Verify

- `gstn agent verify` が FG-AGENT-0003 を出さない（他の問題があれば、それぞれのコードの Runbook へ）。
- サーバー: `forge doctor` の `agent.signing_keys` が `ok`（設定していないホストは `skip`、Windows は `warn`）。

## 関連

- ADR-0032 §4（署名と鍵）、ADR-0013 Amendment（鍵ファイルの kid と入れ替え）、Runbook `agent-supply-chain`。
