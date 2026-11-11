# 15 マイワークのサイドバーとグループ階層の閲覧権限

- 作成: 2026-11-11（利用者の依頼から。設計は `planner`、コードは `main` 7ca1256 で確かめた）
- 状態: 推奨で進める。§6 の 3 点は既定（推奨）で進め、利用者が違えば直す

## 0. 目標（1 文）

サインインした人が、左のサイドバーの「マイワーク」から 4 つの画面に行けるようにする。4 つは、自分の作業の状態（ホーム）、自分のプロジェクト、見てよいグループの階層、関わっているプロジェクトの動き（アクティビティ）。グループの階層には閲覧権限を付け、見えない人には存在も分からないようにする。

利用者の依頼（原文）:
> サイドバーには　マイワークとしてのメニューをひらいて　ホームには自分のマージリクエストや確認承認まちのステータスなどの件数や最近の更新フォローなどが　プロジェクトは自分が作ったリポジトリの一覧が見られ　グループでは全体のグループが見られる　ここでグループの追加やグループの中にグループを作ったりと階層の設定とプロジェクトの追加もできるようになっている　アクティビティでは自分が関わっているプロジェくとのアクティビティが見られるようにする　このグループ階層では閲覧権限を設ける感じにする

## 1. 前提の点検（コード `main` 7ca1256）

### 1.1 もうあるもの（作り直さない）

| 項目 | 状態 | 根拠 |
|---|---|---|
| グループの入れ子 | ある。namespaces に `parent_id`、深さは最大 20 段、パスは最大 255 字 | `crates/forge-db/migrations/20261028100100_users_and_namespaces.sql:72,83`、`crates/forge-core/src/namespace_path.rs:13` |
| サブグループの作成 | ある。`POST /api/v1/groups/{id}/subgroups`、グループの Owner だけ | `crates/forge-web/src/routes/groups.rs:1-4,36`、`crates/forge-auth/src/authorize.rs:619-621` |
| 最上位グループの作成 | ある。管理者だけ | `authorize.rs:618` |
| グループ内のプロジェクト作成 | ある。グループの Maintainer 以上 | `authorize.rs:408` |
| グループの移動と循環の防止 | ある（D5 の `GroupTransfer`） | `authorize.rs:492-505`、`crates/forge-db/src/namespaces/moves.rs:562` |
| 役割の継承 | ある（GitLab と同じ形）。上位グループの役割が下に効く | `crates/forge-db/src/roles.rs:19-30`、ADR-0010 |
| プロジェクトの可視性 | ある。`private` と `internal`（匿名の閲覧はない） | `20261031100000_projects_and_repositories.sql:45-46`、ADR-0010 |
| 一覧の権限の判定 | ある。行ごとに `authorize()` で決める | `crates/forge-db/src/lists.rs:5-12` |
| 見えないものへの返事 | 404（`FG-REPO-0005`） | `authorize.rs:113-124` |
| 通知の受信箱 | ある。`/-/notifications`、未読数の API、ヘッダーのベル | `web/src/App.tsx:49`、`routes/in_app_notifications.rs:42-48` |
| イベントの表 | ある。`events`、索引 `(project_id, seq)`、保持期間、`VisibilityClass` | `20261027100000_events_and_audit_log.sql:44`、`crates/forge-core/src/event_type.rs:9` |
| 今のホーム | 参加中のプロジェクト・グループ、最近見たもの（ブラウザに保存） | `web/src/pages/Home.tsx:46-47,152` |

### 1.2 ないもの（この計画で作る）

| 項目 | 根拠 |
|---|---|
| サイドバー（Layout はヘッダーと main だけ） | `web/src/components/Layout.tsx:80-86` |
| **グループの閲覧権限**。今はサインインした全員がどのグループも見られる。`visibility` の列がない | `authorize.rs:616-617,1513-1518`、`20261029100000_groups_and_members.sql:19-27` |
| プロジェクトの作成者（`projects.created_by` が無い） | `20261031100000_…sql:39-54` |
| 自分の MR をプロジェクトをまたいで集める API と件数の API | ルートが無い |
| レビュアーの割り当て（承認の記録はあるが「誰に頼んだか」は無い） | `20261111100000_merge_request_approvals.sql:46-64` |
| 人向けのアクティビティの API（イベントを読む Action は連携用 `IntegrationEventRead` だけ） | `authorize.rs:1121-1134` |
| フォロー（ウォッチ）の表 | migrations に無い |
| 親を指定した一覧（`parent_id=`）。GroupPage は全件を取ってブラウザで絞るので、上限を超えると欠ける | `web/src/pages/GroupPage.tsx:30-33` |

### 1.3 前提への意見

