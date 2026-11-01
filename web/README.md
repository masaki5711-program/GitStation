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
