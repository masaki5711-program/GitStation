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

## Amendment（2026-11-27、A7 `feat/core-marketplace-compile`: 生成・Dry-Run・ボットのコミットの MR・規則だけの推薦・`forge/agent-lock`）

計画 16 A7 を入れた。利用者は不在で「推奨を選ぶ」と言っているので、計画に書かれていない点は次のとおり決めた。

1. **生成は純粋な `forge_agent::compile`**（`compile_is_deterministic`: 選んだ順によらず同じバイト列）。最初に部品ごとに確かめる（生成のときの
   確かめ、3 か所の 1 つ目）: `part.toml` の欄の lint、ファイルの中身から作り直した要約が DB の要約と同じ（違えば 0002 `digest_mismatch`。
   DB の行を書き換えても通らない）、署名が DB の鍵で通る（0002 / 0003）。次に選び方: slug は 1 回、`adapter`（ベースモデル）はちょうど 1 つ
   （`adapter_missing` / `adapter_multiple`）、`requires` の `slug@range` は選んだ版で満たす（`requirement_unmet`）。範囲は `^`・`~`・`=`・
   `>=`・`>`・`<=`・`<`・`*`・素の版（`^` と同じ）を `,` か空白で並べる。pre-release は `=` で名指ししたときだけ当たる。誤りは新しいコード
   **FG-AGENT-0008** SelectionUnresolved（422）。
2. **同じパスを 2 つの部品が書く**のは `.claude/settings.json` と `.mcp.json` だけ: slug の順に JSON のオブジェクトを深くまとめる（鍵は和、
   配列はつなげて同じ要素は 1 つ、違うスカラーは `merge_conflict`）。他のパスは `path_conflict`（`custom` のパスとも）。
   **決定 6 の「生成したファイルの欄」は作らない**: 代わりに、まとめたファイルの元の部品ごとのファイルの欄に、その部品の文 `content` を lock に
   入れる。`verify_lock` は `content` の SHA-256 が署名した要約の固定する値と同じこと（違えば 0002 `merge_source_mismatch`）を確かめ、
   まとめ直した結果と作業ツリーのファイルを比べる（`file_changed`）。こうすると `gstn agent verify`・`forge/agent-lock`・生成の 3 か所が
   署名したバイト列だけから同じ判定をし、署名する場所（鍵の種を読む所）を増やさない。lock は `schema_version: 1` のまま（`content` は
   任意の欄。古い gstn は知らない鍵として閉じる側に倒れる）。
3. **アダプター**は `kind = "adapter"` の部品で、中身はベースモデルのファイル（`CLAUDE.md`・`.claude/settings.json` など）、`requires` が
   ベースモデルに要る部品。計画 09 の `target-adapter.yaml`（出力の場所と形式）は A7 では作らない: 出力は「コピー」と「JSON のまとめ」の
   2 つで足り、置き場所は部品のパスそのもの。A6 のアダプター 2 つ（`claude-code-v2`・`claude-code-tmux`）はこの形で作る。
4. **推薦は規則だけ**（`forge_agent::recommend`、`source: "rules"`）: 既定の枝の lock のアダプター（無ければ `claude-code-v2`、
   `reason=base_model`）、lock の部品（`current_lock`、安定版が無いものは `unavailable` に出す。黙って落とさない）、それらの `requires` を
   たどったもの（`required_by=<slug>`）。どれも取り消していない最新の stable。`project_type`（ADR-0023 で種類は廃止）と仕様書のタグは、
   部品の側に合わせる欄が無いので使わない（A11 の LLM の推薦と合わせて決める）。
5. **経路**（GitStation の API に合わせて `projects/{id}` の下。計画の `repos/{id}` から変えた）:
   `GET /api/v1/projects/{id}/agent-builder`（ビルドの記録、`AgentBuilderDryRun`。A4 の LOW「状態の読み取りの Action」はこれに決めた）、
   `POST …/agent-builder/recommend`（`AgentBuilderRecommend`。登録の Action は `RepoRead` で、経路の中で既定の先端の丸ごとの写しとして
   判定する: `forge_auth::authorize_agent_recommend`、`DECIDED_IN_ROUTE`）、`POST …/agent-builder/preview`（Dry-Run、`AgentBuilderDryRun`）、
   `POST …/agent-builder/compile`（`AgentBuilderCompile`、人だけ）。計画の `compile {dry_run}` は **2 つの経路に分けた**（Action が違うので、
   本文を読んでから判定を選ぶ仕組みを作らない）。`forge.agent_builds` に記録するのはビルドと失敗した Dry-Run だけ（通った Dry-Run は何も変えないので
   記録しない。エージェントのトークンでも呼べるので、表を増やさない）。
