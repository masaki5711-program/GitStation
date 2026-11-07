# 引き継ぎ — クラウドのセッション（2026-11-07）

`2026-11-06-m3-day5.md` の続き。PC で続けるときは、まずこのファイルの §1 の 3 つのブランチを `main` に入れる。

## 1. このセッションで作ったもの（どれも `main` には未マージ）

ブランチはすべて `claude/clever-gates-j3elqg`（GitHub に push 済み）。マージは PC の手順（`D:\APP\GitStation-main` で `--no-ff`、`origin` と `gitlab` の両方へ push）で行う。クラウドからは GitLab に届かないので、ここではマージしていない。

| リポジトリ | コミット | 中身 |
|---|---|---|
| GitStation | `80ffdf5` | readyz の漏れの検査が、ランダムな request_id の値を除いてから調べる（`5432` を含む ID で落ちた不安定なテスト。§6 の 1 件が消える） |
| GitStation | `0d475e9` | `bootstrap/schema.rs` の forge_ro の列の期待値（8 表分）を手書きの定数から `tables.toml` の `ro_columns` に。項目が無い・空・`"*"` と列の混在は FG-SYS-0002（`reason=ro_columns_missing` / `ro_columns_star_mixed`、`table`）。単体テスト 4 件 |
| Agent_Claudecode_ver2 / Agent_model_TMUX | `1f03178` / `a676ba3` | 部品 `skills/gitstation-mr-workflow`（`gstn` での MR の手順）と `hooks/gstn-mr-guard`（`gstn mr create/edit` の検査、`gstn mr approve` は常に止める）。変更履歴 v2.16.0 |
| Agent_Claudecode_ver2 / Agent_model_TMUX | `e8f166c` / `c6e6e29` | W5: テストのコメント `@req PBI-<n>/AC-<m>`（GitStation の `forge/spec-trace` と同じ書式）と部品 `hooks/ac-trace`（AC → テストの表、テストの無い AC で exit 1）。変更履歴 v2.17.0 |

確かめたこと:

- GitStation: forge-db 902 件すべて合格、forge-web readyz 7 件、`cargo fmt --all -- --check`、clippy `-D warnings`（forge-db・forge-web のみ）。**全体のテストとワークスペース全体の clippy はクラウドでは流していない**（nextest が無く、ディスクの割り当てが足りない）。マージの前に PC で §5 の全体のテストを 1 本。
- エージェントモデル: 両方で `self-heal --check` が緑、テストは失敗 0。独立レビュー（`typescript-reviewer`）と `data-sovereignty-guard` を通した。

## 2. 調べて決まったこと

- **forge_ro が `forge_audit` の 4 表（`bootstrap_log`・`ddl_audit`・`ddl_audit_ack`・`guarded_objects`）を読めるのは意図どおり。** `check_schema()` は ADR-0018 のとおり読み取りのプール（forge_ro）で動き、その中の `forge_audit.ddl_audit_status()` は SECURITY INVOKER で forge_ro に EXECUTE がある（`ops/db/bootstrap/03-database-objects.sql` 290・581 行）。関数が 4 表を読むので SELECT が要る。4 表に資格情報は無く、人に近いのは DB のログイン名（`applied_by`・`acked_by`・`session_user_name`）と確認のメモだけ。コードの変更なし。
- **P4 は済んでいた。** GitHub Actions は `main` の `fe1b90b` で緑（その前の 4 回は赤）。
- `gstn mr approve` / `merge` はエージェントのトークンでは断られる（`help --json` の「People only」）。エージェントモデルのフックは、エージェントが人のトークンで承認するのを止める。

## 3. クラウドでテスト用の DB を立てる方法（Docker が無いとき）

コンテナの PostgreSQL 16 で足りる（forge は timescaledb の拡張を使わない）。ただし **timescaledb のイメージの設定に合わせないと、feed の 2 件（読む行数の上限）と `a_wrong_password_is_auth_failed_and_never_logged` が落ちる**:

```
initdb -D /var/tmp/pgdata -U forge_dev
postgresql.conf: port=55432, listen_addresses=127.0.0.1, max_connections=200,
  random_page_cost=1.1, effective_cache_size=4GB, shared_buffers=1GB, work_mem=16MB,
  password_encryption=scram-sha-256
pg_hba.conf: host all all 127.0.0.1/32 scram-sha-256
ALTER USER forge_dev PASSWORD 'forge_dev'; CREATE DATABASE forge_dev;
```

ディスク: ワークスペース全体のテストのビルドは target が 29 GB になり割り当てを超えた。`CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0` で crate ごとに流す。

## 4. 次にやること

1. §1 のブランチを `main` へ（GitStation は全体のテストの後）。
2. GitStation: P3（試験環境の結果）、O1（保管の道具 Q11）、O2（試験用の GitLab プロジェクト Q7）。利用者の答え待ちは `2026-11-06-m3-day5.md` §8 のまま。
3. GitStation の不安定なテストの残り（`2026-11-06-m3-day5.md` §6。readyz は直した）。
4. エージェントモデル: W4（触る範囲の宣言。Kanflow の `declare_scope` が要る）、W12（`spec-authoring`）、エージェントモデル自身の GitHub Actions（今は `.gitlab-ci.yml` だけ）。
5. エージェントモデルの小さな問題: `npm test` の要約（`scripts/lib/test-reporter-concise.js`）の件数が実行ごとにぶれる（失敗は 0、素の `node --test` は安定）。数え落としの原因を調べる。
