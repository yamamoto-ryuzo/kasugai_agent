---
title: 2-5 設定
---

左ペイン「設定」ナビで中央ペインに設定ビューを開きます。
タブは **接続AI / リポジトリ / システム** の3つです。

## 接続AI

`config.json` の `chatAgents`（実行体レジストリ）と `chatAgent`（active id）を
管理します。登録・更新・解除・active 切替は SSE push で即時反映され、
新規 chat の実行体は作成時に固定されます。

| type | 実行体 | 必須フィールド |
|---|---|---|
| `acp` | ACP 対応 CLI を spawn（stdio JSON-RPC） | `exe` / `args` / `auth` |
| `agent` | `POST {endpoint}` 単発ターン | `endpoint` / `provider` |
| `openai` | OpenAI 互換 `chat/completions` | `endpoint` / `model` |

トークン・API キーは `secrets.json` の `tokens` へ保存し、
`config.json` ・ログ・プロンプトには書きません。

## リポジトリ

監視対象リポジトリ（`config.repos`）の登録・設定更新・解除を行います。
`path` 入力は参照ピッカー（`GET /api/fs/dirs`）で選択できます。
IDE ワークスペース直下の `.git` 付きディレクトリは「IDE管理下」として
自動検出され、そのまま登録ボタンで追加できます。

## システム

「システム再起動 / 強制再起動」ボタン（`POST /api/control/restart`）。
実行中 run が残る間は空くまで延期し、強制なら実行中の chat を中断して
即時再起動します。

また `src/*.mjs` の変更検知時は `autoRestartOnDrift`（既定 ON）により
タスク空きを待って自動再起動します（判断フローは「3-1 統一モデル図」の
自動再起動図を参照）。
