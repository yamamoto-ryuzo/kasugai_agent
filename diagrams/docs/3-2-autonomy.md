---
title: 3-2 自律ループと実装方式
---

KASUGAI Agent の中核は「LOOP」——課題が生まれてから学習で自分自身を改良する
までの閉じた業務サイクルです。本ページでは LOOP の意味、SDK やプラグイン
ではなく Rust 自前実装を選んだ理由、自律性の位置づけを整理します。

## 自律ループ（LOOP）とは

LOOP はプログラムの for/while ではなく、
**課題の検出から学習までを一巡りする運用サイクル**です。
常駐サーバが外部世界を観測し続け、課題が現れるたびにこのサイクルを
回します。

<!-- diagram: autonomy-loop -->
```mermaid
---
title: 自律ループ全景（meta-loop 含む）
---
flowchart TB
    D[detectors<br/>課題候補 emit] --> N[normalize + dedupe]
    N --> A[assess<br/>verifyLevel 決定]
    A --> G{人ゲート}
    G -->|却下| X[rejected]
    G -->|承認| R[dispatch<br/>runner = 接続AI]
    R --> V[verify L0〜L4<br/>実機検証・証跡・push]
    V -->|NG 差し戻し<br/>（保留 → 処理再開・再起動で再送）| R
    V -->|合格| K[learn<br/>SKILL 蓄積 → .devin/skills/<br/>（draft・mermaid 図を必ず明示）]
    K --> P[proposals<br/>skills / prompts / config]
    P --> G
    G -->|採用| SELF[agent 自身を更新]
    SELF -.->|次のループを改良| N
```

| 段階 | 内容 |
|---|---|
| detect | detector（github-issues / ci-failure / local-health / manual-chat）が課題候補を emit |
| normalize/dedupe | 統一 candidate 形式に正規化し `source+repo+symptomHash` で重複排除 |
| assess | 課題の性質と検証深度（verifyLevel L0〜L4）を決定 |
| gate | dispatch（対応）判断・external 起票承認・proposal 採否・push 可否など人だけが越えられる判定地点 |
| dispatch | runner（接続AI 実行体）に実装を委譲。起動は ISSUES 一覧の「対応」ボタンによる人判断のみ（自動起動しない） |
| verify | 実機起動・証跡採取・push 可否。NG なら差し戻し prompt を保留し、処理再開・再起動時の自動再処理で retry へ（上限到達は要人対応） |
| learn | 結果・証跡を SKILL（draft・mermaid 図必須）として `.devin/skills/` へ蓄積し proposals を生成 |

LOOP が「要」なのは、これが**自律性そのもの**だからです。

- **終わらない** — 常駐サーバが観測を続け、課題ごとにサイクルを回す
- **閉じている** — verify が「実装したつもり」を実機で弾き、learn が結果を
  次のループの入力に還流する
- **自己書換えする** — learn → proposals → 採用 で agent 自身が更新され、
  ループがループを改良する（meta-loop）

観測系自身も監視対象です。detector ごとの最終実行時刻と最終 emit 時刻を
`detector-health.json` 台帳に記録し、`detectorSilentMinutes`（既定30）を
超えて emit のない detector は `source=local-health` の meta 課題として
このパイプラインへ emit します。「検出0件（正常）」と「detector 停止
（異常）」は lastRunAt と lastEmitAt の区別で分けます。

<!-- diagram: detector-health -->
```mermaid
---
title: detector 沈黙検知（meta 監視台帳）
---
flowchart LR
    POLL[spawn_detector_poller<br/>issues / ciFailure /<br/>specDrift / health<br/>pollSeconds.&lt;kind&gt; 周期] --> RUN[poll 実行<br/>返却 = 実行成功<br/>panic/タスク死は記録停止]
    RUN --> LED[detector_health.rs 台帳<br/>detector-health.json<br/>lastRunAt = 毎回更新<br/>lastEmitAt = candidates 1件以上<br/>の時のみ更新<br/>firstSeenAt / runs / emits]
    RUN --> ING[detectors::ingest<br/>検出候補を intake へ]
    LED --> CHK{沈黙判定<br/>detectorSilentMinutes<br/>既定30・&lt;=0 で無効}
    CHK -->|lastRunAt が停滞<br/>= poller 死亡・クラッシュ<br/>・起動失敗| STOP[detector 停止として emit<br/>kind=detector-stopped<br/>hash=detector-stopped:&lt;kind&gt;]
    CHK -->|lastRunAt は新しいが<br/>lastEmitAt が停滞<br/>= 実行継続・検出0件| SIL[detector 沈黙として emit<br/>kind=detector-silent<br/>hash=detector-silent:&lt;kind&gt;<br/>未 emit は firstSeenAt 起点]
    CHK -->|閾値内| OK[正常<br/>lastRunAt ≠ lastEmitAt で<br/>0件検出・正常 と<br/>detector 停止・異常 を区別]
    STOP --> META[source=local-health の<br/>meta candidate として ingest<br/>通常の intake 経路で起票]
    SIL --> META
    META --> DD[source+repo+symptomHash で<br/>detector ごと1課題に収束<br/>再 emit は lastSeen 更新のみ]
```

