# 引き継ぎ — M3 O1 バックアップと復元（2026-11-09、クラウド）

前の引き継ぎ: `2026-11-09-next-session.md`。利用者の決定（Q11、2026-11-09）: 保管先は **ローカルのディレクトリ**（設定できるパス）、
**7 世代**（設定できる）、DB は **pgBackRest**、リポジトリは **ファイルの複写**。NAS・別のサーバーへの転送は後。

ブランチ `feat/ops-backup-restore`（`main` `a264369` から）。**push していない。**

## 1. やったこと

| 場所 | 中身 |
|---|---|
| `crates/forge-errors/errors.toml` | **FG-SYS-0008** BackupFailed / **FG-SYS-0009** RestoreVerificationFailed（どちらも alert、runbook `backup-restore`）。`docs/errors/` を再生成 |
| `crates/forge-core/src/config/backup.rs` | `[backup]`: `dir`・`key_file`・`generations`（7、1〜100）・`state_copy_dir`・`state_copy_interval_secs`（300）・`pgbackrest_bin`・`pgbackrest_stanza`・`db_timeout_secs`・`repo_timeout_secs`・`max_age_hours`（26）。鍵（64 桁の 16 進、0600）と写しの置き場は `backup.dir` の外でなければ FG-SYS-0002 |
| `crates/forge-core` | `SessionRevokeReason::Restore`、Webhook の `DisableReason::Restore` |
| マイグレーション `20261201100000_restore_revocation_reasons.sql`（追記） | `tokens` / `web_sessions` / `webhooks` の理由の CHECK に `restore` |
| `crates/forge-db/src/restore/` | verify の読み取り（リポジトリの行・版の期待値）、DB の時計、**すべての資格情報の失効**（トークン・セッション・Webhook、利用者／プロジェクトごとの監査の行）、**権限の状態の写し**（`state.rs`: 読み取りと「狭める変更だけ」の当て直し）、管理者の印をすべて外す、`regrant_admin`、`restore.finished` |
| `crates/forge-cli/src/backup/` | `forge backup`: 権限の確かめ → `.lock` → DB（`DbBackupTool` trait、実物は `PgBackRest`: 引数のベクタ・環境を絞る・時間切れで殺す・stderr を伏せる）→ リポジトリ（ref を先に複写、作業用の複写を `git fsck --connectivity-only`、3 回まで再試行、AES-256-GCM の塊の暗号化 `crypt.rs`、アーカイブ `archive.rs`）→ `manifest.json` → `.partial` を外す → 回転 → 写しを 1 つ |
| `crates/forge-cli/src/restore.rs` | `forge restore begin / repositories / verify / finish / regrant-admin`。`begin` の印（`<storage.root>/.forge-restore-pending`）がある間 `forge serve` / `forge worker` は FG-SYS-0009 `restore_pending` で起動しない |
| `crates/forge-cli/src/state_copy.rs` | 権限の状態の写し（暗号化、48 個まで）。`forge worker` が 300 秒ごとに書く（`backup.state_copy_dir` があるとき、別のタスク） |
| `crates/forge-cli/src/doctor/backup.rs` | doctor `backup.last_success`（FG-SYS-0008 `never` / `overdue` / `insecure_permissions`）、`backup.state_copy`（warn） |
| `ops/runbooks/backup-restore.md` | 設定（pgBackRest の設定例を含む）、Diagnose の表、**復元の手順 1〜11**、DB が古いときの判断、**再発行の表**（セッション・個人・サービスアカウント・Webhook・代理実行の鍵・ミラーの資格情報）、**復元の訓練** |
| `docs/adr/0019-…md` | **Addendum O1**（実装で決めたこと・計画からの変更） |
| `docs/plan/11-m3-plan.md` | §10 に Q11 の答え（**計画を変えた: エージェントモデルの `docs/plan/` へ写し戻すこと**） |
| `crates/forge-cli/README.md` | `forge backup` / `forge restore` の行 |

## 2. 計画からの変更（ADR-0019 Addendum O1 に理由）

1. **verify と finish を分けた**: `forge restore verify` は読むだけ（何度でも流せる）。失効・当て直し・epoch・監査の行は `forge restore finish`。
2. **「監査の写し」を「権限の状態の写し」にした**: 監査の行の `details` を読み直す代わりに、worker が 5 分ごとに権限の **状態** を暗号化して
   バックアップの外に書き、復元は **狭める変更だけ** を当て直す（写しが改ざんされても締め出しにしかならない）。ADR-0024 #41 の
   マージの条件もまとめて扱える。失う範囲は最後の写しより後の縮小（最大 1 間隔）。
