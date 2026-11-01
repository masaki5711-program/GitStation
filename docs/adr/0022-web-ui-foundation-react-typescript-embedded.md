# ADR-0022: Web の画面の土台（React + TypeScript、埋め込みの配信）

**Date**: 2026-11-01
**Status**: accepted（計画 10 §3.1 とともにユーザーが承認、2026-11-01。M2 の MR1 で記録）
**Deciders**: Masaki Fujikawa（ユーザー）、Claude

## 背景

M2 で最初の画面を作る（計画 10）。計画 01 §7 は、React + TypeScript、OpenAPI から生成した API クライアント、headless の部品、
`forge serve` への埋め込み（単一バイナリ）、日本語・英語・インドネシア語を決めている。決まっていないのは、ツールの組み合わせ、
埋め込みの仕組みと開発・テストの時の扱い、資産の URL、CSP と安全のヘッダー、フォントとブランドの色、i18n の規則、
Markdown の描き方、npm の取り寄せ方である。

前提として確かめたこと（作業ツリー、2026-11-01）:

- 殻（`WebShell`、公開）は今、仮の `index.html` を **どのパスにも同じバイト** で返す（ADR-0015。DB も資格情報も見ない）。
- 資産の置き場所は ADR-0015 で `/assets/…` か `/-/static/…` とだけ決めている。`assets` はトップレベルの予約語にある。
  `/-/` の後ろのインスタンスの語は今 `projects`（固定リンク）だけ（`forge_core::web_path::InstanceRoute`）。
- ロゴとアイコンの正は **`docs/brand/`**（`gitstation-v1-*.svg`、色の表つきの README。ユーザーの指示 2026-10-30「必ずこのフォルダの
  ファイルを使う」）。`docs/overview/logo-light.png` と `icon.png` は同じ v1 の PNG で、色を測ると README の値と一致する
  （ティール `#22B8A0`、ネイビー `#1B2A44`、濃いティール `#128F7C`、オレンジ `#FF7A45`。量子化の誤差の範囲）。
  計画 10 §0 の「ロゴは `docs/overview/` にしかない」は誤りで、ここで直す。
- サーバーの利用者向けのエラーは既に ja / en / id の `user_message` を持つ（`forge_errors::Lang`）。
- GitHub Actions はまだ一度も動いていない（計画 10 §8）。

## 決定

### 1. ツールと置き場所

- 置き場所は `web/`（リポジトリの直下。maintainer の PC では `Git/web/`）。
- Vite、React、TypeScript（`strict`、`noUncheckedIndexedAccess`）、React Router、TanStack Query、Radix（headless）、
  CSS Modules と CSS 変数のトークン。
- **実行時に `<style>` を差し込む CSS-in-JS は使わない**（CSP の `style-src 'self'` を保つ）。React の `style` 属性は CSSOM 経由で
  CSP の `style-src` に掛からないので許す。**Radix の部品のうち `<style>` を差し込むものは使わない**（§3a。2026-11-01 の設計レビューの
  MEDIUM で、「Radix の部品も同じ理由で使える」とした最初の文を改めた）。
- API の型は `docs/api/openapi.json` から **`openapi-typescript` と `openapi-fetch` で生成する**（手書きしない）。生成物はコミットし、
  CI で再生成して差分がないことを確かめる（`gen-catalog --check` と同じ考え方）。
- テスト: vitest、MSW、axe（a11y）、Playwright（E2E。認証は ADR-0021 §14 の `sso` の profile）。
- lint: eslint（`react/jsx-no-literals` で JSX の直書きの文字列を止める。§6）、`tsc --noEmit`。
- サーバーの依存の向き（CLAUDE.md §3.9）は変わらない。`web/` は Rust のクレートに依存せず、API（OpenAPI）だけを見る。

### 2. 配信: forge-web に埋め込む

- `rust-embed` を forge-web の feature **`embed-ui`** で使う。**cargo は npm を起動しない**（`build.rs` で npm を呼ばない。
  Rust のビルドがネットワークと Node に依存しないため）。
- CI の web のジョブが `web/dist` を作り、release のビルドが `--features embed-ui` で埋め込む。
  feature があるのに `web/dist/index.html` がなければ **ビルドを失敗させる**（空の UI の配布物を作らない）。
