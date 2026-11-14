# ADR-0032: エージェントの部品の形、要約、署名と手元の確かめ（`forge-agent`）

**Date**: 2026-11-13
**Status**: proposed（accepted は A1 `feat/core-agent-part-format` の `rust-reviewer`・`security-reviewer` のレビューで CRITICAL / HIGH が無いとき）
**Deciders**: Masaki Fujikawa（ユーザー、2026-11-11「推奨で進めて」: 計画 16 §9 の 2〜7。2026-11-12: §9 の 1）、Claude

## Context

計画 16（W15）は、マーケットプレイスの部品（スキル・エージェント・規則・フック・コマンド・MCP・アダプター）から、プロジェクトのリポジトリに
Claude Code の環境（`.claude/`・`CLAUDE.md`・`.mcp.json`）を MR で作り、`.gitstation/agent.lock.json` で版を固定する。フックは開発者の PC で
`node` として動くので、部品 1 つの悪意が使う人全員の PC でのコードの実行になる。計画 16 §2 で次を決めた（議論し直さない）:

- 部品は git で作り、公開でサーバーが DB に固める（案 C）。`part.toml`（`slug`・`kind`・`version`・`requires`・`capabilities`・`files`）と中身。
- 署名はサーバーの Ed25519 の鍵で、決まった並びの要約に（案 S2）。確かめる場所は 3 つ: 生成のとき、必須のチェック `forge/agent-lock`、`gstn agent verify`。
- 純粋なクレート `forge-agent`（I/O なし、`gstn` から使う）と機能のクレート `forge-marketplace`（DB・git・MR・鍵）。契約 9 はエージェントモデルの
  W15-0（ルートの `chore/harness-w15-agent-supply-chain`）で先に変えた: `forge-core → forge-agent → forge-ci → forge-doc → forge-errors`、
  `gstn` が依存してよいクレートに `forge-agent`。

この ADR は A1（形・要約・署名・静的な検査・`gstn agent verify`）の決定を記す。DB（A3）、権限（A4）、公開（A5）、生成と `forge/agent-lock`（A7）は後の MR。

## Decision

1. **クレート `forge-agent`（純粋）**: 依存は `forge-errors` と計算だけのクレート（`serde`・`serde_json`・`toml`・`sha2`・`url`・`base64`・`ring`）。
   ファイル・ネットワーク・時計・DB・非同期の実行時を持たない（`tests/purity.rs`、`forge-hosting/tests/dependency_direction.rs`）。読むのは呼び出し側
   （`gstn`、後の `forge-marketplace`）。
