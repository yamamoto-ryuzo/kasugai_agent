// ファイルクレーム / 実装待機の競合判定（純粋ロジック・副作用なし）
//
// busy 中の chat はクレームを持つ。クレーム = declared（prompt 文中のパス）
// ∪ touched（編集系 tool_call が扱ったパス）。同一ファイルの同時編集を防ぐため、
// クレームが重なった chat は waiting（実装待機）に入り、blocker の busy が
// 解けた時点で保留 prompt を再送する。関数単位の競合はファイル単位に包含して扱う
import { resolve } from 'node:path';

export const normPath = (p, cwd) => {
  let s = String(p ?? '').trim().replace(/\\/g, '/');
  if (!s) return '';
  if (!/^[a-zA-Z]:\//.test(s) && !s.startsWith('/')) s = resolve(cwd, s);
  return s.replace(/\\/g, '/').replace(/\/+$/, '').toLowerCase();
};

// prompt 文中のファイルパス表記を拾う（拡張子で絞り、誤検出を抑える）
const PATH_TOKEN =
  /(?:[A-Za-z]:[\\/])?[\w.~$-]+(?:[\\/][\w.~$-]+)*\.(?:mjs|cjs|js|jsx|ts|tsx|mts|cts|py|rb|go|rs|java|kt|c|h|cc|cpp|hpp|cs|html|htm|css|scss|less|json|ya?ml|toml|ini|md|markdown|txt|sh|bat|ps1|sql|vue|svelte|xml|csv)/g;

export const extractPaths = (text, cwd) => {
  const out = new Set();
  for (const m of String(text ?? '').matchAll(PATH_TOKEN)) {
    const n = normPath(m[0], cwd);
    if (n) out.add(n);
  }
  return [...out];
};

// クレームは busy 中のみ有効（待機中・idle の chat は何も握らない）
export const claimsOf = (chat) =>
  chat.busy ? new Set([...chat.touched, ...chat.declared]) : new Set();

// paths（正規化済み）と衝突する他 chat を返す
export function findConflicts(chats, selfId, paths) {
  const hits = [];
  if (!paths.length) return hits;
  for (const [oid, o] of chats) {
    if (oid === selfId) continue;
    const claims = claimsOf(o.chat);
    const dup = paths.filter((p) => claims.has(p));
    if (dup.length) hits.push({ id: oid, files: dup });
  }
  return hits;
}

// send 前のクレーム登録。新ターンなら過去クレームを捨て、prompt 文中パスを登録
export function setupClaims(chat, text) {
  if (!chat.busy) { chat.touched.clear(); chat.declared = new Set(); }
  for (const d of extractPaths(text, chat.cwd)) chat.declared.add(d);
}

// blockerId の busy が解けた。それにブロックされていた待機 chat を解放する
export function releaseWaiters(chats, blockerId) {
  for (const w of chats.values()) {
    const wt = w.chat.waiting;
    if (!wt || !wt.blockedBy.includes(blockerId)) continue;
    // 残 blocker が busy なら待機継続（消えた chat もここで除去）
    wt.blockedBy = wt.blockedBy.filter((b) => chats.get(b)?.chat.busy);
    if (wt.blockedBy.length) { w.chat.onChange(); continue; }
    const text = w.chat.leaveWait();
    if (!text) continue;
    setupClaims(w.chat, text);
    w.chat.send(text).catch((e) =>
      w.chat.push('sys', `[再開失敗] ${e.message}`));
  }
}