- feature なし（開発、nextest、`cargo build --workspace`）では、今の仮の殻を返す。
  `forge doctor` は `web.ui=placeholder` を **warn** で報告する（黙らない）。`embed-ui` のときは `web.ui=embedded` と
  埋め込んだ `index.html` の SHA-256 の先頭 12 桁を info で出す（配布物の取り違えを診断できるように）。
- 殻（`WebShell`）は、埋め込んだ `index.html` を **どのパスにも同じバイトで** 返す（ADR-0015 を保つ。DB も資格情報も見ない。
  セッションに依存するもの・CSRF のトークンを含めない）。`Cache-Control: no-cache`（資産の名前が変わったら次の読み込みで新しい殻を取る）。
- **資産は新しいインスタンスの語 `/-/assets/{file}`** で配る（ADR-0015 の Amendment M2）。Action は `WebShell`（公開）、Resource は `Instance`。
  - 埋め込みの一覧にない名前は **FG-WEB-0001**（404、`reason=unknown_route`、`part=instance`）。殻は返さない。
  - 名前は Vite が付ける内容のハッシュ入りのもの（`index-<hash>.js` など）。1 セグメントだけ（サブディレクトリを作らない設定にする）。
  - `Cache-Control: public, max-age=31536000, immutable`。
  - ビルドの時に作った br / gzip を、`Accept-Encoding` に合わせて出す（`Vary: Accept-Encoding`）。実行時には圧縮しない。
  - `Content-Type` は拡張子の許可リスト（`js`、`css`、`woff2`、`svg`、`png`、`ico`、`json`、`txt`）から決める。それ以外の拡張子は
    埋め込みの時に拒否する（ビルドの失敗）。`svg` はブランドの自前の資産だけで、リポジトリの中身の SVG は表示しない（ADR-0015）。
  - favicon はトップレベルの `/favicon.ico` に置かず、殻の `<link rel="icon" href="/-/assets/…">` で示す（`favicon.ico` は予約語のまま）。
- 開発: `npm run dev` の Vite が殻と資産を自分で配り、`/api` と `/-/auth` を `forge serve` へ中継する
  （同じオリジンなので Cookie はそのまま。`web.base_url` は Vite のオリジンにする）。

### 3. CSP と安全のヘッダー

1 つのミドルウェア **`security_headers`**（forge-web）がすべての応答に付ける。ルートの種類（殻、資産、API、raw / archive、git、
`/-/auth`）ごとの表を 1 か所に持ち、テストで各応答のヘッダーを確かめる。

| 応答 | `Content-Security-Policy` | ほか |
|---|---|---|
| 殻（`index.html`） | `default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self' <Keycloak のオリジン>` | `Cache-Control: no-cache`。`form-action` の Keycloak のオリジンは、ログアウトのフォームの 303 の先（ADR-0021 §8。リダイレクトにも `form-action` が掛かる）。設定から作るヘッダーの値で、殻のバイトは変わらない。`auth.oidc` がなければ `'self'` だけ |
| 資産 `/-/assets/*` | 殻と同じ | `Cache-Control: public, max-age=31536000, immutable`、`Cross-Origin-Resource-Policy: same-origin` |
| API `/api/v1/*` | `default-src 'none'; frame-ancestors 'none'` | 認証付きは `Cache-Control: private, no-store`、`Vary: Authorization, Cookie` |
| raw・画像・archive | `sandbox; default-src 'none'`（ADR-0015） | `Cross-Origin-Resource-Policy: same-origin`、`Content-Disposition`（ADR-0015） |
| `/-/auth/*` | `default-src 'none'; frame-ancestors 'none'` | `Cache-Control: no-store` |
| Smart HTTP | 付けない（git のクライアントは見ない） | 今のまま |
| すべて | — | `X-Content-Type-Options: nosniff`、`Referrer-Policy: same-origin`、`Cross-Origin-Opener-Policy: same-origin`、`Permissions-Policy: camera=(), microphone=(), geolocation=(), usb=(), payment=()` |

- `Referrer-Policy: same-origin`: 内部のパス（プロジェクトの名前、ファイルのパス）を Keycloak や Kanflow に `Referer` で渡さない。
- **HSTS** は設定 `web.hsts`（既定は付けない。TLS をどこで終えるかで決まるため）。値は `max-age` の秒数で、`includeSubDomains` は付けない
  （同じドメインの他のアプリに影響させない）。
