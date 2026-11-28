---
title: 2-6 開発環境
---

左ペイン「開発環境」ナビで中央ペインに開発環境ビューを開きます。
`config.json` の `devEnvironments[]` に登録したローカル dev 環境
（Rust+Cargo 等）の起動・停止・状態確認を行う画面です。

## カードの操作

| ボタン | 動作 |
|---|---|
| 起動 | `POST /api/dev-envs/launch`。対象 port が未応答なら占有プロセスを `taskkill /F /T` で解放してから `command` を detached spawn |
| 停止 | `POST /api/dev-envs/stop`。port を占有するプロセスを `taskkill /F /T` |
| 開く | `url`（未指定時は `http://127.0.0.1:<port>/`）を新規タブで開く |

状態 pill は port への TCP 応答で判定します（起動中 / 停止）。
起動・停止後は SSE `devenvs` で一覧が即時再描画されます。

## devEnvironments の項目

```json
{
  "id": "myenv",
  "label": "表示名",
  "cwd": "C:/path/to/repo",
  "command": "cargo run",
  "port": 8000,
  "url": "http://127.0.0.1:8000/",
  "env": { "FOO": "bar" }
}
```

- `command` は `cmd /c`（POSIX は `sh -c`）経由で実行されるため、
  `cargo run`・`npm run dev`・`python run.py` 等をそのまま書けます
- 子プロセスの stdout/stderr は `logs/devenv-<id>.log` へ追記され、
  カードに末尾を表示します
- 親（kasugai-agent）の `PORT` 環境変数は子へ継がせません。
  個別に渡したい変数は `env` オブジェクトを使います
- `port` を省略・`0` にすると状態判定・停止は行いません