6. **ボットのコミットの MR**: 作者・コミッターは `forge-agent-builder <agent-builder@forge.invalid>`、push の記録は頼んだ人
   （`repository.pushed` の監査・イベント。パイプライン・MR の版はいつもの push と同じに続く）。枝は `agent-builder/<lock の要約の 16 進 12 桁>`
   （既定の枝の先端から）。ref の更新は頼んだ人の push と同じに判定（`authorize_ref_updates`、保護ブランチの規則）、書くファイルは全部
   push の秘密の規則（FG-GIT-0022）、同じ名前の枝の先端がこのビルドの lock とファイルをそのまま持つなら、コミットを作らずに MR を開くことだけをやり直す（要求が
   コミットの後で切れた・MR を開けなかったときの再開。push の記録は最初の要求のもの）。違う中身の枝なら FG-GIT-0027 `branch_exists`、何も
   変わらなければ FG-GIT-0031。MR を開けなかったビルドは `failed` と記録する。1 回のコミットで 500 パスを超えるビルドは FG-AGENT-0008 `too_many_files`。MR の作成者は
   頼んだ人（`MergeRequestCreate`）。ビルドは管理する範囲を持つ: 先端の管理するファイルで出力に無いもの（手で足したもの、symlink）は消す
   （Dry-Run に `removed` と出る）。先端の lock の `custom` で中身が同じものは残す。
7. **`forge/agent-lock`**（3 か所の 2 つ目）は新しいジョブを作らず、`check.risk` のジョブが `forge/risk` と同じ head に書く
   （`forge_checks::agent_lock`）。管理する範囲（ルートの、大文字・小文字を畳んだ `.claude`・`CLAUDE.md`・`.mcp.json` と
   `.gitstation/agent.lock.json`。`forge_git::browse::Browser::agent_config`、symlink・submodule は `not_a_regular_file`）を読み、
   `verify_lock` と、lock の版が DB に公開されていて要約が同じで取り消していないこと（0002 `not_published` / `digest_mismatch`、0006 `revoked`）。
   管理するファイルも lock も無い head は `success`。読み切れない（2000 ファイル・8 MiB 超、一覧の打ち切り）は `failure`。
8. **`agent_config` の例外**（ADR-0017 Amendment A7）: `forge/risk` の `agent_config` のうち、管理するパスか lock のパスのものは、その commit の
   `forge/agent-lock` が通るとき新しい規則 **`agent_config_locked`**（warn）になる（マージ結果の判定でも、その結果の commit で確かめる）。
   それ以外（`.gitstation/**` の他のファイル、入れ子の `CLAUDE.md`、`.claude` へのリンク、畳むと `.claude` になる名前）は `stop` のまま。
9. **`custom` の承認は作らない**: コードを動かしうる `custom` は今までどおり `custom_unverified`（サーバーでも失敗）。

### A7 の独立レビュー（2026-11-27、security-reviewer・rust-reviewer）で変えた点

- （security CRITICAL、直した）自動マージが Maintainer を待つかを `forge/risk` の要約の行で読むので、発見の上限（1 つの target に 1000）や
  要約の大きさ（64 KiB）で `agent_config_locked` の行が切られると待たずに入りえた。`sort_findings` で `agent_config_locked` を warn の
  先頭に並べ（stop の次）、要約が切られている（`…` で終わる）ときも Maintainer を待つ（閉じる側）。
- （security HIGH、直した）lock の `custom`（署名も公開もしていない手のファイル）がプレーンな Markdown なら `forge/agent-lock` は通るので、
  例外がそれにも効いていた。**`custom` を 1 つでも持つ lock では `agent_config` を `stop` のまま**にする（`forge/agent-lock` は緑のまま。
  承認の確かめは後の MR）。テスト `a_lock_with_hand_added_custom_files_keeps_the_stop`。
- （rust HIGH 2 件、直した）コミットの後で要求が切れた・MR を開けなかったビルドは、同じ選択でもう一度頼むと再開する（6 の枝の再利用）。
  MR を開く呼び出しに時間の上限。`.gitstation` の一覧の打ち切りを `tree_unreadable` に。500 パス超は FG-AGENT-0008 `too_many_files`。
  通った Dry-Run は記録しない。
- （security MEDIUM・rust LOW、残す）Markdown の front matter の `allowed-tools`・`permissionMode` は静的な検査が見ていない（`custom` は
  上のとおり例外にしないので、A7 では影響なし）。`recommend` は版を戻ってやり直さない（選んだ版が後の範囲に合わなければ
  `requirement_unmet`）。

