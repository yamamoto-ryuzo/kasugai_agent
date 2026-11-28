---
title: 3-1 統一モデル図
---

本ページの mermaid 図は `diagrams/*.mmd`（1図1ファイル・正本）を
`tools/sync-diagrams.mjs` が埋め込んだ写しです。図を直すときは `.mmd`
だけを編集し、sync を実行します（`--check` で乖離を検出）。

## 課題ライフサイクル

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
    assessed --> gated : 人ゲート待ち (external・<br/>大規模=分割提案 等)
    assessed --> queued : 自律対象（対応待ち）
    gated --> queued : 人が承認
    gated --> rejected : 人が却下 or 分割起票<br/>（splitInto に子 key 群）
    queued --> running : 一覧の対応/一括対応ボタンで dispatch<br/>（自動起動しない。一括対応承認済みの<br/>autoDispatch 待機は drain が repo 解放時に dispatch。<br/>autoDispatchWhitelist 事前承認の低リスク kind も<br/>intake で autoDispatch フラグを立て同じ drain 経路で<br/>起動。issueExecMode=direct は runner パイプラインで<br/>直接実行（同一 repo 直列・maxConcurrentRuns<br/>クォーラム有効）、parallel=既定 は chat を<br/>起票して並列処理）
    queued --> planning : dispatch（planGate 課題は<br/>先に計画生成）
    queued --> rejected : 人が却下
    planning --> plan_review : 計画+critic 合格<br/>（autoApprovePlan=off）
    planning --> running : 計画+critic 合格<br/>（autoApprovePlan=on）
    planning --> queued : 計画生成失敗・<br/>critic で Severe 検出<br/>（計画差し戻し）
    plan_review --> running : 人が計画承認<br/>or タイムアウト自動承認
    plan_review --> queued : 計画差し戻し
    plan_review --> rejected : 人が却下
    running --> verifying : 実装完了
    running --> done : 課題処理方法=並列（chat 起票）時、<br/>委譲先 chat の終了区分=done<br/>（chat 側の検証を内包するため<br/>verifying を経由しない）
    running --> queued : cancel・run halt<br/>（stuck・予算・HIGHリスク。<br/>verifyBacks 不消費・run-control.mmd）<br/>並列モードの chat 終了区分<br/>unverified も queued 差し戻し
    verifying --> queued : cancel（再 trigger で<br/>旧 run を中断し queued へ）
    verifying --> done : 検証合格
    verifying --> retry : NG (差し戻し)
    retry --> queued : 差し戻し（対応ボタンで<br/>再 dispatch）
    retry --> rollback : 全対処手段失敗<br/>（remediations 昇格の末）
    rollback --> failed : 帰属コミットのみ git revert で除去し<br/>base を健全状態へ（要人対応）
    retry --> failed : 直接の失敗確定（要人対応）
    running --> failed : 異常終了
    failed --> queued : 再実行
    failed --> rejected : 人が却下
    note right of queued
        candidates 一覧は判断待ち（未起票・gated・
        queued・failed）のみ表示し、各行の
        対応/却下ボタンで人が一括判断する。
        対応は起票/承認/再実行を兼ねた即時 dispatch
        （未起票は rate-limit を人判断で bypass）。
        ISSUES 一覧の「一括対応」は queued を一括 dispatch し、
        同一 repo 競合・同時実行上限で起動できない課題は
        autoDispatch フラグ付きで待機する。drain が repo 解放時に
        同じ dispatch 経路で自動起動するが、フラグは起動時に消費する
        （verify 差し戻し後は再び人判断を要する）。
        candidates 一覧の「一括対応」も同じ経路で判断待ちの
        全候補（未起票は即時起票）を一括 dispatch し、
        起動できない課題は同様に autoDispatch で待機する。
        autoDispatchWhitelist（config・既定無し=全課題人判断）:
        [{kind, maxPriority}] のオプトイン式事前承認。
        intake で kind 等値（"*"=任意）かつ priority<=maxPriority
        （既定1）かつ queued 自律対象の課題に autoDispatch
        フラグを立て、drain が一括対応と同じ経路で起動する
        （フラグは起動時に消費、verify 差し戻し後は人判断）。
        gated（external・大規模）は対象外、whitelist 未命中は
        従来通り人ゲート必須。
    end note
    note right of running
        課題の処理方法 issueExecMode（設定システムタブで
        直接/並列を切替・既定=並列）:
        直接 direct = 従来の runner パイプラインで直接実行
        （can_dispatch 完全ガード: 同一 repo 直列・
        maxConcurrentRuns・planGate・stuck/予算制御。
        一括の競合分は autoDispatch 待機）。
        並列 parallel（既定）= 課題を chat セッションとして
        起票し、通常のチャット起票と同じ流れで並列処理する
        （can_dispatch_base のみ: 同一 repo 直列・
        maxConcurrentRuns は課さない）。
        chat は課題 key を保持し、課題は chatId を保持する
        （課題行に chat #N リンク）。
        chat 終了区分の写像: done→課題 done（GitHub 課題は
        完了理由のコメント投稿 → issue クローズ）/
        unverified→課題 queued /
        ng・差し戻し上限→課題 running のまま（chat の
        差し戻し・要人対応で進め、課題側は cancel で queued）。
        cancel は chat のターン中断＋課題 queued、
        再 dispatch は新規 chat を起票する（旧委譲 chat の
        issue_key リンクは解除し、後続完了の誤写像を防ぐ）。
        chatId 付き running は drain の孤児 run
        failed 化の対象外（housekeep.mmd）
    end note
    note right of rejected
        分割起票（manual-0577d650）: assess が閾値超
        （splitMaxBytes/splitMaxItems）の大規模課題を
        splitProposal 付きで gated へ。人の「分割起票」で
        子課題群（source=split・splitFrom=親key）を起票し、
        親は rejected へ。子は通常 intake で queued へ進む
    end note
    done --> [*] : drain の sweep で課題レコード削除<br/>（完了行はこの枠に残さない。削除前に<br/>case/eval 学習へ同期。postPush 監視中は<br/>監視終了まで残存。GitHub 由来は<br/>理由コメント投稿+issue クローズ後に削除）
    rejected --> [*]
    dropped --> [*]
    [*] --> done : GitHub closed Issue 取込<br/>（--import-github・COMPLETED）
    [*] --> rejected : GitHub closed Issue 取込<br/>（--import-github・NOT_PLANNED）
