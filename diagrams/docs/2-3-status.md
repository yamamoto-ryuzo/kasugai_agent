---
title: 2-3 処理状況
---

左ペイン「処理状況」ナビで、カレント repo の chat を状態別カンバン列で
一覧します。列は `idle → exec → verify → resume → live` の順です。

## 列の意味

| 列 | 状態 |
|---|---|
| idle | 待機中（セッション未開始・完了） |
| exec | busy（実行中）+ waiting（実行待機中）を統合。カードのピルで区別 |
| verify | 実装ターン終了で自動遷移（touched あり → autoVerify）。検証中も列に留まる |
| resume | ACP 切断・次の送信で再開可能 |
| live | ACP セッション接続中 |

## カード操作

- **クリック** — 該当 chat を選択表示
- **検証**（verify 列・検証待ち） — 実機検証ターンを起動
  （対象 EXE 起動＋Playwright 等の実機確認を指示）
- **再開**（resume 列） — `session/load` で即時再接続
- **×**（live 列） — chat を削除
- **すべて削除**（live 列ヘッダ） — 完了列の chat を一括削除
  （カレント repo のみ。`POST /api/chat/close-completed`）

いずれも SSE push で全パネルへ即時反映されます。

## verify の流れ

再検証（verify）と push は一体の仕事です。検証を通っていないコミットは
リモートへ公開せず、帰属不明・rejected のコミットは `git revert` で
除去してから push します。検証 NG の場合は課題を分離して差し戻し
（`verifyBackLimit` まで再試行）。