### 残すこと

- 自動マージの Maintainer の承認は、キューに入れるときに読む（着地の判定はマージ結果の危険と lock を確かめ直すが、承認は読み直さない）。
- `gstn agent recommend|preview|apply|status`（計画 §6）と画面は A8。更新・世代の移行・取り消しの更新 MR は A9。
- ADR-0017 Amendment 2026-11-12 の残り: `CLAUDE.md` の `@` が `.claude` の外のファイルを取り込む変更は、まだ `agent_config` に当たらない。
- A5 の MEDIUM（tag の裏の人に MR の作成者・承認者を足す）はこの MR では入れていない。

## Amendment（2026-11-27、A8 `feat/ui-marketplace-builder`: 計画 16 §6 の画面）

新しい API は足さない（A5 の `agent-parts`、A7 の `…/agent-builder` と、既存の project・namespace の解決・group・refs・blob の読みだけ）。
サーバーに足したのは SPA の殻の 2 経路（`/-/marketplace`、`/-/marketplace/{slug}`、`Action::WebShell`）だけ。

1. **`/-/marketplace`**: 種類のタブ（`?kind=`、リンクと `aria-current`）、チャンネル（`?channel=`）、名前の検索（`?q=`）。
   部品ごとに最新の生きた版・beta / stable・**能力の印**（「コマンドを実行する」「ネットワークを使う」「権限の設定を広げる」
   「ファイルを書き込む」）。印は「能力」という名前のリストの項目で、文字そのものが読み上げられる（色とアイコンは繰り返しだけ）。
   「マイワーク」のサイドバーに「マーケットプレイス」。
2. **部品の詳細**（`/-/marketplace/{slug}`）: すべての版（取り消しは「取り消し済み」と理由）、最新の生きた版のファイル・タグ・要約・鍵。
3. **ボタンを出す人**は API が出している役割だけで決める（推測しない。サーバーがもう一度決める）。
   - 公開・昇格・取り消し: 部品のプロジェクト（`source_project_id`）の親グループの役割（`GET /api/v1/groups/{id}` の `role`）が
     Maintainer 以上か管理者。`network` を宣言した版の昇格は管理者だけ（印と一行の説明）。
   - タグを打った人・書いた人の拒否（FG-AGENT-0009）は画面では分からないので、ボタンは出し、サーバーの `user_message` と
     問い合わせ番号を出す。
   - プロジェクトの「エージェント」: 推薦は Reporter 以上（読むだけ）、Dry-Run・組み立ての履歴・「MR を作る」は Developer 以上
     （web のセッションは人なので compile の「人だけ」も満たす）。Reporter 未満には誰ができるかの一行だけで、何も読まない。
4. **プロジェクトの設定 →「エージェント」**は設定の下の自分のページ（`/{project}/-/settings/agent`）。設定の本体には説明とリンクだけを置き、
   部品の一覧・lock・推薦は開いたときだけ読む。アダプターは 1 つを選ぶラジオ、部品はチェックボックスで、推薦のものは理由
   （基本のモデル・いまの lock・`<slug>` が必要とする）とともに最初から選ぶ。「MR を作る」は**同じ選択の Dry-Run を見た後だけ**出す
   （選択を変えると Dry-Run を捨てる）。Dry-Run は追加・変更・削除を文字で、`permissions` を持つ部品は「権限の設定を広げます」の行で示す。
5. **更新の帯**: 既定のブランチの `.gitstation/agent.lock.json` の版と、推薦（lock の部品の最新の stable）の版が違う部品を
   「更新があります」に並べ、次に押すもの（Dry-Run → MR）を一行で言う。lock が無い・読めないときは出さない。
6. **公開のダイアログ**: 公開の前にタグのパイプラインを読む API は無いので、前はいまの最新版の能力、後は公開された版の CI
   パイプライン（公開はその成功が条件、リンク付き）と前の版との能力の差分（増えた・なくなった）。拒否（FG-AGENT-0004 など）は
   `user_message` と問い合わせ番号。

### 残すこと

- 公開の前に CI の結果と能力の差分を見せる読み（`GET …/versions/preview?tag=` のような）は足していない。足すなら authorize() と
  マトリクスの allow・deny の行とともに。
- `…/compile` の `merge_request_error` には問い合わせ番号が無い（メッセージだけ）。履歴の表がその組み立ての問い合わせ番号を出す。
- 取り消された版を lock が使っているときの帯と更新の MR は A9。

