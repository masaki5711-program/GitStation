# forge-marketplace

エージェントのマーケットプレイス（計画 16 §2・A5、ADR-0032 Amendment A5）。部品は git で作り、人が tag を公開したときに DB に固める。
SQL は `forge_db::agent_marketplace`、判定は `forge_auth::authorize()`、部品の解析・検査・要約・署名の確かめは純粋な `forge-agent`。
機能クレートで、他の機能クレートには依存しない（契約 9）。**部品に署名するのはこのクレートの公開の 1 か所だけ**（`clippy.toml` の
`disallowed-methods`）。

| 公開の API | 中身 |
|---|---|
| `Marketplace::new(db, browse, AgentConfig)` | `forge serve` が作る。`agent.parts_repository`（既定 `marketplace/parts`）、`agent.signing_keys_file`、`agent.mcp_allowed_hosts` |
| `load_for_publish(slug, tag, viewer)` | 公開のルートのローダー: 部品のリポジトリとそのグループ、tag（git）、tag を押した人（push の監査だけ。tagger は信じない）、tag の裏の人（tagger・author・committer をメールでアカウントに）、`network` の能力 → `AgentPartCtx`。**tag が今指すコミットを固定する** |
| `publish`（`AgentPartPublish`） | 固定したコミットの `<slug>/` を読み、`parse_part` → `scan`（`gstn agent verify` と同じコード）→ 既定の枝に入っているか → そのコミットの最新のパイプラインが成功か（FG-AGENT-0004）→ 鍵ファイルのいちばん大きい kid で署名（登録済み・退役も失効もしていない・同じ公開鍵、でなければ FG-AGENT-0014）→ 登録した公開鍵で確かめ → 部品・版（beta）・ファイル・監査の行を 1 つのトランザクションで |
| `load_version` / `promote`（`AgentPartPromote`）/ `revoke`（`AgentPartRevoke`） | 昇格は公開の監査の行が記録した事実（押した人・裏の人）で判定する。取り消しは理由 1〜500 文字、最終（FG-AGENT-0006） |
| `plan_build` / `recommend_for` / `record_build`（A7） | ビルダーの Dry-Run と記録、規則だけの推薦（`builder`） |
| `plan_update` / `ask_update` / `AgentUpdater`（A9） | 更新の MR（`updates`）: lock の各部品を取り消していない最新の stable へ（下げない。取り消した版で置き換えが無ければ外す）、`adapter` でベースモデルを移す（世代の移行）。`AgentUpdater::run` はジョブ `agent.update_merge_request`（`forge serve`）の中身で、頼んだ人か最後にビルドを頼んだ人の今の権限で判定し、A7 の枝 `agent-builder/<要約>` にボットのコミットを書いて MR を 1 回だけ開く。取り消しは同じトランザクションで、その版を選んだビルドのある各プロジェクトにジョブを入れる |
| `list` / `part` / `version` / `keys` | 目録（`AgentPartList`）、部品（`AgentPartRead`）、公開鍵（`AgentKeyRead`、`gstn` の `agent-keys.json` の形） |

**エラーコード**: FG-AGENT-0001・0004・0005・0006・0008・0012・0013・0014・0015・0016（`errors.toml`）。**runbook**: `agent-signing-key`、`agent-supply-chain`。

**落とし穴**
- 公開した版は変わらない。tag を動かしても公開済みには効かず、同じ版の再公開は FG-AGENT-0012（`moved_tag_after_publish_has_no_effect`）。
- tag を押した人が分からない（監査の行が無い、100 を超える ref の push、再生した push、サービスアカウント・代理の push）ときは公開できない（閉じる側）。
- サーバーが決めたフックの試験のジョブ（計画 16 A5）はまだ無い（A5b）。それまでは部品のリポジトリのパイプラインが緑であることだけを求める。
