# 00 全体計画の索引と、実装の開始手順（2026-10-31 整理）

**新しいセッションは、まずこのファイルを読む。** GitStation・Kanflow・エージェントモデルの計画を 1 枚に整理したもの。詳しい内容は各計画書にあり、ここは「何がどこにあり、どの順で作るか」だけを書く。全体像の図は `docs/overview/gitstation-kanflow-overview.pdf`（スライドは同じ場所の `.pptx`。W12 と 08 はまだ入っていない）。

## 1. 作るもの（完成形）

社内（オンプレ）で、複数の開発者と複数の AI セッションが同時に、PBI から本番までを速く安全に進める仕組み。システム開発だけでなく、設備導入・工程改善のプロジェクトも同じ流れで扱う（仕様書は Markdown）。

| 部品 | 役割 | リポジトリ |
|---|---|---|
| **Kanflow** | 入口。PBI・スプリント・全体計画、MCP、作業の宣言、承認待ち、フィードバックの輪（教訓）、リリース | `masaki5711-program/Kanflow` |
| **GitStation** | Git の置き場所、MR、CI/CD、マージキュー、自動マージ、デプロイ、仕様書の版と文書ポータル | `masaki5711-program/GitStation` |
| **エージェントモデル** | Claude Code のやり方。TMUX 版（並行）と Ver2（単独）、共通プラグイン `ymma-dev-core` | `Agent_model_TMUX`、`Agent_Claudecode_ver2`（旧 `Agent_Claudecode` は連携ができるまで現役、`GitStation_Agent` は GitStation の開発用） |
| ローカル LLM（Qwen） | 仕様との照合、要約、下書き | 社内のサーバー |

## 2. 計画書の地図

| 文書 | 中身 | 使う時 |
|---|---|---|
| `01-product-plan.md` | GitStation の製品の計画（範囲 A〜D、M1〜M8） | 製品の範囲を確かめる |
| `02-kanflow-integration.md` | GitStation と Kanflow の連携（イベント、API、§13 連携の順序） | 連携を作る |
| `03-spec-driven-development.md` | 仕様駆動の開発（spec / system のリポジトリ、要件 ID） | 仕様・W12 |
| `04-ci-cd-merge-safety.md` | CI/CD、マージキュー、自動マージの 4 方式、危険の検査 | M4・W8 |
| `05-step-d-plan.md` | **GitStation の今の実装計画**（D1〜D5、M1 phase 8 の MR1〜MR8） | GitStation の実装 |
| `06-ai-driven-dev-environment.md` | AI 駆動の開発環境（D-1〜D-7、第 0〜2 段） | 全体の考え方 |
| `07-team-vibe-coding.md` | **チームで速く・壊れにくく**（W1〜W12、目標の数値、順序、決めたこと） | 連携とエージェントモデルの実装 |
| `07-implementation.md` | 07 の GitStation 側の詳細とリポジトリごとの分担 | 同上 |
| `08-ux-simplicity.md` | UI の方針（入口 1 つ、承認待ち 1 か所、言い換え、使いやすさの確かめ方） | 画面を作る |
| Kanflow `docs/plan/team-dev-implementation.md` | Kanflow 側の詳細（CI、ボード、触る範囲、AC の ID、フィードバックの輪、W12） | Kanflow の実装 |
| Kanflow `docs/plan/ux-simplicity.md` | 08 の写し | Kanflow の画面 |
| TMUX 版・Ver2 `docs/team-dev-implementation.md` | エージェントモデル側の詳細（core、宣言の自動化、引き継ぎ、窓口、spec-authoring） | エージェントモデルの実装 |
| `docs/adr/` | 設計の決定（ADR-0015 は URL と名前空間、phase 8 の Amendment あり） | 設計を変える前 |
| `docs/handoff/2026-10-30-phase8-mr1-mr4.md` | **直前の作業の引き継ぎ**（未統合のブランチ、残りの指摘） | 再開のとき |

## 3. 今の状態（2026-10-31）

