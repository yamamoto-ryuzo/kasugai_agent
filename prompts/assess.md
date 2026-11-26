# assess — 課題評価プロンプト（正本: server/src/pipeline.rs assess）

課題候補を評価し、検証の深度（verifyLevel L0〜L4）・受入条件・自律可否を
決定する。全課題一律に L3/L4 を要求しない。

## 判定ルール（現行: ルールベース）

- `external` ラベル → `autonomous=false`・`gated`（自律対象外。候補一覧表示のみ）
- 爆破半径大（`secrets.json` / `force-push` / `rm -rf` を含む）
  → `autonomous=false`・`gated`
- verifyLevel:
  - L3: `playwright` / `画面` / `ui` を含む、または label `ui`
  - L2: `exe` / `実機` / `qgis` を含む、または label `binary`
  - L0: `doc` / `mermaid` / `.mmd` を含む、または label `documentation`
  - 上記以外: L1
- 受入条件: 「{title} が期待どおり動作することを実機で確認する」

<!-- TODO(LLM 判定口): ルール判定の上に LLM 評価を差し込む場合はここに
     判定手順を書き、課題 {title}/{body} を渡して
     {verifyLevel, acceptance, autonomous, reason} を返させる。
     差し替えは pipeline::assess 側で knowledge::load_prompt を読む。 -->
