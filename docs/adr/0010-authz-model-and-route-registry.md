# ADR-0010: 認可のモデルとルートの登録の仕組み

**Date**: 2026-10-26
**Status**: accepted
**Deciders**: Masaki Fujikawa（ユーザー）、Claude

## Context

認可は 1 か所で判定し、すべてのルートと git 操作を権限マトリクスのテストに載せる（CLAUDE.md §3-4）。
ルートを足したときに、マトリクスへの追加漏れを機械で検出したい。OpenAPI もコードから生成する。

## Decision

- `forge_auth::authorize(&AuthzContext, Action, &Resource) -> Decision` は純粋な関数。必要な情報（所属、実効ロール、
  トークンのスコープ、保護 ref のポリシー）は呼び出し前に `AuthzContext` へ読み込む。`Action` は `_` なしで網羅的に match する。
- 判定 = ロール ∩ トークンのスコープ ∩ 保護 ref のポリシー。見えないリソースは 404（`FG-REPO-0001`）で、存在を漏らさない。
- ロール: Guest / Reporter / Developer / Maintainer / Owner（GitLab 型）。実効ロールは、プロジェクトへの直接の所属と
  祖先のグループへの所属の最大値。グループの入れ子は再帰 CTE（深さの上限 20）で解決する。
- 可視性は `private` と `internal`（ログインした全員が読める）。匿名アクセスはない。
- **ルートの登録の仕組み**: すべてのルートを `(メソッド, パス, Action, ハンドラ)` で 1 か所に登録し、そこから
  ルーター、OpenAPI（utoipa）、`Git/docs/authz/routes.generated.json` を作る。マトリクスの要約は
  `matrix.generated.json`。ゲート `authz-coverage` が両者を突き合わせる。
- プロジェクトとリポジトリは別のテーブルにする。リポジトリは種類（`spec` / `system`）を持ち、プロジェクトは
  1 つの主リポジトリを持つ（Wiki や文書スペースを後から足せる）。

## Alternatives Considered

### ハンドラごとに権限を判定する
- **Why not**: 判定が散らばり、マトリクスと食い違う

### OpenAPI を手書きする
- **Why not**: コードとずれる

## Consequences

### Positive
- ルートの追加漏れがビルド（網羅的な match）とゲートで検出される

### Negative
- ルートの登録に専用の関数を通す規則が要る（`Router::route` を直接使わない）

### Risks
- 実効ロールの計算の誤り → マトリクスのテストで祖先のグループの所属を含むケースを検証する
