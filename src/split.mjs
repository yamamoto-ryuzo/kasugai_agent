// 並列モード（execMode=parallel）の課題分割（純粋ロジック・副作用なし）
//
// parallel chat の初回 prompt には PARALLEL_PREAMBLE が前置され、
// エージェントは独立タスクへ分割できると判断した時点で
//   ```kasugai-split
//   [{"title":"子タスク名","prompt":"子 chat へ送る自己完結の指示"}]
//   ```
// を出力する。control はターン終了ごとに新着 agent メッセージへ
// extractSplits を掛け、見つかった要素を子 chat として自動起動する
// （起動・上限・重複抑止・集約は control.mjs 側の責務）

// 初回 prompt の先頭へ付ける分割指示。子 prompt は文脈を引き継がない
// 独立セッションで実行されるため、自己完結で書くよう指示する
export const PARALLEL_PREAMBLE = `[処理モード: 並列]
この指示が複数の独立した作業へ分割できると判断した場合、作業中・回答末尾を問わず
任意のタイミングで次の形式のコードブロックを出力してください:

\`\`\`kasugai-split
[{"title":"短い子タスク名","prompt":"子チャットへ送る指示文"}]
\`\`\`

各 prompt はこの会話の文脈を引き継がない独立チャット（別セッション）で実行されます。
必要な背景・対象ファイル・期待する成果物を prompt 内に自己完結で書いてください。
title・prompt とも日本語可。分割が不要と判断した場合はブロックを出さず、
そのまま作業を進めてください。ブロックを出した後も自分の作業は継続して構いません
（分割結果は子チャットの完了時にこのチャットへ報告されます）。`;

// テキスト中の ```kasugai-split ブロックから [{title,prompt}] を抽出する。
// 壊れた JSON・必須フィールド欠落は捨てる（パース不能で処理を止めない）
const SPLIT_RE = /```kasugai-split[^\S\r\n]*\r?\n([\s\S]*?)```/g;

export function extractSplits(text) {
  const out = [];
  for (const m of String(text ?? '').matchAll(SPLIT_RE)) {
    let arr;
    try { arr = JSON.parse(m[1]); } catch { continue; }
    if (!Array.isArray(arr)) continue;
    for (const e of arr) {
      const title = String(e?.title ?? '').trim().slice(0, 80);
      const prompt = String(e?.prompt ?? '').trim().slice(0, 4000);
      if (title && prompt) out.push({ title, prompt });
    }
  }
  return out;
}

// 重複抑止キー（title+prompt 一致）。chats.json の seenSplits へ永続化される
export const splitKey = (s) =>
  `${String(s.title).toLowerCase()}\n${String(s.prompt).toLowerCase()}`;
