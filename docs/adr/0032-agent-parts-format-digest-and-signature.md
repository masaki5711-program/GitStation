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

## Amendment（2026-11-23、A3: 表・追記だけの守り・サーバーの鍵ファイル。同日の独立レビューを反映）

1. **表**（マイグレーション `20270103100000`、計画 16 §3）: `agent_signing_keys`・`agent_parts`・`agent_part_versions`・
   `agent_part_files`・`agent_builds`・`agent_lesson_injections`。前の 4 つは公開したものを固める表で、DB 自身が断る（`XF001` →
   FG-DB-0009）: 部品の名前・種類・元のプロジェクトは変わらない、取り下げ（`deprecated_*`）は 1 回だけ。版は `beta`・`published` で
   入り、退役も失効もしていない鍵の署名で、取り下げていない部品にだけ足せる（部品の行は `FOR SHARE` で押さえる。鍵の行は forge_app に
   UPDATE が無いので押さえられないが、同時の退役・失効は後の確かめで閉じる側に倒れる）。後で変えてよいのは beta → stable（公開中だけ）と
   published → revoked（理由・人・時刻つき）だけで、失効した版は最終。`manifest` は 1 MiB まで。ファイルは追記だけで、**版を入れた
   トランザクションの中でしか足せない**（版の `txid` を守りが入れる）。`sha256` は中身の SHA-256、1 版 200 ファイル・1 MiB まで、
   `path` は `.claude/…`・`CLAUDE.md`・`.mcp.json` の相対パス（`.`・`..`・空の区切り・`\`・`:`・制御文字を断る）。どれも消せない。
   forge_app には DELETE が無く、INSERT は呼ぶ側の列だけ（`id`・`created_at`・`txid` は表と守りが決める）、UPDATE は変えてよい列だけ。
   部品・版の変更は監査の行が同じトランザクションに要る（`agent_part`、監査の制約トリガー）。ファイルは版の監査の行で覆う。鍵は次の 2 の関数だけが書き、監査の行も同じ関数が書く（保守の接続の役割では遅延の監査トリガーを満たせないため、表の種類は derived）。
2. **鍵の表は運用者だけが書く**: forge_app は `agent_signing_keys` を読むだけ。登録・退役・失効は SECURITY DEFINER の
   `forge.agent_signing_key_manage(action, kid, public_key, reason, request_id)`（所有者 forge_seq、EXECUTE は forge_maint だけ。
   `forge.feed_new_epoch` と同じ形）で、監査の行（`agent_signing_key.registered`・`.retired`・`.revoked`、`actor_label` = 接続した
   ユーザー）も関数が書く。A5 の `forge admin agent-key` は保守の接続（`database.maint_url`）で呼ぶ。HTTP には出さない。
3. **元のプロジェクトに外部キーを張らない**（`source_project_id`、`ci_pipeline_id`）: 公開した部品は元のプロジェクトの消去より長く残る。
   `agent_builds`・`agent_lesson_injections` はプロジェクトの記録なので、消去で消す。
4. **鍵ファイル `agent.signing_keys_file`**（ADR-0013 の形）: 1 行に `<kid> <64 桁の 16 進数>`（kid は 1〜65535 の 10 進、鍵は
   Ed25519 の 32 バイトの種）。いちばん大きい kid が署名する。DB には公開鍵だけ。読み方は他の秘密のファイルと同じ（絶対パス、
   普通のファイル、64 KiB まで、Unix では所有者だけが読める）に加えて、Linux では所有者が forge の実行ユーザーであること。
   **forge-core は署名しない**: `forge_core::agent_signing::AgentKeyFile` は種から公開鍵を作ってすぐ捨てる（doctor が DB と比べるため）。
   種で署名するのは機能のクレート `forge-marketplace`（A5、契約 9・計画 16 §2.3）だけで、設定を持つだけのクレートが公開の判断を
   飛ばして署名することはできない。誤りは新しいコード **FG-AGENT-0014** SigningKeyFileInvalid（`insecure_permissions`・
   `keys_unreadable`・`keys_invalid`・`signing_key_invalid`、doctor の `key_not_registered`・`key_mismatch`・`key_revoked`・
   `signing_key_retired`）。利用者への文は「管理者に連絡」: FG-AGENT-0003（手元の鍵の一覧の更新を利用者に頼む）とは直す人が違う。
   `forge_agent::sign_digest` は pub だが、clippy.toml の `disallowed-methods` で呼び出しを断る（-D warnings で CI が赤）。許すのは
   A5 の `forge-marketplace` の公開の 1 か所と、合成の種で署名する試験（`#![allow(clippy::disallowed_methods)]`）だけ。
   **受け入れた残る危険**: ring の `Ed25519KeyPair` は drop で秘密を消さない（zeroize しない）。種の写しは `Zeroizing` で消すが、
   鍵の組の内部の展開した秘密はプロセスのメモリに残りうる。ed25519-dalek への切り替えは §4 で断った（新しいクレート）ので
   入れない。緩和: 署名は公開の短い区間だけで鍵の組を作り、すぐ捨てる（A5）。サーバーのメモリを読める者はすでに鍵ファイルも読める。
