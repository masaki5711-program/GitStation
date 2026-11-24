# ADR-0041: エッジのリリースのマニフェスト・署名の領域・波・停止の判定・保守の時間帯（`forge-edge`、計画 21 E1）

**Date**: 2026-11-24
**Status**: proposed（accepted は E1 `feat/core-edge-manifest` の `rust-reviewer`・`security-reviewer` のレビューで CRITICAL / HIGH が無いとき）
**Deciders**: Masaki Fujikawa（ユーザー。計画 21 の推奨と §11 の既定）、Claude

> **番号について**: 計画 21 とエージェントモデルの E0（root `CLAUDE.md` 契約 9）はこの ADR を「ADR-0039」と書いた。しかし 0039 は計画 19 L5-1
> （`feat/core-llm-gateway`、`0039-local-llm-gateway.md`）が先に使い、0040 は計画 20（ナレッジ）が使った。計画 19 §「ADR の番号」の
> 「採番のときに確かめる」に従い、次の空き番号 **0041** にした。計画 21 の本文はこの MR で 0041 に直した。root の契約 9 の「ADR-0039」は
> エージェントモデルの側で直す（別の MR）。

## Context

計画 21（M4E）は、CI が作った不変のリリースに GitStation が署名し、工場の数千台のエッジ機器が pull 型で取りに来て、保守の時間帯の中で、
設備が止まっているときだけ入れ、失敗が増えれば展開が自動で止まる仕組みを作る。E0（エージェントモデル）で契約 9 を変え、純粋なクレート
`forge-edge` を `forge-core → forge-edge → forge-agent → forge-ci → forge-doc → forge-errors` の位置に置き、`gstn`（`forge-client`）が
それに依存してよいとした。

E1 は、I/O を持たない判断のすべて — マニフェストの形と要約、署名の確かめ、`edge-release.toml` の lint、波の割り当て、停止の判定、
保守の時間帯 — を 1 か所に置き、サーバー（E4 / E5 の `forge-fleet`）、機器のエージェント（E6 の `forge-edge-agent`）、`gstn` が同じ関数を使う
ようにする。DB・ルート・権限の表は E2 以降。

署名は計画 16 A1（ADR-0032）の `forge-agent::sign` が既にある。ただし署名する文の前置き（領域）が定数 `forge-agent-part-signature-v1\0` に
固定されていて、エッジのマニフェストに流用すると「部品の署名をマニフェストの署名として通す」余地が生まれる。

## Decision

1. **クレート `forge-edge`（純粋）**: 依存は `forge-errors`・`forge-agent`（署名の器と `KeyRing`・`Digest`）と計算だけのクレート
   （`serde`・`serde_json`・`toml`・`sha2`・`jiff`）。ファイル・ネットワーク・時計・DB・非同期の実行時を持たない。時刻は引数（`now`）。
   `tests/purity.rs` の `purity` が依存の一覧、`jiff` の機能（`tz-system`・`tzdb-zoneinfo` を入れない）、ソースに `std::fs`・`std::env`・
   `Timestamp::now`・`TimeZone::system` などが無いことを確かめる。`forge-hosting/tests/dependency_direction.rs` の層の表に
   `forge-edge → forge-agent, forge-errors` を足した。
2. **リリースのファイルとマニフェスト**（`schema_version: 1`、JSON）: `{manifest, digest, key_id, signature}`。マニフェストは計画 21 §5.2 の欄
   （`release_id`・`project`・`version`・`source_sha`・`sequence`・`created_at`・`arch`・`os`・`payloads[]`（`kind`・`blobs[]` = `sha256`・`size`・
   `path`）・`max_install_seconds`・`health_soak_seconds`・`ci_pipeline_id`）。知らない鍵・重なった鍵は断る。どの文字列にも長さと字の種類の
   上限がある。blob の `path` は `edge-release.toml` の成果物のパスと同じ規則（相対、`/` 区切り、`..`・`.`・空の区切り・`\`・`:`・`<>"|?*`・制御文字・
   ASCII でない字・区切りの末尾の `.` や空白・Windows の装置名（`CON`・`NUL`・`COM1`・`LPT1` など、拡張子つきも、大小を問わず）を断る、
   255 バイトまで）で、**リリース全体で一意**（大小の違いだけでも重ねない）、しかも **あるパスが別のパスのディレクトリにならない**
   （`a/b` と `a/b/c` を断る。機器は 1 つのディレクトリに並べる）。`agent` の payload は blob 1 つで、他の payload と一緒にしない（エージェントの自己更新は単独の展開）。
   シークレットらしい名前（`.env`・`*.pem`・`*.key`・`id_ed25519*`・`.ssh/` の下など）はマニフェストでも断る。誤りは FG-EDGE-0002。