| 対象 | 状態 | ブランチ |
|---|---|---|
| GitStation M1 | 約 70%。D1〜D4、phase 8 MR1〜MR3 を統合済み。全体のテスト 868 件中 867 件合格（1 件は既知の不安定なテスト） | `claude/brave-cray-w62c35` |
| GitStation MR4（Web の振り分け） | 実装・レビュー済み。**MEDIUM 2 件の修正待ち** | `feat/web-dispatcher` |
| 不安定なテストの修正 | **未検証の作業中のコミット** | `fix/db-poison-test-flake` |
| Kanflow 連携 第 0 段 | 約 65%。MCP、K1〜K5（`dev:write`、作業の宣言）を統合済み。テスト 8413 件合格。**実 DB（MySQL 8.4）のテストと E2E は未実行** | `claude/brave-cray-w62c35` |
| エージェントモデル | TMUX 版・Ver2 に Kanflow の MCP と作業の宣言を組み込み済み | `feat/Lcross-NOPBI-kanflow-mcp` |
| 計画 | 07（W1〜W12）、08（UI）、各リポジトリの詳細の文書を配布済み | 各リポジトリの上のブランチ |

**main への統合**: GitStation はユーザーが PC 側でブランチを分け直して `main` に入れる（コミットの作成者の名前もそのとき直す）。Kanflow とエージェントモデルも、PC 側の MR（GitLab）で入れる。

## 4. 作る順序（全体）

| 段 | GitStation | Kanflow | エージェントモデル |
|---|---|---|---|
| **第 0 段（今から 1〜2 週）** | MR4 の修正と統合 → 不安定なテストの検証 → **W2 の CI** → MR5 ∥ phase 9（clone / push） | **W2 の CI**（実 DB と E2E）→ W3 ボードの担当中の印 → 08 の「今どこ」と承認待ち（Kanflow の中から） | **W1 共通プラグイン `ymma-dev-core`** の切り出し → W3 宣言の自動化 → W6 引き継ぎの自動化 → W7 レビューの規則 |
| **第 1 段（2〜4 週）** | phase 8b（書き込みの API）→ D5 → phase 10 | W4 触る範囲 → W5 AC の ID → **W11 フィードバックの輪** → W12（`equipment`、全体計画の MCP） | W4・W5 の対応、W11 の窓口への切り替え、W12 の `spec-authoring` とベースのリポジトリ・出力のスクリプト |
| **第 2 段（M2〜M4）** | M2 画面・文書ポータル、M3 連携の土台、M4 CI/CD・マージキュー・自動マージ、W12 の版の公開のゲート | W9 GitStationSource（イベント）、W10 ローカル LLM、承認待ちに GitStation を合流、SSO | 旧版から TMUX 版 / Ver2 へ切り替え |

切り替えの条件（旧 Agent_Claudecode → TMUX 版 / Ver2）: GitStation で clone / push と MR ができる、Kanflow 連携の第 0 段が実環境で動く、core を取り込んで CI が緑。1 チームで 1 スプリント試してから全体を切り替える。

## 5. 明日（2026-11-01）午前の開始手順

### 5.1 準備（30 分）
1. 各リポジトリを最新にする（ブランチは §3 の表）。GitStation は `claude/brave-cray-w62c35`、`feat/web-dispatcher`、`fix/db-poison-test-flake` を取得。
2. GitStation の開発用 DB: `docker compose -f ops/dev/compose.yml up -d --wait`（Postgres 127.0.0.1:55432）。
3. Kanflow のトークンを `dev:write` 込みで発行し直し、`/kanflow setup`（エージェントモデル）。
4. この計画の第 0 段を Kanflow に PBI として登録する（下の §5.2 の 1 つ 1 つが PBI 候補）。プロジェクトに PBI がない作業は、エージェントモデルの規則どおり `NOPBI` のブランチ名にする。

### 5.2 最初に着手する作業（並行できる 3 本）

| 順 | 作業 | リポジトリ | 完了の条件 | 参照 |
|---|---|---|---|---|
| A-1 | MR4 の MEDIUM 2 件（混ざった語のグループの `prove()`、タイムアウトの合計の検査）と LOW を直して統合、全体のテスト | GitStation | 全体のテストが緑、統合して push | 引き継ぎ §2 |
| A-2 | 不安定なテストの修正を検証（fmt / clippy / forge-db のテスト / 負荷をかけた繰り返し）して統合 | GitStation | 負荷で繰り返しても落ちない | 引き継ぎ §2 |
| B-1 | GitHub Actions の CI（fmt、clippy、Postgres 付きのテスト、gen-catalog / gen-authz の確認、cargo deny / audit） | GitStation | PR で CI が回り、main の保護に入る | 07-implementation §1 |
| B-2 | GitHub Actions の CI（tsc、eslint、vitest、MySQL 8.4 付きの DB テスト、Playwright） | Kanflow | K5 の DB テスト（6 人の同時の宣言）が実 DB で緑 | Kanflow team-dev-implementation §1 |
| C-1 | 共通プラグイン `ymma-dev-core` の新しいリポジトリを作り、`kanflow-integration`・`/kanflow`・`lib/kanflow.js`・`kanflow-setup.js` を移す | 新規 + TMUX 版 / Ver2 | 両モデルが core を取り込み、`self-heal --check` が緑 | エージェントモデル team-dev-implementation §2 |

