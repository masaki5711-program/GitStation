# 16 エージェントのマーケットプレイスとエージェントビルダー（W15）

- 作成: 2026-11-11（`planner`。コードは `main` dbc3a30 で確かめた）
- 状態: 案。§9 の 2〜7 は推奨で決定（2026-11-11、利用者の「推奨で進めて」）。**§9 の 1 は利用者の答え待ち**（自動マージの方針を変えるため）。

## 0. 目標（1 文）

プロジェクトのリポジトリに、マーケットプレイスで選んだ部品（スキル・エージェント・規則・フック・コマンド・MCP）から Claude Code の環境（`.claude/`・`CLAUDE.md`・`.mcp.json`）を MR で作り、どの部品のどの版かを `.gitstation/agent.lock.json` で固定し、署名で中身が変わっていないことを確かめられるようにする。

**最大の注意**: フックは開発者の PC で `node` として動く。悪意のある部品が 1 つ入ると、それを使う全員の PC でコードが動く。だからこの計画は、マーケットプレイスを作ることより先に「署名がなく検査を通っていない `.claude/` が `main` に入らないこと」を作る。

## 1. 前提の点検

### 1.1 決まっていること（議論し直さない）

| 決定 | 出典 |
|---|---|
| 1 プロジェクト = 1 リポジトリ。ビルダーが `.claude/`・`CLAUDE.md`・`.mcp.json` を作る | 09 §3 |
| 生成は MR で入れる（直接コミットしない） | 09 §4 |
| 部品に版と署名。`.gitstation/agent.lock.json` で版を固定 | 09 §4 |
| フックは CI でテストしてから公開。公開の権限は権限の表に行を足す | 09 §4 |
| API は `agent-builder/recommend`・`agent-builder/compile`・`rules/inject-lesson` | 09 §7 |
| フックは Node.js（CommonJS、標準モジュールだけ）、フェイルオープン、3 秒で打ち切り、止めるのは `exit 2` だけ | 09 §3.1・§9 |
| IR とアダプターを分ける（`target-adapter.yaml`）。`enabled: false` の部品は次の生成で消す。Dry-Run と世代の移行 | 09 §4 |
| ベースモデルは `claude-code-v2` と `claude-code-tmux` | 09 §3.2 |
| 教訓の注入は確認済みだけ、MR で入れる | 09 §6・§9 |
| 推薦はローカル LLM が提案し、決めるのは人 | 09 §4 |

### 1.2 コードで確かめたこと

| 項目 | 状態 | 根拠 |
|---|---|---|
| マーケットプレイスとビルダーのコード | 無い | `crates/` |
| エラーの領域 `AGENT` | 無い | `crates/forge-errors/build.rs:39-41` |
| **危険の規則が `.claude/`・`CLAUDE.md`・`.mcp.json`・`.gitstation/` を見ていない** | C9（自動マージ、既定 `full`）が入ると、フックを書き換える MR が人の目を通らずに `main` に入りうる | `crates/forge-core/src/risk.rs:391-405,539-577` |
| 鍵ファイルの読み方（kid、所有者だけ、入れ替え） | ある。署名の鍵に使える | ADR-0013 Amendment |
| ADR の番号 | W15 は **0032**（0030・0031 は M4 が予約） | `docs/adr/`、14 §5 |
| 最初の部品の元 | root のエージェントモデルと、TMUX 版・Ver2 の `refactor/Lcross-NOPBI-marketplace-parts`（Kanflow 連携の 8 部品、未統合。中身は着手前に確かめる） | handoff 2026-10-30 §7 |

### 1.3 前提への意見

