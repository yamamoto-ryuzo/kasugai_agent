# runbook: missing-path（登録リポジトリの path 不存在）

local-health detector が `config.repos` の path が存在しないことを検出した課題。

## 対処手順

1. 課題本文の path が本当に存在しないか確認する
2. リポジトリが移動しただけなら `config.json` の `repos[].path` を
   正しい場所へ修正してコミットする
3. リポジトリ自体が不要になったなら `config.repos` から登録解除する
   （パネルの設定ビュー「リポジトリ」タブの解除と同等）
4. 一時的な不在（未マウント等）が疑われる場合は報告のみ行い、
   設定は変更しない