```

## 全体構成

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
        D5[session-watch<br/>Devin セッション観測]
        D6[unattributed-work<br/>帰属不明 git 変更]
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
    D1 & D2 & D3 & D4 & D5 & D6 --> N --> A --> G --> R --> V --> K
    R --> DV
    A & R & V --> GH
    R & V --> RP
    K -->|proposals| G
```

## detect → normalize → dedupe

<!-- diagram: detect-pipeline -->
```mermaid
---
title: detect → normalize → dedupe
---
flowchart TD
    DET[detector / manual API /<br/>派生（done chat の<br/>変更ターン source=derived）<br/>github-issues は open issue 全件<br/>（ラベル選別なし）<br/>closed は --import-github で<br/>終端取込] -->|candidate<br/>source/repo/title/body/<br/>symptomHash/detectedAt<br/>（任意: kind/labels/<br/>count/durationSeconds/<br/>parentIssue）| NORM[正規化]
    NORM --> HASH[issue key =<br/>source+repo+symptomHash]
    HASH --> EXIST{既出?}
    EXIST -->|yes| BUMP[既存課題の<br/>lastSeen 更新のみ]
    EXIST -->|no| PAR{親課題リンク?<br/>candidate.parentIssue<br/>（ci-failure が失敗コミットの<br/>Issue:トレーラを逆引き。<br/>force 起票は bypass）}
    PAR -->|帰属解決| LINK[新規課題は作らず<br/>元課題の followUps へ追記<br/>（ref=候補key で重複抑止）<br/>failed の元課題は queued へ<br/>差し戻し＝再実行要求<br/>candidate は linkedIssue 表示]
    LINK --> INBOX
    PAR -->|帰属不明| SIL{silence?<br/>silences.json の<br/>matcher（source/repo/<br/>kind/severity）+<br/>startsAt〜endsAt 窓内<br/>（設定 UI / API で作成）}
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
    INBOX[候補一覧 = 判断待ち inbox<br/>未起票・gated・queued・failed のみ表示<br/>（実行中・終端の課題行は出さない）<br/>全行 対応/却下ボタンで人が一括判断:<br/>対応=起票/承認/再実行を兼ねた即時 dispatch<br/>（未起票は rate-limit を人判断で bypass）<br/>却下=rejected（未起票は起票して即 rejected）<br/>「一括対応」ボタンは表示中の全候補を順次 dispatch<br/>（起動不可の課題は autoDispatch で待機。<br/>issueExecMode=direct は同一 repo 直列、<br/>parallel 既定は chat 起票で並列処理）]
```

## run 制御・安全（監視・予算・steering・リスク・cancel・保全・ループcap）