- 「階層の設定」と「グループの中にグループ」はサーバーにもうある。足りないのは画面と閲覧権限。
- 閲覧権限は権限の判定を変える（CRITICAL になりうる）。画面とは別の MR にし、権限の表の allow / deny を先に書く。
- プロジェクトのパスにはグループのパスが入る（`group/sub/proj`）。中のプロジェクトを見てよい人に親グループの名前は隠せない。これを規則に入れる（§3.2）。

## 2. 決めたこと（推奨）

### 2.1 グループの閲覧の形 — **A を採る**

| 案 | 中身 | 危険 |
|---|---|---|
| **A（採用）** | `groups.visibility`（`private` / `internal`）。実際に効く可視性は親の鎖で一番狭いもの。親より広げる変更は断る | 鎖の計算の誤りで漏れる → 権限の表と性質のテスト |
| B | グループごとに「見てよい人」の一覧（継承しない ACL） | 役割の継承と二重になり判定が複雑 |
| C | グループは全員に見せ、プロジェクトの `private` だけで守る | 依頼を満たさない |

理由: 役割の継承（ADR-0010）と、プロジェクトの可視性の語をそのまま使える。

### 2.2 サイドバーの「プロジェクト」 — **P3 を採る**

タブを 2 つ: 「自分が作成」（既定）と「参加中」。依頼の言葉（自分が作った）を既定にし、継承で入ったものも辿れる。`created_by` の列と過去分の補完が要る。

### 2.3 「確認・承認待ち」の定義 — **R1 を採る**

計算で出す: 開いていて下書きでなく、作者が自分でなく、自分が承認できる役割を持ち、今の head への自分の有効な承認が無い MR。レビュアーの割り当て（R2）は M5 以降の別計画。

深さの上限は今の 20 段のまま。

## 3. 設計

### 3.1 サイドバー

| 順 | ja | en | id | 行き先 | API |
|---|---|---|---|---|---|
| 見出し | マイワーク | My work | Pekerjaan saya | — | — |
| 1 | ホーム | Home | Beranda | `/` | 新 `GET /api/v1/user/merge-requests/counts`、既存の未読数、新アクティビティ |
| 2 | プロジェクト | Projects | Proyek | `/-/projects` | 既存 `GET /api/v1/projects?membership=true`、新 `?created_by=me` |
| 3 | グループ | Groups | Grup | `/-/groups` | 既存 `GET /api/v1/groups`、新 `?parent_id=` |
| 4 | アクティビティ | Activity | Aktivitas | `/-/activity` | 新 `GET /api/v1/user/activity` |

- `<nav aria-label>` の中に見出しとリスト。今いる画面に `aria-current="page"`。通知・設定・管理はヘッダーとユーザーメニューに残す。
- 畳んだとき（広い画面）: アイコンだけ。名前は見えない文字で残す。開閉の状態は localStorage（秘密ではない）。
- 狭い画面（< 768px）: ヘッダーのメニューボタンで開く（`aria-expanded`）。Esc で閉じてボタンにフォーカスを戻す。
- ホームの項目に「承認待ち n」のバッジ。数の意味は見えない文字でも伝える。
- 予定だけの空の枠は出さない（計画 10）。画面ができた順に項目を足す。
- `-/` のルートは `App.tsx:50` の NotFound より前に足す。

### 3.2 グループ階層の閲覧（案 A）

| 規則 | 中身 |
|---|---|
| 実際の可視性 | 自分から最上位までの鎖で一番狭いもの。プロジェクトにも効く（`internal` のプロジェクトも、上に `private` があれば非公開） |
| 閲覧できる人 | 鎖のどこかで役割を持つ人（直接・継承）、管理者、実際の可視性が `internal` ならサインインした人とボット（サービスアカウントは参加しているときだけ。今の `sees_internal` と同じ） |
| 通り道として見える | 下に見てよいプロジェクトかサブグループがある人には、名前とパスだけ（`via=descendant`）。メンバーやほかの子は見せない |
| 見えない人 | 404 `FG-REPO-0005`。一覧、パスの解決、ページ、作成、git の clone のどれも同じ |
| 見えるが役割が足りない | 403 |
| 変更できる人 | `GroupVisibilityChange` はグループの Owner の人だけ（トークン・エージェントは断る） |
| 親より広げる変更 | 断る（新しいコード）。プロジェクトを `internal` にする変更も親の鎖に照らして断る |
| 狭める変更 | 許す。子の値は書き換えず実際の可視性で効かせる。画面で「n 件のプロジェクトが非公開として扱われます」と確かめてから |
| 既存のデータ | マイグレーションの既定は `internal`（今の動きのまま） |
| 判定点 | `authorize()` だけ。ローダー（`roles.rs` の鎖）が、一番狭い可視性と「見えるものが下にあるか」を 1 つの文で返す |