- **forge 自身（root と `Git/`）はビルダーの対象にしない**（2 つのリポジトリの構成）。root からは部品を切り出すだけ。
- **ビルダー専用の作業場所は作らない**。部品の作成・修正・確認は普通のリポジトリと MR で行い、公開の時に DB に固める。
- **推薦は M5 を待たない**。最初は決まった規則（`project_type`、仕様書の frontmatter のタグ）。LLM は M5 で足す。
- 同僚のフィーチャーブランチをチェックアウトすると、そこのフックが動く（git の性質）。サーバーでは防ぎきれないので §8 の残る危険に書き、`main` を守ることに集中する。
- 着手前に確かめる: サーバー側でコミットを作る仕組み（ADR-0017 / 0027）、C2 のパイプラインにサーバーが決めたジョブを足せるか、TMUX 版・Ver2 の部品の形、Kanflow W11 の「確認済み」の状態。

## 2. 決めたこと（推奨）

### 2.1 部品の形と置き場所 — 案 C

| 案 | 中身 | 判断 |
|---|---|---|
| A | git リポジトリだけ。生成のたびに tag を読む | tag を動かされると中身が変わる。権限の判定が増える |
| B | DB の行と blob（アップロード） | MR の確認の外で作られる。C6 待ち |
| **C（採用）** | **作るのは git、公開で DB に固める。** `marketplace/parts` リポジトリに `<slug>/v1.2.3` の tag。公開でサーバーがその SHA の木を git（argv・`--`・時間切れ）で読み、検査し、決まった並びで要約し、署名し、DB に入れる（追記だけ、1 版 1 MiB まで） | 確認は普通の MR。公開後は不変。生成は DB だけを読む純粋な処理 |

部品の形: ディレクトリ 1 つに `part.toml`（`slug`、`kind` = skill / agent / rule / hook / command / mcp / adapter、`version`（semver）、`requires`、`capabilities`、`files`）と中身。W1 の形（`SKILL.md` の階層と frontmatter）をそのまま入れられる。

### 2.2 署名 — 案 S2

**サーバーの Ed25519 の鍵で、決まった並びの要約に署名する。** 署名は人が公開を承認し、CI が緑になった後だけ。鍵ファイルは ADR-0013 の形（`agent.signing_keys_file`、kid、入れ替え）。公開鍵は `GET /api/v1/agent-keys` で配り、`gstn` は手元にも保持する。誰が出したかは監査とイベントに残す。人ごとの SSH 署名（S1）と両方（S3）は M8 の拡張。

**確かめる場所は 3 つ**: 生成のとき（サーバー）、必須のチェック `forge/agent-lock`（`main` に入る前）、`gstn agent verify`（PC。リポジトリの中のフックに確かめさせない）。

### 2.3 クレート（契約 9 を変える → root の MR W15-0 が先）

- 純粋なクレート **`forge-agent`**: `part.toml` と lock の解析、lint、要約、Ed25519 の確かめ、フックの静的な検査、アダプターによる生成。I/O なし。`gstn` から使う。
- 機能のクレート **`forge-marketplace`**: DB、git、MR の作成、署名（鍵ファイル）。

## 3. データ（すべて追記のマイグレーション。`tables.toml` と doctor も直す）

| 表 | 主な列 | 規則 |
|---|---|---|
| `agent_parts` | `slug`（一意）、`kind`、`source_project_id`、`deprecated_at` | slug は公開のグループの中だけで作れる |
| `agent_part_versions` | `part_id`、`version`、`channel`（beta / stable）、`source_sha`、`tag`、`manifest`、`digest`、`signature`、`key_id`、`capabilities`、`published_by`、`ci_pipeline_id`、`state`、`revoked_reason` | (part, version) は一意で上書きしない。変えてよいのは `state`・`channel` だけ（監査に残す） |
| `agent_part_files` | `version_id`、`path`、`mode`、`sha256`、`content` | 追記だけ（ADR-0018）。1 版 1 MiB、200 ファイルまで |
| `agent_signing_keys` | `kid`、`public_key`、`created_at`、`retired_at`、`revoked_at` | 秘密の鍵は DB に入れない |
| `agent_builds` | `project_id`、`requested_by`、`adapter_version_id`、`selection`、`lock_digest`、`dry_run`、`mr_id`、`state`、`error_code`、`request_id` | 生成と更新の記録 |
| `agent_lesson_injections` | `project_id`、`kanflow_lesson_id`、`idempotency_key`（一意）、`text_digest`、`confirmed_by`、`mr_id`、`state` | 二重の MR を防ぐ |

