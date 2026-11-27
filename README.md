# KASUGAI Agent

課題自律ループ常駐エージェント。
「課題（現状と理想の差分）を検出 → 評価 → 対応 → 実機検証 → 学習」する閉ループを
1本のパイプラインで回す。Issue 対応・エラー自律対応・新機能追加・自己改良は
すべて同一モデルの上に乗る。

この README は **統一モデルの正本** である。実装と乖離したら図・文書を先に直す。
開発規約は `AGENTS.md`。
なお mermaid 図の正本は `diagrams/*.mmd` であり、本ファイル内の図は
`node tools/sync-diagrams.mjs` が生成する写し（直接編集禁止）。

- バージョン: v0.2.0（旧版 agent → kasugai_agent v0.1 の知見を踏まえ全面再構築）
- 実行環境: Windows / Rust（axum + tokio。`server/` が実装正本）+ gh CLI + ACP 対応 CLI（devin 等）。`tools/*.mjs` はビルド時のみ Node.js
- 制御ポート: `8586` / 自動起動名: `KasugaiAgentDispatcher`（HKCU Run キー）

---

## 1. 目的と非目的

**目的**

- 監視対象リポジトリ群の課題を継続的に検出し、人の指示なしに評価〜実装〜実機検証
  まで進める
- 実機検証（証跡採取）を義務化し、「動いたつもり」を排除する
- 処理のたびに知見を蓄積し、agent 自身・各リポジトリ正本へ還流してループ自体を
  改良する

**非目的**

- チャットだけの会話相手になること
- 検証を通っていない成果物を push すること（絶対しない）

## 2. 用語

| 用語 | 定義 |
|---|---|
| **課題 (issue)** | 現状と理想の差分。全ワークの単位。安定した issue key を持つ |
| **candidate** | detector が emit した正規化済み課題候補（未評価） |
| **run** | 1課題に対する1回の処理実行（session + verify の一連） |
| **runner** | 課題を実際に処理する実行体の抽象。既定は `devin -p`（非対話セッション） |
| **verifyLevel** | L0〜L4。課題ごとに assess が決める検証深度 |
| **proposal** | 知見から生成した変更提案（skills/prompts/config/正本）。人の採否ゲートを通る |
| **人ゲート** | 自律で越えてはいけない判定地点（external 起票・proposal 採否・push 可否） |

## 3. 課題ライフサイクル

<!-- diagram: issue-lifecycle -->
```mermaid
---
title: 課題ライフサイクル
---
stateDiagram-v2
    [*] --> detected : detector emit
    detected --> deduped : 正規化・重複判定
    deduped --> dropped : 既知/重複
    deduped --> assessed : 新規
    assessed --> gated : 人ゲート待ち (external等)
    assessed --> queued : 自律対象（対応待ち）
    gated --> queued : 人が承認
    gated --> rejected : 人が却下
    queued --> running : 一覧の対応ボタンで dispatch<br/>（自動起動しない）
    queued --> rejected : 人が却下
    running --> verifying : 実装完了
    verifying --> done : 検証合格
    verifying --> retry : NG (差し戻し)
    retry --> queued : 差し戻し（対応ボタンで<br/>再 dispatch）
    retry --> failed : リトライ上限（要人対応）
    running --> failed : 異常終了
    failed --> queued : 再実行
    failed --> rejected : 人が却下
    note right of queued
        candidates 一覧は判断待ち（未起票・gated・
        queued・failed）のみ表示し、各行の
        対応/却下ボタンで人が一括判断する。
        対応は起票/承認/再実行を兼ねた即時 dispatch
        （未起票は rate-limit を人判断で bypass）
    end note
    done --> [*]
    rejected --> [*]
    dropped --> [*]
    [*] --> done : GitHub closed Issue 取込<br/>（--import-github・COMPLETED）
    [*] --> rejected : GitHub closed Issue 取込<br/>（--import-github・NOT_PLANNED）
```

- `done` でも課題知見は残り、後続課題・proposals の入力になる
- `failed` は人の判断で `queued` に戻せる（パネル操作）
- `retry` の差し戻し回数は課題単位で記録し、上限（既定3）で `failed`
  （要人対応）。再実行は自動ループせず「処理再開」・個別「差し戻し」・
  再起動時の自動再処理で行う
- `queued` の起動は人判断のみ: 一覧の「対応」ボタン
  （`POST /api/issues/:key/dispatch`）だけが dispatch し、drain は
  queued を自動起動しない。対応しない課題は「却下」で `rejected` へ

## 4. 全体構成

<!-- diagram: architecture -->
```mermaid
---
title: 全体構成
---
flowchart LR
    subgraph Detect[検出層]
        D1[github-issues]
        D2[ci-failure]
        D3[local-health]
        D4[spec-drift]
    end

    subgraph Core[コア server/src/]
        N[normalize + dedupe<br/>detectors/mod.rs + store.rs]
        A[assess<br/>pipeline.rs]
        G[gate<br/>人ゲート]
        R[runner<br/>runner.rs + chat/]
        V[verify<br/>verify.rs]
        K[knowledge<br/>knowledge.rs]
    end

    subgraph IO[外部]
        GH[GitHub gh CLI]
        DV[接続AI<br/>ACP CLI / /agent HTTP /<br/>OpenAI互換・chatAgent で選択]
        RP[監視対象リポジトリ]
    end

    C[control.rs<br/>HTTP :8586 + SSE] --> P[panel/<br/>ブラウザUI]
    D1 & D2 & D3 & D4 --> N --> A --> G --> R --> V --> K
    R --> DV
    A & R & V --> GH
    R & V --> RP
    K -->|proposals| G
```

- **detectors は検出のみ**。起動・判定・重複排除には関与しない
- **control.rs はハブ**。状態の唯一の正（state store）・SSE push・パネル配信・
  スケジューラ（ポーリングタイマ）を持つ。重い処理はモジュールに分離
- **pipeline.rs がライフサイクルの state machine**。状態遷移はここだけが行う

## 5. パイプライン詳細

### 5.1 detect → normalize → dedupe

<!-- diagram: detect-pipeline -->
```mermaid
---
title: detect → normalize → dedupe
---
flowchart TD
    DET[detector / manual API /<br/>派生（done chat の<br/>変更ターン source=derived）<br/>github-issues は open issue 全件<br/>（ラベル選別なし）<br/>closed は --import-github で<br/>終端取込] -->|candidate<br/>source/repo/title/body/<br/>symptomHash/detectedAt<br/>（任意: kind/labels/<br/>count/durationSeconds）| NORM[正規化]
    NORM --> HASH[issue key =<br/>source+repo+symptomHash]
    HASH --> EXIST{既出?}
    EXIST -->|yes| BUMP[既存課題の<br/>lastSeen 更新のみ]
    EXIST -->|no| SIL{silence?<br/>silences.json の<br/>matcher（source/repo/<br/>kind/severity）+<br/>startsAt〜endsAt 窓内<br/>（設定 UI / API で作成）}
    SIL -->|窓内| SH[candidate に留め置く<br/>deferred=silenced<br/>課題は作らない]
    SH -.->|期限切れで自動解除| SIL
    SIL -->|窓外・非合致| CD{クールダウン?<br/>同一バケット<br/>intakeCooldownBy<br/>（既定 source+repo+kind）の<br/>reject/verify NG から<br/>intakeCooldownMinutes 未満}
    CD -->|抑止中| CDH[candidate に留め置く<br/>deferred=cooldown<br/>課題は作らない]
    CDH -.->|窓経過で再試行| INH
    CD -->|経過| INH{inhibition?<br/>config.inhibitRules の<br/>原因課題が open<br/>（cause/effect matcher +<br/>match 属性等値。<br/>inhibitedBy 明示も可）}
    INH -->|抑制中| IHOLD[candidate に留め置く<br/>deferred=inhibited<br/>inhibitedBy=原因課題<br/>原因課題の inhibits に追記]
    IHOLD -.->|原因課題が終端で<br/>次回 emit が解除| INH
    INH -->|非抑制| OBS{observing 昇格?<br/>intakeObserveMinutes 継続<br/>or intakeObserveMinHits 回<br/>再検出<br/>（manual/derived は即起票）}
    OBS -->|未昇格<br/>単発・窓内消滅| OHOLD[candidate に留め置く<br/>deferred=observing<br/>課題は作らない]
    OBS -->|昇格| RATE{起票レート超過?<br/>intakeMaxPerHour<br/>source+repo 単位<br/>0=無制限}
    OHOLD -.->|再検出・duration 蓄積で<br/>次回 emit/reconcile が昇格判定| OBS
    RATE -->|超過| DEFER[candidate に留め置く<br/>deferred=rate-limit<br/>課題は作らない]
    DEFER -.->|次回 reconcile で再試行| BF
    RATE -->|以内| STORM{ストーム?<br/>同一 source+repo で<br/>stormWindowMinutes 内に<br/>stormBatchMin 件+の起票<br/>or open の storm 事案あり}
    STORM -->|集中| SAGG[一括事案へ集約<br/>（source=storm・gated・<br/>自律対象外）個別課題は作らず<br/>deferred=storm:<事案key><br/>人レビューで採否]
    SAGG -.->|事案が終端で<br/>個別起票に復帰| STORM
    STORM -->|非集中| NEW[課題レコード作成<br/>＋assess<br/>verifyLevel・受入条件・<br/>priority・自律可否<br/>起票＝本システムの課題<br/>（GitHub 登録は別機構）]
    BF[drain 定期 reconcile:<br/>candidate に課題が無ければ<br/>intake で補起票] -.->|自動起票の保証| NEW
    NEW --> GATE{external 等<br/>自律対象外?}
    GATE -->|no| Q[queued<br/>一覧の対応ボタンで<br/>人が dispatch]
    GATE -->|yes| G[gated<br/>候補一覧で人が<br/>dispatch 判断]
    DEFER --> INBOX
    SH --> INBOX
    CDH --> INBOX
    IHOLD --> INBOX
    OHOLD --> INBOX
    SAGG --> INBOX
    Q --> INBOX
    G --> INBOX
    INBOX[候補一覧 = 判断待ち inbox<br/>未起票・gated・queued・failed のみ表示<br/>（実行中・終端の課題行は出さない）<br/>全行 対応/却下ボタンで人が一括判断:<br/>対応=起票/承認/再実行を兼ねた即時 dispatch<br/>（未起票は rate-limit を人判断で bypass）<br/>却下=rejected（未起票は起票して即 rejected）]
```

- detector は `server/src/detectors/*.rs` に置き、統一 candidate 形式を stdout/ファイルで emit
- dedupe は入口の1箇所のみ（detector 個別に持たせない）
- **silence（起票ミュート）**: `silences.json` の matcher
  （`source`/`repo`/`slug`/`kind`/`severity` の等値・未指定は任意）に合う
  candidate を `startsAt`〜`endsAt`（`endsAt` 必須）の窓内で課題化せず
  `deferred:"silenced"` で留め置く。計画作業・定期メンテ中の既知症状を
  抑える仕組みで、設定画面のシステムタブまたは
  `GET/POST /api/silences`・`POST /api/silences/remove` で作成・解除する。
  期限切れは照合対象から外れ、台帳からも自動的に除去される
- **起票クールダウン**: バケット（`intakeCooldownBy` 既定
  `["source","repo","kind"]` の等値グループ。dedupe hash とは別次元の
  「何を同一とみなすか」パラメータ）内の課題が rejected/failed へ遷移して
  から `intakeCooldownMinutes`（既定60分、0=無効）未満の再 emit は課題を
  作らず `deferred:"cooldown"` で留め置く（reject/verify NG 直後の
  once_within 抑圧）
- **verify NG 再試行の指数バックオフ**: 差し戻しで queued に戻る課題には
  `retryAfter`（`verifyBackoffMinutes` 既定5分 × 2^(verifyBacks-1)、上限
  `verifyBackoffMaxMinutes` 既定60分）を記録し、経過前の再 dispatch は
  `can_dispatch` が止める。`verifyBackLimit` 超過は failed（要人対応）