### 3.3 各画面

- **ホーム**: 件数のカード 4 枚（自分の MR（開いている）、確認・承認待ち、自分の MR で要対応（衝突の予測・必須チェックの失敗）、未読の通知）。カードから `/-/merge-requests?scope=…` へ。その下に最近の更新 10 件と、最近見たもの。
- **プロジェクト**: §2.2。絞り込みは今のホームから移す。「新しいプロジェクト」のボタン。
- **グループ**: 開閉できる入れ子のリスト（tree の役割は使わない）。子は開いたときに `?parent_id=` で読む。各行に可視性の印。役割に合うときだけボタン: 「グループを追加」「サブグループ」（Owner）「プロジェクトを追加」（Maintainer）。判定はサーバーでもう一度。
- **アクティビティ**: 自分が役割を持つプロジェクトのイベントを新しい順に。keyset（`before=<seq>`）で 30 件ずつ。

### 3.4 アクティビティの漏れ止め

- 参加プロジェクトの id を一覧のスキャンで取る（上限 500、超えたら `truncated`）。`events WHERE project_id = ANY($ids) AND event_type IN (許す型)` を `seq DESC` で読む（索引 `events_project_seq`）。
- 行ごとに新しい Action `ActivityRead` × `Resource::ProjectEvents{project, class}` で判定。v1 は `VisibilityClass::Integration`（Reporter 以上）だけ。
- 出す項目: 型、行った人の表示名、プロジェクトのパス、MR の iid とタイトル、時刻。ペイロードは許す項目だけ。見えないプロジェクトの行は数えるだけ（ログに `rows_hidden`）。
- 保持期間を過ぎたイベントは出ない。画面にもそう書く。

## 4. 段と MR

どの MR も、fmt・clippy・nextest、`gen-catalog` と `gen-authz` の差分なし、`/review` で CRITICAL / HIGH なし。Web の MR は `npm run lint`、`npm run typecheck`、`npm test` も。

| # | ブランチ | 中身 | 権限の表 | コード | マイグレーション | 主な試験 | 評価役 |
|---|---|---|---|---|---|---|---|
| 1 | `feat/ui-sidebar-shell` | Sidebar を Layout に。ホーム・プロジェクト（`/-/projects`、参加中のタブだけ、既存 API）・グループ（既存の一覧）。ja/en/id | — | — | — | `aria-current`、狭い画面の開閉と Esc、キーボードだけで巡れる、キーがそろう | `a11y-architect`、`design-reviewer` |
| 2 | `feat/db-group-visibility` | `groups.visibility`（既定 `internal`、監査の対象）。鎖の CTE が一番狭い可視性と `has_visible_descendant` を返す（判定にはまだ使わない） | — | — | 追記のみ。`tables.toml` と doctor も | 既存は `internal`、20 段でも正しい | `database-reviewer`、`security-reviewer` |
| 3 | `feat/auth-group-visibility` | `GroupRead` / `GroupList` / パスの解決 / `ProjectRead` / `RepoRead` を実際の可視性で判定。`GroupVisibilityChange` と `PATCH /api/v1/groups/{id}/visibility`。`GroupBody` に `visibility`・`viewer_role`。`?parent_id=` | §4.1 | 新 `FG-REPO-0033`（仮）VisibilityAboveParent | — | 非メンバーに 404、継承のメンバーは allow、通り道は名前とパスだけ、clone も 404、親より広げると拒む | `authz-matrix-auditor`、`security-reviewer` |
| 4 | `feat/ui-groups-tree` | `/-/groups` の入れ子、作成のダイアログ（グループ・サブグループ・プロジェクト）、可視性の設定（狭めるときの確認）。GroupPage を `parent_id=` で読む | — | 0033 の表示 | — | 権限がないとボタンを出さない、拒否は `user_message` と `request_id`、空・読み込み中・失敗 | `a11y-architect`、`i18n-sync-auditor` |
| 5 | `feat/db-project-creator` | `projects.created_by`（null 可、作成時に書く）。過去分は監査ログから補う。`?created_by=me`。「自分が作成」のタブ | 既存 `ProjectRead` | — | 追記のみ | 補えない行は null（作成者不明）、他人のものが混ざらない | `database-reviewer`、`typescript-reviewer` |
| 6 | `feat/web-user-merge-requests` | `GET /api/v1/user/merge-requests?scope=authored\|approvable\|attention` と `/counts`。索引 `merge_requests (author_id, state)` | `UserMergeRequestList`（本人だけ）。各行は `MergeRequestRead` | 既存 `FG-WEB-0006` | 索引の追記 | 見えないプロジェクトの MR は数に入らない、承認済みの head は出ない、作者自身は承認待ちに出ない | `authz-matrix-auditor`、`database-reviewer` |
| 7 | `feat/ui-home-dashboard` | ホームの件数カードと `/-/merge-requests`。参加中プロジェクトの一覧はプロジェクトの画面へ | — | — | — | 0 件のとき次の一歩が出る、カードから一覧へ、バッジの読み上げ | `design-reviewer`、`a11y-architect` |
| 8 | `feat/web-user-activity` | `GET /api/v1/user/activity?before=`。許す型、許す項目、`rows_hidden` のログ | `ActivityRead`（Reporter 以上、Integration だけ） | — | — | private で役割がないと 0 行で名前も出ない、Maintainer のクラスは出ない、keyset の境目 | `security-reviewer`、`authz-matrix-auditor` |
| 9 | `feat/ui-activity` | `/-/activity` とホームの「最近の更新」 | — | — | — | 型ごとの文、知らない型は一般の文、さらに読む | `i18n-sync-auditor`、`typescript-reviewer` |

