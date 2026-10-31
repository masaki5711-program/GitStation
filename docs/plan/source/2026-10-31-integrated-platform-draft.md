# GitStation × Kanflow × Agent_Claudecode_ver2 統合プラットフォーム基本設計書

**Document Type:** Master Architecture & Implementation Specification  
**Target Systems:** `GitStation` / `Kanflow` / `Agent_Claudecode_ver2`  
**Purpose:** チーム・バイブコーディングおよび設備仕様管理（Spec-as-Code）統合基盤の構築

---

## 0. コアビジョン：人の仕事を「作業」から「基準設定・承認・改善」へシフトする

本プラットフォームは、ソフトウェア開発および工場設備仕様書・業務文書の作成において、人間が白紙からコードを書き、手動でテストし、ドキュメントを更新する作業をゼロにするために設計された統合基盤である。

人間は**「意図と基準の定義（Set：Kanflow）」「AIによる規約遵守と実装（Keep：Agent_Claudecode_ver2）」「エビデンスに基づく承認と仕組みの改善（Adjust：GitStation）」**というマネジメントと意思決定に専念し、実作業と品質統制はすべてプロジェクト特化型に編成された自律エージェントとローカルLLMが担う。

---

## 1. エコシステム全体構想（三位一体アーキテクチャ）

個人のローカル環境で属人化しやすい「バイブコーディング（自然言語による意図ベース開発）」を、チームのガバナンスと資産化ループに乗せるため、以下の3つのリポジトリ・システムを直結させる。

| システム / リポジトリ | 位置づけ・コア役割 | プラットフォーム内での主要機能 |
| :--- | :--- | :--- |
| **Kanflow**<br>`masaki5711-program/Kanflow` | **上流：意図の定義と指令**<br>（PM・現場・非エンジニア窓口） | ・曖昧な要望（Vibe）からPBI・タスク・受け入れ条件を構造化<br>・内蔵ローカルLLMによる仕様の抜け漏れ検品（Spec-Gate）<br>・多言語自動ブリッジと、ステータス連動によるエージェント起動<br>・チーム内で発生したミス・手戻りの記録と教訓データベース化 |
| **Agent_Claudecode_ver2**<br>`masaki5711-program/Agent_Claudecode_ver2` | **中流：自律実装と検証**<br>（AI実働部隊・標準スキル駆動） | ・プロジェクト固有の `CLAUDE.md` と `Skills` / `Rules` に従う自律実装<br>・ヘッドレスブラウザ等による自己検証と画面エビデンス撮影<br>・設備仕様作成における「仮想デザインレビュー（DR）」の実行 |
| **GitStation**<br>`masaki5711-program/GitStation` | **下流：資産保管・CI/CD・ナレッジ**<br>（全社共有ハブ・マーケットプレイス） | ・コードおよび非エンジニア向け文章（仕様書・手順書）のGit版管理<br>・社内サーバーへの即時プレビュー環境デプロイと帳票ビルド<br>・仕様書連動の「エージェントビルダー＆マーケットプレイス」提供 |

---

## 2. 既存ツール（GitLab等）の限界と本基盤の優位性

GitLab等の既存DevOpsツールは「人間のエンジニアが自らコードを書き、コマンドを操作すること」を前提としたモノリスであり、非エンジニアの参画やAIエージェントによる自律駆動には構造的な壁がある。本基盤は以下の5点でこれを克服する。

| 課題領域 | GitLab単体での限界 | GitStation × Kanflow 連携による解決策 |
| :--- | :--- | :--- |
| **① 非エンジニアの障壁** | Branch、Commit、MR等の開発用語が前面に出ており、一般社員の文書版管理には使えない。 | Gitの裏側を隠蔽したエディタを提供。日常の文書編集の延長で厳格なバージョン管理と差分比較が回る。 |
| **② 曖昧な意図の構造化** | Issueは人間が要件を書き切る前提。上流の企画支援や多言語間の認識合わせ機能を持たない。 | KanflowのローカルLLMがGitStationの既存ナレッジを参照し、選択肢形式で質問して要件を自動確定させる。 |
| **③ エージェント統制** | CIランナーは定型スクリプト実行用であり、現場の暗黙知や設計規約をAIに守らせる機構がない。 | マーケットプレイスの `Skills` / `Sub-agents` / `Hooks` をプロジェクトごとに合成し、品質のバラつきを排除。 |
| **④ 仕様書と実装の乖離** | Wikiがコードから遊離しており、仕様変更や実装変更のたびにドキュメントが陳腐化する。 | 仕様書自体をエージェントの入力源（Single Source of Truth）とし、コード変更時に文書改訂PRも自動生成。 |
| **⑤ 検収の認知負荷** | 人間がソースコードのDiffを1行ずつ読む必要があり、AIが大量生成したコードのレビューでパンクする。 | プレビュー環境の即時立ち上げ＋画面スクショ同梱PR＋ローカルLLMによる「業務言語での3行要約」で即決承認。 |

