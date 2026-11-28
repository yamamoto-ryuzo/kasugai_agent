# KASUGAI Agent — 開発規約

「課題の検出 → 評価 → 対応 → 実機検証 → 学習」の自律ループを実装する常駐システム。
全体設計は `README.md`（統一モデルが正本）を参照。

## 基本原則（最重要）：mermaid が唯一の指示書

- **機能追加・変更は必ず mermaid 図の更新から始める**。実装はその図に従う
- mermaid 図の正本は **`diagrams/*.mmd`（1図1ファイル）**。図を直すときは
  ここだけを編集する
- `README.md`・`docs/*.html` 内の埋め込み図は `node tools/sync-diagrams.mjs`
  が生成する写し。**直接編集禁止**。埋め込み位置は
  `<!-- diagram: <name> -->` マーカーで指定する
- 同期の検証は `node tools/sync-diagrams.mjs --check`（乖離時 exit 1）
- HTML エスケープ（`<br/>`・`-->` 等）は sync ツールが行う。`.mmd` には
  生の mermaid を書く
- `.mmd` の先頭には **frontmatter の title が必須**
  （`---` / `title: <図名>` / `---`）。未記載は sync ツールが ERROR で検出する
- 新規図: `.mmd` を作り、挿入位置にマーカー＋空ブロックを置いて sync を実行
- 図と実装が乖離したら**図を先に直す**（図が仕様、実装はその反映）
- ASCII アート図は新規に書かない（既存資産からの引用を除く）

## マニュアル（docs/）

- マニュアル（`docs/*.html`）の書式・正本は **`diagrams/docs/`**
  （書式は KASUGAI Canvas の `docs/` を参考）
  - `<page>.md` — 1ページ1ファイル。先頭の frontmatter `title` が必須
  - `nav.json` — サイト名・章立て・外部リンク（`demos[]`）。
    `groups[].items[].page` = md 名。`label`（章ラベル）・`children`
    （項目の子リンク）・`sectionPage`/`labelPage`（ラベルのリンク先）を
    任意で持てる。`demos[]` の項目は `{action:"launch", target, port}` で
    `POST /api/control/launch` を呼ぶ環境起動ボタンにもできる
  - `template.html` / `style.css` / `nav.js` — 共通書式
    （nav.js は Canvas docs/nav.js 準拠のサイドバーナビ）
- `docs/*.html`・`docs/style.css`・`docs/nav.js` は
  `node tools/sync-docs.mjs` が生成する写し。
  **直接編集禁止**。検証は `--check`（乖離時 exit 1）