3. **要約**: SHA-256（`"forge-edge-manifest-digest-v1\n"` ‖ 各欄。可変長の欄は 4 バイトの長さを前に付ける）。`created_at` は文字列ではなく
   **瞬間**（秒とナノ秒）を入れる（同じ瞬間の `Z` と `+07:00` は同じ要約）。payload は kind の順、blob は（sha256、path、size）の順に並べて入れる
   ので、並べた順によらない（`digest_is_independent_of_blob_order`）。どの欄・blob のハッシュ・大きさ・パスの変化でも変わる。
   **並びは署名されない** ので、読んだマニフェストは常に要約と同じ決まった並び（payload は kind、blob は sha256・path・size）で返す
   （`a_parsed_manifest_is_in_canonical_order`。レビューの指摘: 中継が並びを変えても、機器が「並べた順に入れる」ことで署名していない動きを
   しない）。
4. **署名の領域（`forge-agent::sign` の一般化）**: `SignatureDomain`（署名する文の前置きの newtype）を足し、`verify_signature_in` /
   `sign_digest_in` が領域を取る。既存の `verify_signature` / `sign_digest` は `SignatureDomain::AGENT_PART`（今までと同じ
   `forge-agent-part-signature-v1\0`）を使うので、**既に作った部品の署名はそのまま通る**（`existing_part_signatures_still_verify_after_domains`
   が A1 と同じ文を `ring` で直接署名して確かめる）。エッジは `EDGE_MANIFEST_DOMAIN = "forge-edge-manifest-signature-v1\0"`。領域はどれも NUL で
   終わり（各定数でコンパイル時に `is_well_formed` を assert）、互いの前置きにならない。同じ鍵・同じ 32 バイトでも、部品の署名はマニフェストとして通らず、その逆も通らない
   （`agent_part_signature_does_not_verify_as_edge_manifest`）。そのうえ鍵の一覧もエッジ専用（`edge-keys.json`、E4 で `edge.signing_keys_file`。
   計画 21 §11 問い 7）なので、部品の鍵は引かれもしない。`forge-agent` の問題は理由（`reason`）を保ったまま EDGE のコードに写す
   （FG-AGENT-0002 → FG-EDGE-0004、FG-AGENT-0003 → FG-EDGE-0003）。署名する関数は `clippy.toml` の `disallowed-methods` で呼び出し元を絞る:
   `forge_agent::sign_digest_in` は種類ごとの包み（`sign_digest`・`forge_edge::sign_manifest`）だけ、`forge_edge::sign_manifest` は E4 の
   `forge-fleet` のリリースの作成だけ。
5. **確かめの順**（`verify_release`）: マニフェストの規則（0002）→ 作り直した要約と封筒の `digest` の比較（違えば 0004 `digest_mismatch`）→
   `key_id` の鍵で領域つきの署名（0004 `bad_signature`・`signature_malformed`・`key_revoked`、知らない鍵は 0003 `unknown_key`）。どこでも
   「署名が無いので通す」の逃げ道は無い。
6. **`edge-release.toml` の lint**: `schema_version = 1`・`arch`（`amd64` / `arm64`）・`os`・`max_install_seconds`（1〜7200）・
   `health_soak_seconds`（0〜86400）・`[[payloads]]`（`kind`・`artifacts`）。知らない鍵は断り、問題は **全部** 挙げる（FG-EDGE-0001）。成果物の
   パスはマニフェストと同じ規則で、シークレットらしいパスを断る（`release_definition_refuses_secret_like_file`）。名前での検出は間違いを防ぐ
   ためのもので保証ではない。機器ごとの秘密は機器の用意（PFS）に置く。E4 のリリースの作成は、成果物を展開した各ファイルにも同じ検査をする。