---

## 3. ローカルLLM × Agent_Claudecode_ver2 の二段構え階層化

高推論コストのメインエージェント（`Agent_Claudecode_ver2`）をいきなり動かすのではなく、KanflowとGitStationに内蔵されたローカルLLMを「常駐・低遅延・コストゼロの門番（フィルター）」として前後に配置する。

### 3.1 上流ゲート：Kanflow「Spec-Gate（仕様の門番）」
* **AI実装レディネス判定：** ユーザーがKanflowでタスクを作成すると、内蔵ローカルLLMがGitStation上の関連仕様書・DB定義を即座に検索し、未定義の条件（例：エラー時の振る舞い、対象ロール等）を検知する。
* **選択式プロンプト生成：** 人間は白紙から詳細な指示を書く代わりに、ローカルLLMが提示する確認項目をチェックするだけで、`Agent_Claudecode_ver2` が迷わず実装できる構造化コンテキストが完成する。

### 3.2 中流ビルド：エビデンス同梱PRとピン止めフィードバック
* **ヘッドレス検証エビデンス：** `Agent_Claudecode_ver2` は実装後、テスト通過ログと変更前後の画面スクリーンショット（Playwright等で自動撮影）をセットにしてGitStationへPRを作成する。人間はコードを読まず画面結果のみで検収できる。
* **ビジュアル・ピン止め修正：** GitStationが社内サーバーにデプロイしたプレビュー画面上で、気になる箇所をクリックしてコメントすると、ローカルLLMがDOM要素と対象コンポーネントを特定し、Kanflowへ修正サブタスクを自動起票する。

### 3.3 下流ゲート：GitStation「Living Documentation ＆ 自己学習ループ」
* **非エンジニア向け差分翻訳：** GitStationのローカルLLMがPRのコード差分（または仕様書の改訂差分）を解析し、「業務上何が変わったか・どこを確認すべきか」を平易な言葉で要約表示する。
* **ナレッジ自動追従：** コード変更によって古くなった既存マニュアルをローカルLLMが検知し、マニュアル側の改訂案PRを自動作成する。また、人間が行ったレビュー指摘を汎用ルール化し、`CLAUDE.md` やマーケットプレイスへの追加提案を行う。

---

## 4. プロジェクト別エージェント編成と「動的注入モデル」

プロジェクトごとに専用のエージェントモデルを作成する際、「アプリ用リポジトリ」と「エージェント用リポジトリ」を分けるとクローンや同期が二重管理になり破綻しやすい。そのため、**「1プロジェクト＝1リポジトリ（動的コンパイル＆注入方式）」**を標準仕様とする。

### 4.1 単一リポジトリ内包型ワークフロー
1. **仕様書のリンクとAI選別：** GitStation上でプロジェクトの仕様書（Markdown）を作成・指定すると、ローカルLLMが要件（「決済連携」「オフライン対応」「多言語UI」等）を抽出し、マーケットプレイスから必要な `Skills` / `Sub-agents` / `Hooks` をチェックボックス形式で自動提案する。
2. **リポジトリへの直接コンパイル：** ユーザーがチェックボックスを確定（必要に応じてマーケットプレイスから手動追加）すると、GitStationが対象リポジトリのルートに `.gitstation/agent.lock.json`、`.claude/` ディレクトリ一式、`.mcp.json`、および合成済みの `CLAUDE.md` を直接生成・コミットする。
3. **ローカルPCでのクローンと実行：** メンバーは自分のPCへ対象リポジトリを1つ `git clone` するだけで、最初からそのプロジェクト専用にカスタマイズされたClaude Code環境が立ち上がる。Web側で仕様やスキルが更新された場合も、通常の `git pull` のみで最新状態に同期される。

