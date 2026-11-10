# Runbook: ci-runner（FG-CI-0004、FG-CI-0005。関連: FG-CI-0006、FG-AUTH-0001 / 0003、FG-REPO-0005）

M4 C3（計画 14、ADR-0029）の Runner の API（Runner の登録、ジョブの取得の long-poll、心拍とリース、ログ、完了、リースの回収）が
期待どおりに動かないとき。表 `forge.ci_runners`・`forge.ci_job_tokens`・`forge.ci_log_chunks`・`forge.ci_jobs`（`runner_id`・`lease_until`・
`lease_losses`）、worker のタスク `ci_lease_reaper`、doctor `ci.runners`。

## Trigger

- `forge worker` のログに `forge.ci.no_runner_available`（error、**alert**、FG-CI-0005）: 10 分を超えて `pending` のジョブがあり、
  どの Runner も取っていない。10 分に 1 回まで出る。`forge doctor` の `ci.runners` も同じコードで fail
- `forge worker` のログに `forge.ci.job_lease_lost`（warn、FG-CI-0004）: Runner の心拍が途絶え、リース（120 秒）が切れたジョブを
  回収した（`outcome=requeued` は列に戻した、`failed` は 3 回目の喪失で失敗にした、`released` は取り消し済みのジョブを放した）
- Runner のホストのログに 409 `FG-CI-0004`（その Runner はジョブを止める）、401 `FG-AUTH-0001`（`reason=runner_disabled|unknown|mismatch`）、
  403 `FG-AUTH-0003`（`reason=lease_other_runner|job_finished`）、413 `FG-CI-0006`（ログの上限）
- 利用者から「パイプラインが `pending` のまま進まない」と問い合わせがあった

## Impact

- ジョブが待つだけで、失われはしない（push・閲覧・マージの判定は影響を受けない。`ci/pipeline` を必須にするのは計画 14 X）。
- FG-CI-0005 が全プロジェクトで続くなら CI 全体が止まっている: SEV3。1 つのタグのジョブだけなら SEV4。
- FG-CI-0004 が 1 件なら Runner の一時的な不調。同じ `runner_id` で続くならその Runner のホストの障害（SEV4、他の Runner があれば）。

## Diagnose（読み取りのみ）

1. Runner の一覧と最後の連絡（Web の管理画面が無いときは SQL。`last_contact_at` は Runner の要求のたびに 30 秒に 1 回まで更新）:
   `SELECT id, name, enabled, tags, last_contact_at FROM forge.ci_runners ORDER BY id;`
   - 有効な Runner が 0: 登録されていない・すべて無効にされた
   - `last_contact_at` が 5 分より古い: Runner のプロセスが止まった・ネットワークが切れた（Runner のホストで `systemctl status forge-runner`、
     C4 以降）
2. 待っているジョブと、そのタグ:
   `SELECT id, pipeline_id, project_id, tags, updated_at FROM forge.ci_jobs WHERE status = 'pending' ORDER BY id LIMIT 20;`
   ジョブは **ジョブのタグをすべて持つ** Runner にだけ渡る（タグの無いジョブはどの Runner でも）。機密のリポジトリ用にタグを分けた
   Runner が止まると、そのタグのジョブだけが待つ。
3. 持たれているジョブとリース:
   `SELECT id, runner_id, attempt, lease_until, lease_losses, status FROM forge.ci_jobs WHERE runner_id IS NOT NULL ORDER BY lease_until;`
   `lease_until` が過ぎたものは `ci_lease_reaper`（10 秒ごと）が回収する。過ぎたまま残るなら worker が止まっている（`forge doctor`、
   ログの `task=ci_lease_reaper`）。
4. 1 つのジョブの経過: ログの `job_id` で `forge.ci.job_claimed` → `forge.ci.job_lease_lost` / `forge.ci.job_completed` を追う
   （`request_id`・`runner_id`・`attempt`）。
