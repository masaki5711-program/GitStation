# ADR-0008: 保護ブランチの判定 — 事前の検査と pre-receive フック（ポリシーのスナップショット）

**Date**: 2026-10-26
**Status**: accepted
**Deciders**: Masaki Fujikawa（ユーザー）、Claude

## Context

保護ブランチ・保護タグ（force push 禁止、削除禁止、push できるロール）を確実に守る必要がある。
receive-pack のコマンドだけでは、オブジェクトを受け取る前なので fast-forward かどうか判定できない。
ref ごとの `update` フックでは、複数の ref の push が一部だけ反映される。

## Decision

- **(a) 事前の検査**: `serve` が receive-pack のコマンド（ref 名、旧・新の oid）を読み、
  「このロールはこの ref に push できない」「保護 ref の削除」をパックの受信前に拒否する。
- **(c) 最終判定は `pre-receive`**: フックは `forge hook pre-receive` を実行し、`serve` が認可のときに一時ファイルへ書いた
  **ポリシーのスナップショット** を読む。隔離環境の中で `git merge-base --is-ancestor` により fast-forward を判定する。
  pre-receive は push 全体を一括で拒否する（一部だけ反映されない）。フックは DB に接続せず、秘密情報も持たない。
- `post-receive` は反映された `旧 新 ref` を一時ファイルに書き、`serve` が git の終了後に監査ログと Outbox を 1 つのトランザクションで書く。

## Alternatives Considered

### 事前の検査だけ
- **Why not**: fast-forward を判定できない

### リポジトリ全体の `receive.denyNonFastForwards`
- **Why not**: ref ごとの保護を表現できない

### `update` フック
- **Why not**: 複数の ref の push が一部だけ反映される

## Consequences

### Positive
- 安価な拒否（パック受信前）と、確実な一括判定の両方が得られる

### Negative
- フックのスクリプト（`#!/bin/sh` の薄いラッパー）の配置と検証が要る

### Risks
- ref の更新後に DB が落ちると監査とイベントが残らない → `FG-GIT-0012` で ref の一覧をすべてログに残し、再投入の Runbook を用意する
