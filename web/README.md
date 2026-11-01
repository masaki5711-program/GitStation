# web — GitStation の画面（ADR-0022）

Vite + React + TypeScript（strict）、React Router、TanStack Query、i18next。ビルドしたもの（`dist/`）は
forge-web の feature `embed-ui` で `forge` に埋め込まれ、殻は `/`以下のどのパスでも同じバイト、資産は `/-/assets/{file}` で配られる。

## 使い方

```bash
npm ci --ignore-scripts      # 依存の install スクリプトは実行しない（.npmrc でも止めている）
npm run lint                 # eslint（react/jsx-no-literals、Radix は src/ui/ だけ）
npm run typecheck            # tsc --noEmit
npm test                     # vitest（i18n のゲート、トークンのコントラスト、ブランドの複製、画面）
npm run build                # dist/ と .br / .gz（scripts/compress.mjs が形も検査する）
npm run gen:api              # docs/api/openapi.json → src/api/schema.gen.ts（手で直さない。CI が差分を見る）
npm run licenses             # 依存のライセンスの許可リスト
npm run i18n:report          # id の欠けたキーの数（失敗にしない）
npm run smoke                # Chromium で CSP の違反・コンソールのエラーがないこと（playwright が要る）
npm run journey              # Chromium でログイン → ホーム → プロジェクト → 作成 → ログアウト。axe を全ルールで実行（SHOTS=dir で画面を保存）
npm run dev                  # Vite。/api と /-/auth を FORGE_DEV_SERVER（既定 http://127.0.0.1:8080）へ中継
```

埋め込んだバイナリ: `npm run build` の後に `cargo build --release -p forge-cli --features embed-ui`。

## 決まり（詳細は ADR-0022）

- JSX に文字列を直書きしない。文言は `src/i18n/{ja,en,id}.json`（ja と en は全キー必須、id は欠けると en で表示）。
- 色・余白・フォントは `src/styles/tokens.css` の `--gs-*` だけを使う（`tokens.json` は同じ値の写し）。
- 失敗の表示は `ErrorNotice` だけ。見せるのはサーバーの `user_message` と `request_id` だけ（`error_code` は見せない）。
- 実行時に `<style>` を差し込むもの（CSS-in-JS、Radix の一部）は使わない。Radix は `src/ui/` の包みからだけ使う。
- ロゴ・アイコンは `docs/brand/` の複製（`src/assets/brand/`。テストが SHA-256 を比べる）。
- 依存は版を固定し、公開から 2 週間たった版だけを入れる。

## 画面の構成（M2 MR9）

- `/` ホーム、`/-/projects/{id}` 固定リンク、それ以外のパスは `pages/Namespace.tsx` が resolve（`prefix=true`）で判別する
  （プロジェクト・グループ・個人。GitHub 形の `/{project}/tree/…` は `/-/tree/…` へ `replace`、移動したパスは正規のパスへ）。
- セッションがなければ、同じ URL のままログイン画面（`next` = いまのページ）。ログインは `GET /-/auth/login` を
  `redirect: 'manual'` で確かめてから移る（503 FG-AUTH-0018 は「ログイン未設定」の 1 行）。ログアウトは `/-/auth/logout` への
  フォームの POST（`csrf` の隠しフィールド）。状態を変える API 要求には `X-GitStation-CSRF`（`api/client.ts`）。
- 作成（プロジェクト・グループ・サブグループ）は `ui/Modal.tsx`（HTML の `<dialog>`）の中のフォーム。
- テストは `tests/mock.ts`（`fetch` の経路表。MSW は使わない）。axe は vitest（jsdom、色の比は除く）と `npm run journey`（実ブラウザ、全ルール）。

## リポジトリの画面（M2 MR10）

- `pages/repo/RepoViews.tsx` が `/{project}/-/{tree|blob|blame|commits|commit|compare|branches|tags}/…` を受ける。`settings` / `members` は MR11 まで「まだ使えません」。
- 住所の `{ref}[/{path}]` は 1 つの尾として `resolve` API に渡し、ref とパスの境目はサーバーが決める（`api/repo.ts`）。以降の API には解決した完全な ref 名を渡す。
- Markdown は `features/Markdown.tsx`（`react-markdown` + `remark-gfm`。生の HTML は捨てる。リンクは http(s)・mailto・相対だけ、画像は同じリポジトリの raw の png / jpeg / gif / webp だけ）。別チャンク（`MarkdownView.tsx` が遅延読み込み）。
- 構文の色は `features/highlight.ts`（`lowlight` = highlight.js の木。クラスだけ、`style` 属性なし。言語は個別のチャンク、20 万文字を超えるファイルは色なし）。
- 差分は `features/DiffView.tsx`。切り詰め・バイナリ・大きすぎ・UTF-8 でないものは文で言う。
- `tests/repo-fixtures.ts` の合成データ（vitest）と `scripts/journey-repo.mjs`（Playwright のスタンドイン）は同じ形。