- **inhibition（課題間抑制）**: `config.inhibitRules`（Alertmanager の
  inhibit_rules 相当）。`cause` matcher に合う原因課題が open（終端以外）
  の間、`effect` matcher に合う下流候補は別課題として起票せず
  `deferred:"inhibited"`・`inhibitedBy=<原因課題key>` で留め置く。
  `match`（既定 `["repo"]`）は原因課題と候補で値が一致すべき属性。
  抑制中は原因課題の `inhibits` に候補 key を追記し、原因課題が終端へ
  落ちると次回 emit で自動解除される。候補が `inhibitedBy:<issueKey>` を
  明示する emit でも抑制できる
- **課題ストームの一括事案化**: 同一 `source`+`repo` で
  `stormWindowMinutes`（既定10分）内に `stormBatchMin`（既定5・0=無効）
  件以上の起票が集中したら、以後は個別課題を作らず1件の一括事案
  （`source:"storm"`・`status:"gated"`・自律対象外）へ候補を集約する
  （上流障害・detector 暴走のサインを1件で人レビューへ）。
  集約された候補は事案の `stormMembers` に記録され、
  `deferred:"storm:<事案key>"` で一覧に表示される。事案が終端へ
  落ちると個別起票に復帰する
- **同時実行数の上限**: `maxConcurrentRuns`（既定4・0=無制限）を超える
  running|verifying の課題がある間は `can_dispatch` が dispatch を止める
  （同一 repo 排他に加えた全体クォーラム）
- **observing（保留観測）**: 一過性 flap の起票を排除するため、detector
  由来の candidate は「`intakeObserveMinutes`（既定10分）継続 or
  `intakeObserveMinHits`（既定2）回再検出」を満たすまで課題化せず
  `deferred:"observing"` で留め置く。単発検出で窓内に消えた症状は課題を
  作らない。detector は emit に `count`/`durationSeconds` を載せられ、
  これが昇格判定に使われる。`observingExempt`（既定 `manual`/`derived`）
  の source と人操作（対応ボタンの強制起票）は観測せず即起票する
- **起票ストーム抑制**: `config.intakeMaxPerHour`（既定 0=無制限）を超える
  source+repo 単位の新規起票は課題を作らず candidate に `deferred:"rate-limit"`
  を付けて留め置く。drain の reconcile が窓の空いた次回以降に補起票する
  （detector 暴走時の queued 溢れ防止）
- GitHub Issue は `gh issue list` ポーリングで open issue を全件候補化
  （ラベル選別しない）。**起票＝本システムの課題（issues.json）の作成**を指し、
  GitHub Issue への登録は別機構として後続検討する
- GitHub の既存 Issue（open+closed 全件）の一括取込は
  `kasugai-agent --import-github` または `POST /api/control/import-github`。
  closed は終端課題として起票する（COMPLETED→done / NOT_PLANNED→rejected）

### 5.2 assess

課題ごとに以下を決定する（実装時はルール + LLM 判定のハイブリッド）:

- **verifyLevel**（L0〜L4）— 全課題一律に L3/L4 を要求しない
- **受入条件** — verify の合格基準として課題に紐付ける
- **自律可否** — `external` ラベル・爆破半径・対象リポジトリの設定で判定
- **priority**（0〜3）— 対応順の目安。urgent/critical/security 系=3、
  ci-failure・local-health 由来や bug 系=2、既定=1、
  documentation/enhancement 系=0。一覧は状態→priority→更新時刻順に整列し、
  P2 以上をバッジ表示する

### 5.3 gate（人ゲート）

| ゲート | 条件 | 通過方法 |
|---|---|---|
| external 起票 | `external` ラベル付き Issue | パネルで人が承認 |
| proposal 採否 | 知見還流の提案全般 | パネルで人が採否 |
| push 最終判断 | 対象リポジトリの `requirePushApproval` | パネルで承認（既定は verify 合格で自動） |

匿名起票は prompt injection の入口になり得るため、external は **評価にも進まない**
（候補一覧への表示のみ）。

### 5.4 dispatch（runner）

- **dispatch は人判断のみ**: queued 課題の起動は ISSUES 一覧の「対応」
  ボタン（`POST /api/issues/:key/dispatch`）だけが行う。drain は起票
  reconcile・中断した verifying の再開・孤児 running の failed 化のみを
  担い、queued を自動起動しない
- runner は `chat/`（接続AI 実行体）が担う。既定実装は devin CLI を ACP で
  駆動する非対話セッション（`chat/acp.rs`）
- **起動可否は論理競合ベース**（同時起動数の上限は設けない）:
  同一リポジトリの未完了 run、排他ロック中の実機リソースがある課題は待機
- セッションには課題・受入条件・知見をプロンプトとして注入
- **runbook（kind 別対処手順）**: `prompts/runbook/` に
  `<source>-<kind>.md` → `<kind>.md` → `<source>.md` の順で手順テンプレを
  探し、見つかれば「対処手順」として dispatch prompt に注入する。
  kind の粒度は learn の SKILL draft（slug=source+repo+kind）と共通で、
  runbook が事前知識・SKILL が学習蓄積の対になる
- 成果物は MAIN 運用（既定ブランチで実装 → ローカルコミット → `AI:impl-done`）

<!-- diagram: runbook -->
```mermaid
---
title: runbook（kind 別対処手順の注入）
---
flowchart LR
    ISS[queued 課題<br/>kind = detector emit<br/>ci-failure=workflow名・<br/>local-health=gh-auth等] --> DISP[一覧の対応ボタンで<br/>dispatch]
    DISP --> RB1{prompts/runbook/<br/>source-kind.md?}
    RB1 -->|あり| INJ[runbook 本文を<br/>対処手順として<br/>prompt に注入]
    RB1 -->|なし| RB2{prompts/runbook/<br/>kind.md?}
    RB2 -->|あり| INJ
    RB2 -->|なし| RB3{prompts/runbook/<br/>source.md?}
    RB3 -->|あり| INJ
    RB3 -->|なし| SKIP[runbook なし]
    INJ --> PROMPT[dispatch prompt =<br/>課題 + 受入条件 + 運用ルール<br/>+ runbook + knowledge]
    SKIP --> PROMPT
    PROMPT --> RUN[runner 非対話実行<br/>接続AI -p]
    KN[knowledge/key-slug.md<br/>課題単位の生知見<br/>常に注入] -.-> PROMPT
    SK[.devin/skills/slug<br/>slug=source+repo+kind<br/>learn が育てる SKILL draft<br/>runbook の学習版に相当] -.->|kind の粒度は共通| RB1
```

### 5.5 verify（L0〜L4）

| 層 | 内容 | 例 |
|---|---|---|
| L0 | 静的チェック | 構文・lint・差分の存在 |
| L1 | 単体動作 | テスト・ビルド・起動確認 |
| L2 | 実機実行 | 対象EXE自体（QGIS等の成果物バイナリ）を実機で起動し応答確認 |
| L3 | 画面駆動 | Playwright等でUI操作・スクリーンショット証跡 |
| L4 | 主旨照合 | Issue の意図と結果の突き合わせ |

L2 以上の verify は **実機検証を前提とする**: 成果物のEXE自体を実環境で起動し、
Playwright 等で画面・応答を駆動確認する。「実装の確認」とはコードやビルドの
成否ではなく、実機で動く成果物として期待どおりに振る舞うことの確認をいう。

<!-- diagram: verify-levels -->
```mermaid
---
title: verify（L0〜L4）
---
flowchart TD
    IMPL[実装完了<br/>ローカルコミット] --> VL{verifyLevel}
    VL -->|L0/L1| STATIC[静的・単体チェック<br/>構文・lint・テスト・ビルド]
    VL -->|L2以上| LAUNCH[実機で対象EXE自体を起動<br/>QGIS等・成果物バイナリ]
    LAUNCH --> DRIVE[実機駆動<br/>Playwright等でUI操作・応答確認]
    STATIC --> EVID[証跡採取<br/>shots/logs/応答 → evidence/]
    DRIVE --> EVID
    EVID --> JUDGE{合格?<br/>実装の確認}
    JUDGE -->|OK| ATTRIB[コミット帰属確認<br/>Issue: #N]
    ATTRIB --> CLEAN{帰属不明/<br/>rejected 混入?}
    CLEAN -->|yes| REVERT[git revert で除去]
    CLEAN -->|no| PUSH[git push]
    REVERT --> PUSH
    PUSH --> DONE[status=done]
    JUDGE -->|NG| SEP[課題を分離<br/>実装済みNG課題 × 未実装課題<br/>kasugai-verify verdict]
    SEP --> BACK[差し戻し prompt を組み立て保留<br/>実装済み→修正 / 未実装→実装<br/>（自動送信はしない）]
    BACK --> RETRY[status=retry<br/>要対応列で再送待ち<br/>verifyBackLimit まで]
    RETRY -->|処理再開ボタン /<br/>個別の差し戻しボタン /<br/>再起動時の自動再処理| IMPL
    RETRY -->|上限到達| FAIL[status=failed<br/>要人対応マーク + sys 報告]
```

- **push は verify だけが行う**。検証を通っていないコミットは公開しない
- 未pushコミットの帰属は `Issue: #N` トレーラで判定。帰属不明・closed・
  `devin:rejected` の混入は `git revert` で除去（`git reset` は禁止）
- 実機リソース（QGIS harness・固定ポート等）は `locks` で排他利用
- 証跡は課題キー単位で `evidence/` に保存し、Issue コメントに添付/リンク

### 5.6 learn（知見の還流）

<!-- diagram: learn-flow -->
```mermaid
---
title: learn（知見の還流）
---
flowchart LR
    RUN[run 結果・証跡] --> KN[生の知見を蓄積<br/>knowledge/<br/>課題key単位・無条件]
    KN --> SK[SKILL draft 組立<br/>.devin/skills/<slug>/SKILL.md<br/>slug = source+repo+kind<br/>揮発 id を含まない安定名<br/>mermaid 図必須・図 title = slug]
    SK --> DUP{同 slug path の proposal 既存？<br/>pending + decided/ を対象<br/>または同一内容<br/>（key/題名/知見名の揮発分を<br/>正規化比較）}
    DUP -->|なし| PROP[proposals 生成<br/>kind=skill・issueKey=課題key]
    DUP -->|あり| SKIP[起票しない<br/>同じ種別の再検出は抑止]
    PROP --> VET[vet<br/>機械検査<br/>path許可 / secret混入 /<br/>構文 / frontmatter]
    VET -->|fail| VF[vet 不合格<br/>accept 不能・要修正]
    VET -->|pass| HUMAN{人の採否}
    HUMAN -->|採用| SELF[agent 自身へ反映<br/>or 正本へ課題起票]
    HUMAN -->|却下| DROP[破棄]
    SELF --> REC[proposals/decided/ へ記録<br/>一覧は pending のみ]
    DROP --> REC
```

- 生の知見は課題 key 単位で `knowledge/` に無条件蓄積。SKILL draft
  （mermaid 図必須・図 title = slug）は `.devin/skills/<slug>/` へ起票し、
  **反映は必ず proposals 経由の人ゲート**
- skill 名（slug）は `source+repo+kind` の安定名（run id 等の揮発 id を
  含まない）。同じ種別の課題は同じ skill 名に収束し、同 slug の proposal
  が pending/decided にあれば再起票しない
- **vet（機械検査）**: proposals は登録時・採否時に静的検査される
  （path 許可範囲 / secret 混入 / JSON・frontmatter 構文）。
  vet 不合格は accept 不能（server/src/proposals.rs）。採否そのものは人が行う
- リポジトリ正本（`.devin/skills`・AGENTS.md）への反映は新規課題として起票し、
  通常パイプで対応する

### 5.7 知見の参照ループ（蓄積 → 類似事例 → レッスン → 評価）

蓄積した知見は参照されて初めてループが閉じる。参照側はすべて決定的
（非 LLM）ロジックで、context collapse・プロンプト肥大を防ぐ。

