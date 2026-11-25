# forge-edge

エッジ機器への配布（計画 21 M4E、ADR-0041）の **純粋な** 判断: リリースのマニフェストと並びによらない要約、エッジの領域での Ed25519 の
署名の確かめ、`edge-release.toml` の lint。I/O・時計・DB・非同期の実行時を使わない（バイトと値を受け取り、値を返す。
`tests/purity.rs` の `purity`）。契約 9 の層は `forge-core → forge-edge → forge-agent`。
`gstn`（`gstn edge release verify`）が使う。**2026-11-25 の範囲の変更**（ADR-0041 Amendment、計画 21）: エッジの版は標準のリポジトリと CI で
管理し、封筒は既定では作らない（任意の E4 で使う）。`forge-fleet`・`forge-edge-agent` は作らない。波・停止の判定・保守の時間帯（と `jiff` の tz database）は
**E1b で消した**。FG-EDGE-0005 は退役（番号は再利用しない）。

## 責任

- 何を配ってよいか（マニフェストと `edge-release.toml` の規則、シークレットらしいファイルを断る）。
- 配ったものが署名したものと同じか（要約・署名・blob のハッシュ）。部品の署名や他の領域の署名を通さない。

DB・HTTP・鍵ファイルは持たない。

## 公開の API

| 公開の API | 中身 |
|---|---|
| `parse_release(bytes) -> Result<Release, Problem>` | リリースのファイル `{manifest, digest, key_id, signature}` を読み、マニフェストの規則を確かめる（4 MiB まで、知らない鍵を断る、パスの規則、payload の種類、`agent` は単独で blob 1 つ、Windows の装置名や別のパスのディレクトリになるパスを断る）。返すマニフェストは要約と同じ決まった並び（並びは署名されない）。誤りは FG-EDGE-0002 |
| `manifest_digest(&Manifest) -> Digest` | 要約（`sha256:<hex>`）。payload・blob の並びによらない。`created_at` は瞬間で入れる。形は `src/digest.rs` の docs |
| `verify_release(bytes, &KeyRing) -> Result<VerifiedRelease, Problem>` | 規則 → 要約の比較（FG-EDGE-0004 `digest_mismatch`）→ エッジの領域での署名（FG-EDGE-0004 / 知らない鍵は FG-EDGE-0003） |
| `parse_edge_keys(bytes)` | エッジの公開鍵の一覧（部品の鍵と同じ JSON の形、別の一覧）。誤りは FG-EDGE-0003 |
| `sign_manifest(&seed, &Digest)` / `EDGE_MANIFEST_DOMAIN` | エッジの領域での署名。任意の E4 のリリースの作成だけが呼ぶ（今は呼び出し元なし）（`clippy.toml`） |
| `lint_release_definition(bytes) -> Result<ReleaseDefinition, Vec<Problem>>` / `is_secret_like_path` | `edge-release.toml` の lint。問題は全部挙げる（FG-EDGE-0001） |
| `worst(&[Problem])` | 終了コードに使う、いちばん重い問題（0004 → 0003 → 0002 → 0001） |

`Problem` は `code`（登録したコード）、`reason`（安定した snake_case）、`subject`（欄・パス・kid。ファイルの中身は入れない）。

## 持つエラーコード

| コード | 名前 | Runbook |
|---|---|---|
| FG-EDGE-0001 | EdgeReleaseDefinitionInvalid | — |
| FG-EDGE-0002 | EdgeManifestInvalid | — |
| FG-EDGE-0003 | EdgeSigningKeyUnavailable（alert） | `edge-signing-key` |
| FG-EDGE-0004 | EdgeSignatureInvalid（alert） | `edge-agent` |
| FG-EDGE-0005 | EdgeRolloutPolicyInvalid（退役、E1b） | — |

## 試験

`cargo nextest run -p forge-edge`（`src/tests.rs`: 計画 21 E1 の `digest_is_independent_of_blob_order`・
`agent_part_signature_does_not_verify_as_edge_manifest`・`edited_manifest_is_digest_mismatch_and_resigned_digest_is_bad_signature` ほか。
`tests/purity.rs` の `purity`: 依存・`jiff` の機能・I/O と時計を使うコードが無いこと）。`gstn edge release verify` の通しの試験は `crates/forge-client/tests/edge.rs`。
署名の領域の一般化の試験は `crates/forge-agent/src/tests.rs`（`existing_part_signatures_still_verify_after_domains` ほか）。