5. **doctor `agent.signing_keys`**: ファイルの鍵がすべて登録され、公開鍵が合い、失効していないこと、署名する鍵が退役していないこと。
   設定していないホストは `skip`。権限を確かめられない OS（Windows）では合っていても `warn`。`agent.revoked_in_use` とファイルの
   要約の抜き取りは、使う側の記録ができる A7・A9 で足す。

## Amendment（2026-11-26、A4 `feat/auth-agent-marketplace`: 計画 16 §4 の Action と権限の表）

計画 16 §4 の表を `authorize()` に入れた。経路はまだ無い（A5・A7・A10 で足す。それまで forge-web の `PLANNED`）。行は
`crates/forge-auth/src/matrix_rows_agent.rs`（allow と deny の両方）。利用者は不在で「推奨を選ぶ」と言っているので、表に書かれて
いない点は次のとおり決めた。

1. **Action は 10 個**: `AgentPartRead`・`AgentPartList`・`AgentKeyRead`（インスタンス、`read_api`、サインインした全員 —
   人・ボット・エージェントのトークン・サービスアカウント。部品の目録と公開鍵は秘密ではない。サービスアカウントを断る理由が
   無く、種類で断ると「人だけの Action 以外は種類で判定しない」の不変条件に例外が増える）、`AgentPartPublish`・
   `AgentPartPromote`・`AgentPartRevoke`（セッションだけ・人だけ）、`AgentBuilderRecommend`（`read_api`）、
   `AgentBuilderDryRun`・`AgentBuilderCompile`（`api`。表の 1 行の「compile」を `dry_run` の真偽で 2 つの Action に分けた。
   権限が違うので、経路は本文を読んでから 2 つのどちらかで判定する）、`RuleInjectLesson`（`api`）。代理と CI はすべて `Denied`
   （`RuleInjectLesson` を除く）。
2. **リソース `Resource::Agent(AgentResource)`**: `Part(AgentPartView)` は公開のグループでの役（`group_role`）とその見え方
   （`group_sight`、見えないグループは 404）、tag の裏の人（`tag_authors`）、`network` の能力。`Lesson(LessonView)` は注入先の
   プロジェクトと確認済みか。
3. **「tag の作者」** は 1 人ではなく **tag の裏の人の集まり**: tag を押した人（push の監査）、tagger、tag の指すコミットの
   author と committer を、アカウントに当たるものだけ。tagger と author は自己申告で偽れるので、押した人を必ず入れる（A5 の
   ローダーの条件）。呼び手が含まれれば **FG-AGENT-0009** PublishSelfApproval（管理者も、`DeployApprove` と同じ二人の目）。
   取り消しには作者の条件を付けない（作者が自分の版を引っ込めるのは止めない）。
4. **判定の順**: セッションだけ（トークンは `FG-AUTH-0002 session_required`。エージェントのトークンもここで 403。テスト
   `publish_denied_for_agent_token`）→ グループの可視性（404）→ 人だけ（サービスアカウント・ボットのセッション 403）→
   Maintainer → tag の裏の人（`FG-AGENT-0009`、テスト `publish_denied_for_tag_author`）→ `network` は管理者だけ
   （`FG-AUTH-0003 reason=network_capability`）。役の無い人に作者かどうかを教えないよう、役を先に見る。
5. **推薦は `Resource::RepoCopy`**（既定の枝の先の丸ごとの写し）で判定する: 推薦は既定の枝の仕様書を読むので、`RepoRead` の
   写しと同じ規則（エージェントの読み手は先が `Open` と測られたときだけ、他は **FG-AUTH-0022**）。新しいリソースや印を作らない。