2. **置き場所**: 部品のファイルは `.claude/` の下、または `CLAUDE.md`・`.mcp.json`（mcp の部品だけ）。`..`・`.`・空の区切り・`.git`（大小を問わない）・
   絶対パス・ドライブ文字・`\`・`:`・制御文字・末尾の `.` や空白（Windows で別の名前になる）・255 バイト超を断る。`.claude/settings.local.json` と
   `.claude/worktrees/` は人ごとのファイルなので部品にできない。symlink は断る。`.claude/hooks/` のスクリプトは Node.js（`.js`・`.cjs`・`.mjs`）だけ
   （計画 09 §3.1）。1 版 200 ファイル・1 MiB まで（計画 16 §3）。`part.toml` の知らない鍵は断る（黙って捨てない）。誤りは FG-AGENT-0001。
3. **要約**: SHA-256（`"forge-agent-digest-v1\n"` ‖ slug ‖ kind ‖ version ‖ requires（並べて重複なし）‖ capabilities（同）‖ ファイルごとの
   path と内容の SHA-256（path のバイト順））。可変長の欄は 4 バイトの長さを前に付ける（境目のずらしで別の入力にできない）。ファイルを並べた順・読んだ順に
   よらず（`digest_is_independent_of_file_order`）、どのファイルの 1 バイト・名前・宣言の変化でも変わる。表示は `sha256:<hex>`。モード（実行ビット）は
   v1 では入れない（Windows の作業ツリーで確かめられない）。
4. **署名**: Ed25519（RFC 8032）。署名する文は `"forge-agent-part-signature-v1\0"` ‖ 要約の 32 バイト（同じ鍵の他の署名に流用させない）。確かめは
   `ring` の `ED25519`（正規でない `S`・復号できない公開鍵を断る）。新しいクレート（`ed25519-dalek`）は入れず、`rustls`・`forge-oidc` で既に
   `cargo deny` を通った `ring` を使う。鍵は `kid` で引く: 知らない `kid` は FG-AGENT-0003（手元の鍵が古いか、偽の kid）、失効した鍵は FG-AGENT-0002
   `key_revoked`、合わない署名は FG-AGENT-0002 `bad_signature`。「署名が無いので通す」の逃げ道は作らない。退役した鍵（`retired_at`）は過去の署名を
   確かめられる（新しく署名しないだけ）。秘密の鍵の種（32 バイト）から署名する関数も置く（A5 の公開と試験用。種は呼び出し側から出ない）。
5. **静的な検査**（保守的。見通せないものは信じずに断る）:
   - スクリプト（`.js`・`.cjs`・`.mjs`）は **生のソース**（コメントも含む）を見る。コメントを取り除く字句解析は、正規表現のリテラル（`/'/`）で
     ずらされて本物のコードを「コメント」として隠しうるため（`scan_flags_undeclared_child_process` の 1 例）。
   - `child_process`・`cluster` → 能力 `exec` が要る。`http`・`https`・`http2`・`net`・`tls`・`dgram`・`dns`・`undici`・`fetch(`・`WebSocket`・
     `XMLHttpRequest` → `network`。`settings.json` の `permissions` → `permissions`、`hooks` の `command` が `node` でない → `exec`。宣言が無ければ FG-AGENT-0005。
   - 見通せないコード（`dynamic_code`）は **能力を宣言しても断る**: 文字列のリテラル 1 つでない `require`（連結・変数・エスケープ）、`require` を値として
     渡す、`import(`、`eval(`、`Function(`、`.constructor`、`process.binding`・`_linkedBinding`・`dlopen`、`process[`・`globalThis[`・`global[`、
     `Reflect.`、`mainModule`、`createRequire`、モジュール `vm`・`worker_threads`・`module`・`inspector`・`v8`・`wasi`。
   - `.mcp.json`: `command` のあるサーバー → `exec`、`url` → `network`。サーバーの定義の **どこにある** `http(s)://`・`ws(s)://` の URL も（`url`・`args`・
     `env`）、ホストが許可の一覧（完全一致か、`.suffix` でその下のサブドメインだけ）に無ければ FG-AGENT-0013（契約 8）。一覧が空なら全部断る。
   - これで全部の抜け道は塞げない（間接の参照は無数にある）。静的な検査は層の 1 つで、二人の目・サーバーが決めたフックの試験・beta の期間（計画 16 §8）と
     合わせて守る。
6. **lock（`.gitstation/agent.lock.json`、`schema_version: 1`）**: 計画 09 §4 の欄（`project_id`・`project_type`・`linked_specs`）に、部品ごとの
   `slug`・`kind`・`version`・`requires`・`capabilities`・`digest`・`key_id`・`signature`・`selected_by`・`reason`・`files`（`path`・`sha256`）と、
   手で足したファイル `custom`（`path`・`sha256`・`approved_by`）。計画 09 の種類ごとの組（skills / sub_agents / …）は `parts` 1 つの列に `kind` を
   持たせる形に置き換える（アダプターも `kind = adapter` の部品）。知らない鍵は断る。確かめる鎖: 作業ツリーのファイルの SHA-256 = lock の値 →
   lock の欄から作り直した要約 = lock の `digest` → その要約の署名が `key_id` の鍵で通る。管理するファイル（`.claude/**`・`CLAUDE.md`・`.mcp.json`、
   人ごとのファイルを除く）は、部品のファイルか `custom` のどちらかでなければ `unmanaged_file`。不一致は FG-AGENT-0007（`file_changed`・`file_missing`・
   `unmanaged_file`・`duplicate_path`・`lock_missing`・`lock_invalid`）。フックだけを書き換えれば 0007、lock の値も書き換えれば 0002 `digest_mismatch`、
   要約まで作り直せば 0002 `bad_signature`。アダプターが中身を変えて生成するファイル（A7）は、lock に生成したファイルの欄を足して扱う（A7 の Amendment）。
   `custom` の承認は MR の上でサーバーが確かめる（A7）。
