# 引き継ぎ 2026-11-01 — リポジトリの種類（spec / system）と文書ポータルをやめる（文書だけの MR）

利用者の決定（2026-11-01、チャット）を、計画と ADR に反映した。**コードは変えていない**（コードの削除は後続の MR）。

## 1. 現在の状態

| 項目 | 値 |
|---|---|
| ブランチ | `docs/docs-drop-repo-kind-and-portal`（`5c84942` から）。**未 push** |
| ADR | **ADR-0023** `リポジトリの種類（spec / system）と文書ポータルをやめる`（`docs/adr/0023-drop-repository-kinds-and-document-portal.md`） |
| 変更の種類 | 文書だけ（`docs/`、`README.md`）。`CLAUDE.md` は触っていない |
| ゲート | `gen-catalog --check`（影響なしの見込み。結果は下の §5）。コードがないので fmt / clippy / nextest は対象外 |

## 2. 決定

1. **文書ポータルをやめる。** 画面の系統はリポジトリの画面 1 つ。利用者は Git の基本を学んでから GitStation を使う。M2 の MR12（実装・統合済み）は取り下げ（superseded）。M7「文書ポータル」の別の系統も作らない。
2. **リポジトリの種類 `spec` / `system` の区別をやめる。** すべてのリポジトリは同じ種類。仕様はそのリポジトリの `spec/`（計画 03 の案 A を既定）。複数のシステムで共有する仕様は、ふつうのリポジトリに置き、`.forge/repo.yml` の `specs: - repo / version / scope` で版を固定して参照する。「仕様を持つか」は中身から導く。
3. **リポジトリをまたぐ連携は、まだ実装しない。M3 の別項目にした。** 最初の部分の順序: (a) `.forge/repo.yml` の解析と仕様の参照 → (b) 仕様のずれの通知（`forge/spec-drift`）→ (c) Kanflow のプロジェクト ↔ 複数のリポジトリの登録。残すもの: 要件 ID、`@req`、spec-lint / trace / conformance / drift、AI 向けの文脈 API。

## 3. 変更したファイル

| ファイル | 内容 |
|---|---|
| `docs/adr/0023-drop-repository-kinds-and-document-portal.md` | 新規。背景、決定、代替案（隠す / 統合 / ラベルだけ）と却下の理由、影響、後続のコードの MR の範囲 |
| `docs/adr/0004-…`、`0010-…`、`0015-…`、`0017-…`、`0022-…` | 末尾に Amendment（0004 は Status の行も）。本文は書き換えていない |
| `docs/adr/README.md` | 0023 を追加。0004 に「種類の部分は 0023 が置き換え」 |
| `docs/plan/00-master-plan.md`、`01-product-plan.md`、`02-kanflow-integration.md`、`03-spec-driven-development.md`、`08-ux-simplicity.md`、`09-integrated-platform-design.md`、`10-m2-plan.md` | 冒頭に 2026-11-01 の注記。取り消した所は打ち消し線で残した。01 §3.1・§3.7・§8（M3 / M7）、03 §2〜§4.4・§7〜§9（新しい §8.1）、10 §4.7・MR12・§9 など |
| `docs/plan/07-implementation.md`、`07-team-vibe-coding.md`、`README.md` | 文書ポータルへの言及を取り消し（最小限） |
| `docs/deploy/test-server-guide.md` | ポータルと `kind` が **明日のデプロイの `f95e2e8` には含まれるが、取り下げが決まっている** こと。`gstn project create` の例から `--kind` を外した |

- 「IoT プラットフォームのポータル」（Keycloak の共通の入口。00 §6、09 §8、ADR-0021）は **別のもので変わらない**。
- M2 の完了の条件（10 §9）から「文書ポータルの使いやすさの確かめ」を外した。

## 4. 後続のコードの MR の範囲

ADR-0023 の「後続のコードの MR の範囲」と同じ。ブランチ名の案: `refactor/web-drop-repo-kind-and-portal`（Rust と web を分けてもよい）。

