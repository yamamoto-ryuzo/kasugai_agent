// KASUGAI Agent — registry/refs.mjs
// AgentOps: ルール文書（AGENTS.md 等）が参照するファイルの解決（read-only）。
// 文書内の `code` 表記からパスらしきトークンを抽出し、doc の位置 → 各 root の
// 順で実ファイルへ解決する。glob（* / ?）・ディレクトリ参照（末尾 / または
// 実在 dir）は配下のファイルへ展開する。
// 秘匿系（secrets.json・.env・鍵ファイル等）は denied として一覧には残すが、
// GET /api/doc の本文返却対象からは外す。
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { homedir } from 'node:os';
import { join, dirname, basename, relative, resolve, isAbsolute,
         normalize } from 'node:path';

const isDir = (p) => { try { return statSync(p).isDirectory(); } catch { return false; } };
const isFile = (p) => { try { return statSync(p).isFile(); } catch { return false; } };

// 本文を返させない秘匿系（secrets.json はパネルからも読ませない）
const DENY = [/secret/i, /^\.env(\.|$)/i, /\.(pem|key|pfx|p12|kdbx)$/i,
  /^id_(rsa|dsa|ecdsa|ed25519)/i, /credential/i];
export const denied = (p) => DENY.some((re) => re.test(basename(p)));

export const SKIP_DIRS = new Set(['.git', 'node_modules', '.hg', '.svn']);

// `code` 表記のうちパスらしきものだけを抽出。
// 空白・引用・HTML 記号・: を含むトークン（識別子・コマンド・ラベル名）は除く。
const PATHY = /^[\w.\-~\\/*?]+$/;
const looksPath = (t) => PATHY.test(t) &&
  (t.includes('/') || t.includes('\\') || /[*?]/.test(t) || /\.\w{1,15}$/.test(t));

const codeTokens = (text) => {
  const out = [];
  const re = /`([^`\r\n]+)`/g;
  let m;
  while ((m = re.exec(text))) {
    const t = m[1].trim();
    if (looksPath(t)) out.push(t);
  }
  return out;
};

// 1セグメント分の glob マッチャ（* / ? のみ対応）
const segRe = (seg) => new RegExp('^' + seg
  .replace(/[.+^${}()|[\]\\]/g, '\\$&')
  .replace(/\*/g, '[^\\\\/]*')
  .replace(/\?/g, '[^\\\\/]') + '$', 'i');

// 絶対 glob パターンを展開し実在ファイルを返す（* / ? はセグメント単位）
function expandGlob(absPattern) {
  const segs = absPattern.split(/[\\/]+/);
  const pre = [];
  while (segs.length && !/[*?]/.test(segs[0])) pre.push(segs.shift());
  const base = pre.join('\\') || '\\';
  const out = [];
  const walk = (dir, rest) => {
    if (!rest.length) { if (isFile(dir)) out.push(dir); return; }
    if (!isDir(dir)) return;
    const [head, ...tail] = rest;
    if (!/[*?]/.test(head)) { walk(join(dir, head), tail); return; }
    const re = segRe(head);
    for (const e of readdirSync(dir)) {
      if (re.test(e)) walk(join(dir, e), tail);
    }
  };
  try { walk(base, segs); } catch { /* noop */ }
  return out;
}

// ディレクトリ配下のファイルを浅く列挙（深さ2・上限40。node_modules 等は除外）
function dirFiles(dir, depth = 2, cap = 40) {
  const out = [];
  const walk = (d, lv) => {
    if (out.length >= cap) return;
    for (const e of readdirSync(d)) {
      if (out.length >= cap) return;
      const p = join(d, e);
      if (isDir(p)) { if (lv > 0 && !SKIP_DIRS.has(e)) walk(p, lv - 1); }
      else if (isFile(p)) out.push(p);
    }
  };
  try { walk(dir, depth); } catch { /* noop */ }
  return out;
}

// 解決済みファイルのエントリ。doc 配下なら相対パス、外なら basename を表示名にする
const fileEntry = (p, docDir) => {
  const rel = relative(docDir, p);
  const name = rel && !rel.startsWith('..') && !isAbsolute(rel) ? rel : basename(p);
  return { name, path: p, kind: 'ref', exists: true, denied: denied(p) };
};

// トークンを実パスへ解決。bases の先頭から順に探し最初に実在した場所を採用。
// 見つからない場合は先頭 base に置いた想定パスで exists:false を返す
function resolveToken(tok, bases, docDir) {
  const t = tok.startsWith('~') ? homedir() + tok.slice(1).replace(/\//g, '\\') : tok;
  const globby = /[*?]/.test(t);
  for (const base of bases) {
    const abs = resolve(base, t);
    if (globby) {
      const hits = expandGlob(abs);
      if (hits.length) return hits.map((p) => fileEntry(p, docDir));
      continue;
    }
    if (isFile(abs)) return [fileEntry(abs, docDir)];
    if (isDir(abs)) {
      const kids = dirFiles(abs);
      return kids.length
        ? kids.map((p) => fileEntry(p, docDir))
        : [{ name: tok, path: abs, kind: 'ref', exists: true, isDir: true,
             denied: denied(abs) }];
    }
  }
  const guess = resolve(bases[0] ?? '.', t);
  return [{ name: tok, path: guess, kind: 'ref', exists: false,
            denied: denied(guess) }];
}

// docPath の文書が参照するファイル一覧。roots は解決フォールバック
// （workspace ROOT + config.repos[].path）。scope は表示用ラベル
export function listRefDocs(docPath, roots = [], scope = '') {
  let text = '';
  try { text = readFileSync(docPath, 'utf8'); } catch { return []; }
  const docDir = dirname(resolve(docPath));
  const bases = [docDir, ...roots.filter(Boolean).map((r) => resolve(r))];
  const seen = new Set();
  const out = [];
  for (const tok of codeTokens(text)) {
    for (const e of resolveToken(tok, bases, docDir)) {
      const key = normalize(e.path).toLowerCase();
      if (seen.has(key)) continue;
      seen.add(key);
      e.scope = scope;
      out.push(e);
    }
  }
  return out;
}

// GET /api/doc の返却許可リスト用: 実在・非秘匿のファイルパス（normalize 小文字）
export function listAllRefPaths(docs, roots = []) {
  const out = new Set();
  for (const d of docs ?? []) {
    for (const r of listRefDocs(d.path, roots)) {
      if (r.exists && !r.denied && !r.isDir && isFile(r.path)) {
        out.add(normalize(r.path).toLowerCase());
      }
    }
  }
  return out;
}