- md 内の `<!-- diagram: <name> -->` + ```mermaid ブロックは
  sync-diagrams.mjs が同期する（図の正本は従来どおり `diagrams/*.mmd`）
- パネル左ペインの「マニュアル」ナビで中央ペインに `/docs/` を表示する
  （control が `/docs/*` を静的配信）

## コード構成

- **1ファイル1責務**。`server/src/` の責務分界は README §10 に従う
- **状態遷移は `server/src/pipeline.rs` だけが行う**。他モジュールは遷移要求を投げるだけ
- 実装言語は Rust（axum + tokio、`server/` が正本）。`tools/*.mjs`（図・docs 同期）は Node.js 標準ライブラリのみ
- 制御は loopback bind。外部公開しない

## 状態変更は必ず UI へ反映する（リロード強制）

**状態を変えたら、パネル表示へ即時反映させること。** 状態と画面の乖離は障害とみなす。

- 状態変更を追加・変更した場合、SSE push で即時反映されることを確認する
- **表示用フィールドを追加したら、それを返す API 応答にも必ず含めること**

## パネルのカレントリポジトリ

- 左ペイン title 直下のセレクトは **カレントリポジトリの指定のみ**を行う
- **`すべてのリポジトリ` 相当の選択肢は置かない**。常に1リポジトリを指し、
  未選択（全体表示）状態を作らない
- 選択はパネル全体のスコープ（新規 chat の cwd・
  各一覧の絞込）になり、localStorage に永続化する
- **リポジトリ名の表示はこのセレクトのみ**。選択リポジトリで絞られた一覧・
  画面（candidates・doc 一覧・ヘッダ status 等）では
  行ごとの repo 名を出さない（常に同一リポジトリなので冗長）

## 処理ライフサイクル（verify の混在ルール）

- **再検証（verify）と push は一体の仕事**: push は verify のみが行い、検証を通って
  いないコミットはリモートへ公開しない
- 未pushコミットは `Issue: #N` トレーラで課題に帰属させる。closed /
  `devin:rejected` / 帰属不明のコミットは `git revert` で除去してから push
- `git reset` は禁止（他課題コミットを破壊する）

## 検証の深度（verifyLevel）

- assess が課題ごとに L0〜L4 の深度を決定する。**全課題一律に L3/L4 を要求しない**
- L2 以上は実機検証: 対象EXE自体（QGIS等の成果物バイナリ）を実環境で起動し
  応答確認する。L3 は Playwright 等での画面駆動、L4 は Issue 主旨との照合
- 「実装の確認」= コード/ビルドの成否ではなく、実機で動く成果物の振る舞いの確認
- 証跡（スクショ・応答・ログ）を採取し `evidence/` に残す
- 実機リソース（QGIS harness・固定ポート等）はロックして排他利用する

## 外部起票のゲート規則

- `external` ラベル付き Issue は **自律処理の対象外**。候補一覧にのみ表示し、
  人が dispatch を判断するまで評価・実装どちらにも進まない
- 匿名 Issue は prompt injection の入口になり得るため、内容の目視確認は必須

## 起票と dispatch（人ゲート）

- **起票＝本システムの課題（issues.json）の作成**。GitHub Issue への登録は
  別機構（後続検討）。GitHub Issue は open が全件候補化され、closed は
  `--import-github` / `POST /api/control/import-github` の一括取込で
  終端起票する（COMPLETED→done / NOT_PLANNED→rejected）
- **起票は自動・dispatch は人判断のみ**: detector / 派生 / manual の課題は
  intake で自動起票（queued）されるが、実行の開始は candidates /
  ISSUES 一覧の「対応」ボタンだけが行う。drain は補起票・中断 verify の
  再開・孤児 run の failed 化のみを担い、queued を自動起動しない
  - **例外 = 一括対応（autoDispatch）**: ISSUES 一覧の「一括対応」ボタンは
    人の一括承認として queued を一括 dispatch する。同一 repo 競合・
    `maxConcurrentRuns` で起動できない課題は `autoDispatch: true` を
    立てて待機し、drain が repo 解放時に同じ dispatch 経路で自動起動する
    （フラグは起動時に消費。verify 差し戻し後は再び人判断を要する）
  - **例外 = kind別段階的自律度（autoDispatchWhitelist）**: config の
    `autoDispatchWhitelist: [{kind, maxPriority}]` はオプトイン式の
    事前承認。intake で kind 等値（`"*"`=任意）かつ
    `priority<=maxPriority`（既定1）かつ queued 自律対象の課題に
    `autoDispatch: true` を立て、drain が一括対応と同じ経路で起動する
    （フラグは起動時に消費、verify 差し戻し後は人判断）。gated
    （external・大規模）は対象外、whitelist 未命中は人ゲート必須、
    **未設定の既定は全課題人判断（現行維持）**
- **candidates 一覧は判断待ち inbox**: 未起票・gated・queued・failed の
  み表示し、各行の「対応」「却下」で人が一括判断する。「対応」は
  起票・承認・再実行を兼ねた即時 dispatch（未起票は rate-limit を
  人判断で bypass）、「却下」は `rejected`（未起票は起票して即
  rejected、failed の却下も `rejected` へ）
- 起動済み課題の run→verify→push は従来どおり自動で進む
  （verify NG の差し戻しは queued で再び対応待ち）

## detector プラグイン規約

- detector は `server/src/detectors/` 配下に置き、統一 candidate 形式
  `{source, repo, title, body, symptomHash, detectedAt, kind?}` を emit する
  （`kind` は課題の種別。learn の SKILL draft 名 source+repo+kind に使う。
  run id 等の揮発 id は入れない）
- dedupe は入口で共通実施（source+repo+symptomHash）。detector 個別には持たせない
- detector は検出のみ行い、起動・判定には関与しない

## 自動再起動（code drift）

- 実行中 exe の更新（cargo build 成果物）検知時、`autoRestartOnDrift`（既定 ON）
  ならタスク空きを待って自動再起動する。手動は `POST /api/control/restart`
- 再起動は必ず「実行系停止 → control 再起動」の順
- 判断フローの正本は `diagrams/restart-flow.mmd`（README §8 に埋め込み）
- **開発ループ**: `npm run dev`（`tools/dev.mjs` → `cargo run`）で debug ビルドを
  フォアグラウンド実行（kasugai_qgis の `run.py` 方式に倣う。EXE の別配置・
  常駐 dev インスタンスは持たない）。**通常版も開発版も :8586** — dev 起動時は
  対象ポートの占有プロセスを強制停止してから起動する。検証用の並走インスタンスは
  `PORT=<任意> npm run dev` で適宜ポートを指定する

## 配布とアセット更新（CDN）

- 正本は `diagrams/asset-update.mmd`（README §13 埋め込み）。仕様変更は図を先に直す
- **配布の基本は zip 一式**（exe + `config.example.json` + `panel/` + `docs/` +
  `prompts/`）。exe 単体では動かない（アセットは exe 隣接のディスクから配信）
- **CDN は「配信元」であってランタイム配信先ではない**。
  取得 → sha256 検証 → `assets/<ch>/` へ配置 → loopback でローカル配信。
  検証なしの外部 JS/コンテンツを panel へ流さない（panel JS は制御 API 全権）
- `assets/<ch>/` は `panel/`・`docs/` へのオーバーレイ（存在すれば優先配信）。
  repo の正本ファイルは CDN 適用で汚さない
- **行動系チャネル（prompts 等）は `assetAutoApply.<ch>` 既定 off**。
  proposals 起票 → 人の採否（採用時に root 配下へ書込む機構で適用）
- 版スキューは manifest の `minExeVersion` で防ぐ。exe 版不足時は適用せず通知のみ
- 配布物生成は `node tools/make-dist.mjs`（zip + `dist/cdn/` の manifest 一式）

## 知見の還流

- 生の知見は課題 key 単位で無条件に蓄積する（`knowledge/`）
- agent 自身への反映（prompts/config/skills）は proposals 機構で人が採否する
- proposals は採否の前に **vet（機械検査）** を通る: path 許可範囲・secret 混入・
  構文（JSON/frontmatter）を静的検査し、不合格は accept 不能
- リポジトリ正本への反映は新規課題として起票し、通常パイプで対応する

## シークレット

- API キー等は `secrets.json`（Git 管理外）のみに保存する
- `config.json`・ログ・プロンプト注入へキーを書き込まない
