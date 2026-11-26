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
    assessed --> gated : 人ゲート待ち (external等)
    assessed --> queued : 自律可
    gated --> queued : 人が承認
    gated --> rejected : 人が却下
    queued --> running : runner 起動
    running --> verifying : 実装完了
    verifying --> done : 検証合格
    verifying --> retry : NG (差し戻し)
    retry --> queued : 再実行
    retry --> failed : リトライ上限
    running --> failed : 異常終了
    done --> [*]
    failed --> [*]
    rejected --> [*]
    dropped --> [*]
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
    end

    subgraph Core[コア server/src/]
        N[normalize + dedupe<br/>store.rs]
        A[assess<br/>pipeline.rs]
        G[gate<br/>人ゲート]
        R[runner<br/>session.rs]
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

## detect → normalize → dedupe

<!-- diagram: detect-pipeline -->
```mermaid
---
title: detect → normalize → dedupe
---
flowchart TD
    DET[detector] -->|candidate<br/>source/repo/title/body/<br/>symptomHash/detectedAt| NORM[正規化]
    NORM --> HASH[issue key =<br/>source+repo+symptomHash]
    HASH --> EXIST{既出?}
    EXIST -->|yes| BUMP[既存課題の<br/>lastSeen 更新のみ]
    EXIST -->|no| NEW[課題レコード作成<br/>status=assessed へ]
```

## verify（L0〜L4 / push 統合）

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
    SEP --> BACK[差し戻し prompt で再実装指示<br/>実装済み→修正 / 未実装→実装]
    BACK --> RETRY[status=retry<br/>verifyBackLimit まで]
    RETRY --> IMPL
    RETRY -->|上限到達| FAIL[status=failed<br/>sys 報告]
```

## learn（知見の還流）

<!-- diagram: learn-flow -->
```mermaid
---
title: learn（知見の還流）
---
flowchart LR
    RUN[run 結果・証跡] --> KN[SKILL 蓄積<br/>.devin/skills/<br/>課題key単位・draft・mermaid 図必須]
    KN --> PROP[proposals 生成<br/>skills/prompts/config/<br/>正本ドキュメント]
    PROP --> VET[vet<br/>機械検査<br/>path許可 / secret混入 /<br/>構文 / frontmatter]
    VET -->|fail| VF[vet 不合格<br/>accept 不能・要修正]
    VET -->|pass| HUMAN{人の採否}
    HUMAN -->|採用| SELF[agent 自身へ反映<br/>or 正本へ課題起票]
    HUMAN -->|却下| DROP[記録して破棄]
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
        C->>C: split.rs が [{title,prompt}] を抽出<br/>（parallelMaxChildren 上限・重複抑止）
        C->>A: 子 chat を生成（parent=親id・direct 固定・<br/>同 repo cwd・同 agent）して session/prompt
        Note over C: 子 chat の busy 解除 → 最終回答を<br/>ダイジェストとして親 chat へ投函。<br/>親は統合・さらなる分割を続けられる
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

## パネルレイアウト