7. **`gstn agent verify`**（手元だけ、A1）: リポジトリ（上へ探す）の管理するファイルを `git ls-files --cached --others --exclude-standard` で挙げ、
   ディスクから読む（Claude Code が実際に動かすもの。symlink はたどらず `not_a_regular_file`）。鍵は `--keys` か gstn の設定のディレクトリの
   `agent-keys.json`（`GET /api/v1/agent-keys` と同じ形 `{"keys":[{"kid","public_key","created_at","retired_at","revoked_at"}]}`）。
   出力は「手元の鍵で確かめた・取り消しは確かめていない（オフライン）」と必ず言う（`keys: "local"`・`revocation_checked: false`）。サーバーの鍵と
   取り消しの一覧は A5 で足す。`--part <dir>` は部品のディレクトリ 1 つの lint・検査（`--mcp-allowed-hosts`）・要約。問題は全部挙げ、いちばん重い
   コード（0002 → 0003 → 0005 → 0013 → 0007 → 0001）で終了コード 1。リポジトリの中のフックには確かめさせない（自分を確かめる仕組みを、確かめられる
   側に置かない）。
8. **エラーコード（領域 `AGENT`）**: 計画 16 §5 の番号のまま、A1 が出すものだけ登録する: 0001 PartManifestInvalid、0002 PartSignatureInvalid
   （alert、runbook `agent-supply-chain`）、0003 SigningKeyUnavailable（alert、runbook `agent-signing-key`）、0005 PartCapabilityUndeclared、
   0007 AgentLockMismatch、0013 McpEndpointNotAllowed。空いている番号（0004、0006、0008〜0012）は最初に出す MR（A3〜A10）が登録する。

## Alternatives Considered

### `ed25519-dalek`（`verify_strict`）
- **Pros**: 小さい順序の公開鍵も断る `verify_strict` がある。
- **Cons**: 新しいクレートの系列（`curve25519-dalek` など）。`cargo deny` と監査の対象が増え、メモリの少ないこの機械でのビルドも重くなる。
- **Why not**: 署名するのはサーバーの鍵だけで、公開鍵は管理者が配る。攻撃者が公開鍵を選べないので、`ring` の RFC 8032 の確かめで足りる。

### コメントを取り除いてから検査する
- **Pros**: コメントに `eval(` と書いただけの部品を断らない。
- **Cons**: JavaScript の字句解析を正しく作るのは正規表現のリテラルのため難しく、ずれると本物のコードを隠す（安全側に倒れない）。
- **Why not**: 誤って断るのは作者がコメントを書き直せば済む。誤って通すのは全員の PC での実行になる。

### 要約に `part.toml` のバイト列をそのまま入れる
- **Why not**: 書き方（空白・並び）が違うだけで要約が変わり、lock から作り直せない。欄を決まった形で入れる。

## Consequences

- `gstn` は `forge-agent` を通して `ring` を直接使う（`rustls` 経由で既に入っていた）。`forge-client` の依存の試験は 4 つ（`forge-errors`・`forge-doc`・
  `forge-ci`・`forge-agent`）。