7. **波**: 既定 `[canary, 5 %, 25 %, 100 %]`。canary の波は先頭だけ、割合（basis point、1〜10000）は厳密に増え、最後は 100 %。
   `bucket = u32_be(SHA-256("forge-edge-wave-v1\n" ‖ field(rollout_id) ‖ field(device_id))[0..4]) mod 10000`。計画 21 §5.3 の式に
   **長さの前置き** を足した（`("ab","c")` と `("a","bc")` を区別する）。機器の最初の波 = それを含むいちばん早い波で、以後のすべての波に入る
   （**単調**、`wave_membership_is_monotonic`）。bucket は展開と機器だけで決まるので、計算し直しても動かない。誤った計画は FG-EDGE-0005。
8. **停止の判定 `decide(stats, kind, policy, now)`**: 計画 21 §5.6 の表を、上から最初に当たる規則で: `rollback_failed` が 1 台 → halt、
   canary で `failed` が 1 台 → halt（標本の数によらない）、波に機器が 1 台も無い（全台オフライン、canary の機器が無い canary の波）→ wait
   `no_members`（根拠なしに進めない。7 日で `stalled`。E5 は canary の波を持つ計画を canary の機器の無いグループに作らせない）、割合の波で
   `failed + unresponsive ≥ 3` かつ失敗の割合 **> 2 %**（ちょうど 2 % は止めない。整数の basis point で比べる）→ halt（自分の数の条件で
   小さい標本を防ぐので `min_sample` を待たない）、試みた機器が `min(min_sample, members)` 未満 → wait（止めも進めもしない。標本より
   小さいグループも終われる）、未応答 > 10 % → halt、90 % 以上が終わりの状態で見守りの時間（canary 24 時間、割合 4 時間）が過ぎた → advance、
   ほかは wait（`in_progress` / `soaking`）で、波の開始から 7 日で `stalled`。`failed` は `failed` と `rolled_back` の合計（機器は戻ったが
   リリースは失敗）。数が食い違う（終わりの数 > 試みた数）ときは多い方を試みた数とし、失敗の割合を小さく読まない。境目は表の試験で固定する
   （`canary_single_failure_halts`・`below_min_sample_waits`・`failure_rate_boundary_2_percent`）。halt の理由と数（`Halt`）は E5 が
   `halt_details` に入れ、FG-EDGE `RolloutHalted` を記録する（E5 で登録）。
9. **保守の時間帯**: グループごとの週の繰り返し（曜日・現地の `HH:MM`・1〜1440 分）を 1 つの IANA タイムゾーンで持ち、禁止期間（瞬間の範囲）を
   引く。`install_window(schedule, now)` は `now` を含む窓、無ければ 8 日以内に始まる次の窓を返す。**壁の時計の規則**: 窓は現地の開始から
   現地の終わり（開始 + 長さを現地の時計で足す。23:00 + 240 分は翌日の 03:00）まで。各端は "compatible" で瞬間にする（DST の飛びの中の時刻は
   飛びの分だけ後ろへ、重なりの中の時刻は早い方）。だから窓は、重なり（時計が同じ時間を 2 度示す）のとき以外は壁の時計の長さより長くならず、
   開始が飛びの中で飛びより短い窓はその日は空になる（安全側: ラインの停止は壁の時計で計画される）。重なる・接する窓はつなぐ
   （`window_across_dst_and_midnight`）。`may_start_install(window, now, max_install_seconds)` は残りの時間が足りるときだけ真
   （計画 21 §5.4-3。`can-update` のフックはこの後に機器で聞く）。時刻の計算は **`jiff`**（BurntSushi、Unlicense OR MIT、`cargo deny` の
   許可の範囲）。既定の機能を切り、`tzdb-bundle-always` で IANA の tz database をバイナリに入れる。システムのタイムゾーン・zoneinfo のファイル・
   時計は読まない。サーバー・`gstn`・機器（Windows の開発機を含む）が同じ瞬間を答える。tz database の更新は `jiff` の版の更新で入る。
10. **`gstn edge release verify --release <file> [--keys <file>] [--blobs <dir>]`**（手元だけ）: 上の確かめをし、`--blobs` があれば
    **署名の通ったマニフェストに対してだけ** 各 blob の大きさ（先に比べ、違えばハッシュを計算しない）と SHA-256 を比べる（0004
    `blob_mismatch`・`blob_missing`・`blob_not_a_regular_file`。symlink のディレクトリを通って `--blobs` の外を指す blob も断る）。
    マニフェストの欄（`release_id` など）は署名が通ったときだけ出力に出す（確かめていない値を事実として見せない）。鍵は `--keys` か gstn の設定のディレクトリの `edge-keys.json`。出力は
    `keys: "local"`・`revocation_checked: false` を必ず言う。いちばん重いコード（0004 → 0003 → 0002）で終了コード 1。REST を呼ばないので
    `docs/api/agent-tools.toml` は `[[not_api]]`、`coverage.rs` は `LOCAL_COMMANDS`。