## Amendment（2026-11-27、A9 `feat/core-marketplace-updates`: 更新・世代の移行の MR、取り消しで更新の MR、通知）

計画 16 A9 を入れた。利用者は不在で「推奨を選ぶ」と言っているので、計画に書かれていない点は次のとおり決めた。

1. **更新の選び方**（`forge_marketplace::updates::update_selection`、純粋）: 既定の枝の先端の lock の各部品を、取り消していない最新の
   stable へ（A7 の規則だけの推薦 `recommend` をそのまま使う。ベースモデルと `requires` も同じにたどる）。**下げない**: lock の版が最新の
   stable より新しく、まだ公開中なら残す。stable が無い部品（人が選んだ beta）も公開中なら残す。取り消した版で置き換えが無いものは**外す**
   （その部品を `requires` する部品があれば生成が FG-AGENT-0008 で止まる。半端な更新にしない）。何も変わらなければ新しいコード
   **FG-AGENT-0015** AgentUpdateNotNeeded（`no_lock`・`up_to_date`・ジョブの `not_in_lock`）。生成は A7 の `plan_build`（要約・署名・選び方・
   まとめ）をそのまま使う。
2. **世代の移行** = ベースモデルの付け替え: `POST …/update {"adapter": "<adapter の slug>"}`。lock の前の adapter は残さず、新しい adapter の
   `requires` をたどる（計画 09 の `target-adapter.yaml` は A7 と同じく作らない）。
3. **経路**（`projects/{id}` の下、既存の Action を使う。新しい Action は無い）: `GET …/agent-builder/updates`（`AgentBuilderDryRun`: 部品ごとの
   lock の版・取り消し・次の版と Dry-Run。無いときは `available: false` と `reason`）、`POST …/agent-builder/update`（`AgentBuilderCompile`、
   人だけ。その場で計画して 409 / 422 を返し、ジョブを入れて **202**。同じ人の同じ lock の要約の更新はジョブの dedup
   `agent_update:<project>:<user>:<digest>` で 1 つ）。gstn は `gstn agent update --project N [--adapter SLUG] [--dry-run]`（`--dry-run` は
   GET で何も入れない。`--adapter` との組み合わせは手元で断る）。
4. **ジョブ** `agent.update_merge_request`（`forge serve` のシステムの下書きの runner。revert と AI 評価の基準の候補と同じ所で、枝の書き込みが
   閲覧のキャッシュを無効にする）。ジョブは走るときに計画し直す（202 の答えと違いうる。答えの `preview` は「今の計画」）。上限 300 秒、
   失敗は `serve_merges::retryable` で再試行か終わり（終わりはジョブの runner が 1 回ログに出す）。
5. **取り消しの更新は取り消しと同じトランザクション**（Outbox と同じ考え: 取り消しが確定したときだけジョブがある）。対象は
   `forge.agent_builds` で、その `slug@version` を選んだ、終わった（Dry-Run でない）ビルドのあるプロジェクト（`selection @>`、部分 GIN の
   索引。マイグレーション `20270113100000` で `agent_builds.origin`（`build`・`update`・`revocation`）と一緒に足した）。ジョブは先端の
   lock がまだその版を使うかを確かめる（使っていなければ `not_in_lock` で何もしない）。dedup
   `agent_update_revoked:<version_id>:<project_id>`、取り消しは最終なので 2 回目の取り消しは無い（`revoke_opens_update_mr_once`）。
   lock を手で作ったプロジェクト（ビルドの記録が無い）は対象に入らない — `forge/agent-lock` が FG-AGENT-0006 で失敗するので見える。
6. **誰の名前で開くか**: MR はいつも人のもの。頼んだ人（`update`）か、取り消しなら **そのプロジェクトの終わったビルドを頼んだ人を新しい順に
   （最大 5 人）、最初に許される人**。
   ジョブはその人の今の権限で、git が届く物を書く前に判定する: `AgentBuilderCompile`（人・Developer）、枝の作成をその人の push として
   （`authorize_ref_updates`、保護ブランチの規則）、`MergeRequestCreate`（開くトランザクションの中でもう一度。生きたセッションが無いので
   管理者の印は `standing_admin`）。資格は `Credential::Session`（AI 評価の基準の候補のスケジュールの持ち主と同じ形）。誰も許されなければ
   新しいコード **FG-AGENT-0016** AgentUpdateNotOpened（`no_author`・`author_not_allowed` と `denied_code`・`closed_by_person`、
   **alert**、runbook `agent-supply-chain`）でジョブが終わる（黙らない。`revocation_update_without_an_allowed_author_is_not_silent`）。