<!-- diagram: case-search -->
```mermaid
---
title: 類似事例検索（CBR）と assess/run への知見参照
---
flowchart TD
    ISS[新規課題<br/>source/repo/kind/<br/>symptomHash/title] --> AS[assess<br/>pipeline.rs]
    AS --> SC[類似度スコア<br/>cases.rs・決定的・非LLM<br/>repo一致40 + kind一致20 +<br/>symptomHash一致20 +<br/>title bigram Jaccard20]
    CASES[(knowledge/cases.json<br/>終端課題の case:<br/>kind/repo/symptomHash/<br/>採用アプローチ/結果/verifyLevel)] --> SC
    ISSUES[(issues.json<br/>全課題)] --> CASES
    ISSUES --> SC
    SC --> REL[issue.related<br/>類似事例 top N・score>=50<br/>前回の fix / reject /<br/>run 成否]
    REL --> VL{成功 case と<br/>score>=70 で一致?}
    VL -->|yes| ADOPT[verifyLevel を成功 case の<br/>値に合わせ<br/>verifyLevelBasis を記録]
    VL -->|no| RULE[ルール判定のまま]
    ADOPT --> DISP
    RULE --> DISP
    REL --> DISP[dispatch prompt<br/>runner.build_prompt]
    DISP --> SEC[## 類似過去事例<br/>status・verifyLevel・title・<br/>knowledge/slug.md 抜粋・<br/>成功 case の採用アプローチ<br/>（few-shot）]
    KN[knowledge/key-slug.md<br/>課題単位の生知見] --> SEC
    SEC --> RUN[run 実行<br/>接続AI -p]
    RUN --> REC[runs[].knowledgeRefs に<br/>参照した knowledge/cases/<br/>lessons を記録<br/>（どの知見を使ったか追跡可）]
    RUN --> OUT{run 結果}
    OUT -->|done/failed/rejected| TERM[終端課題]
    TERM -->|learn_pass が<br/>case 化| CASES
```

- **類似度スコア**（`server/src/cases.rs`）: repo 一致・kind 一致・
  symptomHash 一致・title の bigram Jaccard を重み付けした決定的スコア
  （埋め込み基盤なし）。score≥50 を「類似」として top N を選ぶ
- **assess 時の知見参照**: 新規課題に `related[]`（類似事例 top N:
  前回の fix / reject / run 成否）を付記し、dispatch prompt の
  「類似過去事例」節へ注入する。参照した知見は `runs[].knowledgeRefs`
  に記録され、どの知見を使ったか追跡できる
- **case 化（CBR）**: 終端課題は `knowledge/cases.json` へ case
  （kind/repo/symptomHash/採用アプローチ/結果/verifyLevel）として蓄積。
  高一致の成功 case があれば verifyLevel をその値に合わせ
  （`verifyLevelBasis` に出典を記録）、採用アプローチは run prompt の
  few-shot に転用する

<!-- diagram: lesson-loop -->
```mermaid
---
title: 失敗レッスンループ（Reflexion 型リトライ + ACE カウンタ）
---
flowchart TD
    NG[verify NG / run failed] --> GEN[レッスン生成<br/>lessons.rs・決定的<br/>何が・なぜ失敗したか<br/>出典 run idx + 根拠<br/>verifyNote/error + 確度メタ]
    GEN --> PB[(knowledge/lessons.json<br/>ACE 型 playbook<br/>id + helpful/harmful +<br/>text + basis + confidence)]
    DISP[dispatch 時<br/>runner.build_prompt] --> SEL[注入選定・決定的非LLM<br/>同一課題の直近 1-3 件 +<br/>類似課題のレッスン上位 N 件<br/>harmful>=lessonHarmLimit は除外]
    PB --> SEL
    SEL --> PR[## 失敗レッスン<br/>「id helpful=X harmful=Y :: text」]
    PR --> RUN[run 実行<br/>接続AI -p]
    RUN --> REF[runs[].knowledgeRefs.lessons<br/>注入したレッスン id を記録]
    RUN --> OUT{run 結果}
    OUT -->|done| HELP[注入レッスンへ<br/>helpful 加算]
    OUT -->|verify NG / failed| HARM[注入レッスンへ<br/>harmful 加算]
    HELP --> PB
    HARM --> PB
    HARM --> LIM{harmful >=<br/>lessonHarmLimit?}
    LIM -->|no| PB
    LIM -->|yes| EXC[excluded=true<br/>注入対象から自動除外<br/>誤教訓による汚染を防ぐ]
    EXC --> PROP[削除 proposal 起票<br/>lessons.json から当該<br/>レッスンを除く版]
    PROP --> HG{人の採否}
    HG -->|採用| DEL[レッスン削除]
    HG -->|却下| KEEP[除外状態のまま保持]
```

- **失敗レッスン（Reflexion 型リトライ）**（`server/src/lessons.rs`）:
  verify NG / run failed のたびに「何が・なぜ失敗したか」の短いレッスンを
  決定的に生成し、出典（run idx・根拠・証跡 dir・確度メタ）を付けて
  `knowledge/lessons.json` へ蓄積。dispatch prompt には同一課題の直近
  1-3 件 + 類似課題の上位 N 件を注入し、注入分は
  `runs[].knowledgeRefs.lessons` に記録する
- **ACE 型カウンタ**: レッスンは id + helpful/harmful を持ち、注入後 run
  の成否で更新。harmful が `lessonHarmLimit`（既定2）に達したレッスンは
  注入対象から自動除外し、削除 proposal を起票（台帳汚染を止める）

<!-- diagram: consolidation -->
```mermaid
---
title: 知見圧縮（sleep-time consolidation）
---
flowchart TD
    TICK[learn watcher<br/>pollSeconds.learn] --> IDLE{アイドル判定<br/>queued/running/verifying<br/>なし + 前回活動から<br/>consolidateIdleMinutes 経過}
    IDLE -->|busy| SKIP[今回は統合しない]
    IDLE -->|idle| SCAN[knowledge/*.md 走査<br/>課題 key 単位の生知見]
    SCAN --> CLU[クラスタ化・決定的<br/>source + repo + kind で分類<br/>consolidateMinCluster 以上を対象]
    CLU --> SYN[上位レッスン合成・決定的<br/>結果分布 done/failed/rejected +<br/>頻出レッスン advice +<br/>採用アプローチ要約]
    SYN --> RB[runbook md 生成<br/>prompts/runbook/<br/>source-kind.md]
    RB --> EXIST{既存 runbook 同内容<br/>or proposal 済み?}
    EXIST -->|yes| NOP[起票しない]
    EXIST -->|no| PROP[proposal 起票<br/>kind=other<br/>changes: runbook path]
    PROP --> HG{人の採否}
    HG -->|採用| WRITE[root の prompts/<br/>runbook/ へ書込み]
    HG -->|却下| NOCHG[蓄積は変えない]
    WRITE --> INJ[次回 dispatch の<br/>runbook 注入へ反映<br/>knowledge.load_runbook]
    SKIP --> TICK
    NOP --> TICK
```

- **sleep-time consolidation**（`server/src/consolidate.rs`）: 実行系が
  アイドル（活動課題なし・`consolidateIdleMinutes` 経過）のときだけ、
  `knowledge/` の課題知見を source+kind でクラスタ化し、
  `consolidateMinCluster`（既定3）以上のクラスタから上位レッスン
  （傾向・既知の失敗パターン・成功アプローチ）を合成して
  `prompts/runbook/<source>-<kind>.md` の proposal を起票。
  出力は常に proposals 経由で人が採否し、採用時に既存の runbook 注入
  へ反映される

## 6. 状態とファイル

| ファイル | 内容 | Git 管理 |
|---|---|---|
| `config.json` | 監視リポジトリ・ポート・ゲート設定・接続AI（`chatAgents`/`chatAgent`） | 追跡（example から複製） |
| `state.json` | ループ全体の稼働状態 | 対象外 |
| `issues.json` | 課題レコード（status・verifyLevel・retry 回数） | 対象外 |
| `candidates.json` | 未評価の候補一覧 | 対象外 |
| `silences.json` | 起票ミュート（matcher+期限の silence 一覧） | 対象外 |
| `work-queue.json` | 実行待ちキュー | 対象外 |
| `knowledge/` | 課題単位の知見 | 対象外 |
| `proposals/` | 人ゲート待ちの提案 | 対象外 |
| `evidence/` | 検証証跡（shots・ログ・応答） | 対象外 |
| `locks/` | リソースロックファイル | 対象外 |
| `chat-snapshots/` | chat が変更したファイルの turn 単位変更前スナップショット | 対象外 |
| `assets/` | CDN から適用したアセット（panel/docs のオーバーレイ・state.json） | 対象外 |
| `logs/` | 稼働ログ | 対象外 |
| `secrets.json` | API キー等 | **対象外・唯一の保管場所** |

- 状態を変更したら SSE で即時 push（常時ポーリングしない）
- 表示用フィールドを追加したら API 応答にも必ず含める

## 7. API / パネル

`control.rs` が `:8586` で提供（loopback のみ）。

- `GET /api/state` — 全体状態 / `GET /api/issues` / `GET /api/candidates`
- `GET /api/agents` / `GET /api/skills` — registry が収集した doc 一覧。
  `?repo=<slug>` で対象リポジトリの path + global 配置に絞り込む（省略時は全リポジトリ）。
  ただし `/api/skills` は global 配置の skill を一覧から除外する
  （収集・doc 本文/参照解決の対象ではある）
- `GET /api/doc?path=` — doc 本文（registry 収集済み + 参照解決済みパスのみ返す）
- `GET /api/refs?path=` — 収集済み doc が本文中で参照するファイル一覧
  （`path` は収集済み doc のみ。glob・ディレクトリ参照は実ファイルへ展開し、
  秘匿系は `denied` として返すが本文は返さない）
- `POST /api/issues/:key/dispatch` / `POST /api/issues/:key/approve` / `reject`
- `GET /api/proposals` / `POST /api/proposals/:id/accept|decline`
- `GET /api/repos` — repos 一覧。`config.repos` 登録済みに加え、IDE ワークスペース
  （control の ROOT 親ディレクトリ。`config.workspaceRoot` で上書き可）直下の
  `.git` 付きディレクトリを `IDE管理下`（unregistered）として検出して返す。
  GitHub を利用していないユーザーリポジトリも `ユーザー`（unregistered）として
  検出する: ワークスペース直下の `.git` 無しディレクトリ、ユーザーディレクトリ
  直下の `.git` 付きディレクトリ、`config.repoScanDirs` 各直下の全ディレクトリ
- `POST /api/repos` — 監視リポジトリ登録（`{path, slug?}` を `config.json` の
  `repos` へ追記。SSE `repos` で全パネルへ即時反映）
- `POST /api/repos/update` — 登録済み repo の設定更新
  （`requirePushApproval` → `config.json` へ反映）
- `POST /api/repos/remove` — `config.repos` からの登録解除
- `GET /api/fs/dirs?path=` — 登録フォームの path 選択用。指定ディレクトリ直下の
  サブディレクトリと `.git` 有無を返す（`path` 省略時はドライブルート一覧。
  loopback 限定の簡易ブラウザ）
- `GET /api/chat-agents` — 接続AI の一覧と active id（`type`/`exe`/`endpoint`/
  `provider`/`model`/`hasToken` 等。トークン値自体は返さない）
- `POST /api/chat-agents` — エージェント登録（`{id?, label, type?, exe?, args?,
  auth?, endpoint?, provider?, model?, project?, token?}` を `config.json` の
  `chatAgents` へ追記。`token` は `secrets.json` の `tokens` へ保存し
  config には書かない。SSE push で即時反映）
- `POST /api/chat-agents/update` / `remove` — 登録済みエージェントの更新・解除
  （remove は active 以外のみ可。update の `token` は同様に secrets へ、
  空文字で削除）
- `POST /api/chat-agents/active` — active 切替。以後の新規 chat の実行体になる
- `POST /api/chat/verify` — 検証ターンの手動起動（処理状況ビューの
  検証列カード「検証」ボタンが呼ぶ。実装を実機で確認する prompt を
  その chat のセッションへ送る）
- `POST /api/chat/rework` — 要対応列の差し戻し（同列カードの
  「差し戻し」ボタンが呼ぶ。`doneKind`・verify 系・要人対応をリセットし、
  保留の差し戻し prompt（無ければ終了区分に応じた再作業 prompt）を
  送って実行状況列へ戻す）
- `POST /api/chat/close` — chat を1件削除（処理状況ビューの
  各カードの × が呼ぶ。スナップショットも
  除去し SSE `chats` で即時反映）
- `POST /api/chat/close-completed` — 完了（live）列の chat を一括削除
  （処理状況ビュー完了列ヘッダの「すべて削除」ボタンが呼ぶ。
  対象は live 列の chat＝`doneKind=done`（実装完了）と無印。
  `{repo}` 指定時はその slug に属する chat のみ対象）
