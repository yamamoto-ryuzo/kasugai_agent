# runbook: spec-drift（図と実装・写しの乖離）

spec-drift detector が検出した、正本（diagrams/*.mmd・diagrams/docs/）と
生成物・実装の乖離課題。

## 対処手順

1. 乖離内容を確認する（課題本文の対象ファイル・hash）
2. **正本を先に直す**: `diagrams/*.mmd`（図の仕様）または
   `diagrams/docs/*.md`（マニュアル本文）を先に修正する。
   README.md・docs/*.html の埋め込み部分は生成物なので直接編集しない
3. `node tools/sync-diagrams.mjs` / `node tools/sync-docs.mjs` を実行して
   写しを再生成する
4. `--check`（それぞれ）で乖離が消えたことを確認する
5. 実装側が図に追従できていない場合は実装を修正する