6. **`AgentBuilderCompile` は人だけ**（エージェント・サービスアカウント・ボット 403、テスト `compile_mr_denied_for_agent`）。人の
   トークン（`api`）は通す（`gstn agent apply`）。`AgentBuilderDryRun` は何も変えないのでエージェントも通す。
7. **教訓の注入は Kanflow の代理だけ**（ADR-0025 Amendment A4 の `ProposeWithoutAssertion`）。`LessonView` の「代理で来た」印は
   非公開のフィールドで、代理の枝だけが付ける。経路が作ったものは印が無く、本人のセッション・トークン・管理者・サービス
   アカウントは `FG-AUTH-0003 reason=act_as_required`（表の「代理でない → 403」）。本人の役（Developer 以上）→ 確認済み
   （でなければ **FG-AGENT-0010** LessonNotConfirmed、テスト `unconfirmed_lesson_refused`）。
8. **残す点**（A5 への条件）: 計画 16 §6 の `gstn part publish|promote|revoke` はトークンで呼ぶので、この表では断られる。表（人の
   Web セッションだけ）を優先し、`gstn` は公開の画面の URL を出す形にする（A5 で決める）。

### A4 の独立レビュー（2026-11-26、authz-matrix-auditor・security-reviewer）で変えた点

- （security HIGH、直した）二人の目が開いたまま失敗しうる: アカウントに当たらない名前で作ると `tag_authors` が空になる。
  **tag を押した人 `tag_pusher`（push の監査から。自己申告の tagger ではない）を別のフィールドにし、`None` なら断る**
  （`FG-AUTH-0003 reason=tag_pusher_unknown`、閉じる側）。押した人が呼び手でも `FG-AGENT-0009`。ボットや CI のジョブの push は、
  ボットの持ち主・パイプラインを始めた人として記録する（A5 のローダーの条件）。利用者への文は「作った・tag を付けた・push した」。
- （security MEDIUM、直した）教訓の代理で、Kanflow のサービスアカウントがプロジェクトの役を持つことも確かめる
  （`via.role` が無ければ `act_as_not_visible` の 404。読み取りの代理と同じ）。
- （security MEDIUM、A10 への条件）`LessonView` のプロジェクトは代理の事実（`ViaView`）と同じプロジェクトから作る（他の代理の
  経路と同じローダーの約束）。A10 の MR は規則のファイルへの追記だけ（フック・`settings.json`・`.mcp.json` に触れない、計画 16
  の `lesson_text_cannot_touch_settings_json`）、MR に「Kanflow が X の代理で」と残し、X 本人はその MR を承認できない。
- （security MEDIUM、受け入れ）`AgentBuilderCompile` をセッションだけにはしない: 計画 §6 の `gstn agent apply` は人のトークンで
  呼ぶ。エージェントは `is_agent` のトークンで動く約束（ADR-0017 §4.0）で、人のトークンを使われた場合も、作られるのは MR だけで、
  A2 の危険の規則 `agent_config` が自動マージを止め、Maintainer の人の承認と `forge/agent-lock`（A7）が要る。
- （LOW、記録）部品の目録はインスタンス全体に公開（サインインした全員）。公開のグループの可視性は公開・昇格・取り消しだけに効く。
- （LOW、A7 で決める）`GET …/agent-builder`（状態の読み取り）を `AgentBuilderDryRun`（`api`）で判定するか、読み取りの Action を
  分けるか。
- （authz MEDIUM、A10 への条件）`LessonView::new` のプロジェクトは本人の役で作る（サービスアカウントの役を入れない）。A10 で
  ローダーの試験（Reporter のサービスアカウント + Developer の本人 → Developer）を足す。
- 行を足した: 目録と公開鍵のサービスアカウントの allow、昇格の Owner の allow と Guest の deny、取り消しの無効なアカウント、
  Dry-Run と compile の Guest、押した人が自分・押した人が不明、Kanflow のサービスアカウントが役を持たない教訓。

## Amendment（2026-11-26、A5 `feat/core-marketplace-publish`: 公開・昇格・取り消し・`gstn part`）

計画 16 A5 を入れた。新しい機能のクレート **`forge-marketplace`**（契約 9: forge-web / forge-cli → forge-marketplace →
forge-auth / forge-git / forge-db → forge-core → forge-agent）。利用者は不在で「推奨を選ぶ」と言っているので、計画に書かれていない
点は次のとおり決めた。