### 4.2 リポジトリ標準ディレクトリ構造（Claude Code 公式仕様完全準拠）
Claude Code CLIが誤作動なく確実に設定・スキル・フック・サブエージェントを認識できるよう、公式ドキュメントで規定されているディレクトリ階層およびファイル命名規則に100%準拠して出力する。

```text
project-repository/
├── CLAUDE.md                           # [公式] プロジェクト全体の基本憲法・仕様要約
├── .mcp.json                           # [公式] プロジェクト共有MCPサーバー定義 (GitStation/Kanflow接続用)
├── .claude/                            # [公式] Claude Code 構成ルートディレクトリ
│   ├── settings.json                   # [公式] 共有設定およびHooksイベント定義 (permissions / hooks)
│   ├── settings.local.json             # [公式] 個人ローカル用設定 (※.gitignore対象)
│   ├── rules/                          # [公式] モジュール別・過去トラ再発防止ルール群 (*.md 自動読込)
│   │   ├── 01-project-conventions.md
│   │   └── 99-kanflow-lessons-learned.md # ★Kanflowに蓄積されたミスから自動生成されるガードレール
│   ├── agents/                         # [公式] 専門サブエージェント定義 (YAML frontmatter付きMarkdown)
│   │   ├── spec-reviewer.md
│   │   └── qa-tester.md
│   ├── skills/                         # [公式] スキル定義 (必ず <skill-name>/SKILL.md の階層で配置)
│   │   ├── fastapi-strict/
│   │   │   └── SKILL.md
│   │   └── i18n-sync/
│   │       └── SKILL.md
│   ├── commands/                       # [公式] カスタムスラッシュコマンド (/sync-kanflow 等)
│   │   └── sync-kanflow.md
│   └── hooks/                          # Hooksから呼び出される実行シェルスクリプト格納用
│       ├── session-start-kanflow.sh    # 起動時にKanflowから担当タスクと直近のミス事例を取得
│       ├── pre-tool-guard.sh           # 危険な操作や過去のNGパターンの実行前ブロック
│       └── post-commit-report.sh       # テスト結果とステータスをKanflowへ自動送信
├── .gitstation/
│   └── agent.lock.json                 # GitStationマーケットプレイスのロック定義ファイル
├── docs/specs/                         # GitStationで版管理される仕様書・ナレッジ群 (Markdown)
└── src/                                # アプリケーションコード（または設備仕様書本編）
```

### 4.3 公式Hooks仕様に準拠した `.claude/settings.json` 構造
Claude Codeのライフサイクルイベント（`SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`）にスクリプトをバインドし、人間の手動連絡やチェック漏れを物理的にゼロにする。

```json
{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "",
        "hooks": [
          {
            "type": "command",
            "command": "bash .claude/hooks/session-start-kanflow.sh"
          }
        ]
      }
    ],
    "PreToolUse": [
      {
        "matcher": "Write|Edit|MultiEdit|Bash",
        "hooks": [
          {
            "type": "command",
            "command": "bash .claude/hooks/pre-tool-guard.sh"
          }
        ]
      }
    ],
    "Stop": [
      {
        "matcher": "",
        "hooks": [
          {
            "type": "command",
            "command": "bash .claude/hooks/post-commit-report.sh"
          }
        ]
      }
    ]
  }
}
```

---

## 5. エージェントビルダー基本仕様（スキーマ＆アダプター駆動設計）

AIエージェントの進化は極めて速く、`.claude/` のフォルダ構成や新機能の追加、既存機能の廃止が頻繁に発生する。AI側の仕様変更のたびにGitStation本体を改修する事態を防ぐため、エージェントビルダーは**「抽象定義（IR）」と「ターゲット・アダプター（物理出力）」を完全分離したコンパイラ構造**とする。

### 5.1 エージェントビルダーの4大要件
1. **スキーマ駆動型・動的UI生成：** ビルダー画面のタブやチェックボックス群（Skills / Sub-agents / Rules / Hooks / MCP 等）はハードコードせず、アダプター定義ファイルから動的に描画する。将来、AIに新しい概念（例：`Memory` や `Guardrails`）が追加された場合、設定ファイルに1ブロック追記するだけでUIとローカルLLMの選別対象に自動反映される。
2. **ターゲット・アダプターによる出力分離：** マーケットプレイスの部品を「どのパスに」「どの形式で（ディレクトリ展開／単一ファイル結合／JSONキーへのマージ）」配置するかを `target-adapter.yaml` で制御する。
3. **廃止機能の安全なパージ（Deprecation）：** AI本体の進化により特定のHookやSkill形式が不要になった場合、アダプター側で `enabled: false` に設定するだけで、次回ビルド時に既存プロジェクトから古い設定ファイルを安全に削除する。
4. **Dry-Runプレビューとワンクリック世代移行：** ベースモデル（例：`Agent_Claudecode_ver2` から次世代版への更新）が上がった際、構成差分をプレビューした上で、既存プロジェクトの選択状態を維持したまま新構造へ再コンパイルするマイグレーションPRを自動生成する。

