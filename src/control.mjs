// KASUGAI Agent — control.mjs
// HTTP/SSE ハブ。状態の唯一の正（store）・パネル配信・チャット管理・API を持つ。
// 状態遷移（課題ライフサイクル）は pipeline.mjs の責務（M1 以降）。
// ここが管轄するのは「チャット（manual detector の会話入口）」と配信面。
import http from 'node:http';
import { readFileSync, writeFileSync, existsSync, mkdirSync, readdirSync,
         statSync } from 'node:fs';
import { join, dirname, extname, normalize } from 'node:path';
import { fileURLToPath } from 'node:url';
import { randomBytes } from 'node:crypto';
import { AcpChat } from './acp-chat.mjs';
import { listAgentDocs } from './registry/agents.mjs';
import { listSkillDocs } from './registry/skills.mjs';

const ROOT = dirname(dirname(fileURLToPath(import.meta.url)));
const PANEL = join(ROOT, 'panel');
const UPLOADS = join(ROOT, 'chat-uploads');

const loadJ = (p, d) => { try { return JSON.parse(readFileSync(p, 'utf8')); } catch { return d; } };
const saveJ = (p, o) => { try { writeFileSync(p, JSON.stringify(o, null, 2)); } catch { /* noop */ } };

const config = Object.assign(
  loadJ(join(ROOT, 'config.example.json'), {}),
  loadJ(join(ROOT, 'config.json'), {}));
const PORT = +(process.env.PORT ?? config.controlPort ?? 8586);

// ---------- SSE ----------
const sseClients = new Set();
function sse(event, data) {
  const line = `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`;
  for (const res of sseClients) { try { res.write(line); } catch { /* noop */ } }
}

// ---------- 永続化 ----------
const CHATS_PATH = join(ROOT, 'chats.json');       // {id:{label,repo,sessionId,updatedAt}}
const OPTS_PATH = join(ROOT, 'acp-options.json');  // {configId: value}（全チャット共通の既定）
const CAND_PATH = join(ROOT, 'candidates.json');   // manual 課題化の受け皿

// ---------- SkillOps/AgentOps doc browser（read-only スキャン） ----------
const docRoots = () => [...new Set(
  [ROOT, ...(config.repos ?? []).map((r) => r.path)].filter(Boolean))];
const agentDocs = () => listAgentDocs(docRoots());
const skillDocs = () => listSkillDocs(docRoots());

// ---------- チャット管理 ----------
const chats = new Map(); // id -> {chat:AcpChat, label, repo}
let chatSeq = 0;
const sseTimers = new Map(); // id -> debounce timer

function notifyChat(id) {
  const s = chats.get(id);
  if (s) s.updatedAt = new Date().toISOString();
  // session/update は高頻度なので 300ms デバウンスで SSE 化する
  clearTimeout(sseTimers.get(id));
  sseTimers.set(id, setTimeout(() => { sseTimers.delete(id); sse('chat', { id }); }, 300));
}

function persistChats() {
  const o = {};
  for (const [id, s] of chats) {
    o[id] = { label: s.label, repo: s.repo,
      sessionId: s.chat.savedSessionId ?? s.chat.sessionId ?? null,
      updatedAt: s.updatedAt ?? new Date().toISOString() };
  }
  saveJ(CHATS_PATH, o);
}

function makeChat(repo) {
  const chat = new AcpChat({
    devinExe: config.devinExe,
    devinExtraArgs: config.devinExtraArgs ?? [],
    cwd: repo?.path ?? ROOT,
    getPersistedOptions: () => loadJ(OPTS_PATH, {}),
    saveOption: (id, value) => {
      const o = loadJ(OPTS_PATH, {}); o[id] = value; saveJ(OPTS_PATH, o);
    },
    getMcpServers: () => [],
    getStallMinutes: () => +(config.chatStallMinutes ?? 30),
    onOptions: () => {},
    onChange: () => { /* per-id は newChat で包む */ },
  });
  return chat;
}

function newChat({ label, repo, sessionId, updatedAt } = {}) {
  const id = ++chatSeq;
  const chat = makeChat(repo);
  chat.savedSessionId = sessionId ?? null;
  const entry = { chat, label: label ?? `chat ${id}`, repo: repo ?? null,
    updatedAt: updatedAt ?? new Date().toISOString() };
  chat.onSessionId = () => persistChats();
  chat.onChange = () => notifyChat(id);
  chats.set(id, entry);
  persistChats();
  return id;
}

