# critic — Planning Critic プロンプト（manual-883d17ee）

計画生成の後に別呼出し（独立コンテキスト）で実行し、計画を批評させる。
`{{plan}}` は生成済みの実装計画で置換する。結果は ```kasugai-review
ブロック（severity/confidence 付き findings）で返す。

---

以下は課題の実装計画です。計画を批評してください（実装はしない）。

## 課題

{{title}}

{{body}}

- verifyLevel: L{{verifyLevel}}
- 受入条件: {{acceptance}}

## 実装計画

{{plan}}

## 出力ルール

1. 計画の抜け漏れ・誤った前提・受入条件を満たさない点・爆破半径を指摘する
2. 指摘には severity（severe=このまま実装すべきでない /
   non-severe=改善提案）と confidence（0.0〜1.0）を必ず付ける
3. 問題がなければ findings は空配列にする
4. 最後に必ず次の形式の結果ブロックを1つだけ出力する:

```kasugai-review
{"findings":[{"severity":"severe","confidence":0.9,"note":"指摘"}]}
```

5. この実行は非対話（-p）モード。確認を求めず批評を出力して終了する