7. **1 回だけ**: 枝は A7 と同じ `agent-builder/<lock の要約の 16 進 12 桁>`。その枝から開いている MR があれば何も作らない（`AlreadyOpen`）、
   先端がこのビルドそのものの枝は使い直す（中断の再開）、違う中身の枝は FG-GIT-0027 `branch_exists`、人が閉じた（かマージした）MR は
   開き直さない（FG-AGENT-0016 `closed_by_person`）。MR を開けなかったら、開いていないことを確かめてから、作った枝を compare-and-swap で消す。
8. **書く順**（A7・revert の下書きと同じ）: 秘密の規則 → ボットのコミットと `update-ref` の作成（`commit_tree_change`）→ MR → push の記録
   （`repository.pushed`、actor `system`、ラベル `forge-agent-builder`。MR の版とチェックが続く）→ ビルドの記録（`origin`）→ イベント。
   push とビルドの記録の失敗はログに 1 回（枝と MR は残る）。イベントの失敗はジョブの失敗（再試行で `AlreadyOpen` からもう一度書く。
   MR ごとに dedup）。
9. **自動マージしない**: A7 と同じ。`forge/agent-lock` が確かめ、`forge/risk` が `agent_config_locked` と名づけ、自動マージは作成者ではない
   Maintainer の承認を待つ。MR は下書きにしない（A7 の MR と同じ扱い）。
10. **通知**: 既存の仕組み（`notify` の購読者）。新しいイベント `merge_request.agent_update_opened`（integration、actor `system`、MR ごとに
    dedup。`origin`・`lock_digest`・`revoked` の `slug@version`）と通知の種類 **`mr_agent_update`**（メールとアプリ内。MR の作成者 = 開いた
    相手に知らせる）。表の CHECK はマイグレーション `20270113100000` で広げた。画面は受信箱と設定の文言だけ（ja / en / id）。

### A9 の独立レビュー（2026-11-27、database-reviewer・silent-failure-hunter）で変えた点

- （HIGH、直した）ビルドの記録は後の取り消しがプロジェクトを見つける元なのに、MR の後に「ログだけ」で書いていた。MR ごとに 1 行
  （`record_build_once`、`mr_id` で NOT EXISTS）を、開いた実行と `AlreadyOpen` の実行の両方で書き、失敗はジョブの失敗（再試行）にした。
- （HIGH、直した）取り消しの更新を開く人が最後のビルドの人 1 人だけだった。新しい順に最大 5 人を試す（上の 6）。それでも誰もいなければ
  FG-AGENT-0016 を **alert** にした（取り消した版が使われ続ける）。
- （HIGH、受け入れ）ビルドの記録の無いプロジェクト（手で lock を書いた等）は取り消しのジョブに入らない。`forge/agent-lock` が
  FG-AGENT-0006 で失敗して見える（上の 5）。lock の索引の表は後の MR。
- （MEDIUM、直した）push の記録は、MR を開いた実行が（前の実行が枝を書いていても）書く。MR を開けなかった後の確認の失敗が元の誤りを
  隠していた（確認の失敗は別にログに 1 回、元の誤りを返す）。取り消しのジョブで何も開かなかったとき（`not_in_lock` 以外）は warn。
- （MEDIUM、直した）`POST …/update` は同じ更新の MR を先に見る: 開いていれば `queued: false`・`reason: already_open`・`merge_request_iid`、
  人が閉じていれば 409 FG-AGENT-0016 `closed_by_person`、ジョブが dedup なら `reason: already_asked`。取り消しの対象から削除中の
  プロジェクトを外した。
- （MEDIUM、受け入れ）取り消しのトランザクションの中でプロジェクトの数だけジョブを入れる（上限なし）。社内の規模では数百までで、
  ジョブの行は小さい。通知の表の CHECK の入れ替えは C10（`20261220100000`）と同じ形。
- （LOW、直した）カタログに無い種類は `unknown`。

### 残すこと

- A8 の画面の「更新の帯」は推薦から作っている。取り消した版（`locked_revoked`）と「外れる」部品を出すには `GET …/updates` に替える。
- doctor の `agent.revoked_in_use`（計画 16 §8）: 取り消した版を使う lock を先端で数える確かめはまだ無い（今はジョブの `dead` と
  `forge/agent-lock` の失敗で見える）。
- 取り消しの更新を、プロジェクトの Maintainer 全員にも知らせること（今は MR の作成者だけ）。

## Amendment（2026-11-27、A11 `feat/core-llm-agent-recommend`: 推薦の理由をローカル LLM で）