- `POST /api/chat/resume-all` — 止まった処理の一括再開（処理状況ビュー
  上部の「処理再開」ボタンが呼ぶ。差し戻し待ちは保留の差し戻し prompt を
  再送し、再開可 chat は `session/load` で再接続したうえで中断作業の
  続行 prompt を送り、未着手 chat は `session/new` で起動して保留
  prompt を再送し、検証待ちは検証ターンを起動し、ブロッカーが消えた
  実行待機は解放して保留 prompt を再送する。busy 中と repo 確認待ち・
  要人対応は対象外。`{repo}` 指定時はその slug のみ対象）
- `POST /api/control/restart` — control 手動再起動（設定ビューの
  「システム再起動」ボタンもこれを呼ぶ。実行中 run が残る間は空くまで延期し、
  `{force:true}` なら実行中の chat を中断して即時再起動する。
  SSE `restart` で pending/restarting を push）
- `POST /api/control/launch` — 環境起動（docs ナビの「環境を起動」ボタンが
  呼ぶ。`{target:"prod"}` → `server/target/release/kasugai-agent.exe`
  （無ければ現行 exe、既定 :8586）を detached 起動。対象ポートが既に応答して
  いれば `already:true` を返して起動しない。`{port}` でポート上書き可。
  判定フローは §8 末尾の図を参照）
- `GET/POST /api/control/autostart` — ログオン自動起動（HKCU Run キー
  `KasugaiAgentDispatcher`）の状態取得 / `{action:"install"|"uninstall"}`
  で登録・解除（変更は SSE `autostart` で push）
- `GET /api/events` — SSE（状態push・チャット進行表示）
- `GET /docs/*` — マニュアルサイトの静的配信（`docs/` ディレクトリ。
  パネル左ペインの「マニュアル」ナビが中央ペイン iframe で開く。
  正本は `diagrams/docs/` で `tools/sync-docs.mjs` が生成する）

### チャット

チャットは接続AIを実行体に持つ対話面。接続先は `config.json` の
**`chatAgents`**（実行体レジストリ）と **`chatAgent`**（active id）で設定し、
設定ビューからも追加・更新・解除・active 切替ができる。実行体の `type` は3種:

| type | 実行体 | 必須フィールド | 対象 |
|---|---|---|---|
| `acp` | ACP 対応 CLI を spawn し stdio JSON-RPC | `exe`/`args`/`auth` | devin（既定 `devin.exe acp`）・claude-code 等の ACP エージェント |
| `agent` | Canvas ブリッジハブ互換 `POST {endpoint}` | `endpoint`/`provider` | bridge-hub(`:8511`) 経由の copilot/devin/antigravity/汎用acp、外部エージェント |
| `openai` | OpenAI 互換 `chat/completions` | `endpoint`/`model` | OpenAI・Azure・Ollama・LM Studio・GitHub Models 等 |

- `acp`: セッション常駐（`session/new`→`session/prompt`、load で再開）。
  `auth` が `devin` は credentials.toml の API キーで authenticate、`none` は省略
- `agent`: 単発ターン。`{provider, message, project, model?, tools}` を POST し
  `{reply, actions?, error?}` を受ける。`/health` を持つ実装は疎通確認に使える
- `openai`: 単発ターン。`endpoint` は base URL（`/chat/completions` を補完）か
  フルパス。履歴は直近の user/agent メッセージから組み立てて送る
- トークン・APIキーは `secrets.json` の **`tokens`**（`{tokens:{<key>:"…"}}`）に
  置き、エントリの `tokenKey`（省略時は id）で参照する。config.json・ログ・
  プロンプトには書かない

実行体は chat 作成時の active エージェントに固定され `chats.json` に
永続化する（active 切替は以後の新規 chat に適用）。
会話は課題レコードとは独立しており、chat 内で実行体が直接作業する。

<!-- diagram: chat-sequence -->
```mermaid
---
title: chat 送受信シーケンス
---
sequenceDiagram
    participant UI as panel (browser)
    participant C as control.rs
    participant A as 接続AI<br/>（config.chatAgent・図は type=acp の例）
    Note over C,A: type=agent/openai は単発 HTTP（POST → reply 受信）。<br/>常駐セッション・session/load 再接続は acp 型のみ
    Note over C,A: control 起動時は resumable chat を自動で session/load 再接続（§8）
    UI->>C: POST /api/chat/send {id?, text, mode?}
    Note over C: 新規 chat の execMode 既定=parallel<br/>（mode 未指定時。分割子 chat は direct 固定）
    C->>C: リポジトリ不整合チェック（repomatch.rs）:<br/>prompt 文中のパス・repo 名 × chat のカレント repo
    alt 他 repo 向けの内容と判定<br/>（busy 中・keep 回答済みの候補は確認しない）
        C->>C: waiting(kind=repo) へ。<br/>prompt は保留キューへ
        C-->>UI: SSE push（repo 確認待ち。切替/このまま送信ボタン）
        UI->>C: POST /api/chat/repo-answer {id, action}
        alt action = switch
            C->>C: chat の repo/cwd を切替先へ変更。<br/>ACP セッションは cwd 固定のため破棄し<br/>新規セッションで継続（HTTP 型は cwd 差替のみ）
        else action = keep
            C->>C: 候補 repo を declined へ記録<br/>（この chat では再確認しない）
        end
        C->>A: 保留 prompt を session/prompt へ再送
    else 整合（または候補なし）
        C->>C: 競合判定: prompt 文中のパス ×<br/>他 busy chat のファイルクレーム
        alt 同一ファイル競合あり
            C->>C: waiting（実装待機）へ<br/>prompt は保留キューへ
            C-->>UI: SSE push（wait 表示）
        else 競合なし
            C->>A: session/new or session/load → session/prompt
            Note over C,A: execMode=parallel の初回 prompt には<br/>分割指示プリアンブルを前置<br/>（セッション内で指示が残るため1回のみ）
            A-->>C: session/update stream (message/tool_call/plan/usage_update)
            Note over C,A: 編集系 tool_call の対象パスは touched として記録し、<br/>turn 単位で変更前スナップショットを<br/>chat-snapshots/ へ採取。<br/>他 busy chat のクレームと重なったら<br/>session/cancel して waiting へ
            Note over C,A: usage_update {used,size,cost?} は chat 別の<br/>コンテキスト消費量として保持。<br/>type=openai は応答 usage.total_tokens から採取<br/>（size 不明のため使用数のみ）
            C-->>UI: SSE push (chat 更新通知・ctx 消費量も随時反映)
        end
    end
    alt execMode=parallel: ターン終了時に<br/>kasugai-split ブロックを検出
        C->>C: split.rs が [{title,prompt}] を抽出（重複抑止）。<br/>parallelMaxChildren は同時実行上限:<br/>アクティブ子数（busy/待機/検証中）が<br/>上限未満なら即起動、超過分は<br/>pendingSplits 待機キューへ（chats.json 永続化）
        C->>A: 子 chat を生成（parent=親id・direct 固定・<br/>同 repo cwd・同 agent）して session/prompt
        Note over C: 子 chat の busy 解除 → 最終回答を<br/>ダイジェストとして親 chat へ投函。<br/>空いた枠で待機キューから順次起動。<br/>親は統合・さらなる分割を続けられる
    end
    Note over C: blocker chat の busy 解除 → 待機を解き<br/>保留 prompt を session/prompt へ再送
    Note over C,A: acp spawn 時は WINDSURF_EXT_HOST_PID を<br/>env から除去（devin は実親でなくこの変数の pid を<br/>親監視するため、死んだ IDE ホストの値が残ると<br/>実行体が起動直後に自死する）
    Note over C,A: spawn した実行体の stderr・非JSON stdout と<br/>control 自身の stdio は conlog.rs の<br/>リングバッファへ集約 → コンソールタブへ SSE push
    UI->>C: GET /api/chat?id=N
    C-->>UI: messages/busy/waiting/options/snapPoints スナップショット
    UI->>C: POST /api/chat/resume {id}
    C->>A: session/load（resumable chat の手動再接続）
    UI->>C: POST /api/chat/mode {id, mode}
    C->>C: execMode を direct/parallel へ切替<br/>（chats.json へ永続化・SSE push。既定=parallel）
    UI->>C: POST /api/chat/revise {id, text}
    C->>A: session/cancel（busy 時のみ）
    C->>C: 最後の user 送信以降の表示を破棄
    C->>A: session/prompt（修正文で最終ターンをやり直し）
    UI->>C: POST /api/chat/restore {id, turn?}
    C->>C: 指定 turn 以降に touched したファイルを<br/>chat-snapshots/ の変更前スナップショットで復元<br/>（その時点に無かった新規ファイルは削除）<br/>復元済みスナップショットは消費して破棄
    C-->>UI: SSE push + sys message
```

- セッションは複数並行・ラベル付きで管理し `chats.json` に永続化
  （ACP sessionId と作成時の agent id を保持し、切断後は同一エージェントで
  `session/load` により再開。control 起動時は resumable な全 chat を
  自動再接続する — §8）
- モデル/モードは ACP `configOptions` を `session/set_config_option` で変更し、
  `acp-options.json` に永続化してセッション未開始時も選択肢を表示する
- busy 中の送信は追加指示（steer）として `session/prompt` を重ねる
- 最後の user 送信は ✎ または ↑ で入力欄へ呼び戻して修正できる。修正送信は
  `POST /api/chat/revise` が受け、busy 中は `session/cancel` してから最後の
  送信以降の表示を破棄し、修正文を `session/prompt` へ再送する（ACP に
  履歴の巻き戻しはないため「最終ターンのやり直し」として振る舞う）
- **変更復元（snapshots）**: 修正がすべて正しいとは限らないため、chat が加えた
  ファイル変更は turn（user 送信）単位で変更前スナップショットを
  `chat-snapshots/<id>/` に採取する。採取は send 時の prompt 文中パスと、
  busy 中の編集系 tool_call の対象パスが対象（tool_call 通知は変更適用前に
  届く前提の best-effort）。ヘッダの「変更復元」ボタンは復元ポイント
  （turn 単位）を選ばせ、選択 turn 以降の変更をまとめて元へ戻す
  （各ファイルの最古スナップショットへ書き戻し、当時無かった新規ファイルは
  削除）。スナップショットはディスク永続化のため、session/load で会話を
  復元した後や control 再起動後でも復元できる。復元済み分はマニフェストから
  消費される。busy 中は復元を拒否する（先に中断させる）
- 権限要求（`session/request_permission`）は allow 系を自動選択（自律実行前提）
- **実装待機（waiting）**: 同一ファイルを複数 chat が同時に編集する競合を防ぐ。
  busy 中の chat はファイルクレームを持つ（クレーム = prompt 文中の
  ファイルパス `declared` ∪ 編集系 tool_call が扱ったパス `touched`）。
  send 時に prompt 文中のパスが他 busy chat のクレームと重なれば、その
  prompt を保留して waiting に入る。実行中に tool_call が他 chat の
  クレームへ重なった場合も `session/cancel` でターンを止めて waiting に入る。
  blocker の busy が全て解けた時点で待機を解除し、保留 prompt
  （無ければ中断再開の指示）を再送する。待機中の送信は保留キューに積まれ、
  解除時にまとめて再送される。cancel で待機を手動解除できる（保留は破棄）。
  関数単位の競合はファイル単位クレームに包含して検出する
- **リポジトリ不整合チェック**: send 時に prompt 内容と chat の
  カレントリポジトリの整合を検査する（`server/src/repomatch.rs`）。
  prompt 文中のパスが他の登録 repo の path 配下を指す、またはカレント
  repo 名への言及が無いまま他 repo 名に言及している場合、その送信を
  保留して waiting（`kind: 'repo'`）へ入り、パネルに
  「切り替えて送信 / このまま送信」の確認バーを出す。
  `POST /api/chat/repo-answer` の `action: 'switch'` は chat の
  repo/cwd を切替先へ変更して保留 prompt を再送する（ACP セッションは
  cwd 固定のため破棄し、新規セッションで継続。HTTP 型は cwd 差替のみ）。
  パネル側のカレント repo 選択も切替先へ追従する。
  `action: 'keep'` は候補 repo をその chat の `repoDeclined` へ記録し
  （chats.json へ永続化・同じ候補は再確認しない）、カレント repo のまま
  再送する。busy 中の steer には確認を挟まない