| 領域 | 内容 |
|---|---|
| DB | `forge.repositories.kind`（CHECK `spec` / `system`）は **残す**（マイグレーションは追記のみ）。作成時に入れる値を決める（固定値 `system`、または新しいマイグレーションで既定値を足す）。列の値に意味を持たせない |
| forge-core / forge-db | `RepositoryKind`（`crates/forge-core/src/repository.rs` ほか）、作成の入力、`crates/forge-db/src/projects/`・`repositories.rs`、`audit_details`（`ProjectCreated` の `kind`）、カーソルの purpose |
| API（forge-web） | `POST /api/v1/projects` の `kind`、`GET /api/v1/projects?kind=`、応答の `kind` を除く（`routes/lists.rs`、`project_writes.rs`、`projects_tests.rs`、`lists_tests.rs`、`tests/lists.rs`）。OpenAPI（utoipa）、`docs/authz/routes.generated.json`・`matrix.generated.json` の再生成（`gen-authz`）、`web/` の生成型（`gen:api`） |
| `gstn` | `project create --kind` を除く。`crates/forge-client/src/{cli,registry,coverage}.rs`、`tests/hosting.rs`、`tests/snapshots/help.json`。**C-1 のカバレッジの表の行**を直す |
| web | 作成フォームの種類の選択、MR12 の文書ポータル（`web/src/pages/portal/`、`web/src/features/portal.ts`、`web/src/api/portal.ts`、ルート、辞書の `portal.*` と `plain.*`、禁止語の検査、e2e）、ホームの仕様書の一覧（`pages/Home.tsx`）、`ProjectPage.tsx` / `RepoViews.tsx` の `kind` 分岐 |
| テスト | 上の各所のテストの更新と削除。権限の表の行（`kind` の絞り込みの allow / deny）。web の vitest / Playwright（axe 0 を保つ） |
| 文書 | `crates/forge-core/README.md` ほか crate の README、`ops/` の runbook に `kind` があれば |
| ゲート | `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo nextest run --workspace -j 4`（dev DB を起動してから）、web の lint / typecheck / test、`gen-catalog --check`、`authz-coverage` |

## 5. 確かめたこと

- `cargo run -q -p forge-errors --bin gen-catalog -- --check`: `catalog up to date (99 files)`（終了コード 0）。
- 触った ADR・計画・引き継ぎ・ガイドの相対リンク（Markdown のリンク）は、すべて存在する（スクリプトで確認、不正 0）。

## 6. 注意

- **エージェントモデルの側へ書き戻す**: この MR で変えた `docs/plan/` は、メンテナの PC の `docs/plan/` に **コピーして戻す** 必要がある（CLAUDE.md「Where to start」）。対象: `00`、`01`、`02`、`03`、`07-implementation`、`07-team-vibe-coding`、`08`、`09`、`10`。
- メンテナの PC の側の `CLAUDE.md` / ルール（`gitstation_agent` の `forge-routing.md` など）に、文書ポータルや `kind` の前提があれば直す（この MR では触っていない）。
- **明日のテストサーバーのデプロイは `f95e2e8`**。文書ポータルと `kind` を含むが、試験の対象にしない（`docs/deploy/test-server-guide.md` に書いた）。
- Kanflow 側（`docs/platform-integration-iotpf.md`、`ui-kit` など）に、文書ポータルや `spec` の種類に触れる記述があれば別途直す（この MR は GitStation のリポジトリだけ）。

## 7. 未決定

- 非エンジニアが仕様を承認する経路（旧 M7 の承認フロー、Office / PDF の比較、変更の要約）を、リポジトリの画面の機能として作るか。R3（全社展開）の条件の読み替え。今は計画に入れていない。
- 作成時の `kind` の列に入れる値（固定値か、既定値のマイグレーションか）。
- 共有する仕様のリポジトリを複数の Kanflow のプロジェクトに結びつけてよい条件（03 §7.1 の書き換えの詳細。(c) の設計で決める）。

---

## 8. 後続のコードの MR（実施済み）— ブランチ `refactor/cross-drop-repo-kind-and-portal`

ADR-0023 の「後続のコードの MR の範囲」を実施した（`8ec0dcb` から。**未 push**）。2 つのコミット: Rust（API・core・db・gstn）と web。

### 8.1 決めたこと

