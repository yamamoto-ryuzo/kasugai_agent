# runbook: gh-auth（gh CLI 未認証・未インストール）

local-health detector が `gh auth status` の失敗を検出した課題。

## 対処手順

1. `gh auth status` を実行して状態を確認する
2. 未インストールならインストールを案内して課題を closed 相当で報告する
   （自動インストールはしない。winget install GitHub.cli 等は人判断）
3. 未認証なら `gh auth login` が必要 — これは対話操作のため自動対応不可。
   この課題は「ブロック理由を明確に報告」して終了し、人の操作を待つ
4. 認証済みなのに失敗する場合は token 切れ・hosts 設定を確認する