計画 16 A11 を入れた（計画のブランチ名 `feat/llm-agent-recommend` は `<area>` の規則に合わないため `feat/core-llm-agent-recommend`）。
利用者は不在で「推奨を選ぶ」と言っているので、計画に書かれていない点は次のとおり決めた。ゲートウェイ側の決定は ADR-0039 Amendment A11。

1. **選択は規則のまま**: `POST …/agent-builder/recommend` の `parts` は A7 の規則の選択と同じ部品・同じ版・同じ規則の理由。LLM が足すのは
   部品ごとの `llm_reason`（200 文字まで）と並び（ベースモデルを先頭に、理由のある部品を確信度の順に、残りは規則の順）だけ。カタログの
   ほかの部品（アダプターを除く、最大 5 つ）は `suggested`（`selected_by: "llm"`）に出すが **選ばない**: 人がチェックを入れ、Dry-Run で能力の
   印を見る。カタログに無い slug・アダプターの差し替え・仕様書のパスを名指す答え・重複は捨てて `llm.dropped` に数える
   （`forge_core::llm::agent_recommend::merge`、テスト `the_model_never_invents_a_part_or_swaps_the_base_model`・
   `reasons_reorder_the_rules_parts_and_never_change_the_selection`）。`project_type`・仕様書のタグを部品に結びつける欄はまだ作らない
   （A7 の 4 の保留のまま。LLM が仕様書の文から理由を書く）。
2. **経路は待たない**: ゲートウェイは worker の中、経路は `forge serve` の中なので、経路は読んだことを `forge.agent_recommend_asks`
   （プロジェクトごとに 1 行、追記のマイグレーション `20270113110000`）に記録し、worker の `agent_recommend` の周回（20 秒）が答える。
   経路はその時点の答えを返す: その先端の答えがあれば `source: "rules+llm"`、無ければ規則だけで `degraded: true` と `llm.state`
   （`pending` / `unavailable` / `paused` / `refused` / `withheld`）。LLM が落ちている・一時停止・時間切れ・不正な答え・6 分答えが無い
   （`llm.enabled = false`・worker が止まっている）はどれも **エラーにも成功にもしない**（`FG-AGENT-0011` RecommendDegraded を
   `llm.error_code`、原因の FG コードを `llm.cause` に。テスト `llm_down_returns_rules_with_degraded_flag`）。理由の読み取り自体の
   DB の失敗も同じ劣化の答えにし、ログに 1 回。画面は「もう一度読む」で答えを取りに来る（画面の変更はこの MR には無い）。
3. **入力は開いた仕様書だけ**: worker が既定の枝の先端の仕様書（`forge_checks::spec_index::index_commit` の文書）を読み、
   `forge_checks::confidential::confidential_at` と文書自身の印の **両方** が開いているものだけを最大 6 つ・各 4,000 文字まで送る。
   機密の文書はパスも送らない。分類できない（git の失敗・上限）先端は何も送らない（`refused`、閉じる側。テスト
   `confidential_spec_not_sent_to_llm`・`a_tip_with_only_confidential_specs_sends_nothing`）。開いた仕様書が無ければ聞かない（`no_spec`）。
4. **権限**: 新しい Action は作らない。推薦の判定は A7 の `authorize_agent_recommend`（エージェントは機密の印の無い先端だけ）のまま。
   助言の文は `forge_db::llm::advice_decided` が助言自身の対象（この `project`）で読み、読み手の種類は `forge_auth::advice_reader`
   （機密の助言はエージェントに `withheld`。機密の文書は送らないので起きない想定だが閉じておく）。
5. **独立レビュー（2026-11-27、llm-integration-reviewer・security-reviewer）で直した点**: 併合は入力に渡した部品
   （`catalog_given`、最大 60）だけを名指せる（保存した指摘を信じない、HIGH）。worker が読む聞いたことは答えを待つものだけを
   SQL で選び、新しく読まれた順（答えたものが 20 の枠を埋めて新しい依頼が 1 時間届かない穴、HIGH）。読み直しは `asked_at` だけを
   動かし、`updated_at` は答えの時刻（読み直すたびに再試行が延びる穴、MEDIUM）。提案に部品の能力（`capabilities`）を付ける
   （理由の文だけを人が読むことにしない、MEDIUM）。仕様書は行の終わりで切ってから伏せ字にする（秘密を半分で切らない、LOW）。
   助言が掃除された `ready` はもう一度聞く（LOW）。**受け入れた点**: 呼び出し人ごとの上限は作らない — worker は聞くのを 1 件ずつ
   待つので、この機能の対話の依頼は同時に 1 つまで、同じ入力は 3 回まで（MR レビューの予約 20 % は変わらない）。理由の文が
   仕様書を名指すことはコードで確かめない（引用は部品があることだけを示す。文は助言で、選択には効かない）。助言は
   プロジェクトの `RepoRead` の人全員に見える: 入力は機密の印の無い先端の文書だけなので、読める範囲は推薦そのものと同じ
   （パスごとの権限ができたら、文書ごとに人として `authorize()` してから送る）。`part` の manifest の文も信じないデータとして囲む。