並行の目安: A は 1 セッション（GitStation の worktree）、B は 1 セッション（CI のファイルだけ）、C は 1 セッション。Rust のビルドは同時に 2 つまで（ディスク）。

### 5.3 その後（第 0 段の残り）
GitStation MR5 ∥ phase 9 → Kanflow W3（ボードの担当中の印）と 08 の「今どこ」→ エージェントモデル W3（宣言の自動化）・W6（引き継ぎ）・W7（レビューの規則）。

## 6. 決めたこと（2026-10-31 まで）

| 項目 | 決定 |
|---|---|
| 進め方 | 推奨案で進める（ユーザーの指示） |
| エージェントモデル | 旧版は連携ができるまで現役（不具合の修正だけ）、その後 TMUX 版（並行）と Ver2（単独）へ。共通部分はプラグイン `ymma-dev-core` で配る |
| フィードバックの輪 | エージェントモデルではなく Kanflow のサーバーの機能にする（W11）。今の mistake-memory は取り込んで廃止 |
| 作業の宣言 | 別の人・別のセッションは 409 で拒否、4 時間で失効、PO / SM が解除できる。スコープ名は `dev:write`（計画書の `write:dev`） |
| 仕様書 | Markdown に統一し、Word / Excel / PDF は出力のスクリプトで作る。プロジェクトの開始時にベースのリポジトリ（決めるべき項目・ベースのスケジュール）を連携し、Claude Code と仕様を作る。ベースのスケジュールが Kanflow の全体計画の元になる |
| 工程改善 | 文章の改善フレームワークの仕様とつなぎ、Kanflow のナレッジで沿っているかを確かめる |
| UI | 入口は Kanflow、承認待ちは 1 か所、Git の言葉は言い換える（08） |
| CI | いまは GitHub Actions、GitStation の CI ができたら移す |
| 自動マージ | 4 方式（既定 `reserve`）、危険（stop）と本番は必ず人 |
| Kanflow の DB | 運用中なので、**本番サーバー（PFS）へ移すときに PostgreSQL へ移す**（ユーザーの決定、2026-10-31）。それまでの新しい表（W4・W11・W12）は今の MySQL 8.4 で作り、Prisma の型だけを使って MySQL に固有の SQL を書かない（移行のときに PostgreSQL の最初のマイグレーションを作り直す） |

## 7. まだ決めていないこと（推奨案）

| 項目 | 推奨 |
|---|---|
| `ymma-dev-core` の置き場所 | GitHub の非公開リポジトリ（GitStation ができたら移す） |
| プロジェクトの種類 `equipment` | 足す（今は `software` / `process_improvement` / `general`） |
| GitStation_Agent への詳細の文書の配布 | PC 側で写す（このクラウドのセッションには書き込みの権限がなかった） |
| PDF とスライドの更新 | W12 と 08 を入れて作り直す（必要なとき） |

## 8. 守ること（全リポジトリ共通）

- 各リポジトリの CLAUDE.md の契約（GitStation: エラーの契約、権限の一点の判断、失敗の握りつぶしの禁止、マイグレーションは追記だけ、本番は人の承認、オンプレ）。
- 計画 → テストを先に → 実装 → ゲート → 独立レビュー（CRITICAL / HIGH は直してから統合）→ 文書 → コミットはユーザーの指示で。
- ブランチ名・コミットの規則は各リポジトリの規則（GitStation `<type>/<area>-<topic>`、エージェントモデル `<type>/L<layer>-(PBI-<n>|NOPBI)-<topic>`）。
- 計画を変えたら、引き継ぎに書いて PC 側の `docs/plan/` に写す。
