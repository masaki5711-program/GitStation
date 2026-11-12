# web — GitStation の画面（ADR-0022）

Vite + React + TypeScript（strict）、React Router、TanStack Query、i18next。ビルドしたもの（`dist/`）は
forge-web の feature `embed-ui` で `forge` に埋め込まれ、殻は `/`以下のどのパスでも同じバイト、資産は `/-/assets/{file}` で配られる。

## 使い方

```bash
npm ci --ignore-scripts      # 依存の install スクリプトは実行しない（.npmrc でも止めている）
npm run lint                 # eslint（react/jsx-no-literals、Radix は src/ui/ だけ）
npm run typecheck            # tsc --noEmit
npm test                     # vitest（i18n のゲート、トークンのコントラスト、ブランドの複製、画面）
npm run build                # dist/ と .br / .gz（scripts/compress.mjs が形も検査する）、初回ロードの JS の予算（scripts/bundle-budget.mjs）
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

## 速さ（目標: どの画面も 100 ms 以内）

- **先読み**（`features/prefetch.ts`）: サイドバーの項目・プロジェクトのタブ・ファイル一覧の行・コミット一覧・MR 一覧のリンク
  （`components/PrefetchLink.tsx`）に、ポインタが 50 ms 乗るか、フォーカスかタッチで、行き先の画面が最初に読むクエリを
  同じキーで先に読む。1 つの行き先につき同時に 1 本だけ。Save-Data や 2G 回線では読まない。先読みの失敗は画面には出さず、
  画面を開いたときにもう一度読む（失敗すれば `ErrorNotice` に `request_id`）。遅延チャンクの画面（設定・パイプライン・編集など）は
  そのチャンクも先に読む。
- **プロジェクトのトップ**: 描画のあと、ブラウザが暇なとき（`requestIdleCallback`）に既定ブランチの履歴の 1 ページ目と
  開いている MR の一覧を読む（README はファイル一覧が読む）。
- **キャッシュ**（`api/cachePolicy.ts`）: 完全なコミット ID で指したもの（そのコミットの木・ファイル・コミット、
  MR の版の差分）は変わらないので `staleTime: Infinity`、閉じた後も 30 分持つ。ブランチ名・タグ名で指したもの、
  人が打った名前どうしの比較（ブランチ名がコミット ID の形でもありうる）はアプリ既定の 30 秒（それを過ぎたら、手元の値を
  すぐ見せつつ読み直す）。
- **パスの解決**（`api/namespaces.ts`）: `GET /api/v1/namespaces/resolve` の答えはパスごとに 5 分持ち、同じプロジェクトの
  タブの間で使い回す（`/{project}` と `/{project}/-/…` の 2 つの形も互いの答えを使う）。移動・改名（transfer）・削除・
  復元の後は全部捨て、公開範囲の変更・メンバーの削除・別のユーザーでのサインインの後は読み直す。失敗した答え（404 など）は
  キャッシュから出さない。
- **チャンクが読めないとき**: en / id の文言が読めなければ ja で起動し（選んだ言語は読めたときだけ覚える）、画面のチャンクが
  読めなければ「もう一度」付きのエラーを出して、押すと読み直す。先読みは同時に 3 つまで、裏のタブでは読まない。
- **分割**（`pages/lazyScreens.tsx`）: 設定・メンバー・Webhook・パイプライン・編集・代理承認のポップアップは別チャンク。
  初回は `SkeletonList` を出す。Markdown と構文の言語は前から別チャンク。文言は ja だけ本体に入れ、en / id は選んだときに
  読む別チャンク（`i18n/index.ts`。読めなければ起動を失敗させる）。本体は 910 kB → 645 kB（gzip 268 kB → 199 kB）。

### 測りかた

```bash
npm run build                # 最後に scripts/bundle-budget.mjs が初回ロードの JS の大きさを出す（予算を超えると失敗）
BUDGET_KB=600 node scripts/bundle-budget.mjs dist   # 予算を変えて確かめる
```

実際の画面の速さはブラウザで測る:

1. Chrome の DevTools → Network（Disable cache は外す）→ プロジェクトを開く。
2. サイドバーのタブにポインタを乗せる → `merge-requests` や `commits` の要求が**クリックの前に**出ることを確かめる。
3. Performance パネルで記録しながらタブをクリック → クリックから画面の内容が描かれるまで（Interaction の長さ）が 100 ms 以内か見る。
   同じプロジェクトのタブの間で `namespaces/resolve` が出ないこと、コミット ID のページへ戻ったとき要求が出ないことも見る。
4. 遅い回線の確認は Network の Throttling（Slow 4G）。Save-Data の確認は DevTools の Network conditions では出せないので
   `navigator.connection` を持つ Android の Chrome の「ライトモード」で。