| 項目 | 決定 |
|---|---|
| 作成の本文 | `POST /api/v1/projects` から `kind` を除いた。本文は元から `deny_unknown_fields`（厳密な JSON）なので、**古い `kind` を付けると `400 FG-WEB-0006` で拒否**する（黙って無視しない。「失敗を隠さない」に沿う）。テスト: `hosting_routes.rs` |
| 一覧の `?kind=` | 既存の規則どおり、知らない引数は `param=query rule=unknown_parameter`（`400 FG-WEB-0006`）。プロジェクト・グループの両方。テスト: `lists_tests.rs`（`the_removed_kind_filter_is_an_unknown_parameter`）、`tests/lists.rs` |
| 応答 | `ProjectBody` から `kind` を除いた（`openapi.json`、`web/src/api/schema.gen.ts` を再生成）。`project.created` / `repository.created` の監査・イベントの payload からも `kind` を除いた |
| DB | マイグレーションは追加しない。列 `forge.repositories.kind`（NOT NULL + CHECK）は残り、`projects::create` が**定数 `'system'` を明示的に書く**（`LEGACY_REPOSITORY_KIND`）。どこも読まない。テスト `a_project_under_a_group_keeps_its_visibility_and_stores_the_constant_kind` が保存値を確かめる |
| カーソル | 絞り込みの purpose から `kind=` を除いた（`…:membership=…:state=…`）。古い絞り込み付きカーソルは `rule=tag` で拒否される（未公開のため互換の期間は置かない） |
| エラー | コードは変えていない。`FG-DB-0021` の `unknown_repository_kind`（理由の文言のみ）を `errors.toml` から削除し、`gen-catalog` で再生成。`ops/runbooks/namespace-data.md` も直した |
| 権限の表 | ルートは変わらない（`gen-authz --check`: up to date） |

### 8.2 取り除いたもの

- **forge-core**: `RepositoryKind`、`ProjectRecord.kind`、`AuditDetails::{ProjectCreated,RepositoryCreated}.kind`。**forge-db**: 作成の `NewProject.kind`、読み出し（`resolve` / `lists` / `repositories`）の `kind`。**forge-hosting**: `NewProjectFields.kind`。**forge-web**: `KindName`、`ProjectBody.kind`、一覧の `kind` 絞り込み。
- **gstn**: `project create --kind`、`kind` の列・欄（`registry.rs` のスキーマ、`help.json` の snapshot を再生成）。カバレッジの表（`coverage.rs`）は `kind` を含まないため変更なし。
- **web**: 作成フォームの種類の選択、文書ポータル（`pages/portal/`、`features/portal.ts`、`api/portal.ts`、`/{project}/-/docs`、`tests/portal*.ts*`、`scripts/journey-portal.mjs`、journey の 6d）、ホームの「仕様書」、プロジェクト画面の種類のバッジと「仕様書として開く」、一覧の種類のバッジ、辞書の `plain.*` / `portal.*` / `kind.*` / `form.kind` / `home.specs`（ja / en / id）、`tests/i18n.test.ts` の `portal.` の id 必須。ポータルと一緒に、禁止語の検査（`portal.test.tsx` 内）もなくなった。MR11 / MR12 の Step-0 の修正（Tokens の離脱ガードなど）は残した。`/{project}/-/docs` は未知の画面語として 404 になる。
- `Markdown` の `linkTo` / `headingOffset` / `rawRef` はポータルのために入れたが、`BlobScreen` が `rawRef` / `headingOffset` を使うので残した（`linkTo` は現在どこも使わない。コメントだけ直した）。

### 8.3 確かめたこと

| ゲート | 結果 |
|---|---|
| `cargo fmt --all -- --check` | 通過 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通過 |
| `gen-catalog --check` / `gen-authz --check` | up to date（99 files）/ up to date |
| `cargo nextest run -p forge-client` | 99 / 99 通過（help の snapshot 更新後） |
| `cargo nextest run -p forge-core -p forge-errors` | 173 / 173 通過 |
| `cargo nextest run -p forge-web --lib` | 179 / 179 通過 |
| web: lint / typecheck | 通過（警告 0） |
| web: `npm test`（vitest） | 21 ファイル 362 / 362 通過 |
| web: build / i18n:report / smoke / journey / licenses | 通過（id の欠落 0 / 370 キー、CSP 違反 0、axe 違反 0、578 パッケージ許可済み） |

**実行できなかったもの（このセッションは Docker がなく、開発用 PostgreSQL が起動できなかった）**: DB を使うテスト — `forge-db` 全体（`projects/tests.rs`、`resolve_tests.rs` の変更を含む）、`forge-hosting`、`forge-web` の統合テスト（`tests/lists.rs`、`tests/hosting_routes.rs`）、`forge-cli`。これらは**コンパイルは通る**（`cargo check --workspace --all-targets` と clippy）が、実行していない。PostgreSQL のある環境で `cargo nextest run -p forge-db -p forge-hosting -p forge-web -p forge-cli -j 2` を実行すること。

### 8.4 注意

- `docs/deploy/test-server-guide.md` の `--kind` の記述は、明日（2026-11-02）にデプロイする `f95e2e8` のビルドに合わせたものなので変えていない。このブランチをデプロイする場合は直す。
- メンテナの PC 側の `CLAUDE.md` / ルールに `kind` や文書ポータルの前提があれば別途直す（この MR は触っていない）。