6. **残すこと**: 画面（A8）での `llm_reason`・`suggested`・`llm.state` の表示と自動の読み直し。カタログが変わっただけ（先端が同じ）の
   ときの聞き直し（今は先端が変わったときだけ）。部品の説明文（`SKILL.md` の frontmatter など）を入力に足すこと（今は manifest の事実だけ）。

## Amendment（2026-11-28、A12 `feat/ui-agent-builder-followups`: A8・A9・A11 の残すこと）

A8・A9・A11 の「残すこと」のうち、doctor・更新の帯・AI の理由の画面を入れた。利用者は不在で「推奨を選ぶ」と言っているので、次のとおり決めた。

1. **doctor `agent.revoked_in_use`**（計画 16 §8）: 候補は「今は取り消された版を選んだ、終わった（Dry-Run でない）ビルドのある、削除中でない
   プロジェクトの ready なリポジトリ」（`forge_db::agent_builder::projects_with_revoked_builds`、A9 の取り消しの対象と同じ考え）。各候補を
   **既定の枝の先端の lock** で確かめる（`Marketplace::revoked_at_tip`: lock の `slug@version` のうちもう公開されていないもの。A9 の
   `locked_revoked` と同じ判定）。候補は 200 件まで、全体で 30 秒、git の 1 回は 20 秒。読むだけ。
   - 使っていれば **`warn`**（計画のとおり。`fail` にはしない — 取り消しの対応は更新の MR で、doctor の失敗で止めるものではない）。
     数・プロジェクトの数値の ID（最大 20 個。名前・パス・部品のファイルは出さない）・直し方（画面の「更新の MR を作る」か
     `gstn agent update --project <id>`、runbook `agent-supply-chain`）。ID は運用者が該当を見つけるための最小の情報で、機密ではない。
   - 読めない候補・上限・時間切れは `ok` にせず `warn`（`could not be read (first: <FG コード>)`・`cut at 200`・`stopped after 30 s`）。
     読めない理由はコードだけ（メッセージはパスを含みうる）。新しいエラーコードは作らない（DB の失敗は既存のコードで `fail`）。
   - 手で書いた lock（ビルドの記録が無い）は候補にならない（A9 5 と同じ。`forge/agent-lock` が FG-AGENT-0006 で失敗して見える）。
     全リポジトリの先端を読む案は、社内の規模でも doctor の 30 秒に収まらないので採らない。
   - テスト: `revoked_in_use_warns_with_the_count_and_how_to_update`・`an_incomplete_walk_warns`（純粋）、
     `doctor_counts_a_revoked_version_in_a_tips_lock`（実 DB・実リポジトリ: 取り消し前 0、取り消し後 1、先端から lock が消えたら 0）。
2. **更新の帯は `GET …/agent-builder/updates`**: A8 の「lock と推薦の版の差」をやめ、A9 の答えをそのまま出す。取り消された版を先に、
   「取り消し済み」の印（文字。色は繰り返しだけ）と「→ 新しい版」か「置き換える版がないので外します」を文で。足す部品・外す部品も文で、
   変わらない部品は出さない。取り消しがあれば題は「取り消された部品があります」（危険の色）、無ければ「更新があります」。
   Dry-Run の変わるファイルの数を一行。読み込み中・最新（`up_to_date`）・lock が無い（`no_lock`）は一行、読めないときは
   `user_message` と問い合わせ番号。
   - **ボタンは API が言うときだけ**: `UpdatesBody` に `can_update` を足した（`POST …/update` の Action `AgentBuilderCompile` を、その経路の
     `authorize()` で試しに判定した答え。人で Developer。エージェントのトークンは `GET` は読めるが `false`）。新しい経路・Action は無い
     （openapi と `schema.gen.ts` を作り直した）。サーバーは `POST` でもう一度決める。
   - `GET …/updates` は Developer の読み（`AgentBuilderDryRun`）なので、**Reporter には帯を出さない**（A8 では Reporter にも lock と推薦から
     帯を出していた。今は Reporter に呼ぶと 403 になる読みなので呼ばない。Reporter の「Developer 以上が作れます」の行は消した）。
   - 「更新の MR を作る」は `POST …/update {}`（世代の移行の `adapter` は画面に出さない — gstn の `--adapter` だけ）。答えの `queued`・
     `already_open`（MR へのリンク）・`already_asked` を `role="status"` の一行で。拒否（FG-AGENT-0016 `closed_by_person` など）は
     `user_message` と問い合わせ番号。MR は Maintainer の承認を待つと文で言う。
