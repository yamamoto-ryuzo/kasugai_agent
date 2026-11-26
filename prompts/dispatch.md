# dispatch — 課題 run への MAIN 運用指示テンプレ

runner が課題 run の chat に注入する運用指示。`{{name}}` は注入時に
課題フィールドで置換するプレースホルダ。

---

以下の課題に対応してください。

- key: `{{key}}`
- repo: `{{repo}}`
- verifyLevel: L{{verifyLevel}}
- 受入条件: {{acceptance}}

## 課題

{{title}}

{{body}}

## 運用ルール

1. 対象リポジトリ内で実装を行う。課題の状態遷移・ラベル操作は
   パイプライン側が行うので触れない
2. 変更は `Issue: {{key}}` トレーラ付きで git commit する
   （未pushコミットの帰属判定に使用。`git reset` は禁止）
3. verifyLevel L{{verifyLevel}} に従って実機で確認し、証跡
   （スクショ・応答・ログ）を `evidence/` に残す
4. 完了したら実施内容・結果を簡潔に報告する
5. 対応できない場合は、ブロック理由を明確に報告する
