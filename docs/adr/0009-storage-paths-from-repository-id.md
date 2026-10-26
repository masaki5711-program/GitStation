# ADR-0009: リポジトリの保存先はリポジトリ ID だけから決める

**Date**: 2026-10-26
**Status**: accepted
**Deciders**: Masaki Fujikawa（ユーザー）、Claude

## Context

URL のパス（グループ / プロジェクト名）から保存先を作ると、名前の変更や移管でファイルの移動が必要になり、
パストラバーサルの危険もある。Windows の開発機では大文字小文字を区別しないファイルシステムと長いパスの問題もある。

## Decision

保存先は `<root>/repos/<sha256(repo_id) の先頭 2 桁>/<repo_id>.git`。
URL は DB で `RepoId` に解決してからパスを作り、URL の文字列をパスに使わない。
ルートは起動時に一度だけ正規化する（Windows では `\\?\` 接頭辞を外して比較）。
各パスがルートの下にあることと、シンボリックリンクでないこと（`symlink_metadata`）を確認する。

## Alternatives Considered

### 名前ベース（`group/sub/proj.git`）
- **Why not**: 名前の変更・移管でファイルが動く。トラバーサルの危険

### `<owner_id>/<repo_id>.git`
- **Why not**: 移管でファイルが動く

## Consequences

### Positive
- 名前の変更と移管が DB の更新だけで済む。パスに利用者の入力が入らない

### Negative
- ファイルシステムだけを見てもどのリポジトリか分からない → `forge admin repo-path <path>` で逆引きできるようにする

### Risks
- ルートの設定ミス → `forge doctor` がルートの存在、書き込み権限、シンボリックリンクでないこと、空き容量を検査する
