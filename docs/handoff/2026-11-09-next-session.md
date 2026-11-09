# 引き継ぎ — 次のセッション（2026-11-09）: Kanflow K4 から

前の引き継ぎ: `2026-11-07-cloud-session.md`（GitStation 側の今日までの分。すべて `main` `1e9e19f` に入った。GitLab には未 push）。

利用者の指示（2026-11-09）: **GitStation の開発を続ける。あわせて Kanflow ver2 の開発も同時に進める。** Kanflow ver2 は
GitHub `masaki5711-program/KandlowVer2`（`main` = v2 の線、最新 `f6d0484`）。Kanflow は GitStation とつながる、
バイブコーディングの開発環境の中心のアプリ。

## 0. 始め方（クラウド）

- リポジトリ: GitStation、GitStation_Agent、Agent_model_TMUX、Agent_Claudecode_ver2 は作業ブランチ `claude/clever-gates-j3elqg`。
  Kanflow は `add_repo`（masaki5711-program / KandlowVer2）で足して `/home/user/kandlowver2` にクローン。
- Kanflow の CLAUDE.md の約束: 本番（`D:\APP\KanflowV2`）に触れない、合成データだけ、**push は利用者が言ってから**、
  ブランチ `<type>/L4-NOPBI-<topic>`、作成者 `Masaki Fujikawa <masaki.fujikawa@music.yamaha.com>`。i18n は ja / en / id
  （`app/src/lib/i18n/`）、権限は `app/src/lib/authz/policy.ts` の 1 か所（新しい操作は `policy.test.ts` に許可と拒否の行）。
- テスト用の DB（Docker が無いので素の PG16）: `2026-11-07-cloud-session.md` §3 の手順で 127.0.0.1:55432 に立て、Kanflow 用に
  ロール `kanflow_ci`（SUPERUSER、パスワード `kanflow_ci`）と DB `kanflow_ci`・`prisma_shadow`
  （`TEMPLATE template0 ENCODING 'UTF8' LOCALE_PROVIDER icu ICU_LOCALE 'und' LOCALE 'C.UTF-8'`）。環境変数は Kanflow の
  `.github/workflows/ci.yml`（`DATABASE_URL` と `DATABASE_HOST/PORT/USER/PASSWORD/NAME`、DB テストは `KANFLOW_DB_TESTS=1`）。
  `app/` で `npm ci` → `npx prisma generate` → `npx prisma migrate deploy`。

## 1. 次にやること: Kanflow K4（F-36、PBI の画面から MR を承認 / 取り消し）

GitStation 側（代理実行 S1 / S2、ADR-0025）は `main` に入っているので、K4 は着手できる。**ブランチ `feat/L4-NOPBI-mr-approve`**
（Kanflow の `main` から）。

### GitStation の取り決め（呼ぶ側から。根拠: ADR-0025 §5、`crates/forge-web/src/routes/act_as.rs`、`web/src/api/actAs.ts`、
`web/src/pages/confirm/ActAsConfirm.tsx`、`ops/runbooks/act-as.md`）

1. Kanflow のサーバーが `state` を作る（128 ビット以上の乱数、`[A-Za-z0-9_-]{22,128}`）。Kanflow のセッションの利用者に結びつけて保存する。
2. **最上位の別ウィンドウ**で開く（iframe は拒否される）:
   `GET {base}/-/confirm/projects/{gsProjectId}/merge-requests/{iid}?intent=approve|unapprove&sha=<head 40/64 hex>&integration=<id>&state=<state>`。
   画面は 10 分で期限切れ、押すボタンは表示から 1 秒後に有効。`sha` が今の head でなければ「古い」と出る。
3. 本人が押す（GitStation の画面が `POST …/assertions` を送る。Kanflow は関わらない）。
4. **戻り先の URL も postMessage も無い**（COOP same-origin）。Kanflow はサーバーから **claim** する:
   `POST {base}/api/v1/integrations/{id}/act-as/claim`、本文 `{"state":"…"}`、サービスアカウントのトークン、`Forge-Act-As` は付けない、
   スコープ `act_as`。押される前は `FG-AUTH-0020`（claim_unknown）。→ 「確認しました」ボタンを押させるか、間隔を空けて問い合わせる
   （claim は連携ごとに 1 分 120 回まで）。200 の本文: `{assertion:"fga_…", sub:{user_id, employee_number?}, intent, project_id, iid,
   head_sha, target_ref, expires_at, …}`。秘密は 1 回だけ返る（2 回目は `claim_repeated`）。
5. **`sub` が `state` に結んだ Kanflow の利用者と同じかを必ず確かめる**（違えば捨てる。ADR-0025 の Risks。K4 にテストが要る）。
6. 承認: `POST {base}/api/v1/projects/{id}/merge-requests/{iid}/approve`、本文 `{"sha":"<head>"}`、ヘッダー
   `Authorization: Bearer <SA トークン>`・`Forge-Act-As: user:<sub.user_id>`・`Forge-Assertion: fga_…`。取り消しは `…/unapprove`。
   答え `{iid, outcome: approved|withdrawn|unchanged, counted, uncounted_reason, approval}`。