### 5.2 コアデータスキーマ定義（実装規格）

#### （A）プロジェクト内マニフェスト：`.gitstation/agent.lock.json`
```json
{
  "schema_version": "1.1",
  "project_id": "kanflow-proj-001",
  "project_type": "software_dev",
  "adapter_id": "claude-code-v2",
  "adapter_version": "2.2.0",
  "linked_specs": [
    "docs/specs/01_requirements.md",
    "docs/specs/02_architecture.md"
  ],
  "marketplace_items": {
    "skills": [
      {
        "id": "fastapi-strict",
        "version": "1.2.0",
        "auto_selected_by_spec": true,
        "reason": "仕様書内のバックエンドAPI要件より自動選択"
      },
      {
        "id": "i18n-sync",
        "version": "1.0.4",
        "auto_selected_by_spec": true,
        "reason": "多言語対応要件より自動選択"
      }
    ],
    "sub_agents": [
      { "id": "spec-reviewer", "version": "1.1.0" },
      { "id": "qa-tester", "version": "1.0.0" }
    ],
    "rules": [
      { "id": "01-project-conventions", "version": "1.0.0" },
      { "id": "99-kanflow-lessons-learned", "version": "auto-synced" }
    ],
    "hooks": [
      { "id": "session-start-kanflow", "event": "SessionStart" },
      { "id": "pre-tool-guard", "event": "PreToolUse" },
      { "id": "post-commit-report", "event": "Stop" }
    ]
  },
  "custom_rules": [
    "DBマイグレーション変更時は必ずロールバック用スクリプトを同梱すること",
    "UIコンポーネントは社内デザインシステムのトークンのみを使用すること"
  ]
}
```

#### （B）AI構成変化を吸収するアダプター定義：`target-adapter.yaml`
```yaml
adapter_id: "claude-code-v2"
adapter_version: "2.2.0"
description: "Agent_Claudecode_ver2 公式ディレクトリ完全準拠アダプター"

capability_categories:
  - id: "skills"
    label: "スキル (Skills)"
    output_strategy: "directory_copy"
    target_dir: ".claude/skills/{{item.id}}"           # 内部に SKILL.md を配置
    enabled: true

  - id: "sub_agents"
    label: "サブエージェント (Agents)"
    output_strategy: "file_copy"
    target_path: ".claude/agents/{{item.id}}.md"       # YAML frontmatter + プロンプト
    enabled: true

  - id: "rules"
    label: "モジュラールール・過去トラ教訓 (Rules)"
    output_strategy: "file_copy"
    target_path: ".claude/rules/{{item.id}}.md"        # チーム共有ガードレール
    enabled: true

  - id: "commands"
    label: "スラッシュコマンド (Commands)"
    output_strategy: "file_copy"
    target_path: ".claude/commands/{{item.id}}.md"
    enabled: true

  - id: "hooks"
    label: "ライフサイクルフック (Hooks)"
    output_strategy: "json_merge_and_script"
    target_file: ".claude/settings.json"
    script_dir: ".claude/hooks/"
    json_path: "$.hooks"
    enabled: true

  - id: "mcp_servers"
    label: "MCPサーバー連携 (MCP)"
    output_strategy: "json_merge"
    target_file: ".mcp.json"
    json_path: "$.mcpServers"
    enabled: true

root_context_file:
  target_path: "CLAUDE.md"
  sections:
    - "base_template"
    - "spec_summary"
    - "active_capabilities"
    - "custom_rules"
```

---

## 6. ソフトウェア開発から「設備仕様作成（Spec-as-Code）」への展開

GitStationの「非エンジニアでも使える文書版管理」とエージェントビルダーの「マルチプロファイル機能（`project_type` の切り替え）」により、本プラットフォームは**工場の生産設備仕様書（URS）、工程設計、保全基準書の作成・審査基盤**としてそのまま機能する。

