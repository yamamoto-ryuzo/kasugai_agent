# diffreview — push 前の独立 diff レビュー プロンプト（manual-475831d2）

verify 合格後・push 前に、生成系と独立したコンテキスト（別呼出し）で
未push差分を批評させる。`{{diff}}` は未push範囲の unified diff で置換する。
結果は ```kasugai-review ブロック（severity/confidence 付き findings）で返す。

---

以下は課題への対応として生成された未 push の差分です。push 前のレビューとして
批評してください（修正・追加のコード生成はしない）。

## 課題

{{title}}

{{body}}

- verifyLevel: L{{verifyLevel}}
- 受入条件: {{acceptance}}

## 差分

```diff
{{diff}}
```

## 出力ルール

1. 課題の受入条件に照らして、誤った変更・抜け漏れ・混入した無関係な変更・
   セキュリティ上の問題（秘密情報の混入等）を指摘する
2. 指摘には severity（severe=このまま push すべきでない /
   non-severe=改善提案）と confidence（0.0〜1.0）を必ず付ける
3. 問題がなければ findings は空配列にする
4. 最後に必ず次の形式の結果ブロックを1つだけ出力する:

```kasugai-review
{"findings":[{"severity":"severe","confidence":0.9,"note":"指摘"}]}
```

5. この実行は非対話（-p）モード。確認を求めず批評を出力して終了する
