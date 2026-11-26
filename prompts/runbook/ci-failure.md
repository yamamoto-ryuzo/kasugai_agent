# runbook: ci-failure（CI 失敗）

ci-failure detector が GitHub Actions 等の workflow 失敗を検出した課題
（kind = workflow 名。より具体的な `<source>-<kind>` runbook があればそちらが優先）。

## 対処手順

1. 課題本文の run URL から失敗ジョブとログを取得する
   （`gh run view --log-failed` または `gh api`）
2. 失敗の直接原因を特定する（テスト失敗 / lint / ビルド / 環境）
3. 一過性の要因（ネットワーク・rate limit・flake）と思える場合は
   rerun の報告に留め、恒久対策を課題本文に追記する
4. コード起因なら修正して commit・`Issue: {{key}}` トレーラを付ける
5. 修正後はローカルで同等のチェック（test / lint / build）を通し、
   証跡を evidence/ に残す
