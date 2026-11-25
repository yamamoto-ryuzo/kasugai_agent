// チャット内容とカレントリポジトリの不整合検出（純粋ロジック・副作用なし）
//
// send 前に prompt を検査し、「この内容は別の登録 repo 向け」と判断できる場合に
// 切替候補 {slug, path, reason} を返す。手がかりは2段:
//   強: prompt 文中のファイルパスが他 repo の path 配下に解決される
//   弱: 他 repo の名前（slug 名部 / path ベース名）への言及がある
//       （カレント repo 名が文中に出る場合・候補が複数ある場合は提案しない）
// 検出した候補をどう扱うか（確認待ち・切替・keep 記録）は control.mjs の責務
import { basename } from 'node:path';
import { normPath, extractPaths } from './claims.mjs';

export const repoSlug = (r) =>
  typeof r === 'string' ? r : (r?.slug ?? r?.github ?? null);

const repoPath = (r) => normPath(typeof r === 'string' ? '' : r?.path);

// 名前トークン: slug の名部（owner/name の name）と path のベース名。
// 4文字未満は誤検出が多いので捨てる
const nameTokens = (r) => {
  const out = new Set();
  const slug = repoSlug(r) ?? '';
  const sn = slug.includes('/') ? slug.slice(slug.lastIndexOf('/') + 1) : slug;
  const pn = basename(
    String(typeof r === 'string' ? '' : r?.path ?? '').replace(/[\\/]+$/, ''));
  for (const t of [sn, pn])
    if (String(t).length >= 4) out.add(String(t).toLowerCase());
  return [...out];
};

// 名前の言及判定。トークン中の _ / - は 空白・_・- いずれの表記でも許す
// （kasugai_qgis / kasugai-qgis / "kasugai qgis" を同じ言及として扱う）
const escRe = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
const mentionRe = (tok) => new RegExp(
  `(?<![\\w-])${[...tok.replace(/[\s_-]+/g, '')].map(escRe).join('[\\s_-]*')}` +
  `(?![\\w-])`, 'i');
const mentions = (text, tok) => mentionRe(tok).test(text);

// text が cur（chat のカレント repo）以外の repo 向けに見えるなら
// {slug, path, reason} を返す。判断できなければ null
export function suggestRepo(text, cur, repos) {
  const t = String(text ?? '');
  if (!t.trim() || !repos?.length) return null;
  const curSlug = repoSlug(cur);
  const curPath = repoPath(cur);
  const others = repos.filter((r) =>
    repoSlug(r) !== curSlug && repoPath(r) !== curPath);
  if (!others.length) return null;

  // 強: 文中のパスが他 repo の path 配下に解決される
  for (const p of extractPaths(t, curPath || '.')) {
    const hit = others.find((r) => {
      const rp = repoPath(r);
      return rp && (p === rp || p.startsWith(rp + '/'));
    });
    if (hit)
      return { slug: repoSlug(hit), path: hit.path,
        reason: `パス ${p.split('/').slice(-2).join('/')} が ` +
          `${repoSlug(hit) ?? hit.path} 配下` };
  }

  // 弱: 他 repo 名への言及。カレント repo 名も出る文は
  // 「取り込み・比較」等でカレント向けの可能性があるため提案しない
  if (nameTokens(cur ?? {}).some((tok) => mentions(t, tok))) return null;
  const hits = others.filter((r) =>
    nameTokens(r).some((tok) => mentions(t, tok)));
  if (hits.length !== 1) return null;   // 複数候補は決められない
  const tok = nameTokens(hits[0]).find((x) => mentions(t, x));
  return { slug: repoSlug(hits[0]), path: hits[0].path,
    reason: `「${tok}」への言及` };
}
