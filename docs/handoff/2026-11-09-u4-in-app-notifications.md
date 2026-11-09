# 引き継ぎ — M3 U4 アプリ内の通知（2026-11-09）

前の引き継ぎ: `2026-11-09-next-session.md`。計画 11 §5.7 の U4 `feat/ui-in-app-notifications`（「アプリ内の通知」、評価役は UI の 5 つ）。
設計の記録は **ADR-0016 Amendment U4**（末尾）。計画のファイル（`docs/plan/`）は変えていない。

## 1. ブランチとコミット

- ブランチ `feat/ui-in-app-notifications`（`origin/main` `a264369` から）。**push していない**。コミットは §6。

## 2. 作ったもの

- **記録**: N1 の購読者 `notify` が、メールの行と同じ手順・同じ受け手（MR の作者、自分の操作は除く）で `forge.in_app_notifications` に 1 行
  （`UNIQUE (event_id, user_id)`）。書くのは `forge_notify::access::decide_in_app` が通したときだけ（有効な人間・アプリ内を止めていない・
  いま MR を読める）。**`notify.enabled = false`（`Notify::paused`）でもアプリ内は記録する**（メールは記録しない。ここが N1 からの振る舞いの変更）。
- **読むたびに見えるかを判断し直す**: 一覧・未読の数は MR を 1 文でまとめて読み、プロジェクトごとに 1 回 `load_project`、行ごとに
  `prove(MergeRequestRead)`。見えない行は一覧にも数にも出さない（行は残す。権限が戻れば再び出る）。
- **API**（すべて `authorize()`、行列に許可と拒否）: `GET /api/v1/user/notifications?before=&limit=&unread=`、`GET …/unread-count`
  （`NotificationInboxRead`、`read_api`）、`POST …/{id}/read`、`POST …/read-all`（`{"up_to"?}`。`NotificationInboxUpdate`、`api`）。
  資源は N1 の `Resource::NotificationPreferences { owner }`。他人の行と存在しない行は同じ `FG-REPO-0005`（管理者でも）。
- **設定**: `in_app_enabled`・`in_app_muted_kinds`（PUT で省略可、省略は保存値を保つ）。画面のアカウント設定 → 通知のタブに追加。
- **保持**: ワーカーのタスク `in_app_notification_retention`（1 時間ごと）。既読から 30 日・作成から 180 日で削除。
- **gstn**: `notifications list [--unread] [--before] [--limit]`、`notifications count`、`notifications read <id>|--all`、`set --in-app on|off`。
  C-1 の表（`coverage.rs`）に 4 ルート、`help.json` のスナップショットを更新。
- **画面**: ヘッダーのベル（`src/components/NotificationBell.tsx`。未読の数をリンクの名前に、見た目の数字は aria-hidden、60 秒ごとに問い合わせ、
  失敗が続くと倍々で最長 15 分、401 で止める、増えたら polite のライブリージョン）、通知の一覧 `/-/notifications`（`src/pages/Inbox.tsx`。
  すべて / 未読、1 文の説明と MR へのリンク、リンクで既読、1 件ずつ・すべて既読、さらに表示、空・読み込み中・失敗）。ja / en / id（id の欠け 0）。
  サーバーにシェルのルート `GET|HEAD /-/notifications`（`WebShell`）。
- マイグレーション `20261201100000_in_app_notifications.sql`（追記）、`tables.toml`（`derived`）、`bootstrap/schema.rs` の権限
  （forge_app に SELECT/INSERT/UPDATE/DELETE、forge_ro に SELECT）。新しいエラーコードは無い。

## 3. 確かめたこと（このセッションで実行）

- `cargo fmt --all -- --check`: 合格。
- `cargo clippy --workspace --all-targets -- -D warnings`: 合格。
- `cargo run -q -p forge-errors --bin gen-catalog -- --check`: 「catalog up to date (166 files)」。
- `cargo run -q -p forge-web --bin gen-authz -- --check`: 「authz manifests and OpenAPI are up to date」。
- `cargo nextest run -p forge-auth -p forge-db -p forge-notify -p forge-client -j 2`: **1247 / 1247 合格**。
- `cargo nextest run -p forge-web -p forge-cli -j 2`: **682 / 682 合格**。
  （DB は 127.0.0.1:55432 の素の PostgreSQL 16。ワークスペース全体は流していない。触っていない forge-core・forge-git・forge-hosting・
  forge-review・forge-checks・forge-integ・forge-oidc・forge-errors は未実行）
- web: `npm run lint`・`typecheck` 合格、`npm test` **792 / 792**（46 ファイル）、`build`、`i18n:report`（id の欠け 0 / 861）、`smoke`（CSP・
  コンソールの誤り無し）、`journey`（axe 86 画面すべて clean、keycloak の journey は環境が無くスキップ）、`licenses`（578 件すべて許可）。

## 4. 決めたこと・計画からのずれ

1. **表は N1 と分けた**（N1 Amendment §10 (2) は「同じ表に `read_at`」）。メールの行は配信の状態機械と at-most-once を持つため。
   **`forge.notifications`（メールの行）の保持は未決のまま**（U4 の範囲外）。
2. 未読の数は「新しい未読 100 件のうち見えるもの」（`capped`）。見えない未読が多いと実数より少なく出ることがある（受け入れた）。
3. 保持の日数（30 / 180）は定数。設定の鍵は作っていない。
4. ベルの数の失敗は画面に理由を出さない（ベルの名前が「未読の数を読めませんでした」になる。理由と request_id は一覧の画面が出す）。
5. 評価役（typescript-reviewer、a11y-architect、i18n-sync-auditor、design-reviewer、e2e-runner、加えて authz-matrix-auditor・database-reviewer
   が妥当）の独立レビューは**まだ**。

## 5. 次にやること

- 評価役のレビュー → 指摘の修正 → `main` へ。
- 全体のテスト（PC の `cargo nextest run --workspace -j 4`）は流していない（触ったクレートだけ）。
- 後で: メールの行の保持、「レビューの依頼」・メンションの種類（イベントが出たら `NotificationKind` と両方の表の CHECK を一緒に）。

## 6. コミット

| コミット | 中身 |
|---|---|
| `05a47ee` | feat(ui): API・記録・保持（Rust、マイグレーション、gstn、生成物） |
| `3fe3b33` | feat(ui): ベルと通知の一覧の画面（web） |
| （この文書のコミット） | docs: ADR-0016 Amendment U4 とこの引き継ぎ |