- **インラインのスクリプトはない。`eval`（`'unsafe-eval'`）も WASM（`'wasm-unsafe-eval'`）も使わない。**
  - 構文の色付けは highlight.js の core と、言語の定義を遅延で読み込む（shiki は WASM を使うので採らない）。
  - Vite の設定で、モジュールの preload のインラインのスクリプトを出さない（`build.modulePreload.polyfill = false`）。
- ブラウザのコンソールに CSP の違反が出ないことを、Playwright の素の起動で確かめる（MR2）。CSP の報告の受け口（`report-to`）は作らない（見送り）。

### 3a. `<style>` を差し込む部品（Radix）と CSP

Radix の一部の部品は、実行時に `<style>` の要素を差し込む。`style-src 'self'`（`style-src-elem` もこれに従う）はそれを止めるので、
そのまま使うと部品が壊れるか、CSP の違反になる。

- 差し込むもの（2026-11-01 の確認。版で変わりうるので MR2 で実物を確かめる）:
  - `Dialog`・`AlertDialog`、`modal` の `Popover`・`DropdownMenu`・`Select` など: 中の `react-remove-scroll`（`react-style-singleton`）が、
    スクロールを止めるための `<style>` を作る。中身には実行時に測ったスクロールバーの幅（`padding-right: <n>px`）が入る。
  - `ScrollArea`: スクロールバーを隠す `<style>` を差し込む（中身は固定の文字列）。
- 選択肢:

  | 案 | 内容 | 判断 |
  |---|---|---|
  | nonce | 応答ごとの nonce を CSP と殻の `<meta>`（`react-style-singleton` が読む）に入れる | **採らない**。nonce は応答ごとに違う値を殻の HTML に書くので、殻が「どのパスでも同じバイト」でなくなる（ADR-0015。殻が要求ごとに変わるとキャッシュの検証と、「殻は要求の内容に依存しない」ことのテストが成り立たない）。殻を DB・セッションに依存させない原則には反しないが、同じバイトの契約を崩す |
  | `style-src-elem` のハッシュ | 差し込む中身の SHA-256 を CSP に並べる | **採らない**。`react-remove-scroll` の中身はスクロールバーの幅で変わり、ハッシュを決められない。`ScrollArea` の固定の中身には使えるが、依存の版が上がるたびにハッシュが黙ってずれる |
  | **部品を絞る・置き換える** | `<style>` を差し込まない部品だけを使う | **採用**。CSP と殻の契約をどちらも変えない |

- 決定（採用した案の中身）:
  - **モーダルは HTML の `<dialog>` 要素**（`showModal()`。フォーカスの閉じ込め、`Esc`、背景の `inert` はブラウザが行う）を、自前の薄い
    部品（`web/src/ui/Modal.tsx`）で包んで使う。スクロールの停止は CSS（`html:has(dialog[open]) { overflow: hidden; }`、
    `scrollbar-gutter: stable` で幅のずれを防ぐ）で行う。Radix の `Dialog`・`AlertDialog` は使わない。
  - **メニュー・ポップオーバー・選択は `modal={false}`** で使う（`react-remove-scroll` を使わない）。`modal` を既定のまま使えないよう、
    自前の包み（`web/src/ui/`）だけが Radix を import できるようにする（eslint の `no-restricted-imports`）。
  - **`ScrollArea` は使わない**（ネイティブの `overflow: auto` と CSS の `scrollbar-*`）。
  - 使ってよい Radix の部品は `web/src/ui/` の許可リストに書く。部品を足すときは、その部品を描いた画面で CSP の違反が出ないことを確かめる。
- 守り:
  - **MR2 の Playwright の「CSP の違反が出ない」検査を残し、広げる**: 素の起動に加えて、`web/src/ui/` の各部品を開いた状態の見本の画面
    （開発用のルート。release には入れない）を巡回し、`securitypolicyviolation` のイベントとコンソールの CSP の違反が 0 であること。
  - vitest で「`document.head` に `<style>` の要素が増えない」ことを、各部品を開いて閉じる操作で確かめる（jsdom の上で、CSP がなくても見つける）。
  - 依存の更新の MR は、この 2 つが緑であることを条件にする。