lock には 09 §4 の項目に、部品ごとの `digest`・`key_id`、生成したファイルごとの `sha256`、`custom`（手で足したファイル。人の承認が要る）を足す。

## 4. 操作と権限の表（allow と deny の両方）

| Action × Resource | allow | deny |
|---|---|---|
| AgentPartRead / List | サインインした人、ボット、エージェント | 匿名 → 401 |
| AgentPartPublish | `marketplace` グループの Maintainer 以上、**人の Web セッションだけ**、tag の作者とは別の人 | Developer → 403、トークン・エージェント・サービスアカウント・代理 → 403、作者本人 → `FG-AGENT-0009`。`network` の能力の部品は管理者だけ |
| AgentPartPromote（beta → stable） | 同上 | 同上 |
| AgentPartRevoke | 公開グループの Maintainer 以上か管理者、人だけ | エージェント、トークン |
| AgentKeyRead | サインインした人、ボット | 匿名 |
| AgentBuilderRecommend | `RepoRead` と同じ。エージェントは仕様書に機密の印が無いときだけ | 見えない → 404 |
| AgentBuilderCompile（`dry_run`） | Developer 以上、エージェントも可 | Reporter → 403 |
| AgentBuilderCompile（MR を作る） | Developer 以上、**人だけ** | **エージェント → 403**（自分の守りを書き換えない）、サービスアカウント → 403 |
| RuleInjectLesson | Kanflow の代理（ADR-0025）、本人が Developer 以上、教訓が確認済み | 未確認 → `FG-AGENT-0010`、代理でない → 403 |
| 署名の鍵の管理 | HTTP に出さない。`forge admin agent-key` だけ | — |

## 5. エラーコード（仮。領域 `AGENT`）

| 番号・名前 | 内容 | 重さ |
|---|---|---|
| 0001 PartManifestInvalid | `part.toml` の形、許していない置き場所、symlink、大きさ | info |
| 0002 PartSignatureInvalid | 署名・要約が合わない（改ざんの疑い） | **alert**（runbook `agent-supply-chain`） |
| 0003 SigningKeyUnavailable | 鍵ファイルが無い・読めない・DB と合わない | **alert**（`agent-signing-key`） |
| 0004 PartChecksNotGreen | パイプラインかフックのテストが緑でない | info |
| 0005 PartCapabilityUndeclared | 宣言していない `require` や能力 | info |
| 0006 PartVersionRevoked | lock が取り消された版を指す | warn |
| 0007 AgentLockMismatch | 管理しているファイルが lock と違う、未承認の `custom` | warn |
| 0008 SelectionUnresolved | 版の範囲に合わない、部品どうしがぶつかる | info |
| 0009 PublishSelfApproval | tag の作者が自分で公開 | info |
| 0010 LessonNotConfirmed | 未確認の教訓の注入 | info |
| 0011 RecommendDegraded | LLM に届かず規則だけで推薦（`source=rules` と出す） | warn |
| 0012 PartVersionExists | 同じ版の再公開 | info |
| 0013 McpEndpointNotAllowed | `.mcp.json` の行き先が社内の許可の一覧に無い（契約 8） | info |

## 6. API・gstn・画面

- API: `GET /api/v1/agent-parts`（`kind`・`channel`・`q`）、`…/{slug}`、`…/{slug}/versions/{v}`、`POST …/{slug}/versions` `{tag}`、`…/{v}/promote`、`…/{v}/revoke`、`GET /api/v1/agent-keys`、`POST /api/v1/repos/{id}/agent-builder/recommend`、`…/compile` `{selection, dry_run}`、`GET …/agent-builder`、`POST /api/v1/repos/{id}/rules/inject-lesson`。
- `gstn part list|view|publish|promote|revoke`、`gstn agent recommend|preview|apply|status`、**`gstn agent verify`**（手元で lock・要約・署名・取り消しを確かめる。ネットに届かないときは手元の鍵で確かめ、そう出力する）。
- 画面（ja / en / id）: `/-/marketplace`（種類のタブ、beta / stable、**能力の印**「コマンドを実行する」「ネットワークを使う」「権限の設定を広げる」）、部品の詳細、プロジェクトの設定 →「エージェント」（アダプター、推薦のチェックボックス、Dry-Run の差分、「MR を作る」、更新の帯）、公開のダイアログ（CI の結果と能力の差分）。