1. **部品のリポジトリ**は設定 `agent.parts_repository`（既定 `marketplace/parts`、計画 16 §9.4）。公開のグループはそのプロジェクトの
   親のグループ（`AgentPartView::group_role` はそのグループでの呼び手の役）。版の元のプロジェクトがいまの設定と違う版は、昇格・取り消しの
   ローダーで 404（設定を変えても古い部品を別のグループに渡さない）。
2. **公開の経路** `POST /api/v1/agent-parts/{slug}/versions` `{"tag": "<slug>/v<version>"}`。ローダー（`load_for_publish`）が tag を
   git で引き、**そのとき tag が指すコミットを固定**して `AgentPartTarget` に入れる。公開はその固定したコミットの `<slug>/` を読み直す
   （同じコミットなので同じ中身）。公開した版は DB に固まり、後で tag を動かしても効かない。同じ版の再公開は **FG-AGENT-0012**
   （`moved_tag_after_publish_has_no_effect`）。最初の版と違う `kind`・違うプロジェクトからの版も FG-AGENT-0012（`kind_changed`・
   `source_changed`）。
3. **tag を押した人**は push の監査（`repository.pushed` の監査の行で、その tag をその object に更新したもの、いちばん新しい行）の
   `actor_kind = 'user'` の人だけ。行が無い・100 を超える ref の push で載っていない・再生した push・サービスアカウントや代理の push は
   `None` で、A4 の `tag_pusher_unknown` で断る（閉じる側）。A4 の Amendment は「ボット・CI のジョブの push は持ち主・始めた人として
   記録する」としたが、その対応づけは A5 では作らず、より厳しい「公開できない」にした（部品の tag は人が押す）。
4. **tag の裏の人**（`tag_authors`）: 注釈つきの tag の tagger、コミットの author と committer のメールを、大文字・小文字を区別せずに
   アカウントに当てる（当たらないものは除く）。
5. **ローダーで部品の lint が通らない**とき `network` は `false` とし、問題は判定の後に返す（何も公開されないので、能力の印で誰が
   見られるかは決まらない）。公開のときに能力がローダーと違えば内部の誤り（同じコミットなので起きない）。
6. **公開の前の確かめ**（判定の後、署名の前、どれも **FG-AGENT-0004**）: tag のコミットが既定の枝の先から届くこと
   （`not_on_default_branch`: MR を通っていないコミット）、そのコミットの最新のパイプラインが `succeeded` であること（`pipeline_missing`・
   `pipeline_not_green`、`status` つき。push・MR・手動のどれでもよい）。そのパイプラインの id を `ci_pipeline_id` に入れる。
7. **サーバーが決めたフックの試験のジョブ（ダイジェスト固定、ネットワークなし）は A5 では作らない（A5b に分ける）**。パイプラインは
   `.forge-ci.yml` からだけ計画され、ランナーも同じ定義を読むので、サーバーのジョブを足すには forge-pipelines の計画と runner の両方を
   変える（`ci-runner-security-reviewer` の範囲）。それまでの守りは、二人の目（A4）・静的な検査（A1）・既定の枝と緑のパイプライン（6）・
   beta の期間。計画 16 §8 の表の「サーバーが決めたフックのテスト」は A5b が入るまで無い。
8. **署名**: `forge_core::agent_signing::read_signing_seed`（鍵ファイルを `AgentKeyFile::read` と同じに確かめ、いちばん大きい kid の種を
   `Zeroizing` で返す。`Debug` は kid だけ）を forge-marketplace の公開の 1 か所だけが呼ぶ。kid が登録済みで退役も失効もしておらず、
   種の公開鍵が登録した公開鍵と同じでなければ **FG-AGENT-0014**（`key_not_registered`・`signing_key_retired`・`key_revoked`・
   `key_mismatch`。鍵ファイルが無い設定は `not_configured`）。署名した直後に登録した公開鍵で確かめる。`forge-core` は今も署名しない。
   **残す点**: `read_signing_seed` を `clippy.toml` の `disallowed-methods` に足す変更は、エージェントモデルの設定の保護のフックが
   `clippy.toml` の書き換えを止めたため入れていない（人が足す。`sign_digest` の禁止は今までどおり効いている）。
