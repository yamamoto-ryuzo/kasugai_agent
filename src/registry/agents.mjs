// KASUGAI Agent — registry/agents.mjs
// AgentOps: agent 定義・ルール文書の台帳（read-only 文書スキャン）。
// AGENTS.md 系・.devin/rules・.devin/agents 定義・グローバル配置を収集する。
// 収集した path だけが GET /api/doc の本文返却対象になる（secrets 等は拾わない）。
import { readdirSync, statSync } from 'node:fs';
import { homedir } from 'node:os';
import { join, basename } from 'node:path';

const RULE_FILES = ['AGENTS.md', 'AGENTS.local.md', 'AGENT.md', 'CLAUDE.md',
  '.windsurfrules'];
// agents/<name>/ 配下の定義ファイル名（優先順。先に見つかった方を採用）
const AGENT_DEF_NAMES = ['AGENT.md', 'AGENTS.md', 'agent.md', 'agents.md'];

const isDir = (p) => { try { return statSync(p).isDirectory(); } catch { return false; } };
const isFile = (p) => { try { return statSync(p).isFile(); } catch { return false; } };
const mdFiles = (dir) => {
  try {
    return readdirSync(dir)
      .map((f) => join(dir, f))
      .filter((p) => p.toLowerCase().endsWith('.md') && isFile(p));
  } catch { return []; }
};

// agents/ 配下: フラット agents/<name>.md と ディレクトリ agents/<name>/AGENT.md
function agentDefs(dir, scope) {
  const out = mdFiles(dir).map((p) => ({
    name: basename(p, '.md'), kind: 'agent', path: p, scope }));
  for (const e of (isDir(dir) ? readdirSync(dir) : [])) {
    const sub = join(dir, e);
    if (!isDir(sub)) continue;
    const def = AGENT_DEF_NAMES.map((n) => join(sub, n)).find(isFile);
    if (def) out.push({ name: e, kind: 'agent', path: def, scope });
  }
  return out;
}

// .devin/ 相当ディレクトリ内のルール + agents 定義を収集
function scanConfDir(dir, scope, prefix) {
  const out = [];
  const gr = join(dir, 'global_rules.md');
  if (isFile(gr)) out.push({ name: prefix + 'global_rules.md', kind: 'rules', path: gr, scope });
  for (const p of mdFiles(join(dir, 'rules')))
    out.push({ name: prefix + 'rules/' + basename(p), kind: 'rules', path: p, scope });
  out.push(...agentDefs(join(dir, 'agents'), scope));
  return out;
}

// ルート直下のルールファイル + .devin/.agents 配下を収集
function scanRoot(root, scope) {
  const out = [];
  for (const f of RULE_FILES) {
    const p = join(root, f);
    if (isFile(p)) out.push({ name: f, kind: 'rules', path: p, scope });
  }
  out.push(...scanConfDir(join(root, '.devin'), scope, '.devin/'));
  out.push(...agentDefs(join(root, '.agents', 'agents'), scope));
  return out;
}

const appDataDevin = () =>
  join(process.env.APPDATA ?? join(homedir(), 'AppData', 'Roaming'), 'devin');

// グローバル配置: %APPDATA%/devin, ~/.config/devin, ~/.devin, ~/.agents
function scanGlobal() {
  const out = [];
  for (const conf of [appDataDevin(), join(homedir(), '.config', 'devin')]) {
    for (const f of ['AGENTS.md', 'AGENTS.local.md', 'AGENT.md']) {
      const p = join(conf, f);
      if (isFile(p)) out.push({ name: f, kind: 'rules', path: p, scope: 'global' });
    }
    out.push(...scanConfDir(conf, 'global', ''));
  }
  out.push(...scanConfDir(join(homedir(), '.devin'), 'global', '~/.devin/'));
  out.push(...agentDefs(join(homedir(), '.agents', 'agents'), 'global'));
  return out;
}

// roots: [workspace ROOT, ...config.repos[].path]
export function listAgentDocs(roots) {
  const seen = new Set();
  const out = [];
  for (const root of new Set(roots.filter(Boolean))) {
    const scope = basename(root) || root;
    for (const d of scanRoot(root, scope)) {
      if (seen.has(d.path)) continue;
      seen.add(d.path); out.push(d);
    }
  }
  for (const d of scanGlobal()) {
    if (seen.has(d.path)) continue;
    seen.add(d.path); out.push(d);
  }
  return out;
}