<!-- diagram: run-control -->
```mermaid
---
title: run 制御・安全（監視・予算・steering・リスク・cancel・保全・ループcap）
---
flowchart TD
    RUN[run 実行中<br/>runner.rs が child proc の<br/>stdout/stderr を行単位で監視<br/>stdin は steering 用に開放] --> MON{出力行ごとに判定}
    MON --> STK{stuck.rs<br/>同一行4回 / 同一 error 行3回 /<br/>同一行3連続（独白相当） /<br/>2パターン交互6行 /<br/>context window error 連続<br/>stuckDetect=off で無効}
    MON --> BUD{実行予算 issue.budget<br/>maxSteps（出力行）/<br/>maxWallMinutes（壁時計）/<br/>maxCostUsd（累積費用）<br/>assess が課題ごとに設定}
    MON --> RSK{risk.rs<br/>行 → action リスク分類<br/>（外部通信・削除・設定変更等<br/>HIGH のみ人確認）}
    STK -->|検出| HALT
    BUD -->|上限到達<br/>LimitsExceeded| HALT
    RSK -->|HIGH 検出<br/>riskConfirm=on| HALT
    HALT[run halt<br/>ctl.cancel → child kill<br/>構造化理由を lastStuck /<br/>budgetHold / riskHold に記録<br/>trajectory 終了理由=stuck/limit/<br/>halt] --> PSV[部分成果保全<br/>manual-7df2ce2f<br/>未コミット＋未push 作業を<br/>kasugai/partial-* branch へ push<br/>lastBlocker に詰まり内容を記録]
    PSV --> BACK[running → queued<br/>run 制御差し戻し<br/>verifyBacks は消費しない]
    BACK --> HMN{人の判断<br/>issues 一覧・詳細のボタン}
    HMN -->|対応| DISP[再 dispatch<br/>stuck / lastBlocker /<br/>steer / riskRejected を<br/>prompt へ注入]
    HMN -->|extend| EXT[POST extend<br/>cycleCapExtend・budgetExtend<br/>引上げ → 即 dispatch]
    HMN -->|risk-ack allow| ACK[riskHold 解除<br/>riskAllowed 記録 → dispatch]
    HMN -->|risk-ack deny| DENY[riskRejected 記録<br/>queued で待機・次回 prompt に<br/>却下理由を注入し別アプローチへ]
    STR[POST steer text<br/>run 中の方針修正入力] -->|in-flight run| STDIN[child stdin へ注入<br/>best-effort]
    STR -->|常時| SQ[followUps kind=steer 記録<br/>次回 dispatch prompt へ注入]
    RTR{同一 issue key の<br/>再 trigger<br/>dispatch / cancel} -->|in-flight run あり| CXL[cancel_run<br/>RunCtl.cancel → child kill<br/>running・verifying → queued]
    RTR -->|chatId 付き課題<br/>（並列モード）| CXLC[委譲 chat を cancel<br/>課題 → queued・双方向リンク解除]
    CXL -->|課題 key の run は常に1つ| DISP
    CXLC --> DISP
    DISP --> CAP{runCycleCap<br/>既定3・0=無制限<br/>runs 累計で判定<br/>manual-175e7229}
    CAP -->|超過| ESC[can_dispatch Err<br/>人へエスカレーション<br/>extend で上限引上げ]
    CAP -->|以内| PAR{issueExecMode<br/>既定 parallel}
    PAR -->|direct 直列:<br/>同一 repo の active 課題あり| W8[can_dispatch Err<br/>blocked by。<br/>一括対応では autoDispatch<br/>待機 → drain が repo 解放時に起動]
    PAR -->|direct 直列:<br/>同一 repo 排他なし| GO[running へ<br/>runner run 起動]
    PAR -->|parallel 並列:<br/>can_dispatch_base のみ| CHAT[chat セッション起票で委譲<br/>課題 chatId・chat issueKey<br/>chat 終了区分を課題へ写像<br/>done→done（GitHub 由来は理由コメント+<br/>issue クローズ）<br/>※ done 課題は drain sweep が<br/>case/eval 同期後にレコード削除<br/>（postPush 監視中は残存）<br/>unverified→queued<br/>ng・要人対応は running 継続]
    W8 -.->|maxConcurrentRuns<br/>クォーラムは direct で有効| GO
    EXT --> CAP
    ACK --> CAP
    LOOP[verify NG の review↔fix<br/>自動反復] -->|allowAutoLoop<br/>既定 OFF| DRN[drain が verify NG 差戻しの<br/>queued を自動 dispatch<br/>runCycleCap・retryAfter で上限]
    DRN --> CAP
```

## verify（L0〜L4 / push 統合）