11. **エラーコード（領域 `EDGE`）**: E1 が出すものだけ登録する。計画 21 §5.11 の仮の名前の a〜q は E2 以降が登録する（番号は登録の順）。
    0001 EdgeReleaseDefinitionInvalid（計画の EDGE-c）、0002 EdgeManifestInvalid（新規。計画に無かった、マニフェストの形の誤り）、
    0003 EdgeSigningKeyUnavailable（EDGE-f の手元の側。alert、runbook `edge-signing-key`。サーバーの鍵ファイルの誤りは E4 が別のコードで足す、
    ADR-0032 の 0003 / 0014 と同じ分け方）、0004 EdgeSignatureInvalid（EDGE-m。alert、runbook `edge-agent`）、0005 EdgeRolloutPolicyInvalid
    （新規。波・停止の規則・時間帯の値）。名前は `Edge` で始める（`SigningKeyUnavailable` などの AGENT の名前と重ねない）。どれも merge の
    実行には現れないので `forge-cli` の `retryable()` は `false`。

## Alternatives Considered

### 時間帯を UTC のオフセットだけで持つ（タイムゾーンなし）
- **Pros**: 依存が増えない。インドネシアの工場（WIB、DST なし）はこれで足りる。
- **Cons**: DST のある拠点（計画 21 は 30 工場まで）では年 2 回、窓が 1 時間ずれ、稼働中に入れうる。
- **Why not**: 「稼働中は更新しない」が計画の中心。ずれは安全側に倒れない。

### `chrono` + `chrono-tz`
- **Pros**: 広く使われている。
- **Cons**: 2 つのクレート（`chrono-tz` は build 時に tz database から生成し、`phf` 系を引く）。DST の飛び・重なりの扱いを呼び出し側で書く。
- **Why not**: `jiff` は 1 つの系列（`jiff` と `jiff-tzdb`）で、Temporal と同じ "compatible" の規則と曖昧な時刻の型を持つ。

### POSIX の TZ 規則（`CET-1CEST,M3.5.0,M10.5.0/3`）を自前で解く
- **Pros**: 依存なし。
- **Cons**: IANA 名から規則への変換をどこかで持つ必要があり、過去と未来の規則の変更を自前で追う。
- **Why not**: 検証済みのクレート（基本の約束 4「作る前に探す」）がある。

### 署名の領域を足さず、鍵を分けるだけ
- **Cons**: 鍵の取り違え（同じ種をエッジと部品の両方の鍵ファイルに置く）一つで、部品の署名がマニフェストとして通る。
- **Why not**: 領域は 1 行の費用で、鍵の運用の誤りに頼らない二重の防ぎになる。計画 17 P17-7 も同じ一般化を使える。

### AGENT のコード（0002 / 0003）をそのまま使う
- **Why not**: 改ざんの疑いの runbook と影響の範囲（開発者の PC か工場の機器か）が違う。コードが違えば Claude がログだけで見分けられる。

## Consequences

- `forge-agent` に公開の型と関数が 3 つ増える（`SignatureDomain`・`verify_signature_in`・`sign_digest_in`）。既存の呼び出しは変わらない。
- `gstn` は `forge-edge` を通して `jiff`（tz database 入り、数百 KB）を持つ。`forge-client` の依存の試験は 5 つ。
- ワークスペースに `jiff`（と `jiff-tzdb`）が入る。`cargo deny` / `cargo audit` の対象が 2 つ増える。
- 後の MR の約束: E4 の `forge-fleet` は `lint_release_definition` → 成果物の各ファイルに `is_secret_like_path` → マニフェスト →
  `manifest_digest` → `sign_manifest` の順に同じ関数を使い、E5 は `wave_bucket`・`first_wave`・`decide`・`install_window` を、E6 の
  `forge-edge-agent` は `verify_release`・`may_start_install` を使う。判断は 1 か所のコードになる。
- 停止の判定の数（`HaltPolicy`）と時間帯（`Schedule`）は値の型で、DB の形は E3 / E5 が決める（`HaltPolicy::spec` で保存し `HaltPolicy::new`
  で読み戻す）。
