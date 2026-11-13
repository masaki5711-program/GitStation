# forge-agent

エージェントの部品（計画 16、ADR-0032）の **純粋な** 処理: `part.toml` と `.gitstation/agent.lock.json` の解析と lint、並びによらない要約、
Ed25519 の署名の確かめ、フック・設定・MCP の静的な検査。I/O・時計・DB・非同期の実行時を使わない（バイトを受け取り、値を返す。`tests/purity.rs`）。
契約 9 の層は `forge-core → forge-agent → forge-ci`。`gstn`（`gstn agent verify`）が使い、A5 から `forge-marketplace`（公開）、A7 から
`forge/agent-lock` が同じ関数を使う。

| 公開の API | 中身 |
|---|---|
| `parse_part(&[TreeEntry]) -> Result<Part, Vec<Problem>>` | 部品のディレクトリ（`part.toml` と中身）の lint と要約。置き場所は `.claude/**`・`CLAUDE.md`・`.mcp.json`（mcp の部品だけ）。symlink・宣言していないファイル・200 ファイル超・1 MiB 超を断る（FG-AGENT-0001） |
| `check_install_path(&str)` / `is_managed_path(&str)` | 置き場所の判定と、lock が管理する範囲（人ごとの `.claude/settings.local.json`・`.claude/worktrees/` を除く） |
| `digest(&Manifest, &[(path, sha256)]) -> Digest` | 要約（`sha256:<hex>`）。ファイルの並びによらない。形は `src/digest.rs` の docs |
| `verify_signature(&KeyRing, kid, &Digest, sig)` | Ed25519（`ring`）。知らない kid は FG-AGENT-0003、失効した鍵・合わない署名は FG-AGENT-0002 |
| `sign_digest(&seed, &Digest)` / `public_key_from_seed` | 公開（A5）と試験のための署名。種は呼び出し側から出ない |
| `KeyRing::parse(bytes)` | 公開鍵の一覧 `{"keys":[{"kid","public_key","created_at","retired_at","revoked_at"}]}` |
| `scan(&Part, &ScanPolicy) -> Vec<Problem>` | 静的な検査。宣言していない `exec`・`network`・`permissions`、見通せないコード（`dynamic_code`、宣言しても断る）は FG-AGENT-0005、許可の一覧に無い MCP のホストは FG-AGENT-0013 |
| `Lock::parse(bytes)` / `verify_lock(&Lock, &KeyRing, &tree)` | lock と作業ツリー（管理するファイルの path → バイト）。ファイルの SHA-256 → 要約 → 署名の鎖。不一致は FG-AGENT-0007 |
| `worst(&[Problem])` | 終了コードに使う、いちばん重い問題（0002 → 0003 → 0005 → 0013 → 0007 → 0001） |

`Problem` は `code`（登録したコード）、`reason`（安定した snake_case）、`subject`（パス・ホスト・kid。ファイルの中身は入れない）。

## 持つエラーコード

| コード | 名前 | Runbook |
|---|---|---|
| FG-AGENT-0001 | PartManifestInvalid | — |
| FG-AGENT-0002 | PartSignatureInvalid（alert） | `agent-supply-chain` |
| FG-AGENT-0003 | SigningKeyUnavailable（alert） | `agent-signing-key` |
| FG-AGENT-0005 | PartCapabilityUndeclared | — |
| FG-AGENT-0007 | AgentLockMismatch | — |
| FG-AGENT-0013 | McpEndpointNotAllowed | — |

## 試験

`cargo nextest run -p forge-agent`（`src/tests.rs`: 計画 16 A1 の `manifest_rejects_path_outside_claude_dir`・`digest_is_independent_of_file_order`・
`verify_rejects_one_byte_change`・`scan_flags_undeclared_child_process`・`mcp_external_host_rejected` ほか。`tests/purity.rs`: 依存）。
`gstn agent verify` の通しの試験は `crates/forge-client/tests/agent.rs`。