## 実装方式の比較（Rust 自前 / SDK / プラグイン）

| 観点 | Rust 自前（本システム） | SDK 利用 | プラグイン（MCP/skill 等） |
|---|---|---|---|
| 自律ループの制御 | 完全。state machine を自前で保有 | 部分的。課題ライフサイクルは別途自作 | 不可。ループはホスト側のもの |
| 常駐・スケジューリング | 自前デーモン + 自動再起動 | プロセス管理は自前 | ホストの起動中のみ |
| 状態の正 | control が state/issues/queue を保有し SSE push | SDK セッション状態と二重管理になりやすい | ホストの永続機構に依存 |
| 実機検証（L0〜L4） | EXE 起動・証跡・排他ロックまで実装可能 | SDK の責務外。結局自前で書く | ホストのツール呼出に限定 |
| AI 実行体の交換性 | chatAgents 設定で ACP/HTTP/OpenAI互換を選択。**プロバイダ非依存** | SDK 提供者のモデルに実質ロック | ホストが選んだ AI に従う |
| UI / 可視化 | panel + SSE でリアルタイム表示 | UI は丸ごと自作 | ホスト UI 制約内 |
| セキュリティ | loopback・secrets 一元・人ゲートを code に直接埋め込める | SDK 内部の送信先・ツール実行はブラックボックス | ホストの権限モデルに従う |
| 開発・保守コスト | 最大 | 中（エージェント部は SDK が吸収） | 最小 |

本システムが Rust 自前を選んだのは「SDK を使わないことで速い」からではなく、
**SDK に依存しない分だけ、自律ループと検証ポリシーを完全に自分の仕様にできる**
からです。AI を内蔵するのではなく、外付けの実行体としてオーケストレート
する形です。

## 自律性の位置づけ

| 自律性の要件 | 本システム | 完全自立との差 |
|---|---|---|
| 世界を継続観測する | detectors が常駐ポーリング | 観測対象は定義済みソースのみ |
| 自分で課題を立てる | candidate → issue key で自動起票 | 「何を課題とみなすか」の枠組みは人が定義 |
| 実行して結果を得る | runner が接続AIに委譲 | 「頭脳」は外付けセッション。本体は意志を持たない |
| 現実と照合する | verify L0〜L4 で実機検証・証跡 | 多くの自律実験が欠く部分を実装済み |
| 自分を改良する | learn → proposals → prompts/skills/config を更新 | 採否は人ゲート。意図的に自立を止めている |
| 失敗から回復する | retry・drift 自動再起動・snapshot 復元 | ドメイン内の回復のみ |

### 評価

- **入口としては正しい** — 観測→行動→実機検証→学習還流という骨格は
  自律システムの正しい形
- **完全自立ではない** — 人ゲートは除去不能な仕様。課題の枠組みは人が
  定義し、判断・実装の「頭脳」は外付けAIに委譲している
- **現実的な位置づけ** — 「人の監督下で自己改良する閉域自律システム」

完全自立に向かう次の壁は「proposals の採否を機械化する = 自己改良の
検証器を作る」ことです。その第一歩として **vet（機械検査）** を実装済み
（`server/src/proposals.rs`）—— path 許可範囲・secret 混入・構文を静的に
検査し、不合格は accept 不能とする採否の前提検査です。採否そのものは
人ゲートのままです。採否自体の機械化は verify を自分自身のコードに適用
する問題になり、L4（Issue 主旨との照合）よりはるかに難しい領域です。