- **処理モード（execMode）**: chat ごとに `parallel`（既定）か
  `direct`（従来どおり逐次処理）を持ち、`POST /api/chat/mode` か入力行のセレクトで切替、
  `chats.json` へ永続化する。`parallel` では初回 prompt に分割指示
  プリアンブルが前置され（セッション内に残るため1回のみ。ACP 履歴には
  載るが表示はユーザ文のみ）、エージェントは作業の中で独立タスクへ
  分割できると判断した時点で `` ```kasugai-split `` コードブロックに
  `[{title, prompt}]` を出力する。control はターン終了時に新着の agent
  メッセージを走査し（split.rs）、ブロックを見つけると各要素を
  **子 chat**（`parent`=親 id・`direct` 固定・同 repo cwd・同 agent）として
  自動起動する。`prompt` は文脈を引き継がない独立セッションで動くため
  自己完結で書くようプリアンブルで指示する。暴走防止に
  `config.parallelMaxDepth`（既定1）の上限と、title+prompt 一致の
  重複抑止（`seenSplits` を chats.json へ永続化し再起動後の再発射も防ぐ）
  を持つ。`config.parallelMaxChildren`（既定4）は親あたりの
  **同時実行上限**で、アクティブな子 chat（busy／実装待機／検証中）が
  上限に達している間に出た分割は `pendingSplits` 待機キューに積まれ
  （chats.json へ永続化）、子 chat の完了で枠が空くたび順次起動される。
  子 chat の busy が解けると最終回答がダイジェストとして親へ
  投函され（親 busy 中は steer・waiting 中は保留キュー行き）、親が
  統合・追検討・さらなる分割を続けられる。子 chat のファイル競合は
  通常のクレーム／実装待機の経路で直列化される

パネル（`panel/index.html`）はチャットを主画面とし、課題ボード・proposals・
設定は M4 で統合する。agent/thought/plan 応答と doc 本文は Markdown として
描画し、`` ```mermaid `` コードブロックは mermaid.js で図にレンダリングする。
raw HTML は表示時にエスケープする（XSS 防止）。

画面は左・中央・右の3ペイン構成とし、左ペインを SkillOps/AgentOps の
管理面とする。右ペインは常時表示で、エディター/AGENTS/SKILL 選択時は
doc browser / フォルダツリー、それ以外（右ペインに出すものがない時）は
管理セクション（loop state / candidates / issues / proposals）に
割り当てる。chat の切替は処理状況ボードが担う。
タイトル/ブランドは左ペインに固定し、その直下に**カレントリポジトリ選択**を置く。
この選択がパネル全体のスコープになる（マルチリポジトリ対応）:

- 右ペインの doc browser・管理セクション
  （candidates）を選択リポジトリで絞り込む
- 新規チャットの cwd にも適用する
- 常にいずれか1リポジトリを指す。**`すべて` 選択肢は置かず**、
  未選択（全体表示）状態を作らない。選択は localStorage に永続化する
- リポジトリ名の表示はこのセレクトだけに限定する。選択リポジトリで絞られた
  一覧・画面（candidates・doc 一覧・ヘッダ status 等）では
  行ごとの repo 表示をしない（常に同一リポジトリなので冗長）。
  処理状況ボードだけは全 repo の chat を表示し、カレント repo 外の
  カードには repo ピルを付す
- 左ペインはカレントリポジトリの選択に専念する。監視リポジトリの登録・
  登録解除・設定編集はすべて設定ビュー（中央ペイン）で行う
- ナビ「設定」は中央ペインを設定ビューに切り替える。ビューは
  **接続AI / リポジトリ / システム** の3タブにグループ分けし、
  選択タブは localStorage に永続化する。
  「リポジトリ」タブ先頭の登録フォームにローカルパスを入力して
  `POST /api/repos` で登録する。
  path は「参照…」ボタンで選ぶこともできる（`GET /api/fs/dirs` で直下の
  サブディレクトリを辿る簡易ピッカー。`.git` 付きにはバッジを表示）。
  `config.json` の `repos` へ追記され、SSE push で左ペインの選択肢へ
  即時反映される。slug 省略時は `git remote get-url origin` から推定する
  （推定不能時はディレクトリ名）。新規登録リポジトリは
  `requirePushApproval:true` の保守的既定で追加する。
  `GET /api/repos` の一覧をカード表示し、登録済みは
  `requirePushApproval` を編集して
  `POST /api/repos/update` で保存、`POST /api/repos/remove` で登録解除する。
  IDE ワークスペース直下の `.git` 付きディレクトリは「IDE管理下」バッジ付きで
  一覧し、登録ボタンで `POST /api/repos` へ流せる。
  GitHub を使わないユーザーリポジトリも「ユーザー」バッジ付きで検出する
  （ワークスペース直下の `.git` 無しディレクトリ・ユーザーディレクトリ直下の
  `.git` 付きディレクトリ・`config.repoScanDirs` 各直下の全ディレクトリ）。
  一覧はカレントリポジトリ
  選択で絞らず全件表示する（repo 一覧そのものが表示対象）
- 「接続AI」タブ（既定タブ）では `config.json` の
  `chatAgents` に登録されたエージェント（`acp`/`agent`/`openai` 各 type の
  フィールド）の一覧・追加・更新・解除と、active（`chatAgent`）の切替を
  行う。トークンは `secrets.json` の `tokens` へ保存され `hasToken` のみ表示。
  変更は
  `POST /api/chat-agents*` → config.json へ反映され SSE push で即時表示に
  出る。active は以後に作成する chat の実行体（既存 chat は作成時の
  エージェントを保持）。active なエージェントは解除できない
  （先に別エージェントを active にする）

中央ヘッダの「＋ 新規チャット」ボタンは新規 chat 画面を開き、
「最新」ボタンは `updatedAt` 最大のチャットを選択して表示する。
中央ペインのチャット画面は **チャット / コンソール** の2タブに分かれる
（選択は localStorage に永続化）。
コンソールタブは裏側プロセスの出力をモニターする画面で、
spawn した接続AI（acp 型）の stderr・非JSON stdout と
control 自身の stdio を `server/src/conlog.rs` のリングバッファへ集約し、
`GET /api/console`（履歴）+ SSE `console` push（追加分）で
ライブ表示する。行は `{seq, ts, src, stream, text}` 形式で、
`src` は `#<chat id> <agent label>` または `control`。
左右ペインは境界のグリップをドラッグして幅を変更でき、
幅は localStorage に永続化して次回表示に引き継ぐ。

左ペイン上部の縦並びナビ（**処理状況 / AIチャット / エディター / AGENTS /
SKILL / マニュアル / 設定**）のうち、エディター / AGENTS / SKILL は
右ペインを doc browser に切り替えて表示する。AIチャットは
doc/ツリー表示を閉じて右ペインを管理セクションへ戻し、チャット表示へ戻る。
エディターはカレント repo のフォルダツリーを
展開する（dir は `GET /api/fs/tree` で lazy 展開、file 選択で
`GET /api/fs/file` の本文を中央ペインに表示。登録 repo 配下・非秘匿のみ）。
AGENTS / SKILL は
doc browser に切り替えて registry（`server/src/registry/agents.rs` /
`skills.rs`）が収集した agent 定義・ルール文書（`AGENTS.md`・
`.devin/agents`・`.devin/rules` 等）や skill 定義（各種 `skills/` 配下の
`SKILL.md`）をボタン一覧で表示する。
skill の走査範囲はリポジトリ内に加え、グローバル配置
（`%APPDATA%/devin`・`~/.config/devin`・`~/.devin`・`~/.agents`・
`~/.codeium/<channel>` の `skills/`、Devin アプリ同梱 session skills、
`~/.devin/extensions/<ext>` 配下）も含む。
global 配置の skill は収集・doc 本文/参照解決の対象だが、SKILL 一覧には
表示しない。
doc を選ぶと `GET /api/doc` で本文を取得し、中央ペインに表示する
（.md は Markdown 描画、.mmd は mermaid 図として描画。
「原文 / プレビュー」ボタンで切替可。
切替時は一覧先頭の doc を自動で開く）。
doc browser は読み取り専用で、編集・反映は proposals 経由のみ。

**処理状況**（起動時の初期表示）は右ペインを変えずに中央ペインだけを
処理状況ビューへ切り替え、
全 repo の chat を 未着手 / 実行状況 / 検証 / `resumable` /
要対応 / `live` の**状態別カンバン列**（横並び、この順）に配置する。
実行状況列は `busy` と `waiting`（実装待機）を1列に統合し、
各カードに **実行中 / 実行待機中** の状態ピルを明示する
（列内は 実行中 → 実行待機中 の順に並べる）。
検証列は実装ターン（ファイルを touched したターン）の終了で chat が
遷移する検証フェーズ。`config.autoVerify`（既定 true）なら終了時に
実機検証の prompt（対象 EXE 自体の起動・Playwright 等での画面駆動を
指示する VERIFY_PROMPT）を自動送信し、検証ターン中も検証列に留まる
（検証中に修正が入った場合は `retryLimit` 内で再検証）。
検証ターンは報告末尾に ```kasugai-verify ブロックで結果（verdict）を
出力する。verdict が NG なら課題を「実装済み（検証NG）」と「未実装」に
分離した差し戻し prompt を組み立てて**保留**し、chat は要対応列で
再送待ちに止まる（自動送信はしない。再送はボードの「処理再開」・
カードの「差し戻し」ボタン、または再起動時の自動再処理で行い、
`config.verifyBackLimit` 回まで繰り返す）。差し戻しターンの完了で
再度検証列へ戻る。差し戻し中は実行状況列のカードに「差戻N」ピルを
表示する。差し戻し上限に達したものは `ng` で確定し、カードに
「要人対応」ピルを付して人の判断を待つ（要人対応は処理再開の対象外。
カードの「差し戻し」で人手の再作業は可能）。
検証フェーズを解消した時点の終了区分（`doneKind`）で
完了の内訳を分ける: 検証合格は `done`（実装完了→ `live` 列）、
差し戻し上限到達・再送待ちの検証NGは `ng`、verdict を取得できずに
終了したものは `unverified`（この2つと差し戻し再送待ち・要人対応が
要対応列へ）。実装ターンを経ずに終了したものは無印で、操作不要の
入力待ちセッションとして `live` 列へ置く（要対応列には
人の操作が必要なものだけを残す）。要対応列のカードには区分ピルと
「差し戻し」
ボタンを置き、押下で `POST /api/chat/rework` により保留の差し戻し
prompt（無ければ終了区分に応じた再作業 prompt）を送って実行状況列へ
戻す（× ボタンでの個別削除も可）。
`autoVerify:false` 時や再検証待ちではカードの「検証」ボタン
（`POST /api/chat/verify`）で手動起動できる。
検証フェーズ中はチャット画面側にも状態を明示する。検証ターン中
（`verify=running`）はヘッダ表示に加え、メッセージ末尾のインジケータと
送信ボタンが「検証中」を示し、検証起動待ち（`verify=pending`）は
メッセージ末尾に「検証待ち」を出す（いずれも SSE push で即時反映）。
各列は状態名と件数を示し、
chat はカードとして現在地の列に置かれる（列は空でも常時表示し、
「今どこにあるか」を一目で追えるようにする）。
実行待機中のカードには競合元 chat と対象ファイルを併記する。
ACP プロセスも保存セッションも無い chat（未着手）も列に含める。
起動失敗（実行体の spawn・session/new 等の失敗）で未送信になった
prompt は chat に保留され、開始・処理再開で再送される。
列内はカレント repo のカードを先頭に並べ、他 repo のカードには
repo ピルを付す（クリックでカレント repo を切り替えて開く）。
カードクリックで該当 chat を選択表示し（chat の切替はこのボードで行う）、
ヘッダの「← チャット」でチャット表示へ戻る。SSE push で随時再描画する。
全カードに × ボタンを置き（右下にアクションボタンがあるカードでは右上）、
押下で `POST /api/chat/close` により chat を削除できる
（SSE push でボードにも即時反映する）。
完了列のヘッダには「すべて削除」ボタンを置き（カードがある時のみ表示）、
押下で確認のうえ `POST /api/chat/close-completed` により
カレント repo の完了列 chat（`doneKind=done`・無印のもの）を一括削除する
（要対応列の chat は残る。SSE push で即時反映）。
切断・再開可（`resumable`）列の各カードには「再開」ボタンを置き、押下で
`POST /api/chat/resume` により `session/load` でその場で再接続する
（次の送信を待たない。起動時の自動再接続と同じ経路で、
load 失敗時は `session/new` へフォールバックする）。
未着手（`idle`）列の各カードには「開始」ボタンを置き、押下で同じく
`POST /api/chat/resume` により `session/new` で起動し、
起動失敗で保留されていた prompt があれば再送する。
ボード上部には「＋ 新規チャット」（新規 chat 画面を開く）と
「処理再開」ボタンを置き（停止中の件数を併記・0件は無効）、
押下で確認のうえ `POST /api/chat/resume-all` により止まった処理を一括再開する
（差し戻し待ちは保留の差し戻し prompt を再送、再開可 chat は再接続＋
中断作業の続行 prompt 送信、未着手 chat は `session/new` で起動して
保留 prompt を再送、検証待ちは検証ターン起動、ブロッカーが消えた
実行待機は解放して保留 prompt を再送。実行中の chat と repo 確認待ち・
要人対応は対象外）。

doc が本文中で参照しているファイル（`` ` `` 囲みのパス表記・glob・
ディレクトリ参照）は、`GET /api/refs` で実ファイルへ解決する。
AGENTS はルール文書が1枚前提のため、切替時に収集した全 doc の参照を
マージし、doc 自身と「参照」群を1つの一覧として表示する。
SKILL は doc 選択時に右ペイン下部の「参照」ボタンとして一覧する。
いずれも押下で `GET /api/doc` の本文を
中央ペインへ表示する。内容の描画は panel 内の単一 `Viewer` オブジェクトに
集約されており（chat message の Markdown 描画・mermaid 後処理も同じ
ロジックを通る）、「原文 / プレビュー」ボタンで切り替える。
.md / .markdown は Markdown + mermaid 図、.mmd は mermaid 図、
.html / .htm は sandboxed iframe で描画する（それ以外は原文表示のみ）。
`secrets.json`・`.env`・鍵ファイル等の秘匿系はボタンを残すが無効表示とし
本文は返さない。未作成の参照も「未作成」ピル付きの無効表示で残す。