### 4. フォントとブランド

- **外部の CDN は使わない**（オンプレ。CLAUDE.md §3.8）。M2 はシステムのフォントで、配るバイトは 0。
  - 本文: `"Yu Gothic UI", "Meiryo", "Hiragino Sans", "Noto Sans JP", system-ui, sans-serif`
  - 等幅: `"BIZ UDGothic", "Consolas", "Menlo", "Noto Sans Mono", ui-monospace, monospace`
- 自前のフォントが要るときは、ライセンスを確かめた woff2 を `web/` に同梱し、`/-/assets` で配る（`font-src 'self'`）。
- **ロゴとアイコンは `docs/brand/gitstation-v1-*.svg` を正とする**。ビルドの時に `web/src/assets/brand/` へ写す（手で描き直さない。
  写したものがずれていないことを CI で SHA-256 の比較で確かめる）。`docs/overview/logo-light.png`・`icon.png` は資料用の PNG で、
  色の確認に使った（背景を参照）。favicon は `gitstation-v1-icon.svg` と、そこから作った PNG（32 px、180 px）。
- **色のトークン**は `web/src/styles/tokens.css` の 1 か所で定める。起点は `docs/brand/README.md` の値:

  | トークン | 値 | 用途 |
  |---|---|---|
  | `--gs-brand-navy` | `#1B2A44` | ヘッダーの背景、見出しの文字 |
  | `--gs-brand-teal` | `#22B8A0` | 主な強調（大きい面・線。白地の文字には使わない） |
  | `--gs-brand-teal-strong` | `#128F7C` | 白地の上のリンク・主なボタンの文字と背景 |
  | `--gs-brand-orange` | `#FF7A45` | 注意の強調（文字ではなく面や線） |
  | `--gs-brand-paper` | `#F4FBFC` | 淡い背景 |

  - 文字と背景の組は WCAG 2.2 AA のコントラスト（本文 4.5:1、大きい文字 3:1）を満たす組だけをトークンにする（vitest で比を計算して確かめる）。
    ティールとオレンジは白地の本文には足りないので、文字には `teal-strong` とネイビーを使う。
  - 状態の色（成功・警告・失敗）は別のトークンにし、色だけで状態を伝えない（文字とアイコンを併せる）。
  - トークンは Kanflow と共有できる形（接頭辞つきの CSS 変数と、同じ内容の JSON）にする（計画 08 §4）。
- ダークテーマは M2 では作らない（計画 10 §1）。ロゴは light 版を使う。

### 5. 共通のヘッダーとポータル

- 画面の上部の共通のヘッダー（アプリの切り替え、利用者、ログアウト）は、IoT プラットフォームのポータルの形にそろえる（計画 09 §8.4）。
  ポータルの部品を実行時に外から読み込むことはしない（CSP、オンプレ）。リンクの先は設定で渡す（`web.portal_url`、なければ切り替えを出さない）。

### 6. i18n

- `i18next`（`react-i18next`）と、辞書 `web/src/i18n/{ja,en,id}.json`。既定は ja、ブラウザの言語で en / id を選ぶ。利用者が選んだ言語は
  ブラウザの中だけに覚える。API には `Accept-Language` で渡し、サーバーのエラーの `user_message` も同じ言語で受け取る。
- 規則:
  - **ja と en は全部のキーが必須**（vitest のゲート。片方にないキー、使っていないキー、空の値は失敗）。
  - **id は欠けていたら en で表示**し、欠けている数を CI に出す（失敗にはしない）。
  - **文書ポータル、ログイン、エラーの画面は M2 で id まで埋める**（YMMA の現場が読むため。この範囲は id の欠けも失敗にする）。
  - 計画 08 の言い換えの表（Git の言葉 → 画面の言葉）は、辞書の `plain.*` のキーとして持つ。文書ポータルの画面に Git の言葉が出ないことを、
    辞書の禁止語の検査で確かめる（MR12）。
  - JSX の直書きの文字列は eslint の `react/jsx-no-literals` で止める（記号・数字の許可リストだけを例外にする）。
  - 日時は `Intl.DateTimeFormat`（タイムゾーンはブラウザのもの、表示に UTC の差を併せる）。数は `Intl.NumberFormat`。