### 4.1 MR 3 の権限の表（allow と deny の両方）

| Action × Resource | allow | deny |
|---|---|---|
| GroupRead × private | 直接・継承のメンバー、管理者、通り道（制限した見え方） | 非メンバー → 404、サービスアカウントの非メンバー → 404 |
| GroupRead × internal（親が private） | 鎖のメンバー | 非メンバー → 404 |
| ProjectRead / RepoRead × internal（親が private） | 鎖のメンバー | 非メンバー → 404 |
| SubgroupCreate / ProjectCreate × 見えないグループ | — | 404（403 にしない） |
| GroupVisibilityChange | 人の Owner | Maintainer → 403、トークン・エージェント → 403、見えない → 404、親より広げる → 0033 |
| ProjectVisibilityChange → internal（親が private） | — | 0033 |

- 運用: doctor `groups.visibility`（親より広い値で保存された子の数を info）。新しい警報は無いので runbook は足さない。0033 は操作の誤りで runbook なしと catalog に書く。各ルートは `request_id`・`error_code`・`rows_scanned`・`rows_hidden` をログに出す。
- ADR: MR 3 で ADR-0010 に Amendment（グループの可視性と、一番狭いものを取る規則）。

着手の前に確かめること（計画で確かめきれなかった点）:
- アクティビティで許すイベントの型の名前（`forge-core/src/event_type.rs`）。
- 監査ログに `created_by` を補える作成の行があるか（MR 5）。
- `GroupBody` に `viewer_role` が既にあるか（ADR-0015）。
- `TransferTarget::Group { visible }`（`authorize.rs:1550`）の意味と新しい可視性の整合（MR 3）。
- 「承認できる役割」は既存の承認の規則の関数を使う（写さない。MR 6）。
- コードの番号 0033 が空いているか。

## 5. M4 との関係

- 画面だけの MR 1・4・7・9 は M4 と並べて進めてよい（別の worktree）。
- MR 3 は `authorize.rs` と権限の表を変え、C9（自動マージ）と C11（デプロイ）とぶつかる。**C11 が入った後** に入れる。それまでは MR 2・5・6 を先に。
- R1（M4 の完了）の判定には含めない。「M4-UX」の並行の線として数える。

## 6. 危険と、既定で進めること

| 危険 | 対策 |
|---|---|
| 鎖の誤りで、非公開のものが一覧・パスの解決・clone・アクティビティから漏れる | 権限の表、性質のテスト（どの鎖でも実際の可視性 ≤ 各段）、`security-reviewer` |
| 狭めたとき下のプロジェクトが急に見えなくなる | 確認で件数を出す。監査の行とイベント `group.visibility_changed` |
| 参加が多い人のアクティビティと件数が遅い | id の上限 500 と `truncated`、索引、メトリクス |
| 「承認待ち」が多すぎて役に立たない | 「承認できるもの」と明記。声が出たら R2 を別計画に |

既定（推奨）で進める 3 点。違えば言ってもらう:

1. **最上位のグループを作れる人**: 今のまま管理者だけ。サイドバーの「グループを追加」は管理者にだけ出す。ほかの人はグループの Owner としてサブグループを作る。
2. **新しく作るグループの既定の可視性**: `private`（見せたいときに広げる。漏れより手間を選ぶ）。既存のグループは `internal` のまま。
3. **「最近の更新フォロー」**: 関わっているプロジェクトの動きで足りるものとする（表が要らない）。明示の「フォロー」ボタンは後で要れば別計画。
