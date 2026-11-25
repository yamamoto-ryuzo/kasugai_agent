# KASUGAI Agent — 開発規約

「課題の検出 → 評価 → 対応 → 実機検証 → 学習」の自律ループを実装する常駐システム。
全体設計は `README.md`（統一モデルが正本）を参照。

## 基本原則（最重要）：mermaid が唯一の指示書

- **機能追加・変更は必ず mermaid 図の更新から始める**。実装はその図に従う
- 新規機能・新規フローの作業は README/docs の mermaid 図を引いてから始める
- 図と実装が乖離したら**図を先に直す**（図が仕様、実装はその反映）
- mermaid 図は `README.md`（GitHub 表示用）と `docs/*.html`（ブラウザ表示用）の
  2箇所に置き、**内容は常に同期させる**（片方だけの変更は禁止）
- `docs/*.html` の `<pre class="mermaid">` 内では `<br/>` → `&lt;br/&gt;`、
  `-->` → `--&gt;` にエスケープする（HTML パーサに食わせない）
- ASCII アート図は新規に書かない（既存資産からの引用を除く）

## コード構成

- **1ファイル1責務**。`src/` の責務分界は README §10 に従う
- **状態遷移は `pipeline.mjs` だけが行う**。他モジュールは遷移要求を投げるだけ
- Node.js 標準ライブラリのみ（依存を増やさない）
- 制御は loopback bind。外部公開しない

## 状態変更は必ず UI へ反映する（リロード強制）

**状態を変えたら、パネル表示へ即時反映させること。** 状態と画面の乖離は障害とみなす。

- 状態変更を追加・変更した場合、SSE push で即時反映されることを確認する
- **表示用フィールドを追加したら、それを返す API 応答にも必ず含めること**

## 処理ライフサイクル（verify の混在ルール）

- **再検証（verify）と push は一体の仕事**: push は verify のみが行い、検証を通って
  いないコミットはリモートへ公開しない
- 未pushコミットは `Issue: #N` トレーラで課題に帰属させる。closed /
  `devin:rejected` / 帰属不明のコミットは `git revert` で除去してから push
- `git reset` は禁止（他課題コミットを破壊する）

## 検証の深度（verifyLevel）

- assess が課題ごとに L0〜L4 の深度を決定する。**全課題一律に L3/L4 を要求しない**
- L2 以上は成果物の実機実行、L3 は画面駆動、L4 は Issue 主旨との照合
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
- 判断フローの正本は README §8 の mermaid 図

## 知見の還流

- 生の知見は課題 key 単位で無条件に蓄積する（`knowledge/`）
- agent 自身への反映（prompts/config/skills）は proposals 機構で人が採否する
- リポジトリ正本への反映は新規課題として起票し、通常パイプで対応する

## シークレット

- API キー等は `secrets.json`（Git 管理外）のみに保存する
- `config.json`・ログ・プロンプト注入へキーを書き込まない