<!-- diagram: panel-layout -->
```mermaid
---
title: パネルレイアウト
---
flowchart LR
    L[左ペイン<br/>title + カレントrepo +<br/>縦並びナビ<br/>（処理状況/AIチャット/エディター/<br/>AGENTS/SKILL/マニュアル/設定の順。<br/>初期表示は処理状況）] --> C[中央ペイン<br/>chat messages（chat/console タブ）/<br/>doc 本文（Viewer 単一オブジェクトに集約<br/>kind判定・原文/プレビュー切替・mermaid描画<br/>MD+mermaid・HTML・MMD 表示・MMD は縦横切替可<br/>mermaid 図は＋−ボタン/wheel で拡縮<br/>（カーソル位置を維持）<br/>中央配置・ドラッグで視野移動・ズーム率表示）]
    C --> R[右ペイン<br/>管理セクション / doc browser + 参照ファイル /<br/>フォルダツリー（エディター）<br/>（エディター/AGENTS/SKILL 選択時は doc browser。<br/>表示するものがない時は管理セクションを<br/>表示し、ペイン自体は常時表示。<br/>省略ラベルは data-tip<br/>ホバーポップアップで全文表示）]
    G[境界グリップ<br/>ドラッグで幅可変<br/>localStorage 永続化] --> L
    G --> R
    RS[カレントrepo選択<br/>左ペイン title 直下<br/>「すべて」選択肢なし<br/>repo名の表示はここのみ<br/>localStorage 永続化] -->|表示スコープ| L
    RS -->|表示スコープ| R
    RS -->|新規chatの cwd<br/>スコープ外の chat選択・doc表示の解除<br/>（実行中chatはバックグラウンド継続）| C
    C -->|ヘッダレイアウト<br/>1行目=タイトル（概要）が全幅<br/>（タイトル前に処理状態ピル。<br/>削除ボタン表示時はタイトルと1行目を共有）<br/>2行目=plink/status/最新/◀▶ページング/<br/>options/言語select/操作ボタン| HD[flex-wrap で<br/>自然折返し]
    C -->|タイトル前の状態ピル| CPILL[ボードの列区分と同じ色で<br/>現在の処理状態を表示<br/>（実行中/実行待機中/検証中/<br/>検証待ち/要対応など）<br/>SSE push で即時反映]
    C -->|最新ボタン| NEW[updatedAt 最大の<br/>chat を選択]
    C -->|ヘッダの◀戻る/進む▶ボタン<br/>（新規画面からは 進む=先頭・戻る=末尾）| PAGE[処理状況ボードと同じ並びで<br/>前後の chat を選択<br/>（列順 idle→exec→verify→resume→<br/>unresolved→live。他repoのchatは<br/>カレントrepo切替を兼ねる）]
    C -->|ヘッダの＋新規チャットボタン /<br/>処理状況ボード上部の<br/>＋新規チャットボタン| NEWC[新規 chat 画面へ<br/>（chat の切替は処理状況ボードの<br/>カードで行う）]
    C -->|言語 select<br/>localStorage 永続化| I18N[panel/i18n.js<br/>i18n/*.json 辞書<br/>en がフォールバック<br/>data-i18n 属性 + t()<br/>切替時は全ペイン再描画]
    C -->|設定システムタブの<br/>タイムゾーン入力<br/>（IANA名・空欄=ブラウザ既定<br/>datalist に代表ゾーン）<br/>localStorage 永続化| TZ[表示タイムゾーン<br/>fmtTs/fmtTm が Intl で変換<br/>chat・console・一覧の時刻表示<br/>保存 ts は UTC のまま]
    R --> M1[loop state<br/>version/port/repos/chats/candidates<br/>+ KPI（status別件数・done率・MTTR）<br/>行ラベルは i18n 辞書で言語化]
    R --> M2[candidates/issues/proposals<br/>candidates は人の判断待ちのみ表示<br/>（未起票・gated・queued・failed。<br/>起票は自動が既定で実行中・終端の<br/>課題行は出さない）<br/>candidate 行は起票済み課題の status を併記<br/>rate-limit 保留の candidate は<br/>deferred ピルで明示<br/>全行に 対応/却下ボタン:<br/>対応=起票/承認/再実行を兼ねた即時 dispatch<br/>（未起票は rate-limit を人判断で bypass）<br/>却下=rejected（未起票は起票して即 rejected）<br/>proposal カードはクリックで<br/>中央ペインへ詳細表示<br/>queued issue は対応ボタンで即時 dispatch<br/>issue 行は priority バッジ（P2/P3）を併記<br/>一覧は状態→priority→更新時刻順に整列<br/>status・source・kind の表示は<br/>i18n 辞書で言語化（未知の値は原文）<br/>各行の body/rationale は<br/>data-tip ホバーでポップアップ表示]
    M2 -->|proposal カードクリック<br/>詳細（vet 検査結果・changes 本文・<br/>採否ボタン）を中央ペインに表示| C
    L -->|エディターナビ<br/>（AIチャットの直下）| FT[フォルダツリー<br/>右ペインに展開<br/>カレントrepo の path を根<br/>dir は lazy 展開<br/>GET /api/fs/tree<br/>.git/node_modules 等は除外<br/>秘匿系は 🔒 で選択不可]
    FT -->|file 選択<br/>GET /api/fs/file<br/>（登録 repo 配下・非秘匿のみ<br/>binary は本文を返さない）| C
    L -->|AIチャットナビ<br/>doc/ツリー表示を閉じて<br/>chat 表示へ戻す| C
    L -->|縦並びナビ<br/>AGENTS/SKILL| R
    L -->|マニュアルナビ<br/>中央ペインを iframe 表示| C
    C -->|GET /docs/* 静的配信| MAN[docs/ マニュアルサイト<br/>正本=diagrams/docs/<br/>（1ページ1.md・frontmatter title<br/>nav.json=章立て<br/>template.html/style.css=書式<br/>Canvas docs 参考）<br/>tools/sync-docs.mjs が生成]
    L -->|git ナビ（設定下の区切り線以下・ナビ末尾）<br/>コミット=メッセージ入力→<br/>POST /api/git/commit<br/>（git add -A + commit・変更なしは noop）<br/>プッシュ=確認後<br/>POST /api/git/push（人ゲートの承認）| GIT[カレント repo で git を spawn<br/>60s timeout<br/>出力は conlog 集約<br/>→ console タブへ SSE push]
    L -->|設定ナビ<br/>タブ: 接続AI / リポジトリ / システム<br/>（登録済み + IDE管理下検出 + ユーザー検出:<br/>ワークスペース/repoScanDirs 直下の .git 無し dir・<br/>ユーザーディレクトリ/repoScanDirs 直下の .git dir<br/>※GitHub 非依存のローカル repo も対象）<br/>リポジトリタブ内はサブタブでグループ化<br/>（slug の GitHub owner ごと /<br/>ユーザー / (owner なし)。選択は永続化・<br/>グループ説明は data-tip ポップアップ）<br/>GET /api/chat-agents<br/>GET /api/repos| C
    C -->|接続AI 追加/更新/解除<br/>active 切替<br/>POST /api/chat-agents/*| AC[config.json の<br/>chatAgents + chatAgent<br/>SSE push で即時反映]
    AC -->|新規 chat の実行体<br/>（chat 作成時に固定し<br/>chats.json へ永続化）| ACPX[接続AI 実行体<br/>acp=CLI spawn /<br/>agent=/agent HTTP /<br/>openai=chat/completions]
    ACPX -->|stderr / 非JSON stdout<br/>（acp 型の spawn のみ）| CON[conlog.rs<br/>実行体出力 + control 自身の<br/>stdio をリングバッファ集約]
    C -->|console タブ<br/>GET /api/console +<br/>SSE console push| CON
    C -->|処理モード select<br/>（入力行の先頭・添付ボタンの左<br/>目立たせるため強調表示）<br/>POST /api/chat/mode| EM[execMode<br/>（既定=parallel）<br/>direct=逐次 /<br/>parallel=kasugai-split で<br/>子chat自動起動・結果を親へ集約<br/>同時実行上限超過分は<br/>pendingSplits待機キュー・<br/>子完了で順次起動<br/>ボードの子chatカードは↳印で識別]
    C -->|repo設定 保存/登録解除<br/>POST /api/repos/update<br/>POST /api/repos/remove| RC[config.json の repos<br/>SSE push で即時反映]
    C -->|repo登録（設定ビューの<br/>path入力・参照ピッカー /<br/>IDE管理下の登録ボタン）<br/>POST /api/repos| RC
    C -->|path 参照<br/>GET /api/fs/dirs| FS[ローカルFSブラウザ<br/>サブdir + .git 検出<br/>loopback 限定]
    RC -->|選択肢へ追加し<br/>SSE即時反映| RS
    L -->|処理状況ナビ<br/>（起動時の初期表示）<br/>idle/exec/verify/resume/unresolved/live<br/>の順の状態別カンバン列<br/>（列幅は内容量で可変: 空列=半分幅 /<br/>2件以上=件数比例で広げ<br/>カードを多段グリッド表示）<br/>exec列は busy+waiting を統合<br/>カードに実行中/実行待機中ピル<br/>カードに ctx 消費量ピル（usage_update 由来。<br/>行は最小表示のため詳細は data-tip）<br/>verify列は実装ターン終了で自動遷移<br/>（touched あり → autoVerify で検証<br/>prompt 送信・検証中も列に留まる）<br/>idle列は proc無し・saved sid無しの chat<br/>（起動失敗時は prompt を保留し<br/>開始/処理再開で再送）<br/>完了は「人の操作が必要か」で分岐:<br/>live列=接続中の完了セッション全般<br/>（実装完了 doneKind=done と<br/>無印=実装ターンを経ずに終了したもの）<br/>unresolved列=要対応のみ<br/>（検証NG=ng / verdict未取得=unverified /<br/>差し戻し再送待ち・要人対応）を区分ピルで明示。<br/>検証NGの差し戻しは prompt を保留して<br/>同列で再送待ち（自動送信しない）とし、<br/>上限到達は「要人対応」ピルを付す。<br/>doneKind=done は終端: 新規ターンでも消さず<br/>live列に留まる。done の chat で変更が生じた<br/>ターンは派生課題として candidate intake へ<br/>起票（source=derived）し chat 側は done を維持| C
    C -->|statusカードクリック| PICK[該当 chat を選択表示<br/>選択時は最終行へスクロール<br/>（SSE 更新の追尾は末尾付近のみ）]
    C -->|検証列カードの検証ボタン<br/>POST /api/chat/verify| VR[検証ターン起動<br/>対象EXE起動+Playwright等の<br/>実機確認を指示<br/>SSE push で即時反映]
    C -->|verify=running/pending| VSH[chat 画面にも検証中/検証待ちを明示<br/>ヘッダ status＋メッセージ末尾の<br/>インジケータ＋送信ボタン<br/>SSE push で即時反映]
    C -->|doneKind 確定（検証フェーズ解消）| VEND[chat 末尾に終了区分をピル表示<br/>実装完了=done / 検証NG上限=ng /<br/>未検証=unverified<br/>（busy・verify待ちの表示より優先度は低い）<br/>SSE push で即時反映]
    C -->|各列カードの×ボタン<br/>（全カード・ホバー表示。<br/>右下にボタンがあるカードは右上）<br/>POST /api/chat/close| DEL[chat 削除<br/>SSE push で即時反映]
    C -->|chat画面ヘッダの削除ボタン<br/>（タイトル前・処理が終わった chat のみ表示。<br/>実行中・検証中・要対応では非表示。<br/>内容確認後にその場で削除）<br/>POST /api/chat/close| DEL
    C -->|完了列ヘッダの「すべて削除」<br/>POST /api/chat/close-completed<br/>カレントrepoの完了列を一括削除<br/>（live列=doneKind=done・無印が対象。<br/>要対応列は残る）| DEL
    C -->|要対応列カードの差し戻しボタン<br/>POST /api/chat/rework| RWK[保留の差し戻しpromptがあればそれを、<br/>無ければ終了区分に応じた再作業promptを送信<br/>doneKind/verify系・要人対応をリセットし<br/>exec列へ戻す<br/>SSE push で即時反映]
    C -->|再開可列カードの再開ボタン・<br/>未着手列カードの開始ボタン<br/>POST /api/chat/resume| RLOAD[session/load で再接続<br/>saved sid 無しは session/new で起動<br/>未送信の保留promptがあれば再送<br/>SSE push で即時反映]
    C -->|ボード上部の「処理再開」ボタン<br/>（停止中の件数を併記・0件は無効）<br/>POST /api/chat/resume-all| RALL[止まった処理を一括再開<br/>差し戻し待ち→保留の差し戻しprompt再送<br/>再開可→session/load再接続+<br/>中断作業の続行prompt送信<br/>未着手→session/newで起動+<br/>起動失敗で保留のprompt再送<br/>検証待ち→検証ターン起動<br/>ブロッカー消失の実行待機→<br/>解放して保留prompt再送<br/>（busy中・repo確認待ち・要人対応は触らない）<br/>SSE push で即時反映]
    R -->|doc一覧 GET /api/agents<br/>GET /api/skills<br/>SKILL は global scope を<br/>一覧から除外| REG[doc registry<br/>agents.rs /<br/>skills.rs]
    R -->|doc/参照ファイル選択<br/>GET /api/doc| C
    R -->|GET /api/refs<br/>AGENTS: 切替時に全doc分を<br/>マージして一覧化<br/>SKILL: doc選択時| RF[参照ファイル抽出<br/>registry/refs.rs<br/>秘匿・未作成は無効表示]
```