| 構成要素 | ソフトウェア開発プロファイル (`software_dev`) | 設備仕様・工程設計プロファイル (`equipment_spec`) |
| :--- | :--- | :--- |
| **リポジトリ管理対象** | アプリケーションソースコード、API定義、DBスキーマ、システム仕様書 | **設備要求仕様書（URS）、工程フロー、FMEA、検収基準書、保全チェックリスト** |
| **注入されるスキル (`Skills`)** | 型安全チェック、UIデザイントークン規約、E2E自動テスト生成スキル | **安全規格（インターロック）基準、理論サイクルタイム計算、過去トラ照合、4M変動対策スキル** |
| **専門サブエージェント (`Sub-agents`)** | アーキテクト、セキュリティ監査官、QAテスター | **安全衛生審査官、保全性（MTTR）審査官、品質保証（異常時処理）審査官、多言語技術翻訳官** |
| **CI/CD パイプライン** | 自動ビルド・単体テスト実行・社内サーバーへのステージング環境デプロイ | **仕様書の矛盾・抜け漏れ自動検査（Spec Lint）、改訂履歴・承認欄付きPDF/Word帳票の自動ビルド** |

### 6.1 設備仕様作成における「仮想デザインレビュー（DR）」と2つの利用モード
* **過去トラ・標準基準の自動織り込み：** 設備担当者が「塗布・カメラ検査ユニットの導入」といった基本条件を入力すると、エージェントビルダーが「温湿度変動対策スキル」「光学外乱チェックリスト」等を自動提案する。執筆中は安全・保全・品質のサブエージェントが常時クロスチェック（仮想DR）を行い、非常停止時の仕掛け品処理や消耗品交換時間の記載漏れを自動で指摘・補完する。
* **Local Clone モードと Web Studio モードの両立：** パワーユーザーはPCへリポジトリをクローンしてローカルのClaude Codeで高速に仕様群を書き上げられる一方、一般の現場担当者や非エンジニアは、GitStationのWebブラウザ上の文書エディタを開くだけで、裏側で同じ `.claude/` スキル群を読み込んだAIアシスタントと対話しながら仕様書を作成・改訂できる。

---

## 7. チーム分散開発における「ミス防止・自己進化フィードバックループ」

複数メンバーが分散してタスクを進める際、「あるメンバー（またはAI）が犯したミス・手戻り・仕様の誤解」を個人の経験で終わらせず、**即座にチーム全員のAIエージェントへ免疫（予防ルール）として共有・強制する循環システム**を実装する。

### 7.1 Kanflowを起点とする5段階の再発防止ループ（Team Immunity Loop）

1. **① ミス・手戻りの入力と蓄積（Kanflow）：**
   分散タスクの進行中に起きた不具合、PR差し戻し、テスト失敗、または設備仕様の考慮漏れを、Kanflowのタスクカード上の「Incident / 手戻り記録」フィールド（または差し戻しコメント）に入力する。Kanflowはこれをプロジェクト横断の「失敗・教訓データベース（Lessons Learned DB）」として構造化蓄積する。
2. **② ローカルLLMによる「原因の抽象化とガードレール変換」（Kanflow ➔ GitStation）：**
   Kanflow内蔵のローカルLLMが入力されたミス事例を解析し、固有名詞に依存しない**「具体的な再発防止ルール（禁止事項・必須チェック項目）」**へ自動変換する。
   * *変換例：「メンバーAがタイムゾーンを考慮せず日時をDB保存してバグ発生」 ➔ ルール化：『日時データは必ずUTCで保存し、表示層でのみローカルタイムゾーン変換を行うこと』*
3. **③ エージェント資産への即時注入（GitStationの自動コンパイル）：**
   変換された予防ルールはGitStationへ自動送信され、深刻度と性質に応じて以下の3箇所へ自動的に組み込まれる（PRまたは自動コミット）：
   * **知識・制約の共有：** `.claude/rules/99-kanflow-lessons-learned.md` に防止ルールを自動追記。
   * **物理ブロックの追加：** コマンドや特定コードパターンで検知可能なミスは、`.claude/hooks/pre-tool-guard.sh`（`PreToolUse` フック）の検査条件に追加し、AIが同じミスを書こうとした瞬間にツール実行をブロックして書き直させる。
   * **全社資産化：** 他のプロジェクトでも起こりうる汎用的なミスは、マーケットプレイスの該当 `SKILL.md` や審査サブエージェントのチェック項目へ昇格させる。