<!-- diagram: verify-levels -->
```mermaid
---
title: verify（L0〜L4）
---
flowchart TD
    REPRO[L2+ は再現ファースト<br/>修正前に最小再現手順 repro.md<br/>症状の再現・ベースラインを記録] --> IMPL[実装完了<br/>ローカルコミット]
    IMPL --> VL{verifyLevel}
    VL --> VENV[verify-env.toml<br/>repo 別の宣言的 verify 環境<br/>env・setup・checks・probe]
    VENV -->|L0/L1| STATIC[静的・単体チェック<br/>構文・lint・テスト・ビルド<br/>checks 宣言で自動検出を上書き]
    VENV -->|L2以上| LAUNCH[実機で対象EXE自体を起動<br/>probe or verifyExe・成果物バイナリ]
    LAUNCH --> DRIVE[実機駆動<br/>Playwright等でUI操作・応答確認]
    STATIC --> EVID[証跡採取<br/>shots/logs/応答・L2+ は repro.md<br/>→ evidence/]
    DRIVE --> EVID
    EVID --> JUDGE{合格?<br/>実装の確認}
    JUDGE -->|OK| ATTRIB[コミット帰属確認<br/>Issue: #N]
    ATTRIB --> CLEAN{帰属不明/<br/>rejected 混入?}
    CLEAN -->|yes| REVERT[git revert で除去]
    CLEAN -->|no| DREV{独立 diff レビュー<br/>severity+confidence 付き findings}
    REVERT --> DREV
    DREV -->|Severe 無し| PUSH[git push]
    DREV -->|Severe 検出| RETRY
    PUSH --> DONE[status=done]
    JUDGE -->|NG| SEP[課題を分離<br/>実装済みNG課題 × 未実装課題<br/>kasugai-verify verdict]
    SEP --> BACK[差し戻し prompt を組み立て保留<br/>実装済み→修正 / 未実装→実装<br/>（自動送信はしない）]
    BACK --> RETRY[status=retry<br/>要対応列で再送待ち<br/>段階的修復: remediations を順に試行<br/>（retry → replan → human）]
    RETRY -->|処理再開ボタン /<br/>個別の差し戻しボタン /<br/>再起動時の自動再処理| IMPL
    RETRY -->|手段の backs/timeout<br/>で昇格| RETRY
    RETRY -->|全手段失敗| FAIL[status=failed<br/>人ゲートへ差し戻し + sys 報告]
```

## learn（知見の還流）

<!-- diagram: learn-flow -->
```mermaid
---
title: learn（知見の還流）
---
flowchart LR
    RUN[run 結果・証跡] --> KN[生の知見を蓄積<br/>knowledge/<br/>課題key単位・無条件]
    KN --> SK[SKILL draft 組立<br/><skillDir>/<slug>/SKILL.md<br/>既定 .agents/skills（skillDirs）<br/>slug = source+repo+kind<br/>揮発 id を含まない安定名<br/>mermaid 図必須・図 title = slug]
    SK --> DUP{同 slug の起票を抑止？<br/>・proposal 既存（pending+decided/<br/>  同 path または同一内容）<br/>・knowledge/skill-drafts.json<br/>  起票済み台帳に slug あり<br/>・適用済み SKILL.md が既存}
    DUP -->|なし| PROP[proposals 生成<br/>kind=skill・issueKey=課題key<br/>台帳に slug を記録]
    DUP -->|あり| SKIP[起票しない<br/>同じ種別の再検出は抑止]
    PROP --> VET[vet<br/>機械検査<br/>path許可 / secret混入 /<br/>構文 / frontmatter]
    VET -->|fail| VF[vet 不合格<br/>accept 不能・要修正]
    VET -->|pass| HUMAN{人の採否}
    HUMAN -->|採用| SELF[agent 自身へ反映<br/>or 正本へ課題起票]
    HUMAN -->|却下| DROP[破棄]
    SELF --> REC[proposals/decided/ へ記録<br/>一覧は pending のみ]
    DROP --> REC
```