### SkillOps/AgentOps（スキル/エージェント管理）

スキルとエージェントは会話の話題ではなく、台帳・権限・検証・成績を持つ
管理対象として扱う。chat UI は管理命令の入口であり、変更は proposals と
人ゲートを経て反映する。

<!-- diagram: skillops-flow -->
```mermaid
---
title: SkillOps/AgentOps
---
flowchart LR
    U[管理 chat / 手動操作] --> RT[router<br/>入力→対象特定]
    SR[skill registry<br/>server/src/registry/skills.rs]
    AR[agent registry<br/>server/src/registry/agents.rs]
    PR[proposals.rs<br/>skills/prompts/config 差分生成]
    HG{人の採否}
    CONF[.devin/skills<br/>prompts/config]
    PIPE[pipeline.rs<br/>状態遷移]
    RUN[runner/worker]
    VER[verify]
    EV[evidence/]
    KN[knowledge/]
    RT -->|参照/検索| SR
    RT -->|権限/割当| AR
    RT -->|編集要求| PR
    SR --> PR
    AR --> PR
    PR --> HG
    HG -->|採用| CONF
    HG -->|却下| DROP[記録して破棄]
    CONF -->|reload| SR
    CONF -->|reload| AR
    AR --> PIPE
    PIPE --> RUN --> VER
    VER --> EV --> KN --> PR
```

<!-- diagram: skill-lifecycle -->
```mermaid
---
title: スキルライフサイクル
---
stateDiagram-v2
    [*] --> draft : scaffold/提案
    draft --> active : validate+test 合格
    draft --> rejected : 人が却下
    active --> proposal : 改善/失敗知見
    proposal --> active : 採用+verify
    proposal --> draft : 差し戻し
    active --> deprecated : 代替/失敗率悪化
    rejected --> [*]
    deprecated --> [*]
```

- `skill` は `id/path/version/status/triggers/permissions/verify/stats` を持つ
- `agent` は `role/skills/model/permissions/limits(verifyLevelMax)/stats` を持つ
- router は「どの skill/agent/verifyLevel に振るか」と理由を必ず説明する
- 状態変更は SSE push、表示項目は API 応答へ含める
- 現状の registry は文書スキャンの read-only index。台帳項目（権限・成績等）は
  段階的に追加する

## 8. 自動再起動（code drift）

<!-- diagram: restart-flow -->
```mermaid
---
title: 自動再起動（code drift）
---
flowchart TD
    WATCH[実行 exe の更新検知<br/>（cargo build 成果物の mtime）] --> EN{autoRestartOnDrift?}
    EN -->|off| END[何もしない]
    MAN[手動再起動<br/>設定ビューのボタン or<br/>POST /api/control/restart] --> BUSY
    EN -->|on| BUSY{実行中 run（busy）あり?<br/>実行待機（waiting）は含まない}
    BUSY -->|yes| FORCE{force 指定?}
    FORCE -->|no| DEFER[空くまで延期<br/>SSE restart pending push]
    FORCE -->|yes| STOP[runner 停止<br/>busy も含め全 proc を畳む<br/>待機状態・保留 prompt は<br/>chats.json へ persist]
    BUSY -->|no| STOP
    DEFER --> BUSY
    STOP --> RSP{respawner 起動}
    RSP -->|失敗| ABORT[再起動を中止<br/>exit せず SSE で失敗を通知]
    RSP -->|ok| START[control 再起動<br/>detached respawner が親終了を<br/>待って新プロセスを起動]
    START --> RSM{autoResumeOnStart?}
    RSM -->|yes| LOAD[resumable chat を session/load で<br/>自動再接続<br/>失敗時は session/new へフォールバック]
    LOAD --> RPROC[停止中の処理を自動再処理:<br/>差し戻し待ち→保留 prompt 再送 /<br/>検証待ち→検証ターン起動 /<br/>宙ぶら待機→解放して再送 /<br/>未着手の保留 prompt→起動して再送<br/>（再開可への続行 prompt・<br/>要人対応・repo 確認待ちは除く）]
    RSM -->|no| READY[idle 待機]
    RPROC --> READY
```

- 順序は必ず「実行系停止 → control 再起動」（孤児プロセスとロック競合を防ぐ）
- 実行中 exe の更新を監視して検知（code drift。Rust はコンパイル言語のため、
  ソース保存ではなく `cargo build` の成果物更新が契機）し、
  `autoRestartOnDrift`（既定 true）なら手動と同じ経路（busy 解消待ち →
  実行系停止 → 再起動）で自動再起動する。連続した更新は
  デバウンスで1回に吸収する
- 手動再起動は設定ビューの「システム再起動」ボタンまたは
  `POST /api/control/restart`。busy/waiting な chat が残る間は空くまで延期し、
  `force` 指定なら中断して即時再起動する。延期中は SSE `restart`（pending）を
  push する。実行系停止後は detached な respawner が旧プロセス終了を待ってから
  新 control を起動する（ポート競合回避）
- 自動再起動に限らず control 起動時は、`chats.json` から復元した resumable chat
  （savedSessionId 保持・ACP プロセスなし）を全て `session/load` で自動再接続する
  （`autoResumeOnStart`、既定 true）。load 失敗時は `session/new` へフォールバックし、
  認証不能等で繋がらない場合も chat は resumable のまま残る
- 同じく `autoResumeOnStart` 下で、停止中の処理は起動時に自動で再処理する:
  差し戻し待ちは保留 prompt を再送、検証待ちは検証ターン起動、
  宙ぶらの実行待機は解放して再送、未着手の保留 prompt は起動して再送
  （再開可への続行 prompt・要人対応・repo 確認待ちは対象外）

### 環境起動（docs ナビの起動ボタン）

<!-- diagram: env-launch -->
```mermaid
---
title: 環境起動（docs ナビの起動ボタン）
---
flowchart TD
    BTN[docs ナビの起動ボタン] --> UP{対象ポート応答?}
    UP -->|yes| OPEN[そのまま対象ポートを開く]
    UP -->|no| API[POST /api/control/launch]
    API --> APIOK{応答?}
    APIOK -->|yes| PS[detached spawn<br/>server/target/release/kasugai-agent.exe<br/>無ければ現行 exe・PORT=8586]
    APIOK -->|API 無し・失敗<br/>（control 全停止・旧版・静的配信）| PROTO[kasugai-agent:launch?port=..<br/>へ遷移（カスタムプロトコル）]
    PROTO --> PROTREG{protocol 登録済み?<br/>exe が起動時に HKCU 自己登録}
    PROTREG -->|yes| URI[exe --uri で起動<br/>同一 launch_env ロジックへ]
    PROTREG -->|未登録| WARN[起動不可をボタン上に表示<br/>exe または npm run dev が必要]
    URI --> PS
    PS --> WAIT[ブラウザ側で対象ポートの<br/>応答をポーリング]
    WAIT -->|応答 or タイムアウト| OPEN

    subgraph COLD[冷起動の入口（control 全停止時）]
        TS[ログオン自動起動<br/>HKCU Run キー KasugaiAgentDispatcher<br/>ログオンで prod 常駐<br/>--install-task / パネル設定で登録]
        MANUAL[exe 直起動 / npm run dev<br/>（tools/dev.mjs: 対象ポートの占有を強制停止→cargo run。<br/>通常版・開発版とも :8586。検証の並走は PORT= で適宜指定）]
        PROTO2[kasugai-agent:// プロトコル<br/>（本図のフォールバック経路）]
    end
```

- docs ナビの「環境を起動」ボタンは
  `POST /api/control/launch` を呼び、対象ポートの応答を待ってからそのポートを
  新規タブで開く。**起動は稼働中の control が行う**ため、control が落ちている
  状態では `kasugai-agent:launch?port=..` プロトコルへフォールバックし、
  OS に登録済みのハンドラ（exe が起動時に `HKCU\Software\Classes\kasugai-agent`
  へ自己登録）が `kasugai-agent.exe --uri` で同一ロジックを起動する。
  プロトコル未登録（exe 未起動の環境）では起動できずボタン上に警告を表示する。
  冷起動の入口はログオン自動起動（Run キー）・exe 直起動・`npm run dev`・
  プロトコル経由の4系統

## 9. セキュリティ

- API キー等は `secrets.json` のみ。`config.json`・ログ・プロンプト注入へ書かない
- external 起票は評価前に必ず人の目視（prompt injection 対策）
- control は loopback のみ bind。外部公開しない

## 10. ディレクトリ構成（設計）

