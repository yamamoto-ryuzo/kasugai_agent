# Issue #20 検証証跡 — docs 起動ボタンの冷起動対応

検証日: 2026-11-26 / 対象: `server/target/release/kasugai-agent.exe`（:8586 稼働）

## 1. 自動起動（HKCU Run キー）

- `POST /api/control/autostart {"action":"uninstall"}` → `installed:false`、レジストリ値消失を確認
- `POST /api/control/autostart {"action":"install"}` → 再登録、登録コマンドは release exe

```
GET /api/control/autostart
{"ok":true,"autostart":{"task":"KasugaiAgentDispatcher","installed":true,
 "cmd":"\"C:\\devin\\kasugai_agent\\server\\target\\release\\kasugai-agent.exe\"",
 "exe":"C:\\devin\\kasugai_agent\\server\\target\\release\\kasugai-agent.exe"}}

reg query HKCU\Software\Microsoft\Windows\CurrentVersion\Run /v KasugaiAgentDispatcher
    KasugaiAgentDispatcher    REG_SZ    "C:\devin\kasugai_agent\server\target\release\kasugai-agent.exe"
```

※ Task Scheduler は非管理者で `schtasks /Create` がアクセス拒否のため、
HKCU Run キー方式に変更（管理者権限不要）。

## 2. プロトコルハンドラ `kasugai-agent://`

exe 起動時に HKCU\Software\Classes\kasugai-agent へ自己登録されることを確認:

```
reg query HKCU\Software\Classes\kasugai-agent\shell\open\command
    (既定)    REG_SZ    "C:\devin\kasugai_agent\server\target\release\kasugai-agent.exe" --uri "%1"
```

- 冷起動: `kasugai-agent.exe --uri "kasugai-agent:launch?target=prod&port=8586"` → control が detached spawn され :8586 で応答
- 稼働中: 同コマンド → 既稼働を検知して即終了（exit 0）
- nav.js フォールバック: `/api/control/launch` 失敗時に `kasugai-agent:launch?target=X&port=Y` へ遷移

## 3. 多重起動防止

2回目の exe 起動（:8586 占有中）:

```
[kasugai_agent] 既に :8586 で稼働中。パネルを開きます
→ exit 0 / ブラウザでパネルが開く / 既存インスタンスは継続（GET /api/state → 200）
```

## 4. windows subsystem

`#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]` 適用済み。
release exe はコンソール窓を持たず起動（パイプ出力のみ）。

## 5. DEV 環境廃止

- `tools/dev-server.mjs`・`server/target/dev/` 削除、nav.json の dev 起動ボタン撤去
- `npm run dev` = `cargo run --manifest-path server/Cargo.toml`（kasugai_qgis run.py 方式準拠）
- launch API は prod 専用（env-launch.mmd の dev 分岐も削除済み）

## ゲート結果

- `cargo build --release`: 成功（警告のみ・既存分）
- `npm test`: 33 pass / 0 fail
- `npm run diagrams:check` / `npm run docs:check`: all in sync