## 7. MR の分け方と順序

各 MR の終わり: fmt・clippy・nextest、`gen-catalog` / `gen-authz` の差分なし、`/review` で CRITICAL / HIGH なし（評価役は 2 つまで）。

| # | ブランチ | 中身 | 先に書くテスト（例） | 評価役 | 着手 |
|---|---|---|---|---|---|
| W15-0 [root] | `chore/harness-w15-agent-supply-chain` | 契約 9 に `forge-agent`・`forge-marketplace`、誤りの契約に `AGENT`、`security-reviewer` に供給網の観点と eval | eval `undeclared-child-process-hook`、`agent-config-auto-merge` | `harness-optimizer` | 今すぐ |
| A1 | `feat/core-agent-part-format` | `forge-agent`: 解析・lint・要約・Ed25519・静的な検査・`gstn agent verify`。ADR-0032 | `manifest_rejects_path_outside_claude_dir`、`digest_is_independent_of_file_order`、`verify_rejects_one_byte_change`、`scan_flags_undeclared_child_process`、`mcp_external_host_rejected` | `rust-reviewer`、`security-reviewer` | 今すぐ |
| **A2★** | `feat/core-risk-agent-config` | `risk.rs` に `agent_config` の規則（`.claude/**`・`CLAUDE.md`・`.mcp.json`・`.gitstation/**`）。重さは §9 の 1 | `claude_hook_change_is_flagged`、`mcp_json_change_is_flagged`、`unrelated_json_not_flagged` | `security-reviewer`、`pr-test-analyzer` | **今すぐ。C9 より前** |
| A3 | `feat/db-agent-marketplace` | §3 の表、追記だけのトリガー、`agent.signing_keys_file`、doctor | `part_files_update_refused`、`key_file_insecure_permissions_refused` | `database-reviewer`、`security-reviewer` | A1 の後 |
| A4 | `feat/auth-agent-marketplace` | §4 の Action と権限の表 | `publish_denied_for_agent_token`、`publish_denied_for_tag_author`、`compile_mr_denied_for_agent` | `authz-matrix-auditor`、`security-reviewer` | C11 の後（`authorize.rs` の衝突を避ける） |
| A5 | `feat/marketplace-publish` | 公開（tag → SHA → 検査 → 署名）、**サーバーが決めたフックのテストのジョブ**（ダイジェスト固定、ネットワークなし）、取り消し、beta → stable、`gstn part` | `publish_refused_when_pipeline_not_green`、`moved_tag_after_publish_has_no_effect` | `ci-runner-security-reviewer`、`git-protocol-reviewer` | A3・A4 の後 |
| A6 | `feat/marketplace-seed-parts` | `marketplace/parts` を作り、root と TMUX 版・Ver2 の部品を移す。アダプター 2 つ | 全部品で `gstn agent verify` が通る | `security-reviewer` | A5 の後 |
| A7 | `feat/marketplace-compile` | 生成、Dry-Run、ボットのコミットで MR、規則だけの推薦、必須のチェック `forge/agent-lock` | `compile_is_deterministic`、`lock_check_fails_on_hand_edited_hook`、`builder_mr_not_auto_merged` | `security-reviewer`、`rust-reviewer` | C9 の後 |
| A8 | `feat/ui-marketplace-builder` | §6 の画面 | 能力の印の読み上げ、権限の無い人にボタンを出さない | `a11y-architect`、`design-reviewer` | A7 の後 |
| A9 | `feat/marketplace-updates` | 更新と世代の移行の MR、取り消しで更新 MR、通知 | `revoke_opens_update_mr_once` | `database-reviewer`、`silent-failure-hunter` | A7 の後 |
| A10 | `feat/integ-inject-lesson` | 教訓の注入（規則のファイルに追記する MR だけ） | `unconfirmed_lesson_refused`、`lesson_text_cannot_touch_settings_json` | `security-reviewer`、`authz-matrix-auditor` | C9 と Kanflow W11 の後 |
| A11 | `feat/llm-agent-recommend` | 仕様書を M5 のゲートウェイで読み、推薦に理由 | `llm_down_returns_rules_with_degraded_flag`、`confidential_spec_not_sent_to_llm` | `security-reviewer` | M5 の後 |

