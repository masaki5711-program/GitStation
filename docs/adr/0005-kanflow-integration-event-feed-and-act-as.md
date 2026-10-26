# ADR-0005: kanflow 連携 — 汎用の作業項目プロバイダ、イベントの履歴 API、代理実行

**Date**: 2026-10-26
**Status**: accepted
**Deciders**: Masaki Fujikawa（ユーザー）、Claude

## Context

kanflow（社内のプロジェクト管理ツール）は現在、GitLab を定期取得して PBI の「開発」タブに MR を表示している。
ブランチ名に PBI 番号（`feature/PBI-123-…`、実例では `fix/L2-pbi-43-…`）を入れて結びつける。
**クローズした MR が表示されない** 問題がある。PBI 番号はプロジェクト内の連番である。

## Decision

- GitStation は kanflow を直接知らず、汎用の `WorkItemProvider` 契約を定める。kanflow はそれを実装するアダプタを持つ。
- 作業項目は `(kanflow のプロジェクト ID, PBI-番号)` で識別し、プロジェクトはリポジトリの結びつけ（バインディング）で決める。1 つの system リポジトリは 1 つの kanflow のプロジェクトにだけ結びつける。
- kanflow には **カーソル方式のイベントの履歴 API** を第一に提供する（定期取得のままで取りこぼしがない）。MR のすべての状態の変化（クローズを含む）をイベントにする **フル連携** を契約にし、状態の遷移の一覧からテストを生成する。
- リンクは検出時に保存し、ブランチの削除後も残す。
- kanflow の利用者としての操作は、サービスアカウント + `Forge-Act-As` で行い、サービスアカウントの範囲と本人の権限の共通部分で認可する。将来 OAuth2 に移る。
- 移行の初日は、kanflow が使っている GitLab の API の範囲だけを互換で提供する。

詳細: `../../../docs/plan/02-kanflow-integration.md`。

## Alternatives Considered

### Webhook だけで連携する
- **Pros**: 即時に届く
- **Cons**: kanflow に受信口が要る。受信側が落ちていた間のイベントは再送に頼る
- **Why not**: kanflow は定期取得の方式で動いている。カーソル方式なら改修が小さく、取りこぼしもない

### kanflow のユーザーごとに個人トークンを登録させる
- **Pros**: 代理実行が不要
- **Cons**: 利用者の手間が大きく、トークンの管理が分散する
- **Why not**: 導入の障壁になる。OAuth2 への移行で同等の安全性を得る

## Consequences

### Positive
- 「クローズした MR が出ない」問題を構造的に解消する
- 他のツールも同じ契約で連携できる

### Negative
- kanflow 側にアダプタとカーソルの保存の改修が要る

### Risks
- 代理実行は権限昇格の経路になりうる → 認可の単一判定点で判定し、権限マトリクスに行を加え、`security-reviewer` のレビューを必須にする