9. **昇格**（beta → stable、`AgentPartPromote`）は公開の監査の行（`agent_part.version_published`）が記録した押した人・裏の人で判定する
   （マイグレーションを足さない。監査の行は追記だけ）。行が無ければ押した人は `None` で断る。すでに stable なら何も書かず `changed: false`。
   **取り消し**（`AgentPartRevoke`）は理由 1〜500 文字（制御文字なし）、最終。取り消した版の昇格・2 回目の取り消しは **FG-AGENT-0006**
   （`revoked`）。どちらも版の行を `FOR UPDATE` で押さえて状態を確かめ、監査の行（`agent_part.version_promoted` / `.version_revoked`）を
   同じトランザクションで書く。役の判定はローダーの時点のもの（トランザクションの中で役を読み直さない。セッションだけの人の操作で、
   役を外された直後の 1 回だけが通りうる）。
10. **読み取り**: `GET /api/v1/agent-parts`（`kind`・`channel`・`q`、部品ごとに取り消していない最新の版）、`…/{slug}`、
    `…/{slug}/versions/{v}`（ファイルの path・mode・sha256・大きさ。中身は返さない。A7 の生成が DB から読む）、
    `GET /api/v1/agent-keys`（`{"keys":[{kid, public_key, created_at, retired_at, revoked_at}]}`、§7 の `agent-keys.json` と同じ形）。
11. **gstn**: `gstn part list|view` と `gstn agent keys`（`--json` がそのまま `agent-keys.json`）。`gstn part publish|promote|revoke` は
    A4 の 8 のとおりトークンでは呼べないので、何も送らずに画面の URL（`/-/marketplace/<slug>`、画面は A8）を出す。
12. **`forge admin agent-key register|retire|revoke --kid N`**（保守の接続、`forge.agent_signing_key_manage`。`register` は
    `agent.signing_keys_file` の kid N の公開鍵を登録する。種は forge-core から出ない）。HTTP には出さない。
13. **設定** `agent.mcp_allowed_hosts`（カンマ区切り、空は全部断る）を公開の `scan` の許可の一覧に使う。

### A5 の独立レビュー（2026-11-26、security-reviewer・authz-matrix-auditor）で変えた点

- （HIGH、直した）公開のローダーが判定の前に git を読んでいた（部品の中身まで）。ローダーは**公開のグループの Maintainer 以上の
  呼び手にだけ** tag と人を git・監査から読み、他の呼び手には git を読まない事実（押した人なし、何も固定しない）を渡して、判定が
  グループ・役で断る。部品は判定の前には読まない: 上の 5 を改め、`network` は公開の手順が固定したコミットの部品を読んだ後に
  **`authorize()` をもう一度呼んで**決める（署名・確かめの前。テスト `a_network_part_is_decided_again_before_signing`）。tree や blob を
  指す tag は 404。
- （MEDIUM、直した）押した人は、その tag をその object に更新した **全部の** push の監査の行から（古い順）。最初の人が `tag_pusher`、
  他の人は `tag_authors` に入る（後から同じ tag を押し直しても最初の人は消えない）。人でない push が 1 つでもあれば `None`（閉じる側）。
  上の 3 を改める。
- （LOW、直した）昇格の二人の目に公開した人も入れる（公開した人だけで stable にしない）。
- （LOW、直した）`gstn part view` は slug と版を API のパスに入れる前に確かめる。
- （MEDIUM、受け入れ・後の MR）tag の裏の人はメール（自己申告）とアカウントの今のメールの一致だけで当てる。MR の作者・承認者・
  push の監査から範囲のコミットを押した人を足すのは A7（MR を作る側）と合わせて行う。
- （MEDIUM、残す）`read_signing_seed` の `clippy.toml` の禁止は 8 のとおり人が足す。
- （MEDIUM、受け入れ）目録の読み取りはインスタンス全体（A4 の LOW のとおり）。
- （LOW、受け入れ）部品のリポジトリの設定を変えると、古い版は昇格・取り消しのローダーで 404 になる（閉じる側。設定を戻すか、
  A9 で管理者の取り消しの道を足す）。

## Amendment（2026-11-27、A6 `feat/core-marketplace-seed-parts`: 部品の種まきの道具と Dry-Run）