3. **`forge restore regrant-admin` を足した**（ADR-0024 #44）: `bootstrap-admin` は管理者が 1 人でもいると断るため。restore が外した利用者だけ。
4. O3 が予約したロック `repo_maint:<repo_id>` は取らない（O3 が取らないので）。複写の fsck と再試行で代えた。

## 3. 確かめたこと（このセッション、PG16 `forgetest` 127.0.0.1:55432）

- `cargo fmt --all -- --check`: 合格。
- `cargo clippy --workspace --all-targets -- -D warnings`: 合格（警告 0）。
- `cargo run -q -p forge-errors --bin gen-catalog -- --check`: `catalog up to date (168 files)`。
- `cargo run -q -p forge-web --bin gen-authz -- --check`: `authz manifests and OpenAPI are up to date`。
- `cargo nextest run -p forge-core -p forge-errors -j 2`: 363 / 363 合格。
- `cargo nextest run -p forge-db -j 2`: 914 / 914 合格（1 183 秒）。
- `cargo nextest run -p forge-cli -j 2`: 255 / 255 合格（221 秒）。最後の小さな修正（バックアップの中の失敗を FG-SYS-0008 に包む）の後に backup・restore・state_copy の 27 件をもう一度流して合格。
- 新しいテスト（合成データ）: `backup_then_delete_then_restore_then_verify`、`a_changed_archive_is_refused_before_anything_is_written`、
  `wide_permission_storage_is_refused`、`rotation_keeps_n_generations`、`a_failed_database_step_leaves_no_generation_and_the_next_run_cleans_up`、
  `no_secrets_in_logs_or_in_the_backup_files`、`a_database_behind_the_repositories_is_detected`、`repositories_behind_the_database_are_detected`、
  `restore_does_not_resurrect_revoked_credentials`（個人・別の利用者・サービスアカウントのトークン、セッション、Webhook、無効化の当て直し、
  epoch、`restore.finished`）、`restore_reapplies_post_backup_role_changes`（所属の削除・ロールの引き下げ・管理者の印・保護 ref の引き上げと追加、
  広げる変更は当て直さない）、`without_a_state_copy_every_admin_flag_is_removed_and_listed`（+ `regrant-admin`）、
  `a_state_copy_older_than_the_backup_is_not_used`、`serve_and_worker_refuse_while_a_restore_is_pending`、
  `forge_web_never_names_the_restore_commands`、forge-db の 3 件（失効の冪等、写しは決して広げない、印の一覧）、暗号・アーカイブ・
  pgBackRest の台本（失敗・時間切れ・伏せ字）・doctor・設定の単体テスト。

## 4. 確かめていないこと

- **本物の pgBackRest**（このコンテナに無い）。DB の手順は `DbBackupTool` の後ろにあり、テストは偽物とシェルの台本。runbook の
  pgBackRest の設定例（`repo1-cipher-type`・`retention`・`archive_command`）も未確認。**試験環境での訓練で確かめる。**
- DB の復元そのもの（`pgbackrest restore`）。テストは「バックアップの後の変更を SQL で巻き戻す」ことで復元した DB を模した。
- 「復元後に GitStationSource が 410 から立ち直る」: このリポジトリでは `finish` が epoch を進め `feed.epoch_rotated` を書くことまで
  （`restore_does_not_resurrect_revoked_credentials`）。410 `epoch_mismatch` と `/cursor` での回復は forge-integ の既存の
  `epoch_mismatch_is_410` が確かめる。Kanflow 側（K3 の GitStationSource）の通しは Kanflow のリポジトリと試験環境で。
- Windows の ACL（Unix の権限だけ確かめる）。
- **ワークスペース全体のテスト**（他のエージェントと Postgres を共有しているため、触った crate だけ）。forge-web・forge-integ・
  forge-hosting のテストは流していない（変更は enum の値の追加だけ。clippy は全体で合格）。
- 独立レビュー（評価役）はまだ。計画の評価役: `database-reviewer`、`git-protocol-reviewer`、`silent-failure-hunter`、`rust-reviewer`、
  `error-contract-auditor`（+ 失効と当て直しは `security-reviewer` を推奨）。

## 5. 次にやること

1. 独立レビュー（上の評価役）→ CRITICAL / HIGH を直す → PC で全体のテスト → `main` へ。
2. 試験環境で復元の訓練（runbook「復元の訓練」）。pgBackRest を入れ、`forge backup` の timer、`backup.state_copy_dir` を別のディスクに。
3. 後の作業: NAS・別のサーバーへの転送（Q11）、O3 の保守が `repo_maint:<repo_id>` を取る形（バックアップと排他）、
   アプリ内の通知に「バックアップが古い」を出すか。
4. エージェントモデルの `docs/plan/11-m3-plan.md` へ Q11 の答えを写し戻す。

## 6. 追記: 独立レビューの指摘への対処（同じ日）