```
kasugai_agent/
  README.md            # 統一モデル正本（本ファイル）
  AGENTS.md            # 開発規約
  config.example.json
  server/              # Rust 実装（cargo ワークスペース。kasugai-agent.exe）
    src/
      main.rs          # エントリ・状態初期化・drift 監視・自動再接続
      config.rs        # config.json/secrets/chats 等の汎用 JSON 永続化
      control.rs       # HTTP/SSE ハブ・状態の正・スケジューラ
      conlog.rs        # 実行体/control のコンソール出力リングバッファ（console タブ用）
      claims.rs        # ファイルクレーム・実装待機の競合判定
      snapshots.rs     # chat 変更ファイルの turn 単位スナップショット・復元
      repomatch.rs     # prompt とカレントリポジトリの不整合検出
      split.rs         # parallel モードの課題分割（kasugai-split 抽出）
      chat/            # runner 抽象 = 接続AI 実行体
        mod.rs         #   Chat 共通面（Core 表示状態・send/cancel/ensure_session）
        acp.rs         #   ACP 対応 CLI spawn（devin CLI 等）・session/* JSON-RPC
        http.rs        #   /agent HTTP・OpenAI 互換の単発実行体
      pipeline.rs      # 課題ライフサイクル state machine（intake=dedupe・遷移の唯一の正）
      store.rs         # 課題系 JSON 永続化（issues/candidates/work-queue）・issue key/symptomHash
      runner.rs        # 課題 run の実行体（dispatch→run→verify→push・retry 差し戻し。drain は reconcile/再開のみ）
      verify.rs        # L0-L4 検証 prompt・kasugai-verify 結果判定・差し戻し
      housekeep.rs     # drain 定期 retention 掃除（housekeep.mmd・§11）
      heartbeat.rs     # デッドマンスイッチ heartbeat（外部 ping・heartbeat.mmd・§11）
      detector_health.rs # detector 沈黙検知 meta 監視台帳（detector-health.mmd・§11）
      knowledge.rs     # 知見蓄積 → proposals
      proposals.rs     # proposals 格納・vet（機械検査・採否の前提）
      github.rs        # gh CLI ラッパ
      launch.rs        # 環境起動・自動起動 Run キー・kasugai-agent: プロトコル
      assets.rs        # CDN アセット取得・sha256 検証・assets/<ch>/ 適用
      locks.rs         # リソース排他ロック（locks/<name>.lock。verify L2+ の実機排他）
      util.rs          # パス正規化・時刻等の横断ユーティリティ
      detectors/
        mod.rs         #   ingest 入口（candidates.json dedupe → pipeline::intake）
        github_issues.rs
        ci_failure.rs
        local_health.rs
        spec_drift.rs   #   図・正本ドキュメントと実装の乖離検出（sync --check・§10 対応）
      registry/
        agents.rs      # agent定義・ルール文書スキャン（read-only）
        skills.rs      # skills/*/SKILL.md スキャン（read-only）
        refs.rs        # doc参照ファイルの抽出・実ファイル解決（read-only）
  src/                 # 旧版 *.mjs 資産（移植元。実装正本は server/、新規変更しない）
  panel/
    index.html
  prompts/             # assess/dispatch/verify 等のプロンプトテンプレ
  diagrams/            # mermaid 図の正本（*.mmd、1図1ファイル）
    docs/              # マニュアル書式・正本（*.md + nav.json + template.html + style.css）
  docs/                # マニュアルサイト（diagrams/docs/ から生成。control が /docs/ で配信）
  tools/
    sync-diagrams.mjs  # diagrams/ → README/diagrams・docs 埋め込み同期（--check で検証）
    sync-docs.mjs      # diagrams/docs/ → docs/*.html 生成（--check で検証）
  test/
```

旧版の課題だった単一巨大ファイル（control 148KB / dispatcher 86KB）を避け、
**1ファイル1責務・状態遷移は pipeline のみ** を徹底する。

## 11. 運用

- ログオン時自動起動: HKCU Run キー `KasugaiAgentDispatcher`（ログオンで現行
  exe を起動）。タスクスケジューラは非管理者登録不可のため Run キーを使う。
  登録/解除は `kasugai-agent --install-task` / `--uninstall-task`、または
  設定ビュー「システム」タブの自動起動カード
  （`GET/POST /api/control/autostart`）から行う
- 監視対象は `config.json` の `repos`。初期は本リポジトリ自身のみ（自己監視で
  ループを育ててから他リポジトリへ拡大）。パネルの設定ビュー
  （左ペインナビ「設定」→ 中央ペイン）からも登録できる（`POST /api/repos`）
- 設定ビュー「システム」タブのカードに「システム再起動 / 強制再起動」ボタンを置く
  （`POST /api/control/restart`、強制は `{force:true}`。延期中・再起動中は
  画面最上部にバナー表示し、SSE 再接続で自動復帰する）
- **retention**: `config.retentionDays`（既定 0=掃除しない）を超えて古い
  `evidence/<key-slug>`（終端課題=done/failed/rejected/dropped のもののみ）と
  `logs/*.log` を drain 定期 tick で削除する。進行中課題の証跡は保持する
- **loop KPI**: 右ペイン loop state カードに、選択リポジトリの課題を集計した
  status 別件数・done 率・MTTR（検出→done の平均）を併記する
- **dead man's switch heartbeat**（heartbeat.mmd）: agent 自身の死を外部へ
  通知する経路（loopback 完結のため監視対象パイプラインでは自己破壊を
  報告できない）。稼働中は `secrets.json` の `heartbeatUrl` へ
  `config.heartbeatMinutes`（既定5・<=0 で無効）間隔で単純 GET ping を
  送る。送信先未設定時は無効のまま（外部サービス登録は人が行う）。
  ping 途絶 = agent 全停止として外部サービス側が通知する。
  runner/verify/drain とは別経路の独立タスクで、最終 ping の成否は
  `/api/state` の `heartbeat` と loop state カードに出る

<!-- diagram: heartbeat -->
```mermaid
---
title: デッドマンスイッチ heartbeat（agent 自身の死の外部通知）
---
flowchart LR
    HB[heartbeat タスク<br/>server/src/heartbeat.rs<br/>control 起動時に spawn。<br/>runner/verify/drain の<br/>監視対象パイプラインとは別経路] --> CFG{secrets.heartbeatUrl 設定済<br/>かつ heartbeatMinutes>0?}
    CFG -->|no| IDLE[無効のまま待機<br/>60s ごとに secrets/config を再読込<br/>外部サービス登録は人が行う]
    IDLE -.-> HB
    CFG -->|yes| PING[GET ping<br/>timeout 15s<br/>成否は heartbeat status に記録]
    PING --> SLEEP[heartbeatMinutes 経過で次回 ping<br/>ping 成否の変化時のみ SSE push]
    SLEEP -.-> HB
    PING --> EXT[外部 heartbeat サービス<br/>healthchecks.io /<br/>Dead Man's Snitch 互換の単純 GET]
    EXT -->|ping 途絶 = agent 全停止| ALERT[外部サービス側で通知<br/>loopback 完結のパイプラインを<br/>通らない唯一の外部経路]
```

- **detector 沈黙検知**（detector-health.mmd）: 「検出0件」自体が異常に
  なり得るための meta 監視。detector ごとの最終実行時刻（lastRunAt）と
  最終 emit 時刻（lastEmitAt）を `detector-health.json` 台帳に記録し、
  `detectorSilentMinutes`（既定30・<=0 で無効）を超えて emit のない
  detector を `source=local-health` の meta candidate（kind=detector-silent /
  実行停止は detector-stopped）として emit する。lastRunAt≠lastEmitAt の
  区別で「検出0件・正常」と「detector 停止・異常」を分け、安定
  symptomHash で detector ごと1課題に収束する。状態は `/api/state` の
  `detectorHealth` と loop state カードに出る

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

<!-- diagram: housekeep -->
```mermaid
---
title: housekeeping（drain の定期整備）
---
flowchart LR
    TICK[drain 定期 tick] --> REC[起票 reconcile<br/>candidate → intake<br/>deferred 分も窓が開けば起票]
    TICK --> ORPH[中断 verify の再開・<br/>孤児 running の failed 化]
    TICK --> RET[retention<br/>config.retentionDays<br/>0=掃除しない]
    RET -->|期限切れ| EV[evidence/key-slug<br/>終端課題のみ<br/>done/failed/rejected/dropped]
    RET -->|期限切れ| LG[logs/*.log]
    EV --> KEEP[進行中課題の証跡は<br/>保持したまま]
```

## 12. ロードマップ

1. **M0 設計確定** — 本 README（いまここ）
2. **M1 最小ループ** — detect(github-issues) → assess(ルールのみ) → dispatch(devin -p)
   → verify(L0/L1) → ラベル/コメント報告。パネルは課題一覧+操作のみ
3. **M2 検証強化** — L2/L3・証跡・リソースロック・帰属 revert
4. **M3 学習還流** — knowledge/proposals・人ゲート・自己監視拡大
5. **M4 パネル完成** — チャット・ボード・設定・SSE 進行表示

## 13. 配布とアセット更新（CDN）

- 配布の基本は **zip 一式**（exe + `config.example.json` + `panel/` + `docs/` +
  `prompts/`）。exe 隣接に `config.json` があるとそこを root とみなす
  （`server/src/main.rs` の配布版判定）。exe 単体では動かない
- 上乗せとして **CDN からのアセット更新**を持つ。CDN はランタイムに直接
  配信させず「取得 → sha256 検証 → `assets/<ch>/` へ配置 → そこから
  loopback でローカル配信」。panel の JS は制御 API 全権を持つ信頼コード
  なので、検証なしの外部配信はしない
- `assets/<ch>/` が存在すれば `serve_static` が `panel/`・`docs/` より優先して
  配信する（オーバーレイ）。更新で repo の `panel/`・`docs/` は汚れない
- `GET /api/assets`（状態）/ `POST /api/assets/check`（確認+自動適用）/
  `POST /api/assets/apply`（手動適用）/ `POST /api/assets/rollback`（1世代戻し）
- manifest は `{minExeVersion, channels:{<ch>:{version, files:{<rel>:<sha256>}}}}`。
  exe 版が `minExeVersion` 未満なら適用せず通知のみ（版スキュー防止）
- `prompts` など行動系チャネルは `assetAutoApply.<ch>` 既定 off で
  **proposal 起票→人の採否**。採用時に proposal 適用が root 配下へ書込む
- config: `cdnBaseUrl`（空=無効）/ `cdnCheckMinutes`（0=手動のみ）/
  `cdnManifestSha256`（manifest ピン・任意）/ `assetAutoApply`

<!-- diagram: asset-update -->
```mermaid
---
title: アセット更新（CDN 配布）
---
flowchart TD
    TRIG[更新確認<br/>起動時 / 定期 cdnCheckMinutes /<br/>POST /api/assets/check] --> EN{cdnBaseUrl 設定?}
    EN -->|未設定| SKIP[何もしない<br/>（ZIP・ローカル配布のみ）]
    EN -->|設定済| MF[manifest.json 取得<br/>channels / version / minExeVersion /<br/>files + sha256]
    MF -->|取得失敗| KEEP[現行継続<br/>オフライン耐性・lastError を SSE push]
    MF --> PIN{cdnManifestSha256<br/>ピン指定あり?}
    PIN -->|あり・不一致| KEEP
    PIN -->|なし or 一致| CMP{exe version >=<br/>minExeVersion?}
    CMP -->|no| NEED[exe 更新が必要<br/>SSE assets で通知のみ・適用しない]
    CMP -->|yes| DIFF{チャネルごと<br/>remote != installed?}
    DIFF -->|同一| DONE[最新・何もしない]
    DIFF -->|差分あり| GATE{assetAutoApply.&#60;ch&#62;?}
    GATE -->|on<br/>panel/docs 既定| DL[.staging/&#60;ch&#62;/ へ全ファイル DL<br/>各 sha256 を検証]
    DL -->|1件でも不一致| DISCARD[staging 破棄・現行維持]
    DL -->|全件一致| SWAP[assets/&#60;ch&#62;/ を差替<br/>現行は &#60;ch&#62;.bak/ へ退避]
    SWAP --> SSE[installed 版を assets/state.json へ記録<br/>SSE assets push<br/>都度ディスク配信なので再起動不要]
    GATE -->|off<br/>prompts 既定=人ゲート| PROP[proposal 起票<br/>kind=prompt・changes=prompts/*<br/>同 version の起票済なら重複しない]
    PROP --> HUMAN{人の採否}
    HUMAN -->|採用| APPLY[proposal 適用<br/>root 配下へ書込み]
    HUMAN -->|却下| DROP[記録して破棄]
    SSE -.->|rollback API| RB[&#60;ch&#62;.bak/ を復元<br/>version/prevVersion を入替<br/>巻き戻した版を held＝自動適用抑制<br/>（より新しい版が来れば解除）]
```