## chat 送受信シーケンス

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
        C->>C: 競合判定: prompt 文中のパス ×<br/>他 busy chat のファイルクレーム<br/>（conflictWait=false の並列モードでは<br/>判定自体を skip）
        alt 同一ファイル競合あり
            C->>C: waiting（実装待機）へ<br/>prompt は保留キューへ
            C-->>UI: SSE push（wait 表示）
        else 競合なし
            C->>C: 同時起動セッション枠チェック<br/>（maxConcurrentSessions・既定3・0=無制限。<br/>proc 保持 chat 数が上限未満なら通過。<br/>満杯なら最も古い idle proc を suspend——<br/>savedSessionId 保持で次回送信時に自動再開——<br/>して枠を空ける。busy または起動中<br/>（session 未確立）の proc は suspend 不可。<br/>全枠 busy なら waiting kind=capacity へ。<br/>blocker busy 解除で解放。<br/>idle suspend は送信経路のみ——resume/開始のみの<br/>再開要求は枠が無ければ spawn せず<br/>savedSessionId のまま残し、次回送信で lazy 再開。<br/>「resume が前の idle proc を逐次追い出す<br/>spawn→kill コンベア」を防ぐ）
            C->>A: session/new or session/load → session/prompt
            Note over C,A: execMode=parallel の初回 prompt には<br/>分割指示プリアンブルを前置<br/>（セッション内で指示が残るため1回のみ）
            A-->>C: session/update stream (message/tool_call/plan/usage_update)
            Note over C,A: 編集系 tool_call の対象パスは touched として記録し、<br/>turn 単位で変更前スナップショットを<br/>chat-snapshots/ へ採取。<br/>他 busy chat のクレームと重なったら<br/>session/cancel して waiting へ<br/>（並列モードでは抑止しない）
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
    UI->>C: POST /api/chat/parallel {on}
    Note over C: on=true で conflictWait=false を config 永続化<br/>（並列モード）＋全ての実装待機を blocker 未完了<br/>でも解放し保留/続行 prompt を即時送信<br/>（repo 確認待ちは人ゲートのため対象外）。<br/>on=false で直列ガードへ復帰（実行中は継続）
    Note over C,A: acp spawn 時は WINDSURF_EXT_HOST_PID を<br/>env から除去（devin は実親でなくこの変数の pid を<br/>親監視するため、死んだホストの値が残ると<br/>実行体が起動直後に自死する。他ホストでも無害な防御）
    Note over C,A: spawn した実行体の stderr・非JSON stdout と<br/>control 自身の stdio は conlog.rs の<br/>リングバッファへ集約 → コンソールタブへ SSE push
    UI->>C: GET /api/chat?id=N
    C-->>UI: messages/busy/waiting/options/snapPoints スナップショット
    UI->>C: POST /api/chat/resume {id}
    C->>A: session/load（resumable chat の手動再接続）
    UI->>C: POST /api/chat/start-suspended
    C->>A: resumable 全件に session/load（prompt 無しの起動のみ。<br/>同時起動枠は idle 追い出しなしで確保、<br/>枠超過分はスリープのまま残す）
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

## パネルレイアウト

