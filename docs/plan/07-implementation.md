# 07 の詳細実装（GitStation 側と、リポジトリごとの分担）

作成: 2026-10-31（クラウドのセッション）。計画は `07-team-vibe-coding.md`（W1〜W11）。全体像の図は `docs/overview/gitstation-kanflow-overview.pdf`。

## 0. リポジトリごとの分担と文書の場所

| リポジトリ | 作るもの | 詳細の文書 |
|---|---|---|
| GitStation | M1〜M4 の製品の機能、連携の土台、CI、マージキュー、自動マージ、イベント | この文書 |
| Kanflow | CI、作業中の表示、触る範囲、受入基準の ID、**フィードバックの輪（W11）**、GitStationSource、指標 | Kanflow の `docs/plan/team-dev-implementation.md` |
| Agent_model_TMUX / Agent_Claudecode_ver2 | 共通プラグイン `ymma-dev-core`（W1）、宣言の自動化、引き継ぎ、レビューの規則、Kanflow への失敗の報告（窓口） | 各リポジトリの `docs/team-dev-implementation.md` |
| 旧 Agent_Claudecode | 不具合の修正だけ（連携ができるまで使い続ける） | なし |
| GitStation_Agent | GitStation 自体の開発のモデル。W2 の CI のゲート、W11 の窓口を取り込む | 未配布（このセッションに書き込みの権限なし。PC 側で写す） |

## 1. W2 GitStation の CI（最優先）

- `.github/workflows/ci.yml`（GitStation の CI ができたら移す）:
  1. `cargo fmt --all -- --check`
  2. `cargo clippy --workspace --all-targets -- -D warnings`
  3. Postgres 16 のサービスコンテナ → `FORGE_TEST_DATABASE_URL=… cargo nextest run --workspace -j 4 --no-fail-fast`
  4. `cargo run -q -p forge-errors --bin gen-catalog -- --check`、`cargo run -q -p forge-web --bin gen-authz -- --check`
  5. `cargo deny check`、`cargo audit`
  6. エージェントモデルのゲート（`validate-authz-coverage.js`、`validate-error-registry.js`）は GitStation_Agent を取得して実行
- nextest の再実行で通ったテストを「不安定」として記録（`--retries 1` と JUnit の出力を解析）。隔離はせず、Kanflow に PBI を起票。
- 失敗したジョブは Kanflow の `report_failure` に送る（サービスアカウント）。
- 完了の条件: `main` へのマージは CI が緑のときだけ（ブランチ保護）。

## 2. 製品の順序（変えない）

M1 phase 8 MR4（修正待ち）→ MR5（`GET|HEAD /api/v1/projects/{id}`、resolve に `project_id`）∥ phase 9（Smart HTTP: clone / push、FG-WEB の service unsupported、実 git の E2E）→ 8b（書き込みの API、`forge-hosting`）→ D5 → phase 10。詳細は `05-step-d-plan.md`。

## 3. M3 連携の土台（W9 の前提）

- 管理者の履歴（`VisibleSet::Instance`、`FeedCursor`）とサービスアカウントのトークン。
- `integrations` / `integration_bindings`、`prove_visible`、FG-INT-0007/0008/0012、権限の表の行（`02` §13）。
- 連携の履歴の API（`…/cursor`、不透明なカーソル、AEAD）。Kanflow の `GitStationSource` がこれを購読する。
- イベントに PBI 番号（ブランチ名・MR の題名から、Kanflow の K1 と同じ正規表現 `(?<![a-z0-9])pbi[-_ ]?#?(\d{1,7})(?![0-9])`）を付ける。

## 4. M4 CI/CD とマージの安全（W8）

- MR（`04` M-1 / M-2）、マージキュー（M-3）、自動マージの 4 方式 `merge.auto = off | reserve | conditional | full`（M-5、既定 `reserve`）、危険の検査（M-6、`stop` はどの方式でも人の承認）、main が壊れたときの原因の特定と revert の下書き（M-4）。
- 自動マージは GitStation の「自動マージ」の主体が行う。AI のトークン（`is_agent`）にマージの権限を与えない。
- MR の画面に AC の対応表（`PBI-<n>/AC-<m>` のテストの結果）を出し、Kanflow に送る（W5）。
- 本番デプロイは人の承認が必須。成功したら `deployment.succeeded` を Kanflow に（リリースの更新）。

## 5. W11 との関係

- エラーの目録（`errors.toml`）は正のまま。Kanflow の教訓は FG コードでリンクする。
- 障害の振り返りの「フィードバックの行動」は、Kanflow の教訓と PBI に登録する（`ops/incidents/` のテンプレートに欄を足す）。
- M3 以降、パイプライン・マージキュー・障害の失敗を Kanflow の `POST /api/v1/feedback/failures` に送る（`forge-integ` の送信の処理、タイムアウト付き、失敗は FG-INT のコードでログ）。

## 6. W12 仕様書（設備導入・工程改善を含む）

- **ベースのリポジトリ**: リポジトリの種類 `spec` のテンプレートとして登録する（プロジェクトの種類ごとに 1 つ）。中身は `spec/*.md`（Markdown に統一）、`checklist.yml`（決めるべき項目: ID・説明・必須・担当の役割。支払い条件・検収の条件・納期・保証・安全など）、`schedule.yml`（ベースのスケジュール: フェーズ・マイルストーン・相対の期間・依存）、`export.yml` と出力のスクリプト。
- **作成**: Kanflow のプロジェクトの作成から、テンプレートで仕様のリポジトリを作る（M3 の連携の API。それまでは手で複製）。
- **版の公開のゲート**: `checklist.yml` の必須の項目に「未決定」が残っていると、版の公開（タグ）を拒否する必須チェック。新しいエラーコードを `errors.toml` に登録する（番号は登録時に決める）。
- **出力のジョブ**: 版の公開のとき、Markdown から Word（社内のひな形）・Excel・PDF を作り、成果物として添付する。文書ポータル（範囲 D）から Git を知らない人が受け取り、承認する。
- **イベント**: `spec.released`（版・承認者・未決定の項目の数）を Kanflow へ。

## 7. 完了の条件（GitStation 側）

| W | 条件 |
|---|---|
| W2 | CI が緑でないとマージできない。未検証の統合が 0 |
| W8 | 人の判断が 4 か所（計画の承認、`stop` の解除、PO の完了の確定、本番の承認）だけで PBI が本番に届く |
| W9 | MR・パイプライン・デプロイの変化が数秒で Kanflow に出る |
| W11 | GitStation の失敗が Kanflow の教訓の照合に乗る |
| W12 | 必須の項目が未決定の仕様は公開できない。公開すると Word / PDF が自動で作られ、Kanflow に版が出る |