3. **AI の理由の表示**（A11 の残すこと）: 部品ごとの `llm_reason` を「AI の助言（提案）」という**文字の札**とともに（色だけにしない）、
   `suggested` は「AI の提案（まだ選んでいません）」の札・理由・**能力の印**（API の `capabilities`）とともに、**最初は選ばない**（チェックを
   入れると `selected_by: "user"` で送る）。Reporter の読みだけの一覧にも理由と提案を出す。文はすべて React の文字（HTML として解かない。
   テストは `<b>` が要素にならないことを確かめる）。
   - 「AI の助言（提案）」の欄（札を見出しにした region。文だけが `role="status"` で、ボタンはその外）: `ready` は要約（あれば）と「AI の助言は提案です。部品を選ぶのはあなたです。」、
     `degraded` なら「一部は使えませんでした」。`ready` 以外は「AI の理由は今ありません（<理由>）。おすすめは規則だけで選んでいます。」で、
     理由は `pending`・`unavailable`・`paused`・`refused`・`withheld`・その他をそれぞれ文で。`llm.error_code`・`cause` は画面に出さない
     （利用者に見せるのは文と問い合わせ番号だけ。原因はログと `forge diagnose`）。規則だけで `ready`・劣化なし（LLM を使っていない）なら欄を出さない。
   - **自動の読み直し**: `pending` の間だけ 20 秒ごとに推薦を読み直す（サーバーが 6 分で `unavailable` に変えるので止まる）。「もう一度読む」の
     ボタンは `pending` と `unavailable`（LLM が戻れば答えが変わりうる）の間、読み直し中は「読み直しています…」。理由が届いても**選択は消さない**: 選ぶ画面の作り直しの鍵を推薦の部品と版（順と理由は除く）にし、Dry-Run の
     一致も選択の順によらない鍵にした（LLM の並べ替えで Dry-Run と「MR を作る」が消えない）。
4. 文言は ja / en / id（`agentBuilder.update.*` を足し、`agentBuilder.ai.*` を新設、使わなくなった `update.nextReader` を削除）。
   テスト（vitest）: 更新の帯 6 件・AI の理由 9 件（`tests/agent-builder.test.tsx`）、`updateChanges`（`tests/marketplace.test.tsx`）。
   画面で使わなくなった `lockedVersions`・`updatesOf`・`LOCK_PATH` は消した。

### A12 の独立レビュー（2026-11-28、design-reviewer・a11y-architect。CRITICAL・HIGH なし）で変えた点

- （MEDIUM、直した）`can_update: false` の行は「今は更新の MR を頼めません。下で部品を選び、Dry-Run から MR を作れます。」（帯は
  Developer にしか出ないので「Developer 以上」は次の一手にならない）。「AI の理由は今ありません」の文に「このまま部品を選べます。」、
  `unavailable` にも「もう一度読む」。
- （MEDIUM、直した）頼んだ後にボタンが消えるので、フォーカスを答えの行（`tabIndex=-1` の status）へ移す。AI の欄は region と status を
  分け、ボタンを status の外に出した。
- （MEDIUM・LOW、直した）印と札の文字を `--gs-font-size-sm`（xs は節の見出しだけ）、帯の行に `overflow-wrap`・等幅の数字、危険の帯の
  リンクは文字色と下線、頼んだ後に `updates` も読み直す。
- （MEDIUM、受け入れ）理由のある行ごとに「AI の助言（提案）」の札が繰り返す: 依頼の条件（文字の札で示す、色だけにしない）なので
  短くしない。（LOW、受け入れ）帯から Dry-Run の差分を開く切り替えは作らない（下の選択の Dry-Run で見られる）。

### 残すこと

- 取り消しの更新を、プロジェクトの Maintainer 全員にも知らせること（A9 のまま）。
- 手で書いた lock の索引（doctor と取り消しの対象に入れるため。A9 のまま）。
- 画面からの世代の移行（`adapter` を選んで `POST …/update`）。
- カタログが変わっただけのときの聞き直しと、部品の説明文を LLM の入力に足すこと（A11 のまま）。