<!-- diagram: panel-layout -->
```mermaid
---
title: パネルレイアウト
---
flowchart LR
    L[左ペイン<br/>title + カレントrepo +<br/>縦並びナビ<br/>（セッション/メトリクス/AIチャット/エディター/<br/>AGENTS/SKILL/マニュアル/開発環境/設定の順。<br/>初期表示はセッション）] --> C[中央ペイン<br/>chat messages（chat/console タブ）/<br/>doc 本文（Viewer 単一オブジェクトに集約<br/>kind判定・原文/プレビュー切替・mermaid描画<br/>MD+mermaid・HTML・MMD 表示・MMD は縦横切替可<br/>mermaid 図は＋−ボタン/wheel で拡縮<br/>（カーソル位置を維持）<br/>中央配置・ドラッグで視野移動・ズーム率表示）]
    C --> R[右ペイン<br/>管理セクション / doc browser + 参照ファイル /<br/>フォルダツリー（エディター）<br/>（エディター/AGENTS/SKILL 選択時は doc browser。<br/>表示するものがない時は管理セクションを<br/>表示し、ペイン自体は常時表示。<br/>省略ラベルは data-tip<br/>ホバーポップアップで全文表示）]
    G[境界グリップ<br/>ドラッグで幅可変<br/>localStorage 永続化] --> L
    G --> R
    RS[カレントrepo選択<br/>左ペイン title 直下<br/>「すべて」選択肢なし<br/>repo名の表示はここのみ<br/>localStorage 永続化] -->|表示スコープ| L
    RS -->|表示スコープ| R
    RS -->|新規chatの cwd<br/>スコープ外の chat選択・doc表示の解除<br/>（実行中chatはバックグラウンド継続）| C
    C -->|ヘッダレイアウト<br/>1行目=タイトル（概要）が全幅<br/>（タイトル前に処理状態ピル。<br/>削除ボタン表示時はタイトルと1行目を共有）<br/>2行目=plink/status/最新/◀▶ページング/<br/>options/言語select/操作ボタン| HD[flex-wrap で<br/>自然折返し]
    C -->|タイトル前の状態ピル| CPILL[ボードの列区分と同じ色で<br/>現在の処理状態を表示<br/>（実行中/実行待機中/検証中/<br/>検証待ち/要対応など）<br/>SSE push で即時反映]
    C -->|最新ボタン| NEW[updatedAt 最大の<br/>chat を選択]
    C -->|ヘッダの◀戻る/進む▶ボタン<br/>（新規画面からは 進む=先頭・戻る=末尾）| PAGE[status 順（旧処理状況の並び）で<br/>前後の chat を選択<br/>（列順 idle→exec→verify→resume→<br/>unresolved→live。他repoのchatは<br/>カレントrepo切替を兼ねる）]
    C -->|ヘッダの＋新規チャットボタン| NEWC[新規 chat 画面へ<br/>（chat の切替は左ペイン一覧で行う）]
    C -->|言語 select<br/>localStorage 永続化| I18N[panel/i18n.js<br/>i18n/*.json 辞書<br/>en がフォールバック<br/>data-i18n 属性 + t()<br/>切替時は全ペイン再描画]
    C -->|設定システムタブの<br/>タイムゾーン入力<br/>（IANA名・空欄=ブラウザ既定<br/>datalist に代表ゾーン）<br/>localStorage 永続化| TZ[表示タイムゾーン<br/>fmtTs/fmtTm が Intl で変換<br/>chat・console・一覧の時刻表示<br/>保存 ts は UTC のまま]
    R --> M1[loop state<br/>version/port/repos/chats/candidates<br/>+ KPI（status別件数・done率・MTTR）<br/>行ラベルは i18n 辞書で言語化]
    R --> M2[candidates/issues/proposals<br/>candidates は人の判断待ちのみ表示<br/>（未起票・gated・queued・failed。<br/>起票は自動が既定で実行中・終端の<br/>課題行は出さない）<br/>candidate 行は起票済み課題の status を併記<br/>rate-limit 保留の candidate は<br/>deferred ピルで明示<br/>全行に 対応/却下ボタン:<br/>対応=起票/承認/再実行を兼ねた即時 dispatch<br/>（未起票は rate-limit を人判断で bypass）<br/>却下=rejected（未起票は起票して即 rejected）<br/>proposal カードはクリックで<br/>中央ペインへ詳細表示<br/>queued issue は対応ボタンで即時 dispatch<br/>candidates/issues 各セクションの<br/>「一括対応」ボタンは表示中の判断待ち行を<br/>一括 dispatch（起動不可は autoDispatch 待機）<br/>課題処理方法（issueExecMode・設定システムタブ。<br/>既定=並列）: 直接=runner パイプライン（同一repo直列）/<br/>並列=chat セッション起票で並列処理<br/>（課題は chatId を持ち行に chat #N リンクバッジ。<br/>chat は課題 key を持ち終了区分を課題へ写像。<br/>done 課題行は drain の sweep が issues.json から<br/>削除しこの枠に残さない（完了表示は委譲 chat<br/>カードが担う。postPush 監視中は監視終了まで残存。<br/>GitHub 由来は理由コメント+issue クローズ後に削除）<br/>issue 行は priority バッジ（P2/P3）を併記<br/>GitHub Issue 由来の行は GitHub #番号の<br/>リンクバッジを併記（dispatch 後は<br/>manual と同一パイプライン）<br/>一覧は状態→priority→更新時刻順に整列<br/>status・source・kind の表示は<br/>i18n 辞書で言語化（未知の値は原文）<br/>各行の body/rationale は<br/>data-tip ホバーでポップアップ表示]
    M2 -->|proposal カードクリック<br/>詳細（vet 検査結果・changes 本文・<br/>採否ボタン）を中央ペインに表示| C
    L -->|エディターナビ<br/>（AIチャットの直下）| FT[フォルダツリー<br/>右ペインに展開<br/>カレントrepo の path を根<br/>dir は lazy 展開<br/>GET /api/fs/tree<br/>.git/node_modules 等は除外<br/>秘匿系は 🔒 で選択不可]
    FT -->|file 選択<br/>GET /api/fs/file<br/>（登録 repo 配下・非秘匿のみ<br/>binary は本文を返さない）| C
    L -->|AIチャットナビ<br/>doc/ツリー表示を閉じて<br/>chat 表示へ戻す| C
    L -->|縦並びナビ<br/>AGENTS/SKILL| R
    L -->|マニュアルナビ<br/>中央ペインを iframe 表示| C
    C -->|GET /docs/* 静的配信| MAN[docs/ マニュアルサイト<br/>正本=diagrams/docs/<br/>（1ページ1.md・frontmatter title<br/>nav.json=章立て<br/>template.html/style.css=書式<br/>Canvas docs 参考）<br/>tools/sync-docs.mjs が生成]
    L -->|セッションナビ<br/>（ナビ先頭・初期表示。1本に統合）<br/>中央ペインにエージェントタブ<br/>（Devin / Copilot）＋カンバン<br/>状況（turn 列・lastNote・failedTools）<br/>と統一ツールバー（維持操作）<br/>（共通テンプレ agent-sessions.mmd）| SESS["devinenv.rs + copilotenv.rs<br/>GET /api/&lt;agent&gt;-sessions<br/>POST delete / cleanup /<br/>vacuum・done / undone<br/>（詳細は devin-sessions.mmd・<br/>copilot-sessions.mmd）"]
    SESS -->|操作後は GET で再描画| C
    L -->|git ナビ（設定下の区切り線以下・ナビ末尾）<br/>コミット=メッセージ入力→<br/>POST /api/git/commit<br/>（git add -A + commit・変更なしは noop）<br/>プッシュ=確認後<br/>POST /api/git/push（人ゲートの承認）| GIT[カレント repo で git を spawn<br/>60s timeout<br/>出力は conlog 集約<br/>→ console タブへ SSE push]
    L -->|設定ナビ<br/>タブ: 接続AI / リポジトリ / システム<br/>（登録済み + ワークスペース管理下検出 + ユーザー検出:<br/>ワークスペース/repoScanDirs 直下の .git 無し dir・<br/>ユーザーディレクトリ/repoScanDirs 直下の .git dir<br/>※GitHub 非依存のローカル repo も対象）<br/>リポジトリタブ内はサブタブでグループ化<br/>（slug の GitHub owner ごと /<br/>ユーザー / (owner なし)。選択は永続化・<br/>グループ説明は data-tip ポップアップ）<br/>GET /api/chat-agents<br/>GET /api/repos| C
    C -->|接続AI 追加/更新/解除<br/>active 切替<br/>POST /api/chat-agents/*| AC[config.json の<br/>chatAgents + chatAgent<br/>SSE push で即時反映]
    AC -->|新規 chat の実行体<br/>（chat 作成時に固定し<br/>chats.json へ永続化）| ACPX[接続AI 実行体<br/>acp=CLI spawn /<br/>agent=/agent HTTP /<br/>openai=chat/completions]
    ACPX -->|stderr / 非JSON stdout<br/>（acp 型の spawn のみ）| CON[conlog.rs<br/>実行体出力 + control 自身の<br/>stdio をリングバッファ集約]
    C -->|console タブ<br/>GET /api/console +<br/>SSE console push| CON
    C -->|処理モード select<br/>（入力行の先頭・添付ボタンの左<br/>目立たせるため強調表示）<br/>POST /api/chat/mode| EM[execMode<br/>（既定=parallel）<br/>direct=逐次 /<br/>parallel=kasugai-split で<br/>子chat自動起動・結果を親へ集約<br/>同時実行上限超過分は<br/>pendingSplits待機キュー・<br/>子完了で順次起動<br/>ボードの子chatカードは↳印で識別]
    C -->|repo設定 保存/登録解除<br/>POST /api/repos/update<br/>POST /api/repos/remove| RC[config.json の repos<br/>SSE push で即時反映]
    C -->|設定システムタブの<br/>課題処理方法 select<br/>（直接=direct / 並列=parallel・既定並列）<br/>POST /api/control/issue-mode| IMD[issueExecMode<br/>config.json 永続化<br/>direct=runner パイプラインで直接実行<br/>（同一 repo の active 課題がある間<br/>dispatch 停止=直列・maxConcurrentRuns<br/>クォーラム有効） /<br/>parallel=chat 起票で並列処理<br/>（課題は chatId リンクを持ち<br/>chat 終了区分を課題へ写像）<br/>SSE push で即時反映]
    C -->|設定システムタブの<br/>同時起動セッション数 input<br/>POST /api/control/session-limit| SLM[maxConcurrentSessions<br/>既定3・0=無制限<br/>config.json 永続化<br/>proc 保持 chat 数の上限。<br/>超過起動は最古 idle proc を<br/>suspend（savedSessionId 保持で<br/>次回送信時に自動再開）して枠確保、<br/>全枠 busy なら capacity 待機<br/>SSE push で即時反映]
    C -->|repo登録（設定ビューの<br/>path入力・参照ピッカー /<br/>ワークスペース管理下の登録ボタン）<br/>POST /api/repos| RC
    C -->|path 参照<br/>GET /api/fs/dirs| FS[ローカルFSブラウザ<br/>サブdir + .git 検出<br/>loopback 限定]
    RC -->|選択肢へ追加し<br/>SSE即時反映| RS
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
    C -->|ボード上部の「スリープ起動」ボタン<br/>（スリープ中の件数を併記・0件は無効）<br/>POST /api/chat/start-suspended| SUSP[スリープ中セッションを一括起動<br/>resumable（acp・proc無・savedSessionId有）<br/>全件に ensure_session→session/load のみ<br/>prompt は送らない（busy化しない）。<br/>同時起動枠は idle追い出しなしで確保し<br/>枠超過分はスリープのまま残す<br/>（次回送信時に lazy 再開）<br/>SSE push で即時反映]
    C -->|実行状況列ヘッダの「並列」ボタン<br/>（待機件数を併記・ON中は強調）<br/>POST /api/chat/parallel {on}| PAR[並列モード切替<br/>on→conflictWait=false を config 永続化し<br/>全ての実装待機を blocker 未完了でも<br/>解放して即時送信（repo確認待ちは除く）<br/>以後の送信・FileTouch 競合も待機しない<br/>off→直列ガードへ復帰（実行中は継続）<br/>SSE push で即時反映]
    R -->|doc一覧 GET /api/agents<br/>GET /api/skills<br/>SKILL は global scope を<br/>一覧から除外| REG[doc registry<br/>agents.rs /<br/>skills.rs]
    R -->|doc/参照ファイル選択<br/>GET /api/doc| C
    R -->|GET /api/refs<br/>AGENTS: 切替時に全doc分を<br/>マージして一覧化<br/>SKILL: doc選択時| RF[参照ファイル抽出<br/>registry/refs.rs<br/>秘匿・未作成は無効表示]
```

