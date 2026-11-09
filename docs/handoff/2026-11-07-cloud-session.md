# 引き継ぎ — クラウドのセッション（2026-11-07）

> **追記（同じ日の後半）**: §1 のブランチは `main` に入れた（PC が使えないため利用者の指示でクラウドからマージ。**GitLab には未 push**）。続けて git 2.43 の互換の修正と **O2（GitLab への push ミラー）** を `main` に入れた（§6）。PC が戻ったら `git push gitlab main` を 1 回。

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

## 6. 追記: 後半に `main` に入れたもの

| コミット | 中身 |
|---|---|
| `8f29150` | **git 2.43 の互換**: `git grep` は 2.44 より前は `--end-of-options` を受け付けず、仕様の索引（X4: spec-lint・spec-trace・仕様と文脈の API）がどのコミットでも FG-GIT-0003 だった。完全な 16 進の ID は印なしの引数にした（`object_id_operand`）。`merge-base --is-ancestor` は読めないコミットでも 1 を返すので、stderr があれば `reason=unreadable_history`。GitHub Actions 緑 |
| `6d79707` | **O2 前半**: 表 `push_mirrors`（audited）・`push_mirror_status`（derived、20261130100000）、宛先の方針（https、`mirror.allowed_hosts`）と資格情報のファイル（`mirror.credentials_dir`、1 行 `user:token`、0600）、`git push`（`refs/heads/*`・`refs/tags/*` だけ、`--prune`、ヘッダーは子の環境だけ）、購読者 `push-mirror`、専用のランナー、FG-INT-0013 / 0014、runbook `gitlab-mirror`、ADR-0024 Amendment O2 |
| `a7fbd2a` | **O2 後半**: `PushMirrorRead` / `PushMirrorManage`（管理者・人だけ・`admin` スコープ）、`GET|PUT|DELETE /api/v1/projects/{id}/push-mirror`・`POST …/sync`、`gstn mirror show|set|remove|sync`、doctor `mirror.config` / `mirror.status` / `mirror.lag`。独立レビュー（security・authz）の HIGH（リダイレクトでヘッダーが漏れる → `http.followRedirects=false` と `http.<url>.extraHeader`）と MEDIUM（sync の再確認 → `sync_decided`）を直し、再レビューは両方 APPROVE |

計画からの変更（ADR-0024 の O2 の節）: 資格情報は credential helper ではなく子の環境変数のヘッダー。再送は運用者の CLI ではなく管理者の HTTP（契約 4 の例外を増やさない）。

O2 の残り: **試験環境で本物の GitLab に送る通し**（Q7: 試験用の GitLab プロジェクトとミラー用アカウントのトークン）。試験環境の `mirror.credentials_dir` と `mirror.allowed_hosts` を設定し、`gstn mirror set` → push → `gstn mirror show` で `pending_since` が NULL になることを見る。R1 の条件 ①「そのマージが GitLab へ遅れなくミラーされる」はこれで確かめる。

全体のテスト（クラウド、PG16 素の設定は §3）: 2 943 合格・3 失敗。3 件とも変更と無関係で単独では通る（不安定）:

- forge-cli `worker::maintenance::tests::a_killed_pack_refs_removes_its_own_lock_and_keeps_a_foreign_one`（**単独でも 3 回中 2 回落ちる**。`2026-11-06-m3-day5.md` §6 の `pack_refs` より悪い。次に直す候補）
- forge-git `browse::history_tests::answers_that_do_not_fit_are_process_failed`（負荷の下で `GitUnavailable`）
- forge-review `conflicts_git a_git_timeout_on_a_pair_is_stored_on_the_pair`（負荷の下で時間切れが起きない）

## 7. 追記（2026-11-08〜09、クラウド）: pack_refs のテストが落ちる原因を直した

- **原因**: ファイルの更新時刻は粗い時計（カーネルの tick、ファイルシステムによっては秒）で付き、`SystemTime::now()` は細かい時計。
  このコンテナでは、作った直後のファイルの更新時刻が直前の `now()` より早いことが 2000 回中 1501 回（最大 7.7 ms）あった。
  そのため、殺した pack-refs が残した**自分の** `packed-refs.lock` が「ステップより古い = 他人のもの」と判定され、
  残ったうえに誤った FG-GIT-0020 の警告が出ていた。`a_killed_pack_refs_removes_its_own_lock_and_keeps_a_foreign_one` は単独で 5 回中 5 回落ちた（§6 の「3 回中 2 回」はこれ）。
- **修正**: `forge-git` の `clear_own_packed_refs_lock` で、下限を `step_started - LOCK_MTIME_SLACK（2 s）` にした。2 s は ext2/ext3（秒）と FAT（2 秒）も含めるための意図した値。
  文書に「残る窓が 2 s 広がる（ステップの 2 s 前から持つ書き手のロックを、殺した時点でも持っていた場合）。可能性は極めて低く、受け入れる」と書いた。
- **テスト**: 新しい単体テスト `a_lock_stamped_by_a_coarse_clock_just_before_the_step_is_ours`（10 ms 前 → 自分のもの、3 s 前 → 他人のもの）。修正前に落ち、修正後に通ることを確かめた。forge-cli の上のテストは修正後 5 回中 5 回合格。
- **確かめたこと**: `cargo fmt --all -- --check`、`cargo clippy -p forge-git -p forge-cli --all-targets -D warnings`、`cargo nextest run -p forge-git -p forge-cli`（549 件中 548 合格。落ちた 1 件は下の別の不安定なテスト）。独立レビュー（rust-reviewer）は HIGH なし、MEDIUM（文書の窓の説明）と LOW（境界のテスト、行の長さ、2 s の理由）を直した。**ワークスペース全体のテストは流していない。**
- **別の不安定なテスト**: forge-git `mergeability::tests::merge_base_questions_time_out_and_refuse_missing_objects` が 2 並列の実行で 1 回落ち、単独では 3 回中 3 回合格。負荷の下の時間切れの前提。§6 の一覧に足す。