5. ジョブのトークン（平文は残らない。ハッシュも読み取り用の接続からは見えない）:
   `SELECT id, job_id, attempt, runner_id, expires_at, revoked_at, revoked_reason FROM forge.ci_job_tokens WHERE job_id = <id>;`
   `revoked_reason` は `job_finished` / `job_canceled` / `lease_lost` / `project_deleted` / `project_moved`。期限は DB の `now()` で判定する。
6. 401 `FG-AUTH-0001` の Runner: ログの `credential=runner`・`reason`。`runner_disabled` は管理者が無効にした、`unknown` / `mismatch` は
   トークンのファイルが古い・壊れた（再登録が要る。トークンは登録のときに 1 回しか表示されない）。

## Mitigate（変更は人の承認のあと）

- Runner のプロセスを起こす・ホストを直す（C4 の `forge-runner`）。回収されたジョブは自動でやり直される（新しい `attempt`）。
- 無効にした Runner を有効に戻す（管理者の Web のセッション）: `PATCH /api/v1/admin/runners/{id}` `{"enabled": true}`。
- ジョブのタグに合う Runner が無い: 管理者がタグを足す（`PATCH … {"tags": [...]}`）か、そのタグの Runner を登録する
  （`POST /api/v1/admin/runners` `{"name", "tags"}`、応答の `token` を Runner のホストのトークンのファイルに置く。ADR-0013）。
- トークンが漏れた・失われた: その Runner を無効にし、新しい名前で登録し直す（Runner は消さない。ジョブの記録が `runner_id` を持つ）。
- FG-CI-0004 で `failed` になったジョブ: Runner が健全になってから、パイプラインを再実行する（`gstn pipeline retry --project <id> <pipeline_id>`）。
- **行を直接書き換えない**。`lease_until` や `status` を手で変えると、Runner と DB の状態が食い違い、同じジョブが 2 回動く。

## Verify

- `forge doctor` の `ci.runners` が ok（有効な Runner と生きている Runner が 1 以上、10 分を超えて待つジョブが無い）。
- 待っていたジョブに `forge.ci.job_claimed` が出て、`gstn pipeline show` で `running` → `succeeded` / `failed` に進む。

## 知っておくこと

- **2 つの Runner が同じジョブを取ることは無い**: 取得は `FOR UPDATE OF job, pipeline SKIP LOCKED` の 1 文。
- **リースの喪失はジョブを 1 回だけ列に戻す**: 回収はパイプライン → ジョブの順にロックし、リースが切れていることを同じ文で確かめる。
  3 回目の喪失で FG-CI-0004 の `failed`。古い `attempt` の Runner の呼び出しは何も変えない（409）。
- **取り消し**: 人がパイプラインを取り消すと、実行中のジョブは Runner を持ったまま `canceled` になり、トークンは同じトランザクションで失効する。
  Runner は次の心拍で `{"state": "canceled"}` を受け取って止め、完了の呼び出しで放す（来なければ回収が放す）。
- **シークレット**: 渡してよいかは `StoredPipeline::secrets_allowed()`（作成時に保護されたブランチへの push）だけで決める。応答の
  `secrets_allowed` が偽なら `secret_names` は空。値は C6 まで存在しない。
- **ログ**: 追記だけ（`seq` の再送は何もしない）。1 チャンク 64 KiB（JSON で送るので文字は 32 KiB まで）、1 試行 4096 チャンク・16 MiB まで
  （FG-CI-0006）。**C3 にはログを読む経路が無い**。読み取りは C5 で、出力の機密の印を継いでエージェントに FG-AUTH-0022 を返す（ADR-0029 決定 5）。
- **Runner のトークンはリポジトリも API も読めない**（FG-AUTH-0003）。ジョブのトークンは自分のプロジェクトの固定した SHA の複製
  （`PinnedCopy`）だけで、それ以外の API は見えないプロジェクトと同じ FG-REPO-0005。ジョブの fetch の Smart HTTP の経路は C4 で入れる。

## 関連

- 計画 14（M4）§4・§5 C3・§6、ADR-0029、runbook `ci-pipelines`、`crates/forge-pipelines/README.md`、`crates/forge-auth/README.md`（CI principals）。