- サーバーが返す `error_code` と `request_id` は翻訳しない（そのまま表示し、コピーできるようにする）。

### 7. Markdown の描き方（HTML を描かない）

- `react-markdown` と `remark-gfm`（表、取り消し線、タスクの一覧、自動リンク）で描く。**HTML は描かない**: `rehype-raw` を使わず、
  `skipHtml` を有効にする（生の HTML は文字としても出さず捨てる）。
- リンクは `http:`・`https:`・`mailto:` と相対パスだけ（`urlTransform` で許可リストに合わないものは捨てる。`javascript:`、`data:`、
  `vbscript:` を通さない）。外部のリンクは `rel="noopener noreferrer"`。
- 画像は **同じリポジトリの raw** だけ（相対パスを `/-/raw/{ref}/{path}` に解決したもの。ADR-0015 の raw のヘッダーで返る）。
  外部の URL と `data:` の画像は描かず、リンクとして出す。SVG の画像は表示しない（raw が `octet-stream` にするので描かれない）。
- 見出しの `id` は利用者の文字列から作らず、`user-content-` の接頭辞を付ける（DOM の clobbering を防ぐ）。
- XSS のベクタ（`<script>`、`<img onerror>`、`javascript:` のリンク、`data:text/html`）が描かれないことを vitest で確かめる（MR10）。

### 8. npm の供給網（計画 10 §7 の Q5）

- **`package-lock.json` をコミット**し、インストールは常に **`npm ci --ignore-scripts`**（依存の install スクリプトを実行しない）。
  スクリプトが要る依存は採らないか、理由を書いて個別に許す。
- CI で `npm audit --audit-level=high`（失敗）と、ライセンスの検査（許可リストは `deny.toml` と同じ考え方: MIT、ISC、BSD-2/3、
  Apache-2.0、0BSD、CC0-1.0。ほかは失敗にしてレビューする）。
- 新しい依存は、`cargo deny` と同じく、保守・ライセンス・勧告を MR に書いてから足す。版は lockfile で固定し、公開から 2 週間たっていない版は
  採らない（乗っ取られた版の配布の窓を避ける）。
- **取り寄せ先**: 社内のミラー（Nexus / Verdaccio）があれば CI はそこから取る（`.npmrc` の `registry`。認証の情報はリポジトリに置かず
  CI の秘密で渡す）。**なければ当面は GitHub Actions がビルドの時だけ公開の npm から取る**。
- **実行時は外に依存しない**: 配布物は `forge` の 1 つのバイナリ（埋め込み）で、Node も npm も要らない。サーバーで `npm` を実行しない。
- 生成物（`web/dist`）はコミットしない。release の配布物と、その元のコミット・lockfile の SHA-256 を CI の記録に残す。

## 検討した案

| 案 | 内容 | 判断 |
|---|---|---|
| **A（採用）** | `rust-embed` で `forge` に埋め込む（feature `embed-ui`） | 配布物は 1 つ。殻の「どのパスでも同じバイト」を 1 か所で守れる |
| B | 別の静的サーバー（nginx）で配る | 配布物が 2 つになり、殻の規則とヘッダーを 2 か所で守ることになる |
| C | `build.rs` で npm を起動して埋め込む | cargo のビルドが Node とネットワークに依存し、nextest と clippy が遅くなる |
| D | CSS-in-JS（emotion など） | 実行時の `<style>` の差し込みに `'unsafe-inline'` か nonce が要る |
| E | shiki で構文の色付け | WASM に `'wasm-unsafe-eval'` が要る |

## 結果

- `forge` の 1 つのバイナリが UI を配り、Node は CI のビルドの時だけ要る。
- CSP は殻と資産で `'self'` だけ。インラインのスクリプト・`eval`・WASM がないので、XSS の影響を小さくできる。
- 開発と nextest は npm なしで回り、仮の殻であることを doctor が warn で知らせる。

**受け入れたリスク**

- 社内のミラーがない間、CI のビルドは公開の npm に依存する（実行時の依存ではない）。lockfile、`--ignore-scripts`、audit、2 週間の規則で抑える。
- システムのフォントに頼るので、OS によって見た目が少し違う。
- React の `style` 属性は CSP の `style-src` に掛からない。利用者の文字列から `style` を組み立てない（レビューの観点にする）。