- 静的な検査は保守的で、コメントに禁じた書き方の名前があるだけでも断る。A6 で root と TMUX 版・Ver2 の部品を移すとき、書き直しが要る部品がありうる。
- lock の形は v1。生成したファイル（A7）、取り消しの確かめ（A5 の API）、サーバーの許可の一覧（`agent.mcp_allowed_hosts`、A3 の設定）は後の MR で足す。
- 後の MR の約束: `forge-marketplace` は公開で `parse_part` → `scan` → `sign_digest` の順に同じ関数を使い、`forge/agent-lock`（A7）は `verify_lock` を使う。
  3 か所の確かめは同じコードになる。

## Amendment（2026-11-14、A1 の security-reviewer の指摘 HIGH 7 件・MEDIUM 2 件・LOW 1 件）

決定 2・5・6・7 を次のとおり強める（それぞれに試験 `finding1_*`〜`finding7_*`・`medium_*`、`crates/forge-client/tests/agent.rs` の `finding2_*`・`finding3_*`）。

1. **大文字・小文字**: Windows と macOS は `.Claude/settings.json`・`CLAUDE.MD`・`.MCP.json` を本物の設定として開く。管理する範囲は大文字・小文字を
   区別せずに判定する（`.claude` そのものも含む）。部品の置き場所は ASCII だけ・決まった綴りだけ（Unicode の正規化で 2 つの名前が 1 つのファイルに
   なる余地を無くす）。部品の中と lock の中で大文字・小文字だけが違う 2 つのパスは `duplicate_file` / `duplicate_path`、作業ツリーでは `case_collision`。
2. **人ごとのファイル**: Claude Code は `.claude/settings.local.json` からもフックを読む。除くのは **git が追跡していない** ときだけ。追跡していれば
   他のファイルと同じく管理し、lock に無いので `unmanaged_file`。
3. **`.claude` がディレクトリでない**（symlink、symlink を切った checkout で git が書くテキストのファイル）: `not_a_regular_file` で失敗する。
   「設定なし」にしない。
4. **フックのコマンド**: `exec` なしで許すのは **`node <この部品の .claude/… の .js|.cjs|.mjs>`** の形だけ（`"$CLAUDE_PROJECT_DIR"/` は可）。
   空白 1 つ、フラグなし、シェルの記号なし。`node x.js; curl …`・`node -e`・`node --require` は `exec` が要る。部品に無いスクリプトを指す `node` は
   `hook_target_not_in_part` で断る。
5. **settings.json は許可の一覧**（`$schema`・`hooks`・`permissions`・`model`・`includeCoAuthoredBy`・`cleanupPeriodDays`・`outputStyle`）。他の鍵
   （`statusLine`・`apiKeyHelper`・`env` など）は `settings_key_not_allowed`。Markdown の front matter の `hooks:` のコマンドは 4 と同じに検査し、
   スラッシュコマンドの `` !` `` は `exec` が要る。他の JSON の `command` も 4 と同じ。
6. **`custom`**: 署名も検査も無いので、コードを動かしうる `custom` のファイル（コマンドの無い Markdown 以外）は `custom_unverified`（FG-AGENT-0007、
   終了コード 1）。A7 で承認を確かめられるようになるまで「確かめた」と言わない。
7. **スクリプトの検査**: Markdown と JSON 以外の **全部のファイル** をスクリプトとして検査する（node は拡張子を問わず動かす）。生のソースを
   **識別子の列** として読み、空白やコメントを挟んでも同じ（`eval (`、`import/**/(`）。`\u` のエスケープはどこでも断る。`getBuiltinModule`・
   `globalThis`・`global`、`.` が続かない `process`、呼び出しの `import` を断る。

MEDIUM: MCP の行き先は、どのスキームの `scheme://`・`//host`・素のホスト名（ファイル名の拡張子でない）・IPv4 アドレスも許可の一覧で確かめる。
能力 `fs-write` を足す（ファイルの書き込み・削除・名前の変更。書き先が `.claude/` の中かを静的に確かめられないため、使うだけで宣言が要る）。

LOW: `core.autocrlf=true` の checkout は LF を CRLF にするので、全部のファイルが `file_changed` になる（安全側に失敗する）。FG-AGENT-0007 の
diagnosis / fix に書いた。