## SkillOps/AgentOps

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
    CONF[.agents/skills<br/>prompts/config]
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

## スキルライフサイクル

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

## 自動再起動（code drift）

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

## 環境起動（docs ナビの起動ボタン）

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

## Devin セッション管理（環境維持）

<!-- diagram: devin-sessions -->
```mermaid
---
title: Devin セッション管理（環境維持）
---
flowchart LR
    NAV[セッションナビ → Devin タブ] --> VIEW[中央ペイン<br/>Devin セッション管理ビュー]
    VIEW --> GET[GET /api/devin-sessions<br/>sessions・live判定・<br/>孤立ファイル数・DBサイズ]
    GET --> DB[(Devin CLI<br/>%APPDATA%/Devin/cli/<br/>sessions.db)]
    GET --> LK[session_locks/<br/>&lt;id&gt;.lock<br/>LockFile バイトロック試行<br/>= live 判定]
    GET --> SM[summaries/<br/>&lt;id&gt;.md]
    DB --> TURN[ターン状態判定<br/>message_nodes 末尾を遡り最初の実ノードで判定<br/>（通知系 system ノードは読み飛ばし）:<br/>assistant+finish=stop → answered<br/>user/tool 末尾 or tool_calls 途中 → working<br/>割込マーカー or 非liveの途中 → interrupted<br/>※ answered は報告済みの意味。<br/>指示の実現性は lastNote で人が確認]
    DB --> NOTE[最終応答抜粋 lastNote<br/>最後の assistant ノード冒頭<br/>+ tool_call_state 失敗数 failedTools]
    VIEW --> DONE[POST /api/devin-sessions/done<br/>{ids} を完了化<br/>live 中は拒否<br/>sessions.db は触らない]
    VIEW --> UNDONE[POST /api/devin-sessions/undone<br/>{ids} を一覧へ戻す]
    VIEW --> DEL[POST /api/devin-sessions/delete<br/>{ids} 行削除<br/>live 中は拒否<br/>FTS は delete トリガーで連動]
    VIEW --> CLS[POST /api/devin-sessions/cleanup<br/>孤立 lock/summary を除去]
    VIEW --> VAC[POST /api/devin-sessions/vacuum<br/>sessions.db を VACUUM]
    VIEW --> ATT[POST /api/devin-sessions/attach<br/>{id} → saved_session_id 付き<br/>kasugai chat を作成<br/>（トランスクリプトを履歴に流し込み:<br/>user=本文/assistant=thinking→thought・<br/>本文→agent・tool_calls→tool 行<br/>※tool 結果行・system は除外）<br/>→ chatSelect で本格 AI チャット]
    DONE --> DF[devin-done.json<br/>kasugai_agent root<br/>done: id→ doneAt/title/wd]
    UNDONE --> DF
    DF --> GET2[GET 応答の各 session に<br/>done/doneAt を付与。<br/>DB 消滅 id は読み時に purge]
    GET2 --> VIEW
    VIEW --> TABS[タブ: 一覧（!done）/ 完了（done）<br/>完了タブは戻す・削除ボタン]
    DEL --> DB
    CLS --> LK
    CLS --> SM
    VAC --> DB
    DEL --> DF2[削除時は done エントリも除去]
    DF2 --> DF
    DONE --> REF[応答後に GET で再描画<br/>（状態と画面の乖離を残さない）]
    UNDONE --> REF
    DEL --> REF
    CLS --> REF
    VAC --> REF
```