評価役: security（HIGH 2）、git-protocol（HIGH 1）、rust / database（MEDIUM のみ）。計画からの変更 1〜3 は利用者が受け入れた。
本物の pgBackRest での復元の訓練は試験環境で後に行う（後の作業）。詳細は ADR-0019「O1 の独立レビューで決めたこと」。

- **HIGH**: 写し（形式 2）にプロジェクトの公開範囲・承認できる人・チェックの名前空間の許可を足し、狭める向きだけ当て直す。
  push ミラーと連携を `finish` で無効にする（`reason=restore`）、保留中の代理実行の主張も終わらせる。バックアップと O3 の保守が
  `repo_maint:<repo_id>` のセッションのロックで排他（バックアップは待つ、保守は飛ばす `backup_running`）。commit-graph・
  multi-pack-index は複写しない。
- **MEDIUM**: 最新の写しが読めなければ写し無し（`--allow-older-copy`）、`regrant-admin` の条件を厳しく（最新の `restore.finished` の
  一覧・その後に変更が無い）、復元のファイル 0600 / ディレクトリ 0700・時間切れ（`restore_timeout`）・退避先を実行ごとに、AAD に世代の名前、
  `manifest.mac`（HMAC）、pgBackRest のラベルは前後の `info` の差、所属の当て直しは利用者ごとの監査の行、`finish` の `rerun`、
  管理者が 0 人になったら報告。LOW: 止まったバックアップの `.lock` を取り戻す、ロックのファイルの fsync。
- 新しいテスト: `push_mirrors_and_integrations_are_disabled_with_reason_restore`、
  `visibility_approvers_and_check_grants_are_narrowed_from_the_copy`、`the_repository_lock_is_exclusive_and_ends_with_its_connection`、
  `a_copy_past_its_deadline_is_repository_copy_timeout`、`a_copy_that_never_passes_fsck_is_inconsistent_after_three_attempts`、
  `a_repository_under_maintenance_is_waited_for_then_refused`、`a_failing_state_copy_after_the_rename_keeps_the_generation_and_fails_loudly`、
  `the_lock_of_a_backup_that_no_longer_runs_is_reclaimed`、`commit_graphs_and_multi_pack_indexes_are_not_copied`、
  `regrant_is_refused_after_a_later_change_of_the_user`、`a_finish_that_stopped_half_way_runs_again_and_says_so`、
  `an_unreadable_newest_copy_counts_as_no_copy_unless_older_ones_are_allowed`、と既存のテストの追加の確認（マニフェストの改ざん、
  別の世代の AAD、退避先、権限、pgBackRest のラベルが変わらない）。
- 確かめたこと: `cargo fmt --all -- --check` 合格、`cargo clippy --workspace --all-targets -- -D warnings` 終了コード 0、
  `gen-catalog -- --check` 168 files up to date、`cargo nextest run -p forge-cli -p forge-db -j 2 -E 'test(/backup|restore|state_copy|crypt|archive|db_tool|maintenance|commit_graphs|lock/)'`
  118 / 118 合格（errors.toml の文の変更の前）。
- **確かめていないこと（ディスクの空きが 1.5 GB で、3 GB を切ったため止めた）**: forge-db・forge-cli・forge-core・forge-errors の
  全体のテスト、`gen-authz -- --check`（forge-web の bin のビルドが要る。今回は権限の表に触れていない）、errors.toml の cause の文を
  変えた後の再実行。

## 7. 追記: 再レビュー（同じ日）

- **HIGH（security）**: `narrow_approvers` が広げうる（DB {A}、写し {B}、B はバックアップの後に作られて DB に無い → A を消して B を入れられず
  空 = Developer 以上の誰でも）。共通の承認者が無いときは DB の一覧をそのまま残す（空だった一覧だけを写しから埋める）。テスト
  `approvers_the_database_does_not_share_never_empty_the_rule`。
- **MEDIUM（rust）**: `forge restore repositories` は書いたファイルを 1 つずつ `sync_all`、名前を変える前にリポジトリの全ディレクトリと、
  後で親を fsync。
- **LOW**: manifest の MAC の鍵を HKDF で分けた（AES-GCM の鍵を使い回さない）、MAC で確かめたバイトそのものを解析、印のファイルは
  シンボリックリンクを辿らず create-new、止まったロックは rename で退けてから create-new（ホスト名を書き、別のホストのロックは取り戻さない。
  テストに別のホストと生きた pid の場合）、保守は解放の前に `alive()` を確かめて warn。runbook: 写しから外した印は `restore.reapplied` だけで
  `regrant-admin` の対象外（戻し方は `bootstrap-admin` か残った管理者）、`regrant-admin` は serve の起動の前に流す。