## 見送り

- ダークテーマ（ロゴは dark 版が `docs/brand/` にある）。
- CSP の違反の報告の受け口。
- SSE（M3）。
- プレビュー用のオリジン（ADR-0015 の Amendment M2: M2 では作らない）。

## 影響するエラーコードと Runbook

- 新しいコードはない（`/-/assets` の不明な名前は既存の FG-WEB-0001）。
- `forge doctor` の `web.ui`（`placeholder` は warn）を足す。Runbook は `server-start-failed` に「UI が仮の殻のとき」の確認を足す（MR2）。

## Amendment（2026-11-01、M2 MR2: 実装で決めたこと）

MR2（`feat/web-spa-foundation`）で実物を作って分かったこと。上の本文は変えず、ここが優先する。

- **リンクの色**: `--gs-brand-teal-strong`（`#128F7C`）は白地で約 4.0:1 で、本文の AA（4.5:1）に足りない。
  白地の本文のリンクには新しいトークン `--gs-color-link`（`#0F7A69`、白で約 5.2:1）を使い、`teal-strong` は大きい文字
  （3:1）と面だけにする。`web/src/styles/tokens.json` の `textPairs` / `largeTextPairs` と vitest が比を確かめる。
- **ヘッダーのロゴ**: ヘッダーの背景はネイビーなので、`docs/brand/README.md`（暗い背景に light 版を置かない）に従い
  **dark 版**（`gitstation-v1-logo-dark.svg`、背景の矩形がヘッダーと同じネイビー）を使う。§4 の「ロゴは light 版」は、
  明るい背景（文書・印刷・ログインの明るい画面）のこととする。
- **ブランドの資産の写し**: ビルドの時に写すのではなく、`web/src/assets/brand/` に写しをコミットし、vitest が `docs/brand/` と
  SHA-256 を比べる（ビルドが `docs/` に依存しない）。
- **殻の CSP の `form-action` の IdP のオリジン**と **HSTS**: `forge_web::SecurityHeaders::new(hsts, form_action)` が受け取り、
  検証する（オリジンでない値は FG-SYS-0002）。設定のキー（`web.hsts`、`auth.oidc`）は `web.*` / `auth.*` の名前空間を作る
  MR3 / MR4 でつなぐ。それまでは `form-action 'self'` だけで、HSTS は出さない（`AppConfig::hsts` は既定 `None`）。
- **応答の種類の決め方**: 殻と資産はハンドラーが印（`ResponseClass`）を付け、ほかは生のパスで決める。印のない応答
  （資産のパスの FG-WEB-0001 など）は厳しい「その他」（`default-src 'none'`）になり、資産の 1 年のキャッシュは付かない。
- **Radix は MR2 では入れない**（`web/src/ui/README.md` の許可リストは空）。`no-restricted-imports` の規則と、vitest の
  「`<style>` が増えない」検査、Chromium での CSP の違反の検査（`npm run smoke`）は先に入れた。部品の見本の画面は、最初の
  Radix の部品を足す MR で作る。
- **Playwright の検査**は、`web/dist` を forge-web と同じ形（どのパスも同じ殻、`/-/assets/{file}`、殻の CSP は
  `security_headers.rs` の定数を読む）で返す Node の代役のサーバーで行う（`forge serve` は DB が要るため）。
  `npm run smoke -- --url <forge serve>` で本物にも当てられる。playwright は lockfile の devDependency（版を固定）で、
  ブラウザは CI の別のジョブ（`web-smoke`、dist の成果物を受け取る）でだけ取る。dist を作るジョブは `npm ci --ignore-scripts` だけ。
- **ビルドの検査**: `index.html` にインラインの `<script>`・`<style>`・`style=`・`on*=`・`javascript:` があればビルドを失敗させる
  （`web/scripts/inline-check.mjs`）。
- **npm の依存**: vitest は 4 系（3 系は `tinypool` の critical の勧告がある）。npm 10.9 は vitest 4 の optional な
  peer の解決で落ちる（arborist の不具合）ので、lockfile は npm 11.6 で作った。`npm ci --ignore-scripts` は npm 10 でも通る。
  ライセンスの許可リスト（§8）に、開発の道具だけが使う `MIT-0`・`BlueOak-1.0.0`・`Python-2.0`・`CC-BY-4.0`（caniuse-lite の
  データ、配布物に入らない）を理由つきで足した（`web/scripts/check-licenses.mjs`）。