計画 16 A6（`marketplace/parts` を作り、root と TMUX 版・Ver2 の部品を移す。アダプター 2 つ）のうち、**GitStation の側**を入れた。
サーバーの `marketplace/parts` プロジェクトを作ること、tag を押すこと、エージェントモデルの 2 つのリポジトリ
（`Agents_skills_Claudecode_TMUX`・`Claudecode_Agent_Ver2.0`）を書き換えることはしていない（本番の変更と他のリポジトリへの push は
人の承認が要る）。利用者は不在で「推奨を選ぶ」と言っているので、計画に書かれていない点は次のとおり決めた。

1. **変換は純粋な関数**（`forge_agent::import`）と、手元だけのコマンド **`gstn part import --from DIR --format marketplace|claude
   [--out DIR] [--only SLUG,…] [--mcp-allowed-hosts …]`**（何も送らない、トークンを読まない）。`--out` が無ければ Dry-Run（報告だけ）。
   `--out` は新しいか空のディレクトリだけで、**全部の部品が通ったときだけ** `<out>/<slug>/part.toml` と中身を書く（全部か無し。
   `marketplace/parts` の並び = A5 の公開が読む `<slug>/`）。空でなければ FG-CLI-0007 `out_not_empty`、元が読めなければ FG-CLI-0009。
   新しいエラーコードは無い（変換の問題は FG-AGENT-0001 の新しい reason: `source_manifest_invalid`・`source_path_invalid`・
   `source_directory_mismatch`・`source_file_missing`・`merge_unsupported`・`mcp_fragment_invalid`・`settings_conflict`・
   `overlay_*`・`slug_collision`・`require_not_in_import` など）。
2. **slug は `<kind>-<id>`**（`hook-kanflow-claim`・`command-kanflow`・`mcp-kanflow`）。W1 では同じ id を 2 つの種類が使う
   （`commands/kanflow` と `mcp_servers/kanflow`）。衝突したときだけ名前を変える形は、後で部品が増えると古い slug が変わりうる
   （公開した slug は不変）ので採らない。`requires` の `<category>/<id>` も同じ規則で slug にする（版の範囲は無し）。
3. **フックの `events`** は部品自身の `.claude/settings.json`（`{"hooks": …}`、決まった並び）にする。静的な検査がすべての
   コマンドを確かめられる（`node <この部品の .js>` の形）。部品どうしの `settings.json` を 1 つにまとめるのはアダプター（A7）。
   **MCP の `merge: "$.mcpServers"`** はそのまま部品の `.mcp.json`（`mcpServers` が 1 つ以上ある JSON だけ）。他の `merge` は断る。
4. **アダプター 2 つ** = 部品 `adapter-claude-code-tmux`・`adapter-claude-code-v2`（`kind = adapter`、1.0.0、ファイル
   `.claude/gitstation/adapter.json`: `schema_version`・`adapter_id`・`outputs`（`settings.json` の `$.hooks` と `.mcp.json` の
   `$.mcpServers` は JSON のマージ、他はコピー）・`overlays`）。W1 の部品の **`overlays` は部品から外してアダプターに移す**
   （署名した部品はどのベースモデルでも同じバイト）。上乗せで中身が変わるファイルは、§6 のとおり A7 の生成したファイルの欄で扱う。
   上乗せの行は検査されずに署名した部品へ足されるので、**決まった一覧の行だけ**を受け付ける（今は `disable-model-invocation: true`
   だけ。力を取り去る行だけで、フック・道具・コマンド・権限を足す行は `overlay_line_invalid`）。A7 は上乗せを当てた後の中身も検査する。
