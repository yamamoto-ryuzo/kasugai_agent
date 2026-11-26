---
title: 2-3 処理状況
---

パネル起動時の初期表示です（左ペイン「処理状況」ナビからも開けます）。
全 repo の chat を状態別カンバン列で
一覧します（カレント repo のカードは各列の先頭。他 repo のカードには
repo ピルを付し、クリックで repo を切り替えて開きます）。
chat の切替はこのボードで行います。列は
`idle → exec → verify → resume → unresolved → live` の順です。

## 列の意味

| 列 | 状態 |
|---|---|
| idle | 未着手（セッション未開始。起動失敗で未送信の prompt は保留される） |
| exec | busy（実行中）+ waiting（実行待機中）を統合。カードのピルで区別 |
| verify | 実装ターン終了で自動遷移（touched あり → autoVerify）。検証中も列に留まる |
| resume | ACP 切断・次の送信で再開可能 |
| unresolved | 人の操作が必要なものだけ。検証NGの差し戻し再送待ち・要人対応・区分ピルで 検証NG(ng) / 未検証(unverified) を明示 |
| live | 接続中の完了セッション。実装完了（doneKind=done: 実装→実機検証の合格まで済んだもの）と、実装ターンを経ずに終了したもの（無印） |

## カード操作

- **クリック** — 該当 chat を選択表示
- **検証**（verify 列・検証待ち） — 実機検証ターンを起動
  （対象 EXE 起動＋Playwright 等の実機確認を指示）
- **再開**（resume 列） — `session/load` で即時再接続
- **開始**（idle 列） — `session/new` で起動。起動失敗で保留された
  prompt があれば再送する（`POST /api/chat/resume`）
- **差し戻し**（unresolved 列） — 保留の差し戻し prompt（無ければ終了区分に
  応じた再作業 prompt）を送り exec 列へ戻す（`POST /api/chat/rework`）
- **×**（全列・ホバー表示。右下にボタンがあるカードでは右上） —
  chat を削除
- **すべて削除**（live 列ヘッダ） — 完了列の chat を一括削除
  （live 列＝doneKind=done・無印が対象。カレント repo のみ。
  要対応列は残る。`POST /api/chat/close-completed`）
- **＋ 新規チャット**（ボード上部） — 新規 chat 画面を開く
- **処理再開**（ボード上部） — 止まった処理を一括再開
  （`POST /api/chat/resume-all`。差し戻し待ちは保留 prompt を再送、
  再開可 chat は再接続＋中断作業の続行 prompt、未着手 chat は
  `session/new` で起動して保留 prompt を再送、検証待ちは検証ターン
  起動、ブロッカーが消えた実行待機は解放して再送。busy 中と
  repo 確認待ち・要人対応は対象外。ボタンには停止中の件数を併記し、
  0 件では無効）

いずれも SSE push で全パネルへ即時反映されます。

## verify の流れ

再検証（verify）と push は一体の仕事です。検証を通っていないコミットは
リモートへ公開せず、帰属不明・rejected のコミットは `git revert` で
除去してから push します。検証 NG の場合は課題を分離した差し戻し
prompt を**保留**して unresolved 列で再送待ちにします（自動送信は
しません。再送は「処理再開」・個別の「差し戻し」ボタン、または
再起動時の自動再処理で）。再実装ターンの完了で再度検証列へ戻り、
`verifyBackLimit` 回まで再試行します。上限到達は `ng` 確定で
「要人対応」ピルを付します（人が採否・再作業を判断）。
検証フェーズを解消した時点で終了区分（`doneKind`）が確定します:
合格は `done`（実装完了 → live 列）、差し戻し上限到達は `ng`、
verdict を取得できずに終了したものは `unverified`（この2つが
unresolved 列へ）。実装ターンを経なかったものは無印で、要対応
ではなく live 列へ置きます（操作不要の入力待ちセッション）。
