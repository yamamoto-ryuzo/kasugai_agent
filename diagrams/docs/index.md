---
title: 1-1 概要
---

KASUGAI Agent は、**課題の検出 → 評価 → 対応 → 実機検証 → 学習** の自律ループを
実装する常駐システムです。統一モデルの正本は `README.md`、開発規約は
`AGENTS.md` にあります。

## 画面構成

操作の中心はブラウザのパネル UI（`http://127.0.0.1:8586/`）です。
制御は loopback bind のみで、外部には公開しません。

| UI 領域 | 内容 |
|---|---|
| 左ペイン | title・カレントリポジトリ選択・縦並びナビ（処理状況 / AIチャット / エディター / AGENTS / SKILL / マニュアル / 設定。初期表示は処理状況） |
| 中央ペイン | chat messages（chat/console タブ切替）・doc 本文・処理状況カンバン・設定ビュー・マニュアル（本サイト） |
| 右ペイン | 管理セクション（loop state / candidates / issues / proposals）/ エディター（フォルダツリー） / doc browser（doc + 参照ファイル）。エディター/AGENTS/SKILL 選択時は doc browser、それ以外は管理セクション |

左ペインのナビから各ビューを開き、右ペインまたは中央ペインに表示します。
このマニュアルは「マニュアル」ナビで開いた `docs/` サイトそのものです。

## 自律ループ（課題ライフサイクル）

detector が課題候補（candidate）を emit し、入口で正規化・重複判定
（source+repo+symptomHash）して課題レコードを作ります。`external` ラベル付きは
人ゲート待ち、それ以外はキュー → runner 起動 → 実装 → verify（L0〜L4 の
実機検証）→ push まで自動で進みます。詳細は「3-1 統一モデル図」を参照。

- **verify**: L2 以上は対象 EXE を実機で起動して応答確認。証跡は `evidence/` へ
- **learn**: 実行結果・証跡は SKILL（draft・mermaid 図必須）として
  `.agents/skills/` へ蓄積し、agent 自身への反映は proposals 機構で人が採否する

## 関連ドキュメント

- `README.md` — 統一モデル正本（設計の詳細はこちら）
- `AGENTS.md` — 開発規約（mermaid 正本・1ファイル1責務・SSE 即時反映 等）
