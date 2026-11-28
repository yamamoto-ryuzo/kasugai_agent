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
| `acp` | ACP 対応 CLI を spawn（stdio JSON-RPC）。`devin acp`・`copilot --acp` 等 | `exe` / `args` / `auth` |
| `agent` | `POST {endpoint}` 単発ターン | `endpoint` / `provider` |
| `openai` | OpenAI 互換 `chat/completions` | `endpoint` / `model` |

`acp` 型の `auth` 指定:

| 値 | 動作 |
|---|---|
| `"none"` | `authenticate` を送らない（Copilot CLI 等、環境変数トークン運用） |
| `"devin"` | `%APPDATA%/devin/credentials.toml` の `windsurf_api_key` で `methodId: devin-browser` |
| その他の文字列 | そのまま ACP `authenticate` の `methodId` として送信 |
| `{"methodId": "…", "tokenEnv"/"tokenSecret": "…"}` | 環境変数または secrets.json からキーを解決して送信 |

`env` フィールド（オブジェクト）は spawn する子プロセスへ環境変数を注入します。
値は `"リテラル"` / `"env:VAR"`（親環境から転写）/ `"secret:key"`（secrets.json
`tokens[key]` から解決）が使えます。トークン等の秘匿値は `secret:` で引いてください
（`config.json` に平文で書かない）。

トークン・API キーは `secrets.json` の `tokens` へ保存し、
`config.json` ・ログ・プロンプトには書きません。

## リポジトリ

監視対象リポジトリ（`config.repos`）の登録・設定更新・解除を行います。
`path` 入力は参照ピッカー（`GET /api/fs/dirs`）で選択できます。
ワークスペース直下の `.git` 付きディレクトリは「ワークスペース管理下」として
自動検出され、そのまま登録ボタンで追加できます。
GitHub を利用していないユーザーリポジトリも「ユーザー」として検出します:

- ワークスペース直下の `.git` 無しディレクトリ（Git 未使用のプロジェクト dir）
- ユーザーディレクトリ直下の `.git` 付きディレクトリ
- `config.repoScanDirs` に列挙した各ディレクトリ直下のディレクトリ（全件）

いずれも登録ボタンで `POST /api/repos` へ流せます
（slug 省略時は git remote から推定、無ければディレクトリ名）。

リポジトリ一覧はサブタブでグループ化されます。slug に owner を持つ
リポジトリは GitHub owner ごとのタブに、owner を持たないものは
「ユーザー」（GitHub 非依存の検出 repo）または「(owner なし)」タブに
振り分けられます。選択中のサブタブは localStorage に保存されます。
各サブタブにホバーするとグループの意味をポップアップで確認できます。

## システム

「システム再起動 / 強制再起動」ボタン（`POST /api/control/restart`）。
実行中 run が残る間は空くまで延期し、強制なら実行中の chat を中断して
即時再起動します。

実行 exe の更新検知時は `autoRestartOnDrift`（既定 ON）により
タスク空きを待って自動再起動します（判断フローは「3-1 統一モデル図」の
自動再起動図を参照）。

「時刻表示」カードでタイムゾーンを IANA 名（例: `Asia/Tokyo`）で指定できます。
空欄はブラウザ既定（OS のゾーン）です。選択は localStorage に保存され、
chat・コンソール・各一覧の時刻表示に適用されます
（保存される時刻データ自体は UTC のままです）。

dead man's switch heartbeat（agent 自身の死の外部通知）は
`secrets.json` の `heartbeatUrl` に外部 heartbeat サービスの
ping URL（healthchecks.io 等の単純 GET）を設定すると有効になり、
`config.heartbeatMinutes`（既定5・0 以下で無効）間隔で ping を送ります。
送信先未設定時は無効のままです。最終 ping の成否は右ペインの
loop state カードに表示されます。

## アセット更新（CDN）

CDN 配布アセットの状態カードです（「3-3 配布とアセット更新」参照）。
`cdnBaseUrl` 設定時にチャネル（panel/docs/prompts）ごとの
適用済版・最新版・保留状態を表示し、「更新確認」で manifest 取得、
「適用」「ロールバック」で個別操作できます。
panel/docs は自動適用、prompts は proposals の採否ゲート経由です。