5. **能力は静的な検査から宣言する — 頼まれた種類だけ**: `--declare-from-scan exec,network,fs-write` で名指しした能力だけを、検査が
   `undeclared_child_process`・`undeclared_network`・`undeclared_fs_write` と言うときに `part.toml` に足す（既定は何も足さない）。
   **`permissions` は決して足さない**（`settings.json` の権限を広げる部品は作者が自分で宣言する）。足した能力ごとに、元になった
   検査の結果（reason とファイル）を報告の `inferred_from` に出し、`--out` では報告を `import-report.json` として部品の横に書く
   （部品の MR のレビューで二人が見る。`exec` は読めないコマンド — `node` でないフックのコマンド・`` !` `` — の受け皿でもあるため）。
   `dynamic_code`・許可の一覧に無い MCP の行き先・他の断りは **決して宣言で通さない**。
6. **`description` は捨てる**（`part.toml` v1 に欄が無く、足すと要約の形が変わる）。目録の説明は A8 の画面で決める。
7. **root（エージェントモデル）からは `.claude/` の skills・agents・commands・rules だけ**を 1.0.0 の部品にする。`settings.json`・
   `mistakes/`・`state/`・`worktrees/` はそのモデルの道具で、読まずに `skipped` に出す。root のフック（`scripts/hooks/`）は
   `scripts/lib/` と forge の配置に依存するので A6 では切り出さない（計画 16 §9.6「forge 自身はビルダーの対象にしない」）。

8. **元の読み方**: `src` は区切りごとに `.`・`..`・空・末尾の `.` や空白・`.git` を断り、symlink をたどらず（`part.json` も）、
   開く前に正規化したパスが部品のディレクトリの下にあることを確かめる（`source_path_invalid`）。一覧の後に消えたファイルは
   `source_file_missing`（空の部品にしない）。
### Dry-Run（2026-11-27、`gstn part import --declare-from-scan exec,network,fs-write` を各元に。報告の JSON は残さない — 同じコマンドで作り直せる）

| 元 | 部品 | 通る | 通らない |
|---|---|---|---|
| root `D:/APP/git_station`（`--format claude`） | 66（agent 28・command 12・rule 13・skill 13） | **66** | 0。能力 `exec` の宣言 5（`agent-database-reviewer`・`agent-rust-reviewer`・`agent-typescript-reviewer`・`rule-rust-coding-style`・`skill-rust-testing`: 本文の `` !` ``） |
| Ver2 `marketplace/`（`--format marketplace`） | 24 + アダプター 2 | 11（skill 6・command 1・rule 1・`mcp-gitstation`・アダプター 2） | 15 |
| TMUX `marketplace/` | Ver2 とバイト単位で同じ（報告も同じ） | — | — |

通らない 15 は、どれも **A1 の保守的な検査の想定どおり**（§Consequences「書き直しが要る部品がありうる」）:

- フック 14 の `dynamic_code`: `unicode_escape`（`\u` のエスケープ。ac-trace・gstn-conflict-check・kanflow-client の `lib/kanflow.js`・
  kanflow-setup・mr-conflict-brief・mr-conflict-resolve・session-handoff）、`computed_require`（kanflow-claim・kanflow-mr-comment・
  kanflow-session-tasks: 隣か一つ上の `lib/` を探す `require(変数)`）、`process_access`（kanflow-pbi-check）、`global`
  （kanflow-feedback）、`binding`（gstn-mr-guard）、`constructor`（spec-check の `lib/spec-yaml.js`）・`require_not_called`（spec-check）。
- `mcp-kanflow`: `${KANFLOW_URL:-https://kanflow-url-not-configured.invalid}/api/mcp` の行き先が許可の一覧に無い（FG-AGENT-0013）。

**残す点**（エージェントモデルの側の MR、人の承認と push の後に `--out` で書き、`marketplace/parts` に入れる）: フックの書き直し
（`\u` のエスケープを文字そのものに、`lib/` の探索を文字列のリテラル 2 つの `require` に、`process[…]`・`global`・
`.constructor`・`binding` の書き方を避ける）、`mcp-kanflow` の行き先を社内のホスト名にするか、サーバーの `agent.mcp_allowed_hosts` に
載せる名前にする。`marketplace/parts` のプロジェクトの作成と最初の tag（人が push、A5 の二人の目）は本番の操作で、この MR には無い。

### A6 の独立レビュー（2026-11-27、security-reviewer）で変えた点

- （CRITICAL、直した）上乗せの行は検査されずに A7 で署名した部品へ足される: フックや `allowed-tools` を足す行で、部品もアダプターも
  検査を通りえた。→ 決まった一覧の行だけ（上の 4、テスト `overlay_lines_outside_the_fixed_list_are_refused`）。
- （HIGH、直した）全部の `undeclared_*` を宣言に変えると、`exec` を受け皿に `curl … | sh` のフックのコマンドも「ok」になりえた。
  → 名指しした能力だけ、`permissions` は無し、元の結果を報告と `import-report.json` に（上の 5、テスト
  `inference_is_opt_in_never_permissions_and_keeps_its_evidence`）。
- （MEDIUM、直した）`src` の末尾の `.`・空白（Windows が名前を変える）と、開く前の正規化したパスの確かめ（上の 8、テスト
  `source_segments_windows_renames_are_refused`）。
- （LOW、直した）`part.json` も symlink をたどらない。一覧の後に消えたファイルを空の部品にしない。
