# forge-edge

エッジ機器への配布（計画 21 M4E、ADR-0041）の **純粋な** 判断: リリースのマニフェストと並びによらない要約、エッジの領域での Ed25519 の
署名の確かめ、`edge-release.toml` の lint、波の割り当て、停止・前進の判定、保守の時間帯。I/O・時計・DB・非同期の実行時を使わない（バイトと
値を受け取り、値を返す。時刻は引数 `now`。`tests/purity.rs` の `purity`）。契約 9 の層は `forge-core → forge-edge → forge-agent`。
`gstn`（`gstn edge release verify`）が使う。**2026-11-25 の範囲の変更**（ADR-0041 Amendment、計画 21）: エッジの版は標準のリポジトリと CI で
管理し、封筒は既定では作らない（任意の E4 で使う）。`forge-fleet`・`forge-edge-agent` は作らない。波・停止の判定・保守の時間帯（と FG-EDGE-0005）は
**退役の候補** で、新しい呼び出し元を作らない（任意の E1b で消す）。

## 責任

- 何を配ってよいか（マニフェストと `edge-release.toml` の規則、シークレットらしいファイルを断る）。
- 配ったものが署名したものと同じか（要約・署名・blob のハッシュ）。部品の署名や他の領域の署名を通さない。
- 誰がいつ入れるか（波の単調な所属、保守の時間帯、残りの時間）と、いつ止めるか（`decide`）。

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
| `WavePlan` / `wave_bucket` / `first_wave` / `in_wave` | 波の計画（canary は先頭だけ、割合は厳密に増えて 100 % で終わる。FG-EDGE-0005）と、機器の bucket・最初の波（単調） |
| `decide(&WaveStats, WaveKind, &HaltPolicy, now) -> Decision` | halt（理由と数）/ advance / wait（`no_members`・`below_min_sample`・`in_progress`・`soaking`、`stalled`）。規則は `src/decide.rs` の docs |
| `HaltPolicy::new(HaltPolicySpec)` / `HaltPolicy::spec` | しきい値（既定は計画 21 §5.6）。範囲の外は FG-EDGE-0005 |
| `Schedule::new(tz, windows, blackouts)` / `WeeklyWindow::new` / `Blackout::new` | IANA のタイムゾーン（tz database は `jiff` に組み込み）、曜日 + `HH:MM` + 分、禁止期間。誤りは FG-EDGE-0005 |
| `install_window(&Schedule, now) -> Option<InstallWindow>` / `may_start_install` | `now` を含む窓か次の窓（DST と日付の境をまたぐ。壁の時計の規則は `src/window.rs` の docs）と、残りの時間が `max_install_seconds` 以上か |
| `worst(&[Problem])` | 終了コードに使う、いちばん重い問題（0004 → 0003 → 0002 → 0001 → 0005） |

`Problem` は `code`（登録したコード）、`reason`（安定した snake_case）、`subject`（欄・パス・kid。ファイルの中身は入れない）。

## 持つエラーコード

| コード | 名前 | Runbook |
|---|---|---|
| FG-EDGE-0001 | EdgeReleaseDefinitionInvalid | — |
| FG-EDGE-0002 | EdgeManifestInvalid | — |
| FG-EDGE-0003 | EdgeSigningKeyUnavailable（alert） | `edge-signing-key` |
| FG-EDGE-0004 | EdgeSignatureInvalid（alert） | `edge-agent` |
| FG-EDGE-0005 | EdgeRolloutPolicyInvalid | — |

## 試験

`cargo nextest run -p forge-edge`（`src/tests.rs`: 計画 21 E1 の `digest_is_independent_of_blob_order`・
`agent_part_signature_does_not_verify_as_edge_manifest`・`wave_membership_is_monotonic`・`canary_single_failure_halts`・
`below_min_sample_waits`・`failure_rate_boundary_2_percent`・`window_across_dst_and_midnight` ほか。`tests/purity.rs` の `purity`: 依存・
`jiff` の機能・I/O と時計を使うコードが無いこと）。`gstn edge release verify` の通しの試験は `crates/forge-client/tests/edge.rs`。
署名の領域の一般化の試験は `crates/forge-agent/src/tests.rs`（`existing_part_signatures_still_verify_after_domains` ほか）。
