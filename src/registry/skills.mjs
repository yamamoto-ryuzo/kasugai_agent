// KASUGAI Agent — registry/skills.mjs
// SkillOps: skill 定義の台帳（read-only 文書スキャン）。
// 各種 skills/ ディレクトリ配下の SKILL.md を収集する。
// 収集した path だけが GET /api/doc の本文返却対象になる（secrets 等は拾わない）。
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { homedir } from 'node:os';
import { join, basename, dirname } from 'node:path';

// プロジェクト内で走査する skills ディレクトリ（devin 規約 + 他ツール互換）
const SKILL_DIRS = ['.devin/skills', '.agents/skills', '.windsurf/skills',
  '.kiro/skills', '.claude/skills', '.github/skills', '.codex/skills'];

const isDir = (p) => { try { return statSync(p).isDirectory(); } catch { return false; } };
const isFile = (p) => { try { return statSync(p).isFile(); } catch { return false; } };

// SKILL.md の frontmatter から name/description を軽く拾う（YAML 非依存）
function readMeta(path) {
  try {
    const head = readFileSync(path, 'utf8').slice(0, 4000);
    const m = head.match(/^---\r?\n([\s\S]*?)\r?\n---/);
    if (!m) return {};
    const pick = (k) =>
      m[1].match(new RegExp('^' + k + ':\\s*["\']?(.+?)["\']?\\s*$', 'm'))?.[1];
    return { name: pick('name'), desc: pick('description') };
  } catch { return {}; }
}

// <dir>/<name>/SKILL.md を収集
function scanSkillsDir(dir, scope, source) {
  const out = [];
  for (const e of (isDir(dir) ? readdirSync(dir) : [])) {
    const p = join(dir, e, 'SKILL.md');
    if (!isFile(p)) continue;
    const meta = readMeta(p);
    out.push({ name: meta.name || e, kind: 'skill', path: p, scope,
      source, dir: e, desc: meta.desc ?? '' });
  }
  return out;
}

const appDataDevin = () =>
  join(process.env.APPDATA ?? join(homedir(), 'AppData', 'Roaming'), 'devin');

// グローバル配置: %APPDATA%/devin/skills, ~/.config/devin/skills,
// ~/.devin/skills, ~/.agents/skills, ~/.codeium/<channel>/skills,
// Devin アプリ同梱 session skills, ~/.devin/extensions/<ext>/<SKILL_DIRS>
function scanGlobal(opts = {}) {
  const out = [];
  out.push(...scanSkillsDir(join(appDataDevin(), 'skills'), 'global', 'devin'));
  out.push(...scanSkillsDir(join(homedir(), '.config', 'devin', 'skills'), 'global', 'devin'));
  out.push(...scanSkillsDir(join(homedir(), '.devin', 'skills'), 'global', 'devin'));
  out.push(...scanSkillsDir(join(homedir(), '.agents', 'skills'), 'global', '.agents'));
  const codeium = join(homedir(), '.codeium');
  for (const ch of (isDir(codeium) ? readdirSync(codeium) : [])) {
    out.push(...scanSkillsDir(join(codeium, ch, 'skills'), 'global', 'codeium/' + ch));
  }
  // Devin アプリ同梱の session skills（devin.exe …/app/extensions/…/bin/devin.exe
  // から resources/app を逆算し out/vs/sessions/skills を指す）
  const appRoot = opts.devinExe
    ? join(dirname(opts.devinExe), '..', '..', '..', '..')
    : join(process.env.LOCALAPPDATA ?? '.', 'Programs', 'Devin', 'resources', 'app');
  out.push(...scanSkillsDir(join(appRoot, 'out', 'vs', 'sessions', 'skills'),
    'global', 'devin-app'));
  // Devin 拡張（VSIX）同梱の skills（<ext>/ 配下を SKILL_DIRS で走査）
  const extRoot = join(homedir(), '.devin', 'extensions');
  for (const e of (isDir(extRoot) ? readdirSync(extRoot) : [])) {
    for (const d of SKILL_DIRS) {
      out.push(...scanSkillsDir(join(extRoot, e, d), 'global', 'ext/' + e));
    }
  }
  return out;
}

// roots: [workspace ROOT, ...config.repos[].path]
// opts.devinExe: アプリ同梱 skills の探索起点（config.devinExe）
export function listSkillDocs(roots, opts = {}) {
  const seen = new Set();
  const out = [];
  for (const root of new Set(roots.filter(Boolean))) {
    const scope = basename(root) || root;
    for (const dir of SKILL_DIRS) {
      for (const d of scanSkillsDir(join(root, dir), scope, dir)) {
        if (seen.has(d.path)) continue;
        seen.add(d.path); out.push(d);
      }
    }
  }
  for (const d of scanGlobal(opts)) {
    if (seen.has(d.path)) continue;
    seen.add(d.path); out.push(d);
  }
  return out;
}