// chats.json から復元（プロセス再起動後も session/load で再開できる）
{
  const saved = loadJ(CHATS_PATH, {});
  for (const [id, s] of Object.entries(saved)) {
    const nid = newChat({ label: s.label, repo: s.repo,
      sessionId: s.sessionId, updatedAt: s.updatedAt });
    // id を元の番号へ揃える（永続化キーと会話の対応を維持）
    if (String(nid) !== id) {
      const e = chats.get(nid); chats.delete(nid); chats.set(+id, e);
      chatSeq = Math.max(chatSeq, +id);
      persistChats();
    }
  }
}

function chatList() {
  return [...chats.entries()].map(([id, s]) => ({
    id, label: s.label, busy: s.chat.busy, ready: s.chat.ready,
    live: !!s.chat.proc, sessionId: s.chat.sessionId,
    resumable: !s.chat.proc && !!s.chat.savedSessionId,
    updatedAt: s.updatedAt ?? null,
    repo: s.repo?.github ?? s.repo?.slug ?? null,
  }));
}

// ---------- HTTP ----------
const MIME = { '.html': 'text/html; charset=utf-8', '.js': 'text/javascript',
  '.css': 'text/css', '.json': 'application/json', '.png': 'image/png',
  '.svg': 'image/svg+xml', '.ico': 'image/x-icon' };