<!-- diagram: panel-layout -->
```mermaid
---
title: パネルレイアウト
---
flowchart LR
    L[左ペイン<br/>title + カレントrepo +<br/>縦並びナビ + 管理セクション] --> C[中央ペイン<br/>chat messages（chat/console タブ）/<br/>doc 本文（Viewer 単一オブジェクトに集約<br/>kind判定・原文/プレビュー切替・mermaid描画<br/>MD+mermaid・HTML・MMD 表示・MMD は縦横切替可<br/>mermaid 図は＋−ボタン/wheel で拡縮<br/>（カーソル位置を維持）<br/>中央配置・ドラッグで視野移動・ズーム率表示）]
    C --> R[右ペイン<br/>chat sessions（行は状態ドット+ラベルの<br/>最小表示・状態確認は処理状況ボード） /<br/>doc browser + 参照ファイル /<br/>フォルダツリー（エディター）<br/>（省略ラベルは data-tip<br/>ホバーポップアップで全文表示）]
    G[境界グリップ<br/>ドラッグで幅可変<br/>localStorage 永続化] --> L
    G --> R
    RS[カレントrepo選択<br/>左ペイン title 直下<br/>「すべて」選択肢なし<br/>repo名の表示はここのみ<br/>localStorage 永続化] -->|表示スコープ| L
    RS -->|表示スコープ| R
    RS -->|新規chatの cwd<br/>スコープ外の chat選択・doc表示の解除<br/>（実行中chatはバックグラウンド継続）| C
    C -->|ヘッダレイアウト<br/>1行目=タイトル（概要）が全幅<br/>2行目=plink/status/最新/<br/>options/言語select/操作ボタン| HD[flex-wrap で<br/>自然折返し]
    C -->|最新ボタン| NEW[updatedAt 最大の<br/>chat を選択]
    C -->|言語 select<br/>localStorage 永続化| I18N[panel/i18n.js<br/>i18n/*.json 辞書<br/>en がフォールバック<br/>data-i18n 属性 + t()<br/>切替時は全ペイン再描画]
    C -->|設定システムタブの<br/>タイムゾーン入力<br/>（IANA名・空欄=ブラウザ既定<br/>datalist に代表ゾーン）<br/>localStorage 永続化| TZ[表示タイムゾーン<br/>fmtTs/fmtTm が Intl で変換<br/>chat・console・一覧の時刻表示<br/>保存 ts は UTC のまま]
    L --> M1[loop state]
    L --> M2[candidates/proposals]
    L -->|エディターナビ<br/>（処理状況の直下）| FT[フォルダツリー<br/>右ペインに展開<br/>カレントrepo の path を根<br/>dir は lazy 展開<br/>GET /api/fs/tree<br/>.git/node_modules 等は除外<br/>秘匿系は 🔒 で選択不可]
    FT -->|file 選択<br/>GET /api/fs/file<br/>（登録 repo 配下・非秘匿のみ<br/>binary は本文を返さない）| C
    L -->|縦並びナビ<br/>AIチャット/AGENTS/SKILL| R
    L -->|マニュアルナビ<br/>中央ペインを iframe 表示| C
    C -->|GET /docs/* 静的配信| MAN[docs/ マニュアルサイト<br/>正本=diagrams/docs/<br/>（1ページ1.md・frontmatter title<br/>nav.json=章立て<br/>template.html/style.css=書式<br/>Canvas docs 参考）<br/>tools/sync-docs.mjs が生成]
    L -->|git ナビ（設定下の区切り線以下・ナビ末尾）<br/>コミット=メッセージ入力→<br/>POST /api/git/commit<br/>（git add -A + commit・変更なしは noop）<br/>プッシュ=確認後<br/>POST /api/git/push（人ゲートの承認）| GIT[カレント repo で git を spawn<br/>60s timeout<br/>出力は conlog 集約<br/>→ console タブへ SSE push]
    L -->|設定ナビ<br/>タブ: 接続AI / リポジトリ / システム<br/>（登録済み + IDE管理下検出 + ユーザー検出:<br/>ワークスペース/repoScanDirs 直下の .git 無し dir・<br/>ユーザーディレクトリ/repoScanDirs 直下の .git dir<br/>※GitHub 非依存のローカル repo も対象）<br/>GET /api/chat-agents<br/>GET /api/repos| C
    C -->|接続AI 追加/更新/解除<br/>active 切替<br/>POST /api/chat-agents/*| AC[config.json の<br/>chatAgents + chatAgent<br/>SSE push で即時反映]
    AC -->|新規 chat の実行体<br/>（chat 作成時に固定し<br/>chats.json へ永続化）| ACPX[接続AI 実行体<br/>acp=CLI spawn /<br/>agent=/agent HTTP /<br/>openai=chat/completions]
    ACPX -->|stderr / 非JSON stdout<br/>（acp 型の spawn のみ）| CON[conlog.rs<br/>実行体出力 + control 自身の<br/>stdio をリングバッファ集約]
    C -->|console タブ<br/>GET /api/console +<br/>SSE console push| CON
    C -->|処理モード select<br/>（入力行の先頭・添付ボタンの左<br/>目立たせるため強調表示）<br/>POST /api/chat/mode| EM[execMode<br/>（既定=parallel）<br/>direct=逐次 /<br/>parallel=kasugai-split で<br/>子chat自動起動・結果を親へ集約<br/>sessions一覧は親子でグループ表示]
    C -->|repo設定 保存/登録解除<br/>POST /api/repos/update<br/>POST /api/repos/remove| RC[config.json の repos<br/>SSE push で即時反映]
    C -->|repo登録（設定ビューの<br/>path入力・参照ピッカー /<br/>IDE管理下の登録ボタン）<br/>POST /api/repos| RC
    C -->|path 参照<br/>GET /api/fs/dirs| FS[ローカルFSブラウザ<br/>サブdir + .git 検出<br/>loopback 限定]
    RC -->|選択肢へ追加し<br/>SSE即時反映| RS
    L -->|処理状況ナビ<br/>idle/exec/verify/resume/live<br/>の順の状態別カンバン列<br/>exec列は busy+waiting を統合<br/>カードに実行中/実行待機中ピル<br/>カードに ctx 消費量ピル（usage_update 由来。<br/>行は最小表示のため詳細は data-tip）<br/>verify列は実装ターン終了で自動遷移<br/>（touched あり → autoVerify で検証<br/>prompt 送信・検証中も列に留まる）| C
    C -->|statusカードクリック| PICK[該当 chat を選択表示]
    C -->|検証列カードの検証ボタン<br/>POST /api/chat/verify| VR[検証ターン起動<br/>対象EXE起動+Playwright等の<br/>実機確認を指示<br/>SSE push で即時反映]
    C -->|verify=running/pending| VSH[chat 画面にも検証中/検証待ちを明示<br/>ヘッダ status＋メッセージ末尾の<br/>インジケータ＋送信ボタン<br/>SSE push で即時反映]
    C -->|完了列カードの×ボタン<br/>POST /api/chat/close| DEL[chat 削除<br/>SSE push で即時反映]
    C -->|完了列ヘッダの「すべて削除」<br/>POST /api/chat/close-completed<br/>カレントrepoの完了列を一括削除| DEL
    C -->|再開可列カードの再開ボタン<br/>POST /api/chat/resume| RLOAD[session/load で再接続<br/>SSE push で即時反映]
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
    EN -->|on| BUSY{実行中 run あり?}
    BUSY -->|yes| FORCE{force 指定?}
    FORCE -->|no| DEFER[空くまで延期<br/>SSE restart pending push]
    FORCE -->|yes| STOP[runner 停止<br/>busy も含め全 proc を畳む]
    BUSY -->|no| STOP
    DEFER --> BUSY
    STOP --> START[control 再起動<br/>detached respawner が親終了を<br/>待って新プロセスを起動]
    START --> RSM{autoResumeOnStart かつ<br/>resumable chat あり?}
    RSM -->|yes| LOAD[全対象を session/load で自動再接続<br/>失敗時は session/new へフォールバック]
    RSM -->|no| READY[idle 待機]
    LOAD --> READY
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
        MANUAL[exe 直起動 / cargo run<br/>（npm run dev）]
        PROTO2[kasugai-agent:// プロトコル<br/>（本図のフォールバック経路）]
    end
```
