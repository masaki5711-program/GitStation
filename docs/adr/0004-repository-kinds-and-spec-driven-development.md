# ADR-0004: リポジトリの種類（spec / system）と仕様駆動開発

**Date**: 2026-10-26
**Status**: accepted
**Deciders**: Masaki Fujikawa（ユーザー）、Claude

## Context

製品の根幹の目的は、AI によるシステム開発の加速。AI が正しい仕様を根拠付きで参照し、
実装と検証を行えるようにしたい。仕様とコードを同じリポジトリに置くか、別にするかを決める必要がある。
現在の運用では 1 つの PBI が複数のリポジトリにまたがり、仕様には Git を知らない人も関わる。

## Decision

- リポジトリに種類 `spec`（仕様）と `system`（システム開発）を持たせる。種類はデータモデルに M1 から入れる。
- **併用**: 製品・業務レベルの仕様は `spec` リポジトリ、コードと同時に変わる部品レベルの設計は system リポジトリ内の `spec/`。
- 要件には変わらない ID（`<製品>-<領域>-<番号>`）を付け、PBI・MR・テスト（`@req`）・デプロイと結びつける。
- system リポジトリは `.forge/repo.yml` で準拠する仕様の版を固定する。
- AI 向けの文脈 API と MCP で、要件・関連コード・過去の MR を出典付きで返す。

詳細: `../../../docs/plan/03-spec-driven-development.md`（エージェントモデルのリポジトリ）。

## Alternatives Considered

### すべて同じリポジトリに置く
- **Pros**: 仕様とコードを 1 つの MR で変えられる
- **Cons**: 複数のリポジトリにまたがる仕様の置き場所が決まらない。Git を知らない人が触りにくい
- **Why not**: 現在の運用（複数リポジトリの PBI）と、文書ポータルの方針に合わない

### すべて別のリポジトリに置く
- **Pros**: 仕様の承認と公開の流れを独立させられる
- **Cons**: API の定義のようにコードと同時に変わるものまで分かれ、ずれやすい
- **Why not**: 部品レベルの設計は、コードと同じ MR で変える方が安全

## Consequences

### Positive
- 要件 → PBI → MR → テスト → デプロイの結びつきから、影響範囲と要件の状態を機械的に出せる

### Negative
- 要件 ID とテストへの `@req` の記入という規則を運用で守る必要がある

### Risks
- 規則が守られない → `forge/spec-trace` で検出する（助言から始め、計測後に必須化を判断）

## Amendment（2026-10-31、手順 D4 MR1: `repositories` の表）

- **種類は作成の時に決まり、変わらない**（手順 D の計画 Q3）。`forge.repositories.kind`（CHECK `repositories_kind`: `spec` / `system`）は、
  `forge_app` に列の UPDATE の権限がないことで守る（`forge_app` が更新できるのは `state` と `ready_at` だけ。DELETE は D5 まで与えない）。
  テスト `repository_kind_cannot_change`。`forge-core` の `RepositoryKind`（`ALL`、`as_str`、`parse`）が同じ一覧を持つ。
- M1 ではプロジェクトに 1 つのリポジトリ（`project_id` UNIQUE）。状態は `provisioning` → `ready`（`RepositoryState`）で、
  `ready_at` は状態が `ready` のときに限って入る（CHECK `repositories_ready_at`）。
- `provision_token`（uuid、既定値 `gen_random_uuid()`）は、プロビジョニングのジョブが bare リポジトリの設定に書く目印（D4 の案 A。再試行は
  目印が一致するディレクトリだけを引き取る）。書き手は変えない（`forge_app` に UPDATE の権限がない）。読み取りのプール（`forge_ro`）からは
  読めない（ADR-0018 のトークンのハッシュと同じ扱い。テスト `the_read_pool_cannot_read_provision_token`）。
- 4 つの表（`projects`、`repositories`、`project_members`、`protected_refs`）はすべて、プロジェクトの監査の行（`resource_type = 'project'`、
  `resource_id` = プロジェクトの ID）で覆う（ADR-0016 §1 の状態の表）。状態の変更にも監査の行が要る
  （テスト `a_repository_state_change_needs_a_project_audit_row`）。