## Amendment（2026-11-01、M2 MR9）

- テストの「MSW」は `web/tests/mock.ts`（`fetch` の経路表。本物の API クライアントを通す）に読み替えた。依存を増やさずに同じ層を確かめられるため。`axe-core`（MPL-2.0、dev のみ、`dist` に入らない）は `check-licenses.mjs` の許可リストに理由つきで足した。
- 主ボタンの白文字は `--gs-color-action`（`#0f7a69`、5.4:1）。`--gs-brand-teal-strong` は大きい文字だけ。操作できる部品は `controls.module.css` の `.interactive`（hover・active・フォーカスの輪 `--gs-focus-ring`）を共有する。

## Amendment（2026-11-01、M2 MR10: リポジトリの画面で決めたこと）

- **構文の色付け**: §3 の「highlight.js の core と言語の定義を遅延で読み込む」を、`lowlight` 3.3.0（highlight.js の結果を HTML の文字列ではなく木で返す）+ `highlight.js` 11.11.1 で実装した。木を React の要素に直接描くので `innerHTML` を使わず、`class="hljs-…"` だけを通す（正規表現で許可リストにある名前以外は捨てる）。言語は 23 個を個別のチャンクにし、開いたファイルの言語だけを読む。20 万文字を超えるファイルは色をつけない（メインスレッドで動かすため）。トークンの色は `tokens.css`（`--gs-code-*`、コードの背景 `--gs-color-code-surface` の上で 4.5:1 以上）。
- **Markdown**: §7 のとおり `react-markdown` 10.1.0 + `remark-gfm` 4.0.1。`skipHtml`、`rehype-raw` なし。`urlTransform` が http / https / mailto / 相対 / `#` だけを通し、制御文字・バックスラッシュ・`//` で始まるものを捨てる。`<img>` は同じリポジトリの png / jpeg / gif / webp を raw の URL にしたものだけ（SVG・HTML・PDF・外部・`data:` は描かない。外部はリンク）。リポジトリのルートより上に出る相対パスはリンクにしない。見出しの `id` は `user-content-` + 文字列。見出しの段は、ページ自体の h1 / h2 の下に入るよう `headingOffset` で下げる（README は +2、ファイルは +1）。表とコードは名前つきの `role="region"` + `tabindex=0` の中に置く（キーボードでスクロールできる）。XSS のベクタの検査は `web/tests/markdown.test.tsx`。
- **チャンク**: Markdown の部品（約 177 kB、gzip 50 kB）は `React.lazy` で別のファイルにした。最初に読む JS は 448 kB（gzip 143 kB）。
- **推移的な依存の版**: 2 週間の規則（§8）に合わせ、公開から 2 週間たっていない版が選ばれた 5 つを `package.json` の `overrides` で古い版に固定した（`hast-util-to-jsx-runtime` 2.3.6、`mdast-util-to-markdown` 2.1.2、`mdast-util-find-and-replace` 3.0.2、`unist-util-visit` 5.1.0、`unist-util-visit-parents` 6.0.2）。いずれも react-markdown / remark-gfm の範囲（`^`）の中。上げるときは overrides を消して 2 週間後に。
- **画面の語の扱い**: SVG・HTML・XML・PDF のファイルは、画面では「描かない」という注意を出し、テキストのものは中身を文字として（`<pre>` の中で）見せる。描画・実行はしない（ADR-0015 の「表示せず、ダウンロードだけ」の「表示」を、描画の意味に読んだ）。

## Amendment（2026-11-01、ADR-0023: 文書ポータルをやめる）

- 「文書ポータル、ログイン、エラーの画面は M2 で id まで埋める」から文書ポータルを外す（ログインとエラーの画面は変わらない）。
- 計画 08 の言い換えの表を辞書の `plain.*` として持つこと、文書ポータルの画面に Git の言葉が出ないことの禁止語の検査（MR12）は、取り下げた。
  GitStation の画面は Git の言葉を使う。後続のコードの MR で `plain.*` と検査を取り除く。
