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
  - `nav.json` — サイト名・章立て（`groups[].items[].page` = md 名）
  - `template.html` / `style.css` — 共通書式
- `docs/*.html`・`docs/style.css` は `node tools/sync-docs.mjs` が生成する写し。
  **直接編集禁止**。検証は `--check`（乖離時 exit 1）
- md 内の `<!-- diagram: <name> -->` + ```mermaid ブロックは
  sync-diagrams.mjs が同期する（図の正本は従来どおり `diagrams/*.mmd`）
- パネル左ペインの「マニュアル」ナビで中央ペインに `/docs/` を表示する
  （control が `/docs/*` を静的配信）

## コード構成

- **1ファイル1責務**。`src/` の責務分界は README §10 に従う
- **状態遷移は `pipeline.mjs` だけが行う**。他モジュールは遷移要求を投げるだけ
- Node.js 標準ライブラリのみ（依存を増やさない）
- 制御は loopback bind。外部公開しない

## 状態変更は必ず UI へ反映する（リロード強制）

**状態を変えたら、パネル表示へ即時反映させること。** 状態と画面の乖離は障害とみなす。

- 状態変更を追加・変更した場合、SSE push で即時反映されることを確認する
- **表示用フィールドを追加したら、それを返す API 応答にも必ず含めること**

## パネルのカレントリポジトリ

- 左ペイン title 直下のセレクトは **カレントリポジトリの指定のみ**を行う
- **`すべてのリポジトリ` 相当の選択肢は置かない**。常に1リポジトリを指し、
  未選択（全体表示）状態を作らない
- 選択はパネル全体のスコープ（新規 chat の cwd・課題化の repo・
  各一覧の絞込）になり、localStorage に永続化する
- **リポジトリ名の表示はこのセレクトのみ**。選択リポジトリで絞られた一覧・
  画面（chat sessions・処理状況・candidates・ヘッダ status 等）では
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

## detector プラグイン規約

- detector は `src/detectors/` 配下に置き、統一 candidate 形式
  `{source, repo, title, body, symptomHash, detectedAt}` を emit する
- dedupe は入口で共通実施（source+repo+symptomHash）。detector 個別には持たせない
- detector は検出のみ行い、起動・判定には関与しない

## 自動再起動（code drift）

- `src/*.mjs` の変更検知時、`autoRestartOnDrift`（既定 ON）ならタスク空きを待って
  自動再起動する。手動は `POST /api/control/restart`
- 再起動は必ず「実行系停止 → control 再起動」の順
- 判断フローの正本は `diagrams/restart-flow.mmd`（README §8 に埋め込み）

## 知見の還流

- 生の知見は課題 key 単位で無条件に蓄積する（`knowledge/`）
- agent 自身への反映（prompts/config/skills）は proposals 機構で人が採否する
- リポジトリ正本への反映は新規課題として起票し、通常パイプで対応する

## シークレット

- API キー等は `secrets.json`（Git 管理外）のみに保存する
- `config.json`・ログ・プロンプト注入へキーを書き込まない