- トークンのスコープ: `act_as` + `merge_approve`（`api` では承認できない）。読み取りの代理は `act_as` + `read_api`。
- 主張の有効期限は 5 分（DB の時計）、1 回限り。検証の後に失敗しても使用済みになる。新しい head には引き継がれない。
- ボタンを出すかの判定: `GET …/merge-requests/{iid}/status` を `Forge-Act-As: employee:<User.employeeNumber>` 付きで読み、
  `viewer.act_as.can_approve / can_unapprove`（`{allowed, reason}`）と `viewer.approved_by_me`。社員番号は保存したとおりに送る（大文字小文字を変えない）。
- 承認数: 開発状況の API（I1d）`GET …/integrations/{id}/work-items/PBI-<n>/dev-status?project_id=…&state=all` の `approvals.given`
  （`Forge-Act-As` は拒否される、SA トークンで）。マージできるかと理由は `/status`。
- エラーと利用者への文（ja / en / id は Kanflow が出す）: `FG-AUTH-0020`（主張の問題すべて → 「もう一度確認してください」）、
  `FG-INT-0006`（本人を特定できない → 「この利用者の代わりに操作できません」）、`FG-AUTH-0003`（代理の許可外・権限なし）、
  `FG-AUTH-0002`（スコープ不足 → 設定の問題）、`FG-REVIEW-0013`（自分の MR）、`FG-REVIEW-0008`（古い sha）、`FG-REPO-0005`（結びつけ無し）、
  `FG-WEB-0010`（429、`Retry-After`）。本文に `reason` は無い。

### Kanflow 側で手を入れる場所

- HTTP クライアント `app/src/lib/git/gitstation-client.ts`: 今は GET だけ（`sendOnce` のヘッダーが固定）。代理のヘッダー付きの POST を足す
  （15 秒の時間切れ、`redirect:"error"`、`GITSTATION_ALLOWED_HOSTS` はそのまま）。
- トークン: `GitIntegration.tokenCipher`（`app/src/lib/secret-box.ts`）、`repositoryToken()`（`services/git/git-sync.ts`）。
- MR の行: `GitLink`（`kind:"mr"`、`ref` = iid、GitStation のプロジェクト ID は `GitRepository.externalId`）。**head の SHA は保存されていない**
  → `/status` か開発状況の API で読む。表示は `components/git-dev/git-link-list.tsx`（`LinkRow`）を `pbi-dev-panel.tsx` が使い、
  `components/pbi-detail-modal.tsx` と `sprints/components/task-pbi-detail-panel.tsx` に出る。POST とトーストの手本は `use-git-actions.ts`。
  「自分のタスク」（`dashboard/my-project-tasks.tsx`）には MR の行が無い（`GitCardBadge` だけ）。
- 権限: `policy.ts` に `git.approve` のような操作を足す（`requireProject(projectId, "<action>")` の形の route）。
- `state` の保存: 新しい表（state、利用者、プロジェクト、MR、intent、sha、期限、使用済み）をマイグレーションで足す（追記だけ）。
- テスト: vitest（`*.test.ts` は node、`*.test.tsx` は jsdom）。MSW は無い。偽の GitStation `services/git/__tests__/fake-gitstation.ts`
  （注入する `fetchImpl`）を広げる。試験の要件（計画 11 の K4）: 主張なしの送信は拒否、権限の無い人にはボタンを出さない（理由つき）、
  二重押し、`sub` の取り違え。評価役は `typescript-reviewer`・`security-reviewer`・`a11y-architect`（加えて i18n）。
- 決定は Kanflow の `references/decisions.md` に K4 の行を足す（「確認しました」ボタンか問い合わせか、など）。

## 2. その後の候補

- Kanflow: 開発状況の API（I1d）で MR の一覧の代用をやめる（Kanflow `references/decisions.md` l.11）。
- GitStation: 不安定なテスト `a_killed_pack_refs_removes_its_own_lock_and_keeps_a_foreign_one`（単独でも 3 回中 2 回落ちる）の修正。
  O1 バックアップと復元（保管の道具 Q11 の答えが要る。無ければローカルのディレクトリの案で）。
- エージェントモデル: W12（`spec-authoring`）、エージェントモデル自身の GitHub Actions、`npm test` の要約の件数のぶれ。

## 3. 利用者の作業待ち

- PC で GitStation の `git pull --ff-only origin main` → `git push gitlab main`（`gitlab` が gitstation のプロジェクトを指すことを確かめる。
  kanflow_ver2 ではない）。
- O2 を本物の GitLab で確かめる: 試験用の GitLab プロジェクトとミラー用のトークン（計画 11 Q7）。