## 8. 運用と危険

- runbook: `agent-supply-chain`（疑いの版の取り消し → 使っているプロジェクト → 更新の MR → 開発者への `gstn agent verify` の依頼文 → 鍵の失効の判断。変更は人の承認）、`agent-signing-key`、`agent-lock`、`agent-marketplace`。
- doctor: `agent.signing_keys`、`agent.revoked_in_use`、ファイルの要約の抜き取りの確かめ。メトリクス: `agent_signature_failures_total`（警報）、`agent_lock_check{result}`、`agent_publish_total`。

| 危険（すべて CRITICAL として扱う） | 対策 |
|---|---|
| 悪意のある部品の公開（内部の人） | 二人の目、能力の宣言と静的な検査、サーバーが決めたフックのテスト、beta で 3 日、取り消しと自動の更新 MR |
| 公開後の DB や tag の書き換え | 3 か所での確かめ、追記だけ、tag を動かしても公開済みは不変 |
| マーケットプレイスを通さず `.claude/hooks` を手で書き換える MR | A2 の危険の規則、`forge/agent-lock`、自動マージから外す |
| `settings.json` の permissions を広げる | 能力 `permissions` の宣言、Dry-Run で赤く出す |
| 規則や CLAUDE.md の文へのプロンプトの注入 | 文の部品も二人の目。教訓は規則のファイルにだけ |
| `.mcp.json` が社外を向く・秘密を含む | 0013 の許可の一覧、公開時の秘密の検査 |
| 署名の鍵の盗難 | 鍵ファイルの権限、kid の失効、`gstn` の手元の鍵の更新 |
| **残る危険**: 同僚のブランチをチェックアウトするとフックが動く | ワークスペースの信頼の確認、チェックアウト後の `gstn agent verify` の案内、MR の画面でフックの変更を目立たせる |
| 部品の更新で全員のモデルが一度に壊れる | beta / stable、前の版に戻す MR を 1 クリックで |

## 9. 利用者に聞くこと

1. **（答え待ち）手で `.claude/` を変える MR を自動マージで止めるか**: 今の「止めるのは 3 つだけ」（プログラムの破壊・DB の破壊・個人情報の流出）に、**4 つ目「開発者の PC で動くコードの変更」を足す**ことを推奨。対象は `forge/agent-lock` が緑でない `.claude/**`・`.mcp.json` の変更。ビルダーの MR も Maintainer の人の承認を 1 つ要る。ほかの自動マージは今のまま。A2 が C9 より前に要る。
2. 公開できる人: `marketplace` グループの Maintainer 以上で、作者とは別の人。**決定（推奨）**。
3. 署名の鍵: サーバーの Ed25519 の鍵 1 本と予備の kid。オフラインの root 鍵は M8。**決定（推奨）**。
4. 部品のリポジトリ: `marketplace/parts` の 1 つに部品ごとの tag。**決定（推奨）**。
5. 社外の部品: 入れない（オンプレの契約）。**決定（推奨）**。
6. forge 自身はビルダーの対象にしない。root からは部品を切り出すだけ。**決定（推奨）**。
7. 教訓からのフック: 自動の注入は規則のファイルだけ。止めるフックは人が部品として公開する道を通す。**決定（推奨）**。