const json = (res, code, obj) => {
  res.writeHead(code, { 'content-type': 'application/json; charset=utf-8' });
  res.end(JSON.stringify(obj));
};
const readBody = (req) => new Promise((res) => {
  let b = ''; req.on('data', (c) => { b += c; if (b.length > 64 * 1024 * 1024) req.destroy(); });
  req.on('end', () => { try { res(JSON.parse(b || '{}')); } catch { res({}); } });
});

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url, 'http://localhost');
  const p = url.pathname;

  // SSE
  if (p === '/api/events') {
    res.writeHead(200, { 'content-type': 'text/event-stream',
      'cache-control': 'no-cache', connection: 'keep-alive' });
    res.write('retry: 2000\n\n');
    const hb = setInterval(() => { try { res.write(': hb\n\n'); } catch { /* noop */ } }, 15000);
    sseClients.add(res);
    req.on('close', () => { clearInterval(hb); sseClients.delete(res); });
    return;
  }

  // API
  if (p === '/api/state' && req.method === 'GET') {
    return json(res, 200, { ok: true, version: '0.2.0',
      port: PORT, uptime: process.uptime(),
      repos: (config.repos ?? []).map((r) => ({ slug: r.slug ?? r.github, path: r.path })),
      chats: chatList() });
  }
  if (p === '/api/chat/sessions' && req.method === 'GET') {
    return json(res, 200, { ok: true, sessions: chatList() });
  }
  if (p === '/api/chat/options' && req.method === 'GET') {
    // セッション未開始でも選択肢を出すため、既知 options を最初のチャット or キャッシュから返す
    const any = [...chats.values()].find((s) => s.chat.options?.length);
    return json(res, 200, { ok: true,
      options: any?.chat.snapshot().options ?? [], persisted: loadJ(OPTS_PATH, {}) });
  }
  if (p === '/api/chat' && req.method === 'GET') {
    const id = +url.searchParams.get('id');
    const s = chats.get(id);
    if (!s) return json(res, 200, { ok: true, messages: [], options: loadJ(OPTS_PATH, {}) });
    return json(res, 200, { ok: true, id, label: s.label, ...s.chat.snapshot() });
  }
  if (p === '/api/chat/send' && req.method === 'POST') {
    const a = await readBody(req);
    let id = a.id;
    if (id == null || !chats.get(id)) {
      const repo = (config.repos ?? []).find((r) => (r.slug ?? r.github) === a.repo)
        ?? (config.repos ?? [])[0] ?? null;
      id = newChat({ label: String(a.text ?? '').slice(0, 24) || undefined, repo });
    }
    const s = chats.get(id);
    try {
      await s.chat.send(String(a.text ?? ''));
      return json(res, 200, { ok: true, id });
    } catch (e) {
      return json(res, 500, { ok: false, id, error: e.message });
    }
  }
  if (p === '/api/chat/cancel' && req.method === 'POST') {
    const a = await readBody(req);
    const s = chats.get(+a.id);
    if (s) await s.chat.cancel();
    return json(res, 200, { ok: true });
  }
  if (p === '/api/chat/clear' && req.method === 'POST') {
    const a = await readBody(req);
    chats.get(+a.id)?.chat.clear();
    return json(res, 200, { ok: true });
  }
  if (p === '/api/chat/reset' && req.method === 'POST') {
    const a = await readBody(req);
    const s = chats.get(+a.id);
    if (s) { await s.chat.reset(); persistChats(); }
    sse('chat', { id: +a.id });
    return json(res, 200, { ok: true });
  }
  if (p === '/api/chat/close' && req.method === 'POST') {
    const a = await readBody(req);
    const s = chats.get(+a.id);
    if (s) { try { await s.chat.reset(); } catch { /* noop */ } chats.delete(+a.id); persistChats(); }
    sse('chats', {});
    return json(res, 200, { ok: true });
  }
  if (p === '/api/chat/rename' && req.method === 'POST') {
    const a = await readBody(req);
    const s = chats.get(+a.id);
    if (s && a.label) {
      s.label = String(a.label).slice(0, 60);
      s.updatedAt = new Date().toISOString();
      persistChats();
    }
    sse('chats', {});
    return json(res, 200, { ok: true });
  }
  if (p === '/api/chat/option' && req.method === 'POST') {
    const a = await readBody(req);
    const o = loadJ(OPTS_PATH, {}); o[a.id] = a.value; saveJ(OPTS_PATH, o);
    const sid = a.sid != null ? +a.sid : null;
    const s = sid != null ? chats.get(sid) : null;
    if (s) {
      try { await s.chat.setOption(a.id, a.value); } catch (e) {
        return json(res, 500, { ok: false, error: e.message });
      }
    }
    sse('chat', { id: sid });
    return json(res, 200, { ok: true });
  }
  if (p === '/api/chat/attach' && req.method === 'POST') {
    const a = await readBody(req);
    mkdirSync(UPLOADS, { recursive: true });
    const files = [];
    for (const f of (a.attachments ?? []).slice(0, 8)) {
      const name = String(f.name ?? 'file').replace(/[^\w.-]/g, '_').slice(0, 80);
      const m = String(f.data ?? '').match(/^data:.*?;base64,(.*)$/s);
      if (!m) continue;
      const dest = join(UPLOADS, `${Date.now()}-${randomBytes(3).toString('hex')}-${name}`);
      try { writeFileSync(dest, Buffer.from(m[1], 'base64')); files.push({ name, path: dest }); }
      catch { /* noop */ }
    }
    return json(res, 200, { ok: true, files });
  }
  // manual detector 相当: チャットやUIから課題を候補として登録する
  if (p === '/api/candidates' && req.method === 'POST') {
    const a = await readBody(req);
    if (!a.title) return json(res, 400, { ok: false, error: 'title required' });
    const list = loadJ(CAND_PATH, []);
    const cand = { source: 'manual', repo: a.repo ?? null,
      title: String(a.title), body: String(a.body ?? ''),
      symptomHash: 'manual-' + randomBytes(4).toString('hex'),
      detectedAt: new Date().toISOString() };
    list.push(cand);
    saveJ(CAND_PATH, list);
    sse('candidates', {});
    return json(res, 200, { ok: true, candidate: cand });
  }
  if (p === '/api/candidates' && req.method === 'GET') {
    return json(res, 200, { ok: true, candidates: loadJ(CAND_PATH, []) });
  }
  // SkillOps/AgentOps doc browser
  if (p === '/api/agents' && req.method === 'GET') {
    return json(res, 200, { ok: true, entries: agentDocs() });
  }
  if (p === '/api/skills' && req.method === 'GET') {
    return json(res, 200, { ok: true, entries: skillDocs() });
  }
  if (p === '/api/doc' && req.method === 'GET') {
    // 収集済み doc のパスのみ本文を返す（任意ファイル参照を防ぐ）
    const target = normalize(String(url.searchParams.get('path') ?? '')).toLowerCase();
    const hit = [...agentDocs(), ...skillDocs()]
      .find((d) => normalize(d.path).toLowerCase() === target);
    if (!hit) return json(res, 404, { ok: false, error: 'not found' });
    let text = '';
    try { text = readFileSync(hit.path, 'utf8'); } catch { /* noop */ }
    const truncated = text.length > 200 * 1024;
    return json(res, 200, { ok: true, ...hit,
      text: truncated ? text.slice(0, 200 * 1024) : text, truncated });
  }

  // 静的ファイル（panel/）
  const rel = p === '/' ? '/index.html' : p;
  const file = normalize(join(PANEL, rel));
  if (!file.startsWith(PANEL)) return json(res, 403, { ok: false });
  if (existsSync(file) && statSync(file).isFile()) {
    res.writeHead(200, { 'content-type': MIME[extname(file)] ?? 'application/octet-stream' });
    return res.end(readFileSync(file));
  }
  json(res, 404, { ok: false, error: 'not found' });
});

server.listen(PORT, '127.0.0.1', () => {
  console.log(`[kasugai_agent] control :${PORT}  http://127.0.0.1:${PORT}/`);
});
