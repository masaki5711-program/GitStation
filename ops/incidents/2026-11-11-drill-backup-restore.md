# 訓練の記録: 復元の訓練（2026-11-11）

runbook `backup-restore`「復元の訓練（四半期に 1 回）」の記録。M3 完了の条件 2（計画 11 §9）の根拠。

- 対象: 使い捨てのインスタンス `gs-drill`（合成のデータだけ。本番・試験用インスタンスには触れていない）
- 時刻はすべて UTC、日付は 2026-11-11
- 2 回の実行: 1 回目は runbook と製品の不具合を見つけるため、2 回目は直した runbook と直した製品で通すため

## 結果の要約

| | 1 回目 14:25 | 2 回目 15:46 |
|---|---|---|
| 復元（DB・リポジトリ） | 成功 | 成功 |
| RTO（目標 4 時間） | 56.8 s | 23.6 s |
| `forge restore verify` | **失敗**（偽陽性 `version_ref_unknown refs/merge-requests/1/merge`） | consistent（終了コード 0） |
| 結果 | 製品の不具合を修正（38c3065、`dbc3a30` で main に統合）、runbook を更新 | 合格（下の確認項目） |

## 1 回目（14:25 UTC）

- 復元は成功し、RTO は 56.8 秒。
- `forge restore verify` が偽陽性を出した: マージ可否の検査が作るマージの結果の ref `refs/merge-requests/1/merge` を、
  DB が知らない版の ref と誤判定した。マージの結果の ref は検査のたびに作り直される派生の ref で、版の ref ではない。
- 修正: 38c3065 `fix(ops): restore verify treats a known merge request's merge ref as derived`。
  回帰テスト `restore::tests::the_merge_result_ref_of_a_known_merge_request_is_no_finding`。
  runbook の手順 6（検証の表と「DB が知っている MR の ref」の段落）を更新。

## 2 回目（15:46 UTC、直した runbook と直した製品で実行）

手順（runbook の訓練の 1〜5 に対応）:

1. `forge backup` と `forge doctor` の `backup.last_success` が ok。
2. バックアップの後に、トークンの失効・利用者の無効化・所属の削除・ロールの引き下げ・管理者の印の削除（試験用 SQL）・保護 ref の追加を行い、写しが書かれるのを待った。
3. 復元: `forge restore begin` → pgBackRest `restore --type=immediate --target-action=promote` でバックアップの点に戻した。
   WAL はアーカイブに後の分まであったが、バックアップの点で止まった。
   → 次の 4 と 5、`restore finish`。
4. 確かめ:
   - トークン 6 件を `revoked_reason=restore` で失効。失効したものは復活しなかった。Web セッション 1 件を失効。連携（kanflow）を無効化。
   - 権限の縮小を当て直し 5 件（無効化、管理者の印、所属の削除、ロールの引き下げ、保護 ref）。
   - 監査の行: `restore.finished` 1、`restore.credentials_revoked` 6、`restore.reapplied` 6、`restore.integration_disabled` 1。
   - 付け直したトークンで認証できた。
   - 古いフィードのカーソルは 410 `epoch_mismatch`、新しいカーソルは 200。
5. `forge restore verify` が consistent（終了コード 0）。

### この訓練で確かめていないこと

- 手順 11〜12: 管理者の Keycloak 経由の再サインインと、ブラウザからの連携の再有効化（PATCH）。訓練では SQL で代行した。
- 実際の Kanflow v2 が復元の後に 410 から回復すること。同じ 410 `epoch_mismatch` からの回復は、同日に試験環境の Kanflow v2 で確認済み
  （`docs/plan/11-m3-plan.md` §9 の 2 の注記）。
- Webhook、push ミラー、代理実行（act-as）の再発行と無効化の確認。
- 写しを置かない回（管理者の印がすべて外れる経路）は、この訓練では実行していない。

## 見つかった問題と対応

| 項目 | 種類 | 対応 |
|---|---|---|
| `restore verify` の偽陽性（マージの結果の ref） | 製品の不具合 | 38c3065 と回帰テスト。`dbc3a30` で統合済み |
| runbook 手順 10 の `feed.sink_compare` の文言が古い（`restore finish` が epoch を既に進めている） | runbook | runbook を更新（`new-epoch` を再度流さない）。doctor の文言は製品側に残る（下の「残り」） |
| runbook 手順 10 の「audit.seal_backlog は番号付けが止まっている間も ok」の記述 | runbook | 製品の判定（`forge-db` の `seal/backlog.rs`）は最も古い未封印の行が 5 分を超えると、`audit_sequencing=off` でも FG-DB-0015 で失敗する。runbook を実装に合わせて更新 |
| 手順 4 のコンテナの例の確認状況 | runbook | pgBackRest のリポジトリを `<backup.dir>/pgbackrest` にマウントした形で確認済みとして更新。データのボリュームを失ったときの流れを追記 |

### 観測: `audit.seal_backlog` は 90 秒の時点で ok

2 回目の復元の後、worker を起動してから 90 秒で `audit.seal_backlog` は ok だった（`nothing sealed yet … unnumbered rows waiting, audit sequencing off`）。
これは判定の窓（5 分）の中だからで、番号付けが止まっていることを示す ok ではない。窓を過ぎると `audit_sequencing=off` のままでも
FG-DB-0015 で失敗する。この点は runbook `audit-seal-backlog` の「想定どおりの場合」（警報が出続ける）と一致する。
この訓練の後の状態（窓を過ぎた後の `doctor` の結果）は記録していない。

## 残り（この記録の範囲外）

- 製品: `forge doctor` の `feed.sink_compare` の警告の文言（`crates/forge-cli/src/doctor/integrations.rs`）は「復元の後に `new-epoch`」のまま。
  runbook と食い違うので、文言の修正を別の変更で行う。
- 試験用インスタンス `gs-m3-test` での訓練: 手順 11〜12、実際の Kanflow v2 の回復、Webhook・push ミラー・代理実行の確認。
- 記録に入っていない項目: 使った世代の ID（`db.label`）と `forge restore verify` の出力全文。必要なら追記する。