4. **④ 他メンバー環境へのリアルタイム同期（Hooksによる自動伝播）：**
   別タスクを担当している他のチームメンバーが自分のPCでClaude Codeを起動（またはプロンプトを入力）した際、`SessionStart` フック（`session-start-kanflow.sh`）が自動的に走る。このフックがバックグラウンドで最新の `.claude/rules/` をプルすると同時に、KanflowのAPIから「現在自分が担当しているタスクに類似する過去のミス事例」をフェッチし、セッション冒頭のコンテキストに自動注入する。
5. **⑤ 事前警告（Spec-Gate への還流）：**
   以降、別のメンバーがKanflowで類似のタスクを起票・着手しようとすると、KanflowのローカルLLMが「過去に別タスク（#104）で〇〇の考慮漏れが発生しています。今回の受け入れ条件に自動で追加しました」と事前に対策を織り込む。

### 7.2 ミスの種類別・反映形態マトリクス

| ミスの種類 | Kanflowへの入力トリガー | 他メンバーのClaude Codeへの反映形態 |
| :--- | :--- | :--- |
| **実装・設計上のミス**<br>（API仕様ズレ・型不整合等） | PRレビューでの差し戻しコメント、またはタスクの「手戻り（Rework）」ラベル付与 | `.claude/rules/99-kanflow-lessons-learned.md` へのルール追記 ＋ `PreToolUse` フックによる該当パターンの書き込みブロック |
| **要件・仕様の考慮漏れ**<br>（例外処理・権限漏れ等） | 検収時のバグ起票、またはプレビュー画面でのピン止め修正指摘 | Kanflowの「Spec-Gate」チェックリストへの自動追加 ＋ 審査サブエージェント（`.claude/agents/`）の監査項目更新 |
| **設備仕様の過去トラ再発**<br>（安全インターロック・保全性漏れ） | デザインレビュー（DR）での指摘事項登録、現地立ち上げ時の不具合記録 | 設備用スキル（`.claude/skills/*/SKILL.md`）の必須記載項目への昇格（次回以降、AIが最初から仕様書に記述） |

---

## 8. システム間連携API・Webhook インターフェース仕様（実装ガイド）

Claude Codeで各システム（GitStation / Kanflow）の連携モジュールを実装する際の標準エンドポイント定義である。

### 8.1 Kanflow 側提供 API（GitStation / ローカルHooks からの参照用）
* `GET /api/v1/projects/{project_id}/active-context?user={git_user}`
  * **用途：** ローカルPCの `.claude/hooks/session-start-kanflow.sh` から呼び出し、現在担当中のタスク要件・受け入れ条件・関連仕様書パス・直近のチーム内注意喚起（ミス事例）を取得する。
* `POST /api/v1/tasks/{task_id}/status`
  * **用途：** `.claude/hooks/post-commit-report.sh` またはGitStationのCI/CDから呼び出し、テスト結果・プレビューURL・画面エビデンスを添付してタスクを「Review（検収待ち）」へ自動遷移させる。
* `POST /api/v1/incidents/extract-rule`
  * **用途：** タスク差し戻しやミス記録時にKanflow内蔵ローカルLLMを起動し、再発防止ルールを生成してGitStationのAgent Builder APIへ送信する。

### 8.2 GitStation 側提供 API（Kanflow / Agent Builder 用）
* `POST /api/v1/repos/{repo_id}/agent-builder/recommend`
  * **用途：** リポジトリ内の `docs/specs/*.md`（仕様書）をローカルLLMで解析し、マーケットプレイス内の推奨 `skills`, `sub_agents`, `rules`, `hooks` のリストを選出する。
* `POST /api/v1/repos/{repo_id}/agent-builder/compile`
  * **用途：** 選択されたアイテムと `target-adapter.yaml` に基づき、対象リポジトリへ `.gitstation/agent.lock.json`、`.claude/` 一式、`.mcp.json`、`CLAUDE.md` を生成・コミットする。
* `POST /api/v1/repos/{repo_id}/rules/inject-lesson`
  * **用途：** Kanflowから送信された再発防止ルールを `.claude/rules/99-kanflow-lessons-learned.md` および `.claude/hooks/pre-tool-guard.sh` に追記・自動コミットする。