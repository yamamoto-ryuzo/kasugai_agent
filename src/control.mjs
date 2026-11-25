// KASUGAI Agent — control.mjs
// HTTP/SSE ハブ。状態の唯一の正（store）・パネル配信・チャット管理・API を持つ。
// 状態遷移（課題ライフサイクル）は pipeline.mjs の責務（M1 以降）。
// ここが管轄するのは「チャット（manual detector の会話入口）」と配信面。
import http from 'node:http';
import { readFileSync, writeFileSync, existsSync, mkdirSync, readdirSync,
         statSync, rmSync, watch } from 'node:fs';
import { join, dirname, extname, normalize, basename } from 'node:path';
import { fileURLToPath } from 'node:url';
import { randomBytes } from 'node:crypto';
import { execSync, spawn } from 'node:child_process';
import { AcpChat } from './acp-chat.mjs';
import { HttpChat } from './http-chat.mjs';
import { normPath, extractPaths, findConflicts, setupClaims,
         releaseWaiters } from './claims.mjs';
import { extractSplits, splitKey, PARALLEL_PREAMBLE } from './split.mjs';
import { VERIFY_PROMPT, extractVerdict, buildBackPrompt } from './verify.mjs';
import { suggestRepo, repoSlug } from './repomatch.mjs';
import { SnapshotStore } from './snapshots.mjs';
import { listAgentDocs } from './registry/agents.mjs';
import { listSkillDocs } from './registry/skills.mjs';
import { listRefDocs, listAllRefPaths, denied, SKIP_DIRS }
  from './registry/refs.mjs';
import { conlog, consoleTail, onConsoleLine, hookOwnConsole }
  from './conlog.mjs';

const ROOT = dirname(dirname(fileURLToPath(import.meta.url)));
const PANEL = join(ROOT, 'panel');
const UPLOADS = join(ROOT, 'chat-uploads');
const SNAPS = join(ROOT, 'chat-snapshots');    // chat 変更ファイルのスナップショット

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

// ---------- コンソール出力の集約（パネルの console タブ用） ----------
// control 自身の stdio をリングバッファへも写し、実行体プロセスの出力は
// newChat で chat.onConsole 経由 conlog へ繋ぐ。追加分は stderr が高頻度
// なので 250ms のバッチで SSE push する
hookOwnConsole();
const conQueue = [];
let conTimer = null;
onConsoleLine((line) => {
  conQueue.push(line);
  if (!conTimer) conTimer = setTimeout(() => {
    conTimer = null;
    sse('console', { lines: conQueue.splice(0) });
  }, 250);
});

// ---------- 永続化 ----------
const CHATS_PATH = join(ROOT, 'chats.json');       // {id:{label,repo,sessionId,updatedAt}}
const OPTS_PATH = join(ROOT, 'acp-options.json');  // {configId: value}（全チャット共通の既定）
const CAND_PATH = join(ROOT, 'candidates.json');   // manual 課題化の受け皿
const SECRETS_PATH = join(ROOT, 'secrets.json');   // {tokens:{key:value}}（Git 管理外）

// 接続AI の Bearer トークン/APIキー。secrets.json の tokens[<tokenKey ?? id>]
// を参照し、config.json・ログ・プロンプトには書き込まない
const secretsToken = (a) => {
  const s = loadJ(SECRETS_PATH, {});
  return s.tokens?.[a.tokenKey ?? a.id] ?? null;
};
const secretsSetToken = (key, value) => {
  const s = loadJ(SECRETS_PATH, {});
  s.tokens = s.tokens ?? {};
  if (value === '' || value == null) delete s.tokens[key];
  else s.tokens[key] = String(value);
  saveJ(SECRETS_PATH, s);
};

// ---------- SkillOps/AgentOps doc browser（read-only スキャン） ----------
// repo slug 指定時はその path のみ（+ registry 側で global 配置）に絞る
const docRoots = (slug) => {
  const all = [...new Set(
    [ROOT, ...(config.repos ?? []).map((r) => r.path)].filter(Boolean))];
  if (!slug) return all;
  const hit = (config.repos ?? []).find((r) => (r.slug ?? r.github) === slug);
  return hit?.path ? [hit.path] : [];
};
const agentDocs = (slug) => listAgentDocs(docRoots(slug));
const skillDocs = (slug) =>
  listSkillDocs(docRoots(slug), {
    devinExe: config.devinExe ?? config.chatAgents?.devin?.exe });

// ---------- repos 一覧（config.repos 登録済み + IDEワークスペース配下の検出） ----------
// IDE管理下 = ワークスペースルート（config.workspaceRoot、既定は ROOT の親）
// 直下の .git 付きディレクトリ。未登録でも設定ビューの一覧へ出す
const normP = (p) =>
  String(p ?? '').replace(/\\/g, '/').replace(/\/+$/, '').toLowerCase();
const WS_ROOT = String(config.workspaceRoot ?? dirname(ROOT))
  .replace(/\\/g, '/').replace(/\/+$/, '');
const isUnderWs = (p) => normP(p).startsWith(normP(WS_ROOT) + '/');
// エディター（ツリー/本文）の許可境界: 登録 repo の配下のみ
const underRepo = (p) => {
  const n = normP(p);
  return (config.repos ?? []).some((r) => {
    const root = normP(r.path);
    return root && (n === root || n.startsWith(root + '/'));
  });
};
const repoPub = (r) => ({ slug: r.slug ?? r.github ?? null, path: r.path,
  autoTrigger: !!r.autoTrigger, triggerLabel: r.triggerLabel ?? 'devin',
  requirePushApproval: r.requirePushApproval !== false });
function repoEntries() {
  const reg = (config.repos ?? []).map((r) => ({ ...repoPub(r),
    registered: true, ide: isUnderWs(r.path) }));
  const seen = new Set(reg.map((r) => normP(r.path)));
  try {
    for (const e of readdirSync(WS_ROOT, { withFileTypes: true })) {
      if (!e.isDirectory()) continue;
      const p = join(WS_ROOT, e.name);
      if (seen.has(normP(p)) || !existsSync(join(p, '.git'))) continue;
      reg.push({ slug: null, path: p.replace(/\\/g, '/'),
        registered: false, ide: true });
    }
  } catch { /* noop */ }
  return reg;
}
const persistConfig = () => {
  const cur = loadJ(join(ROOT, 'config.json'), {});
  cur.repos = config.repos;
  cur.chatAgents = config.chatAgents;
  cur.chatAgent = config.chatAgent;
  saveJ(join(ROOT, 'config.json'), cur);
};

// ---------- カレント repo の git 操作（パネルのコミット/プッシュボタン） ----------
// 登録済み repo のみ対象。実行は spawn で非同期・60s timeout、
// 出力は conlog へ集約 → パネルの console タブへ SSE push される
const gitRepo = (slug) => (config.repos ?? []).find((r) =>
  (r.slug ?? r.github) === slug || normP(r.path) === normP(slug));
const runGit = (cwd, args) => new Promise((resolve) => {
  let cp;
  try { cp = spawn('git', args, { cwd, windowsHide: true }); }
  catch (e) { resolve({ code: -1, out: String(e.message ?? e) }); return; }
  let out = '';
  const kill = setTimeout(() => { try { cp.kill(); } catch { /* noop */ } }, 60000);
  cp.stdout.on('data', (d) => { out += d; });
  cp.stderr.on('data', (d) => { out += d; });
  cp.on('error', (e) => {
    clearTimeout(kill); resolve({ code: -1, out: String(e.message ?? e) }); });
  cp.on('close', (code) => {
    clearTimeout(kill); resolve({ code: code ?? -1, out: out.trim() }); });
});
const gitLog = (slug, r) => {
  for (const l of r.out.split(/\r?\n/).filter(Boolean).slice(0, 30))
    conlog('git', r.code ? 'err' : 'out', `[${slug}] ${l}`);
};

// ---------- 接続AI（ACP エージェント）レジストリ ----------
// config.chatAgents = {id: {label, exe, args, auth}}、config.chatAgent = active id。
// devinExe/devinExtraArgs は旧形式のフォールバック（devin エントリを補完）
function chatAgents() {
  const agents = { ...(config.chatAgents ?? {}) };
  if (config.devinExe && !agents.devin) {
    agents.devin = { label: 'Devin', exe: config.devinExe,
      args: ['acp', ...(config.devinExtraArgs ?? [])], auth: 'devin' };
  }
  const ids = Object.keys(agents);
  const active = agents[config.chatAgent] ? config.chatAgent : ids[0] ?? null;
  return { active, agents };
}
// type: acp=ACP CLI spawn / agent=Canvas /agent HTTP / openai=chat/completions。
// 省略時は exe があれば acp、endpoint があれば agent とみなす
const agentType = (a) =>
  a.type === 'agent' || a.type === 'openai' ? a.type
    : a.exe ? 'acp' : a.endpoint ? 'agent' : 'acp';
const agentPub = (id, a) => ({
  id, label: a.label ?? id, type: agentType(a),
  exe: a.exe ?? '', args: a.args ?? [], auth: a.auth ?? 'none',
  endpoint: a.endpoint ?? '', provider: a.provider ?? '',
  model: a.model ?? '', project: a.project ?? '',
  tokenKey: a.tokenKey ?? '',
  hasToken: !!secretsToken({ ...a, id }) });

// ---------- チャット管理 ----------
const chats = new Map(); // id -> {chat:AcpChat, label, repo}
let chatSeq = 0;
const sseTimers = new Map(); // id -> debounce timer
const busyPrev = new Map();  // id -> 前回 busy（待機解放の判定用）

// ---------- ファイルクレーム / 実装待機 ----------
// 競合判定のロジックは claims.mjs。ここでは検出した衝突を待機状態へ繋ぐだけ

function notifyChat(id) {
  const s = chats.get(id);
  if (s) s.updatedAt = new Date().toISOString();
  // busy 終了を検知して、この chat にブロックされていた待機 chat を解放する
  const was = busyPrev.get(id) ?? false;
  const now = s?.chat.busy ?? false;
  if (s) busyPrev.set(id, now); else busyPrev.delete(id);
  if (was && !now) { releaseWaiters(chats, id); onChatDone(id); }
  // session/update は高頻度なので 300ms デバウンスで SSE 化する
  clearTimeout(sseTimers.get(id));
  sseTimers.set(id, setTimeout(() => { sseTimers.delete(id); sse('chat', { id }); }, 300));
}

// ---------- 並列モード（execMode=parallel → 課題分割と子 chat 集約） ----------
// parallel chat の初回 prompt には PARALLEL_PREAMBLE が前置され、
// エージェントが ```kasugai-split ブロックで [{title,prompt}] を出すと
// control が子 chat（parent=親id・direct 固定・同 repo・同 agent）を
// 自動起動する。子の busy 解除で最終回答をダイジェストとして親へ投函し、
// 親が統合・追検討・さらなる分割を続けられる。上限・重複抑止は親単位。
const parMaxChildren = () => +(config.parallelMaxChildren ?? 4);
const parMaxDepth = () => +(config.parallelMaxDepth ?? 1);
const childIds = (id) => [...chats.entries()]
  .filter(([, c]) => c.parent === id).map(([cid]) => cid);

// 検証の差し戻し（verdict NG → 再実装）回数の上限（config.verifyBackLimit）
const backLimit = () => +(config.verifyBackLimit ?? 3);

// 検証ターン（verifyScan 以降の agent メッセージ）から kasugai-verify verdict を
// 取り出す。同ターンに複数あれば最後のものを採用（scanVerdict 内で末尾勝ち）
function scanVerdict(s) {
  const msgs = s.chat.messages;
  let v = null;
  for (const m of msgs.slice(Math.min(s.verifyScan ?? msgs.length, msgs.length))) {
    if (m.role !== 'agent') continue;
    v = extractVerdict(m.text) ?? v;
  }
  return v;
}

// busy→idle のフック。検証遷移（実装ターンの終了 → verify）を先に行い、
// 子なら親へ集約、parallel かつ深度内なら分割を走査する
function onChatDone(id) {
  const s = chats.get(id);
  if (!s) return;
  if (s.verify === 'running') {
    // verdict が NG なら差し戻し（実装済み/未実装に分離した再実装 prompt）。
    // 差し戻しターンは通常の実装ターンとして走り、終了時に再び verify へ戻る。
    // 回数は verifyBackLimit まで。verdict が OK/未取得なら従来どおり
    // touched あり → retryLimit 内で再検証待ちへ戻す
    const v = scanVerdict(s);
    if (v && v.ok === false) {
      s.verify = null;
      s.verifyCount = 0;
      if ((s.verifyBack ?? 0) < backLimit()) {
        s.verifyBack = (s.verifyBack ?? 0) + 1;
        persistChats();
        sse('chats', {});
        sendToChat(id, buildBackPrompt(v),
          `[差し戻し ${s.verifyBack}回目] 検証NG → 再実装`)
          .catch((e) => s.chat.push('sys', `[差し戻し失敗] ${e.message}`));
      } else {
        s.verifyBack = 0;
        s.chat.push('sys',
          `[検証] 差し戻し上限（${backLimit()}回）に達したため終了します`);
        persistChats();
        sse('chats', {});
      }
    } else {
      s.verify = s.chat.touched.size > 0 &&
        s.verifyCount < +(config.retryLimit ?? 3) ? 'pending' : null;
      if (!s.verify) { s.verifyCount = 0; s.verifyBack = 0; }
      persistChats();
    }
  } else if (s.chat.touched.size > 0) {
    s.verify = 'pending';
    s.verifyCount = 0;
    persistChats();
  }
  if (s.parent != null) aggregateToParent(id);
  if (s.execMode === 'parallel' && s.depth < parMaxDepth()) spawnSplits(id);
  if (s.verify === 'pending' && config.autoVerify !== false) startVerify(id);
}

// 検証ターンの起動。verify=running にして検証 prompt を送る。
// 送信自体は通常の sendToChat 経路（待機・ファイル競合の判定も同じ）
function startVerify(id) {
  const s = chats.get(id);
  if (!s || s.chat.busy || s.verify === 'running') return false;
  s.verify = 'running';
  s.verifyCount = (s.verifyCount ?? 0) + 1;
  // verdict の走査開始位置: 検証ターンで新たに出た agent メッセージのみ見る
  s.verifyScan = s.chat.messages.length;
  persistChats();
  sse('chats', {});
  sendToChat(id, VERIFY_PROMPT, '[検証] 実装を実機で確認してください')
    .catch((e) => s.chat.push('sys', `[検証起動失敗] ${e.message}`));
  return true;
}

// ターン終了ごとに新着 agent メッセージを走査し、kasugai-split の要素を
// 子 chat として起動する。seenSplits は chats.json へ永続化済みのため
// control 再起動後に履歴を再走査しても二重起動しない
function spawnSplits(id) {
  const s = chats.get(id);
  if (!s) return;
  const msgs = s.chat.messages;
  if (s.splitScan > msgs.length) s.splitScan = msgs.length;  // 500件切り詰め対策
  const fresh = msgs.slice(s.splitScan);
  s.splitScan = msgs.length;
  let spawned = false;
  for (const m of fresh) {
    if (m.role !== 'agent') continue;
    for (const sub of extractSplits(m.text)) {
      const key = splitKey(sub);
      if (s.seenSplits.has(key)) continue;
      s.seenSplits.add(key);
      if (childIds(id).length >= parMaxChildren()) {
        s.chat.push('sys',
          `[並列] 子チャット上限（${parMaxChildren()}）のため「${sub.title}」は起動しません`);
        continue;
      }
      const cid = newChat({ label: sub.title, repo: s.repo,
        agent: s.chat.agent?.id, mode: 'direct',
        parent: id, depth: s.depth + 1 });
      spawned = true;
      s.chat.push('sys',
        `[並列] 分割タスク「${sub.title}」→ 子チャット #${cid} を起動しました`);
      sendToChat(cid,
        `[親チャット #${id} の分割タスク「${sub.title}」]\n${sub.prompt}`)
        .catch((e) => chats.get(cid)?.chat.push('sys', `[起動失敗] ${e.message}`));
    }
  }
  if (spawned || s.seenSplits.size) { persistChats(); }
  if (spawned) sse('chats', {});
}

// 子 chat の完了を親へダイジェスト投函（親 busy → steer / waiting → 保留キュー）
function aggregateToParent(id) {
  const c = chats.get(id);
  const p = c?.parent != null ? chats.get(c.parent) : null;
  if (!c || !p) return;
  const last = [...c.chat.messages].reverse()
    .find((m) => m.role === 'agent' && m.text.trim());
  const body = (last?.text ?? '').trim().slice(0, 3000) || '（応答なし）';
  p.chat.push('sys',
    `[並列] 子チャット #${id}「${c.label}」が完了 → 結果を統合します`);
  sendToChat(c.parent,
    `[子チャット #${id}「${c.label}」の完了報告]\n` +
    `以下は子チャットの最終回答です。内容を踏まえて統合・追検討を続けてください:\n\n${body}`)
    .catch((e) => p.chat.push('sys', `[集約失敗] ${e.message}`));
}

// 内部経路の送信（子 chat 起動・集約ダイジェスト）。/api/chat/send と同じく
// 待機中は保留キュー、prompt 文中パスのファイル競合は実装待機へ繋ぐ
async function sendToChat(id, text, display) {
  const s = chats.get(id);
  if (!s) return;
  if (s.chat.waiting) { await s.chat.send(text, display); return; }
  const hits = findConflicts(chats, id, extractPaths(text, s.chat.cwd));
  if (hits.length) {
    s.chat.hold(text, { files: hits.flatMap((h) => h.files),
      blockedBy: hits.map((h) => h.id) }, display);
    return;
  }
  setupClaims(s.chat, text);
  await s.chat.send(text, display);
}

function persistChats() {
  const o = {};
  for (const [id, s] of chats) {
    o[id] = { label: s.label, repo: s.repo, agent: s.chat.agent?.id ?? null,
      sessionId: s.chat.savedSessionId ?? s.chat.sessionId ?? null,
      updatedAt: s.updatedAt ?? new Date().toISOString(),
      mode: s.execMode, parent: s.parent, depth: s.depth,
      primedFor: s.primedFor, seenSplits: [...s.seenSplits],
      repoDeclined: [...(s.repoDeclined ?? [])],
      verify: s.verify ?? null, verifyCount: s.verifyCount ?? 0,
      verifyBack: s.verifyBack ?? 0 };
  }
  saveJ(CHATS_PATH, o);
}

function makeChat(repo, agentId) {
  const { active, agents } = chatAgents();
  const aid = agents[agentId] ? agentId : active;
  // chat 作成時の実行体に固定（active 切替は以後の新規 chat に適用）
  const spec = aid ? { id: aid, ...agents[aid] } : undefined;
  const common = {
    agent: spec,
    cwd: repo?.path ?? ROOT,
    getPersistedOptions: () => loadJ(OPTS_PATH, {}),
    saveOption: (id, value) => {
      const o = loadJ(OPTS_PATH, {}); o[id] = value; saveJ(OPTS_PATH, o);
    },
    getMcpServers: () => [],
    getStallMinutes: () => +(config.chatStallMinutes ?? 30),
    onOptions: () => {},
    onChange: () => { /* per-id は newChat で包む */ },
  };
  if (spec && agentType(spec) !== 'acp')
    return new HttpChat({ ...common, getToken: secretsToken });
  return new AcpChat({
    ...common,
    devinExe: config.devinExe,
    devinExtraArgs: config.devinExtraArgs ?? [],
  });
}

function newChat({ label, repo, sessionId, updatedAt, agent, id: wantId,
                   mode, parent, depth, primedFor, seenSplits,
                   repoDeclined, verify, verifyCount, verifyBack } = {}) {
  const id = wantId ?? ++chatSeq;
  chatSeq = Math.max(chatSeq, id);
  const chat = makeChat(repo, agent);
  chat.savedSessionId = sessionId ?? null;
  // 変更復元: chat が触ったファイルの turn 単位変更前スナップショット
  const snaps = new SnapshotStore(join(SNAPS, String(id)));
  chat.snapTurn = snaps.lastTurn();
  // execMode: direct=逐次 / parallel=kasugai-split で子 chat を自動起動。
  // parent=親 chat id（子のみ）、depth=分割深度、splitScan=走査済み messages 位置、
  // primedFor=プリアンブルを届けた sessionId（false=未付与、HTTP型は null=付与済）、
  // seenSplits=発射済み分割キー、
  // repoDeclined=repo 不整合確認で「このまま送信」を選んだ候補（再確認しない）
  const entry = { chat, snaps, label: label ?? `chat ${id}`, repo: repo ?? null,
    updatedAt: updatedAt ?? new Date().toISOString(),
    execMode: mode === 'parallel' ? 'parallel' : 'direct',
    parent: parent ?? null, depth: depth ?? 0,
    primedFor: primedFor ?? false, splitScan: 0,
    seenSplits: new Set(seenSplits ?? []),
    repoDeclined: new Set(repoDeclined ?? []),
    // verify: null | 'pending'（検証待ち）| 'running'（検証ターン中）。
    // 復元時はプロセスが無いので running は pending へ落とす
    verify: verify === 'running' ? 'pending' : (verify ?? null),
    verifyCount: verifyCount ?? 0,
    // 検証NG→再実装への差し戻し回数（verifyBackLimit まで）。
    // verifyScan=検証ターン中の verdict 走査開始位置（永続化しない）
    verifyBack: verifyBack ?? 0, verifyScan: 0 };
  chat.onSessionId = () => persistChats();
  chat.onChange = () => notifyChat(id);
  // 実行体の stderr/非JSON stdout・spawn/exit をコンソールバッファへ
  chat.onConsole = (stream, text) =>
    conlog(`#${id} ${chat.agent?.label ?? 'agent'}`, stream, text);
  // 新ターン開始: prompt を復元ポイントのラベルにし、文中パスを変更前採取
  chat.onTurnStart = (turn, text) => {
    snaps.beginTurn(turn, text);
    for (const n of extractPaths(text, chat.cwd)) snaps.capture(n, turn);
    notifyChat(id);
  };
  // 編集系 tool_call の対象パスを touched へ登録し、他 busy chat のクレームと
  // 衝突したら実装待機へ移す（ターンは acp-chat 側で session/cancel される）。
  // busy 中の対象パスは turn 単位で変更前スナップショットも採取する
  chat.onFileTouch = (paths) => {
    const norm = paths.map((p) => normPath(p, chat.cwd)).filter(Boolean);
    for (const n of norm) chat.touched.add(n);
    if (chat.busy) for (const n of norm) snaps.capture(n, chat.snapTurn);
    if (chat.waiting || !norm.length) return;
    const hits = findConflicts(chats, id, norm);
    if (hits.length) chat.enterWait({
      files: hits.flatMap((h) => h.files),
      blockedBy: hits.map((h) => h.id), resume: true });
  };
  chats.set(id, entry);
  persistChats();
  return id;
}

// chats.json から復元（プロセス再起動後も session/load で再開できる）
{
  const saved = loadJ(CHATS_PATH, {});
  for (const [id, s] of Object.entries(saved)) {
    // id を元の番号へ揃える（永続化キー・chat-snapshots/<id> と会話の対応を維持）
    newChat({ label: s.label, repo: s.repo, agent: s.agent,
      sessionId: s.sessionId, updatedAt: s.updatedAt, id: +id,
      mode: s.mode, parent: s.parent, depth: s.depth,
      primedFor: s.primedFor, seenSplits: s.seenSplits,
      repoDeclined: s.repoDeclined,
      verify: s.verify, verifyCount: s.verifyCount,
      verifyBack: s.verifyBack });
  }
}

// 起動時の自動再開: resumable（savedSessionId 保持・プロセスなし）な chat を
// 全て session/load で再接続する。直列で1本ずつ起動し、失敗しても chat は
// resumable のまま残る（ensureSession 内で session/new へフォールバックする
// 場合もある）
if (config.autoResumeOnStart !== false) {
  (async () => {
    for (const s of chats.values()) {
      if (!s.chat.savedSessionId || s.chat.proc) continue;
      try { await s.chat.ensureSession(); }
      catch (e) { s.chat.push('sys', `[自動再開失敗] ${e.message}`); }
    }
  })();
}

// ---------- control 自己再起動（README §8） ----------
// busy/waiting な chat が残る間は延期し、空き次第「実行系停止 → 再起動」。
// 再起動は detached な respawner（node -e）が旧プロセス終了を検知してから
// 新 control を起動する方式（ポート競合と孤児化を防ぐ）
let restartPending = false;   // busy 解消待ちの延期中
let restarting = false;       // 再起動シーケンス実行済み（exit 待ち）
const busyIds = () => [...chats.entries()]
  .filter(([, s]) => s.chat.busy || s.chat.waiting).map(([id]) => id);
function maybeRestart(force) {
  if (restarting) return true;
  const ids = busyIds();
  if (ids.length && !force) {
    if (!restartPending)
      conlog('control', 'sys',
        `再起動を延期（busy: ${ids.map((i) => '#' + i).join(',')}）`);
    restartPending = true;
    sse('restart', { pending: true, busy: ids });
    setTimeout(maybeRestart, 2000);
    return false;
  }
  restartPending = false;
  restarting = true;
  conlog('control', 'sys',
    `control を再起動します${force && ids.length ? '（強制・実行中を中断）' : ''}`);
  sse('restart', { restarting: true, forced: !!force && !!ids.length });
  // 実行系停止: ACP プロセスを全て畳む（force 時は busy も含む）。
  // savedSessionId は残るので chats.json 経由で resumable になり、
  // 起動側の自動再接続へ回る
  for (const s of chats.values()) {
    try { s.chat.proc?.kill(); } catch { /* noop */ }
  }
  persistChats();
  const respawner = [
    'const{spawn}=require("node:child_process");',
    'const pp=+process.argv[1];',
    'const iv=setInterval(()=>{try{process.kill(pp,0)}catch{',
    'clearInterval(iv);',
    'const c=spawn(process.execPath,[process.argv[2]],',
    '{cwd:process.argv[3],detached:true,stdio:"ignore",windowsHide:true});',
    'c.unref();process.exit(0)}},300);',
    'setTimeout(()=>process.exit(1),120000).unref();',
  ].join('');
  try {
    spawn(process.execPath,
      ['-e', respawner, String(process.pid), join(ROOT, 'src', 'control.mjs'), ROOT],
      { detached: true, stdio: 'ignore', windowsHide: true }).unref();
  } catch { /* noop */ }
  setTimeout(() => {
    for (const res of sseClients) { try { res.end(); } catch { /* noop */ } }
    try { server.close(); } catch { /* noop */ }
    process.exit(0);
  }, 300);
  return true;
}

// code drift: src/*.mjs（非再帰）の変更を検知して自動再起動（README §8）。
// autoRestartOnDrift（既定 true）なら maybeRestart へ非 force で委ね、
// busy/waiting が空くまで延期される。エディタの連続保存・複数ファイル保存を
// デバウンスで1回に吸収する
if (config.autoRestartOnDrift !== false) {
  let driftTimer = null;
  try {
    watch(join(ROOT, 'src'), (ev, name) => {
      if (!name || !String(name).endsWith('.mjs')) return;
      clearTimeout(driftTimer);
      driftTimer = setTimeout(() => {
        conlog('control', 'sys',
          `code drift を検知（src/${String(name).replace(/\\/g, '/')}）→ 自動再起動`);
        maybeRestart();
      }, 1000);
    });
  } catch { /* noop */ }
}

function chatList() {
  return [...chats.entries()].map(([id, s]) => ({
    id, label: s.label, busy: s.chat.busy, ready: s.chat.ready,
    execMode: s.execMode, parent: s.parent, depth: s.depth,
    children: childIds(id),
    agent: s.chat.agent?.id ?? null, agentLabel: s.chat.agent?.label ?? null,
    agentType: s.chat.agent ? agentType(s.chat.agent) : 'acp',
    // HTTP 型は常駐プロセスを持たず、ready（endpoint 設定済み）を live とみなす
    live: !!s.chat.proc || (s.chat.transport === 'http' && s.chat.ready),
    sessionId: s.chat.sessionId,
    resumable: !s.chat.proc && !!s.chat.savedSessionId,
    waiting: !!s.chat.waiting,
    verify: s.verify ?? null, verifyBack: s.verifyBack ?? 0,
    waitInfo: s.chat.waiting
      ? { kind: s.chat.waiting.kind ?? 'conflict',
          files: s.chat.waiting.files, blockedBy: s.chat.waiting.blockedBy,
          held: s.chat.heldPrompts.length,
          suggest: s.chat.waiting.suggest ?? null }
      : null,
    updatedAt: s.updatedAt ?? null,
    repo: repoSlug(s.repo),
  }));
}

// chat の削除（個別 × と完了列「すべて削除」の共通処理）:
// セッションを畳み、台帳・スナップショット・待機ブロックを片付ける
const closeChatEntry = async (id) => {
  const s = chats.get(id);
  if (s) {
    try { await s.chat.reset(); } catch { /* noop */ }
    chats.delete(id);
    try { rmSync(join(SNAPS, String(id)), { recursive: true, force: true }); }
    catch { /* noop */ }
  }
  busyPrev.delete(id);
  releaseWaiters(chats, id);          // 閉じた chat にブロックされていた待機を解放
  return !!s;
};

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

  // コンソールタブ: 集約した裏側プロセス出力の履歴
  // （?after=<seq> で差分、?limit=<n> で直近件数。既定 800）
  if (p === '/api/console' && req.method === 'GET') {
    return json(res, 200, { ok: true,
      lines: consoleTail(+url.searchParams.get('after') || 0,
        +url.searchParams.get('limit') || 800) });
  }
  // API
  if (p === '/api/state' && req.method === 'GET') {
    return json(res, 200, { ok: true, version: '0.2.0',
      port: PORT, uptime: process.uptime(), restartPending,
      repos: (config.repos ?? []).map(repoPub),
      chats: chatList() });
  }
  // repos 一覧（登録済み + IDEワークスペース直下の .git 検出）
  if (p === '/api/repos' && req.method === 'GET') {
    return json(res, 200, { ok: true, workspace: WS_ROOT,
      entries: repoEntries() });
  }
  // 登録済み repo の設定更新（autoTrigger/triggerLabel/requirePushApproval）
  if (p === '/api/repos/update' && req.method === 'POST') {
    const a = await readBody(req);
    const repo = (config.repos ?? []).find((r) =>
      (r.slug ?? r.github) === a.slug || normP(r.path) === normP(a.path));
    if (!repo) return json(res, 404, { ok: false, error: 'repo not found' });
    if (a.autoTrigger != null) repo.autoTrigger = !!a.autoTrigger;
    if (a.triggerLabel != null)
      repo.triggerLabel = String(a.triggerLabel).trim() || 'devin';
    if (a.requirePushApproval != null)
      repo.requirePushApproval = !!a.requirePushApproval;
    persistConfig();
    sse('repos', {});
    return json(res, 200, { ok: true, repo: repoPub(repo) });
  }
  // 登録解除（config.repos から除去）
  if (p === '/api/repos/remove' && req.method === 'POST') {
    const a = await readBody(req);
    const i = (config.repos ?? []).findIndex((r) =>
      (r.slug ?? r.github) === a.slug || normP(r.path) === normP(a.path));
    if (i < 0) return json(res, 404, { ok: false, error: 'repo not found' });
    const removed = config.repos.splice(i, 1)[0];
    persistConfig();
    sse('repos', {});
    return json(res, 200, { ok: true, removed: repoPub(removed) });
  }
  // 監視リポジトリ登録（config.json の repos へ追記し SSE で即時反映）
  if (p === '/api/repos' && req.method === 'POST') {
    const a = await readBody(req);
    const rpath = String(a.path ?? '').trim().replace(/\\/g, '/').replace(/\/+$/, '');
    if (!rpath) return json(res, 400, { ok: false, error: 'path required' });
    if (!existsSync(rpath) || !statSync(rpath).isDirectory())
      return json(res, 400, { ok: false, error: 'not a directory' });
    let slug = String(a.slug ?? '').trim();
    if (!slug) {
      // git remote から owner/repo を推定。取れなければディレクトリ名
      try {
        const u = execSync('git remote get-url origin',
          { cwd: rpath, encoding: 'utf8', timeout: 5000, windowsHide: true,
            stdio: ['pipe', 'pipe', 'ignore'] }).trim();
        slug = u.match(/[:/]([^/:\s]+\/[^/\s]+?)(?:\.git)?$/)?.[1] ?? '';
      } catch { /* noop */ }
      slug ||= basename(rpath);
    }
    const dup = (config.repos ?? []).find((r) =>
      (r.slug ?? r.github) === slug || normP(r.path) === normP(rpath));
    if (dup) return json(res, 409,
      { ok: false, error: 'already registered', slug: dup.slug ?? dup.github });
    const repo = { slug, path: rpath, autoTrigger: false,
      triggerLabel: 'devin', requirePushApproval: true };
    config.repos = [...(config.repos ?? []), repo];
    persistConfig();
    sse('repos', {});
    return json(res, 200, { ok: true, repo: { slug: repo.slug, path: repo.path } });
  }
  // 登録フォームの path 選択用: 指定ディレクトリ直下のサブdir と .git 有無を返す
  // （loopback 限定の簡易ブラウザ。path 省略時はドライブルート一覧）
  if (p === '/api/fs/dirs' && req.method === 'GET') {
    const q = String(url.searchParams.get('path') ?? '').replace(/\\/g, '/')
      .replace(/\/{2,}/g, '/').trim();
    if (!q) {
      const roots = process.platform === 'win32'
        ? [...'ABCDEFGHIJKLMNOPQRSTUVWXYZ'].map((d) => d + ':/')
            .filter((d) => existsSync(d))
        : ['/'];
      return json(res, 200, { ok: true, path: '', parent: '',
        dirs: roots.map((d) => ({ name: d, path: d, git: false })) });
    }
    const dir = /^[A-Za-z]:\/$/.test(q) || q === '/' ? q : q.replace(/\/+$/, '');
    let st = null;
    try { st = statSync(dir); } catch { /* noop */ }
    if (!st?.isDirectory())
      return json(res, 400, { ok: false, error: 'not a directory' });
    const dirs = [];
    try {
      for (const e of readdirSync(dir, { withFileTypes: true })) {
        if (!e.isDirectory()) continue;
        const dp = (dir.endsWith('/') ? dir : dir + '/') + e.name;
        dirs.push({ name: e.name, path: dp,
          git: existsSync(join(dp, '.git')) });
        if (dirs.length >= 500) break;   // 巨大ディレクトリで応答を打ち切る
      }
    } catch (e2) {
      return json(res, 400, { ok: false, error: e2.message });
    }
    dirs.sort((a, b) => (b.git - a.git) || a.name.localeCompare(b.name));
    const parent = dirname(dir).replace(/\\/g, '/');
    return json(res, 200, { ok: true, path: dir,
      parent: parent === dir ? '' : parent, dirs });
  }
  // エディターのフォルダツリー用: 登録 repo 配下の1階層分（dir + file）を返す。
  // .git/node_modules 等は列挙しない。秘匿系は denied 付きで一覧のみ返す
  if (p === '/api/fs/tree' && req.method === 'GET') {
    const q = normalize(String(url.searchParams.get('path') ?? ''))
      .replace(/\\/g, '/').replace(/\/{2,}/g, '/').replace(/\/+$/, '').trim();
    if (!q || !underRepo(q))
      return json(res, 403, { ok: false, error: 'outside repos' });
    let st = null;
    try { st = statSync(q); } catch { /* noop */ }
    if (!st?.isDirectory())
      return json(res, 400, { ok: false, error: 'not a directory' });
    const dirs = [], files = [];
    try {
      for (const e of readdirSync(q, { withFileTypes: true })) {
        const ep = q + '/' + e.name;
        if (e.isDirectory()) {
          if (SKIP_DIRS.has(e.name)) continue;
          dirs.push({ name: e.name, path: ep });
        } else if (e.isFile()) {
          files.push({ name: e.name, path: ep, denied: denied(ep) });
        }
        if (dirs.length + files.length >= 1000) break;   // 巨大dirで応答を打ち切る
      }
    } catch (e2) {
      return json(res, 400, { ok: false, error: e2.message });
    }
    const byName = (a, b) => a.name.localeCompare(b.name);
    return json(res, 200, { ok: true, path: q,
      dirs: dirs.sort(byName), files: files.sort(byName) });
  }
  // エディターの本文表示用: 登録 repo 配下・非秘匿の実在ファイルのみ返す。
  // binary（NUL 含有・2MB 超）は本文を返さず binary フラグだけ返す
  if (p === '/api/fs/file' && req.method === 'GET') {
    const q = normalize(String(url.searchParams.get('path') ?? ''))
      .replace(/\\/g, '/').replace(/\/{2,}/g, '/').replace(/\/+$/, '').trim();
    if (!q || !underRepo(q))
      return json(res, 403, { ok: false, error: 'outside repos' });
    if (denied(q)) return json(res, 403, { ok: false, error: 'denied' });
    let st = null;
    try { st = statSync(q); } catch { /* noop */ }
    if (!st?.isFile())
      return json(res, 404, { ok: false, error: 'not found' });
    const head = { ok: true, name: q.split('/').pop(), path: q, scope: 'file' };
    if (st.size > 2 * 1024 * 1024)
      return json(res, 200, { ...head, binary: true, text: '', truncated: false });
    let text = '';
    try { text = readFileSync(q, 'utf8'); } catch { /* noop */ }
    if (text.slice(0, 8192).includes('\0'))
      return json(res, 200, { ...head, binary: true, text: '', truncated: false });
    const truncated = text.length > 200 * 1024;
    return json(res, 200, { ...head,
      text: truncated ? text.slice(0, 200 * 1024) : text, truncated });
  }
  // カレント repo の全変更をローカルコミット（git add -A && git commit -m）。
  // 変更が無ければ noop。リモートへの公開は行わない（push は別ボタン/verify のみ）
  if (p === '/api/git/commit' && req.method === 'POST') {
    const a = await readBody(req);
    const repo = gitRepo(String(a.slug ?? a.repo ?? ''));
    if (!repo) return json(res, 404, { ok: false, error: 'repo not found' });
    const message = String(a.message ?? '').trim().slice(0, 300);
    if (!message)
      return json(res, 400, { ok: false, error: 'message required' });
    const st = await runGit(repo.path, ['status', '--porcelain']);
    if (st.code !== 0) {
      gitLog(repo.slug, st);
      return json(res, 200, { ok: false, error: st.out.slice(0, 400) });
    }
    if (!st.out) return json(res, 200, { ok: true, noop: true });
    conlog('git', 'sys', `[${repo.slug}] git commit: ${message}`);
    const add = await runGit(repo.path, ['add', '-A']);
    if (add.code !== 0) {
      gitLog(repo.slug, add);
      return json(res, 200, { ok: false, error: add.out.slice(0, 400) });
    }
    const r = await runGit(repo.path, ['commit', '-m', message]);
    gitLog(repo.slug, r);
    if (r.code !== 0)
      return json(res, 200, { ok: false, error: r.out.slice(0, 400) });
    return json(res, 200, { ok: true, out: r.out.slice(0, 400) });
  }
  // カレント repo のコミットをリモートへプッシュ（パネル操作 = 人ゲートの承認）。
  // upstream 未設定のブランチ（初回 push）は origin（無ければ唯一の remote）へ
  // -u で送り、追跡関係を同時に設定する
  if (p === '/api/git/push' && req.method === 'POST') {
    const a = await readBody(req);
    const repo = gitRepo(String(a.slug ?? a.repo ?? ''));
    if (!repo) return json(res, 404, { ok: false, error: 'repo not found' });
    const br = (await runGit(repo.path, ['branch', '--show-current']))
      .out.split(/\r?\n/)[0]?.trim() ?? '';
    const up = br ? await runGit(repo.path,
      ['rev-parse', '--abbrev-ref', '--symbolic-full-name', '@{u}'])
      : { code: 1 };
    let args = ['push'];
    if (br && up.code !== 0) {
      const rem = (await runGit(repo.path, ['remote']))
        .out.split(/\r?\n/).filter(Boolean);
      const remote = rem.includes('origin') ? 'origin' : rem[0];
      if (remote) args = ['push', '-u', remote, br];
    }
    conlog('git', 'sys', `[${repo.slug}] git ${args.join(' ')}`);
    const r = await runGit(repo.path, args);
    gitLog(repo.slug, r);
    if (r.code !== 0)
      return json(res, 200, { ok: false, error: r.out.slice(0, 400) });
    return json(res, 200, { ok: true, out: r.out.slice(0, 400) });
  }
  // control 手動再起動（設定ビューのボタンからも呼ぶ。README §8）。
  // busy/waiting な chat が残る間は延期。{force:true} は中断して即時再起動
  if (p === '/api/control/restart' && req.method === 'POST') {
    const a = await readBody(req);
    return json(res, 200, { ok: true,
      pending: !maybeRestart(!!a.force), busy: busyIds() });
  }
  // 接続AI 一覧・active（config.chatAgents/chatAgent）
  if (p === '/api/chat-agents' && req.method === 'GET') {
    const { active, agents } = chatAgents();
    return json(res, 200, { ok: true, active,
      agents: Object.entries(agents).map(([id, a]) => agentPub(id, a)) });
  }
  // エージェント登録（type=acp: {exe,args,auth} / agent|openai:
  // {endpoint,provider,model,project}。token は secrets.json へ）
  if (p === '/api/chat-agents' && req.method === 'POST') {
    const a = await readBody(req);
    const exe = String(a.exe ?? '').trim();
    const endpoint = String(a.endpoint ?? '').trim();
    const type = a.type === 'openai' || a.type === 'agent' ? a.type
      : exe ? 'acp' : endpoint ? 'agent' : 'acp';
    if (type === 'acp' && !exe)
      return json(res, 400, { ok: false, error: 'exe required' });
    if (type !== 'acp' && !endpoint)
      return json(res, 400, { ok: false, error: 'endpoint required' });
    const { agents } = chatAgents();
    const id = String(a.id ?? a.label ?? '').trim().toLowerCase()
      .replace(/[^\w-]+/g, '-').replace(/^-+|-+$/g, '')
      || basename(exe || endpoint).replace(/\.\w+$/, '').toLowerCase();
    if (agents[id]) return json(res, 409,
      { ok: false, error: 'already registered', id });
    const entry = { label: String(a.label ?? id) || id };
    if (type === 'acp') {
      const args = Array.isArray(a.args) ? a.args.map(String)
        : String(a.args ?? '').split(/\s+/).filter(Boolean);
      entry.exe = exe;
      entry.args = args.length ? args : ['acp'];
      entry.auth = a.auth === 'devin' ? 'devin' : 'none';
    } else {
      entry.type = type;
      entry.endpoint = endpoint;
      if (String(a.provider ?? '').trim()) entry.provider = String(a.provider).trim();
      if (String(a.model ?? '').trim()) entry.model = String(a.model).trim();
      if (String(a.project ?? '').trim()) entry.project = String(a.project).trim();
      if (String(a.tokenKey ?? '').trim()) entry.tokenKey = String(a.tokenKey).trim();
    }
    config.chatAgents = agents;
    agents[id] = entry;
    if (a.token != null) secretsSetToken(entry.tokenKey ?? id, a.token);
    if (!config.chatAgent) config.chatAgent = id;
    persistConfig();
    sse('chatAgents', {});
    return json(res, 200, { ok: true, agent: agentPub(id, agents[id]) });
  }
  // 登録済みエージェントの更新（label + type 別フィールド。token は secrets へ、
  // 空文字で削除）
  if (p === '/api/chat-agents/update' && req.method === 'POST') {
    const a = await readBody(req);
    const { agents } = chatAgents();
    const id = String(a.id ?? '');
    const ag = agents[id];
    if (!ag) return json(res, 404, { ok: false, error: 'agent not found' });
    if (a.label != null) ag.label = String(a.label).trim() || ag.label;
    if (a.exe != null) ag.exe = String(a.exe).trim();
    if (a.args != null) ag.args = Array.isArray(a.args) ? a.args.map(String)
      : String(a.args).split(/\s+/).filter(Boolean);
    if (a.auth != null) ag.auth = a.auth === 'devin' ? 'devin' : 'none';
    for (const k of ['endpoint', 'provider', 'model', 'project', 'tokenKey'])
      if (a[k] != null)
        String(a[k]).trim() ? ag[k] = String(a[k]).trim() : delete ag[k];
    if (a.token != null) secretsSetToken(ag.tokenKey ?? id, a.token);
    config.chatAgents = agents;
    persistConfig();
    sse('chatAgents', {});
    return json(res, 200, { ok: true, agent: agentPub(id, ag) });
  }
  // 登録解除（active は解除不可。先に別エージェントを active にする）
  if (p === '/api/chat-agents/remove' && req.method === 'POST') {
    const a = await readBody(req);
    const { active, agents } = chatAgents();
    const id = String(a.id ?? '');
    if (!agents[id]) return json(res, 404, { ok: false, error: 'agent not found' });
    if (id === active) return json(res, 400,
      { ok: false, error: 'active agent cannot be removed' });
    config.chatAgents = agents;
    delete agents[id];
    persistConfig();
    sse('chatAgents', {});
    return json(res, 200, { ok: true, removed: id });
  }
  // active 切替（以後に作成する chat の実行体）
  if (p === '/api/chat-agents/active' && req.method === 'POST') {
    const a = await readBody(req);
    const { agents } = chatAgents();
    const id = String(a.id ?? '');
    if (!agents[id]) return json(res, 404, { ok: false, error: 'agent not found' });
    config.chatAgents = agents;
    config.chatAgent = id;
    persistConfig();
    sse('chatAgents', {});
    return json(res, 200, { ok: true, active: id });
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
    return json(res, 200, { ok: true, id, label: s.label, ...s.chat.snapshot(),
      snapPoints: s.snaps.points(), repo: repoSlug(s.repo),
      execMode: s.execMode, parent: s.parent, depth: s.depth,
      verify: s.verify ?? null, verifyBack: s.verifyBack ?? 0,
      children: childIds(id) });
  }
  if (p === '/api/chat/send' && req.method === 'POST') {
    const a = await readBody(req);
    let id = a.id;
    if (id == null || !chats.get(id)) {
      const repo = (config.repos ?? []).find((r) => (r.slug ?? r.github) === a.repo)
        ?? (config.repos ?? [])[0] ?? null;
      id = newChat({ label: String(a.text ?? '').slice(0, 24) || undefined, repo,
        mode: a.mode });
    }
    const s = chats.get(id);
    // mode 指定があれば既存 chat の execMode も更新（UI セレクトの現在値を送る想定）
    if ((a.mode === 'direct' || a.mode === 'parallel') && s.execMode !== a.mode) {
      s.execMode = a.mode;
      persistChats();
      sse('chats', {});
    }
    let text = String(a.text ?? '');
    let display;
    // parallel ではセッションごとに1回だけ分割指示プリアンブルを前置する。
    // session/load 後は履歴に残るので、届けた sessionId（primedFor）と
    // 現在の接続先が同じなら再付与しない。エージェントへは text、
    // 表示履歴には display（ユーザ文のみ）を残す
    const curSid = s.chat.sessionId ?? s.chat.savedSessionId;
    if (s.execMode === 'parallel' && s.primedFor !== curSid) {
      display = text;
      text = PARALLEL_PREAMBLE + '\n\n' + text;
    }
    if (s.chat.waiting) {
      await s.chat.send(text, display);     // 待機中: 保留キューへ追加
      if (display != null) { s.primedFor = s.chat.sessionId ?? curSid; persistChats(); }
      return json(res, 200, { ok: true, id, waiting: true });
    }
    // リポジトリ不整合チェック: prompt 内容が他 repo 向けなら確認待ちへ
    // （busy 中の steer や keep 回答済みの候補は確認しない。判定はユーザ入力文）
    if (!s.chat.busy) {
      const sug = suggestRepo(String(a.text ?? ''), s.repo, config.repos ?? []);
      if (sug && !(s.repoDeclined ??= new Set()).has(sug.slug)) {
        s.chat.hold(text, { kind: 'repo', suggest: sug }, display);
        if (display != null) { s.primedFor = s.chat.sessionId ?? curSid; persistChats(); }
        return json(res, 200, { ok: true, id, waiting: true, repoAsk: sug });
      }
    }
    // 実装待機判定: prompt 文中のパスが他 busy chat のクレームと衝突したら保留
    const hits = findConflicts(chats, id, extractPaths(text, s.chat.cwd));
    if (hits.length) {
      s.chat.hold(text, { files: hits.flatMap((h) => h.files),
        blockedBy: hits.map((h) => h.id) }, display);
      return json(res, 200, { ok: true, id, waiting: true });
    }
    // 新規ターン開始: 前ターンの検証待ち・差し戻しサイクルは新しい実行へ吸収される
    if (s.verify || s.verifyBack) {
      s.verify = null; s.verifyBack = 0;
      persistChats(); sse('chats', {});
    }
    setupClaims(s.chat, text);
    try {
      await s.chat.send(text, display);
      if (display != null) { s.primedFor = s.chat.sessionId; persistChats(); }
      return json(res, 200, { ok: true, id });
    } catch (e) {
      return json(res, 500, { ok: false, id, error: e.message });
    }
  }
  // 処理モード切替（direct=逐次 / parallel=kasugai-split で子 chat を自動起動）
  if (p === '/api/chat/mode' && req.method === 'POST') {
    const a = await readBody(req);
    const s = chats.get(+a.id);
    if (!s) return json(res, 404, { ok: false, error: 'chat not found' });
    if (a.mode !== 'direct' && a.mode !== 'parallel')
      return json(res, 400, { ok: false, error: 'mode must be direct|parallel' });
    if (s.execMode !== a.mode) {
      s.execMode = a.mode;
      s.chat.push('sys', `[処理モード] ${a.mode === 'parallel' ? '並列' : '直接'} に切り替えました`);
      persistChats();
      sse('chats', {});
      sse('chat', { id: +a.id });
    }
    return json(res, 200, { ok: true, id: +a.id, mode: s.execMode });
  }
  // リポジトリ不整合の確認回答（waiting kind=repo）。
  // keep=候補を declined へ記録してカレント repo のまま再送、
  // switch=chat の repo/cwd を切替先へ変更して再送
  // （ACP セッションは cwd 固定のため破棄し新規セッションで継続。
  //   HTTP 型は常駐接続を持たないので cwd 差替のみ）
  if (p === '/api/chat/repo-answer' && req.method === 'POST') {
    const a = await readBody(req);
    const s = chats.get(+a.id);
    if (!s) return json(res, 404, { ok: false, error: 'chat not found' });
    const w = s.chat.waiting;
    if (!w || w.kind !== 'repo')
      return json(res, 400, { ok: false, error: 'リポジトリ確認待ちではありません' });
    const sug = w.suggest ?? {};
    if (a.action === 'keep') {
      (s.repoDeclined ??= new Set()).add(sug.slug);
      const text = s.chat.leaveWait();
      persistChats();
      if (text) await sendToChat(+a.id, text);
      return json(res, 200, { ok: true, id: +a.id, action: 'keep' });
    }
    if (a.action === 'switch') {
      const target = (config.repos ?? []).find((r) => repoSlug(r) === sug.slug)
        ?? (config.repos ?? []).find((r) => normP(r.path) === normP(sug.path));
      if (!target)
        return json(res, 400, { ok: false, error: 'repo not found: ' + sug.slug });
      const from = repoSlug(s.repo) ?? s.chat.cwd;
      const text = s.chat.leaveWait();
      s.repo = target;
      s.chat.cwd = target.path;
      await s.chat.closeRemote();
      try { s.chat.proc?.kill(); } catch { /* noop */ }
      s.chat.proc = null; s.chat.sessionId = null;
      s.chat.savedSessionId = null;
      s.chat.onSessionId(null);
      s.chat.push('sys',
        `[リポジトリ切替] ${from ?? '(未設定)'} → ${repoSlug(target) ?? target.path}` +
        '。新規セッションで継続します');
      persistChats();
      sse('chats', {});
      if (text) await sendToChat(+a.id, text);
      return json(res, 200, { ok: true, id: +a.id, action: 'switch',
        repo: repoSlug(target) });
    }
    return json(res, 400, { ok: false, error: 'action must be keep|switch' });
  }
  // 最後の user 送信を修正して最終ターンをやり直す（busy 中は cancel して再送）
  if (p === '/api/chat/revise' && req.method === 'POST') {
    const a = await readBody(req);
    const s = chats.get(+a.id);
    if (!s) return json(res, 404, { ok: false, error: 'chat not found' });
    if (!String(a.text ?? '').trim())
      return json(res, 400, { ok: false, error: 'text required' });
    try {
      await s.chat.revise(String(a.text));
      return json(res, 200, { ok: true, id: +a.id });
    } catch (e) {
      return json(res, 400, { ok: false, error: e.message });
    }
  }
  // chat が加えたファイル変更を、指定 turn 以降まとめて元へ戻す。
  // turn 省略時は最新ポイントのみ戻す。busy 中は拒否（先に中断させる）
  if (p === '/api/chat/restore' && req.method === 'POST') {
    const a = await readBody(req);
    const s = chats.get(+a.id);
    if (!s) return json(res, 404, { ok: false, error: 'chat not found' });
    if (s.chat.busy)
      return json(res, 409, { ok: false, error: '実行中です。先に中断してください' });
    const r = s.snaps.restore(a.turn != null ? +a.turn : undefined);
    if (!r.ok) return json(res, 400, r);
    const names = (arr) => arr.map((f) => f.split('/').slice(-2).join('/'));
    const parts = [];
    if (r.restored.length)
      parts.push(`復元 ${r.restored.length} 件: ${names(r.restored).join(', ')}`);
    if (r.deleted.length)
      parts.push(`削除 ${r.deleted.length} 件: ${names(r.deleted).join(', ')}`);
    if (r.failed.length)
      parts.push(`失敗 ${r.failed.length} 件: ${names(r.failed).join(', ')}`);
    s.chat.push('sys',
      `[変更復元] turn ${r.turn} 以降の変更を元へ戻しました（${parts.join(' / ') || '対象なし'}）`);
    sse('chat', { id: +a.id });
    return json(res, 200, r);
  }
  if (p === '/api/chat/cancel' && req.method === 'POST') {
    const a = await readBody(req);
    const s = chats.get(+a.id);
    if (s) await s.chat.cancel();
    // 待機に保持されていた検証 prompt の中断: running のまま残さず pending へ戻す
    if (s && s.verify === 'running' && !s.chat.busy) {
      s.verify = 'pending';
      persistChats();
    }
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
    if (s) { s.primedFor = false; await s.chat.reset(); persistChats(); }
    sse('chat', { id: +a.id });
    return json(res, 200, { ok: true });
  }
  // resumable chat の手動再接続（処理状況ボードの再開ボタン）。
  // 次の送信を待たず ensureSession → session/load する。起動時の
  // 自動再接続（autoResumeOnStart）と同じ経路で、失敗時は resumable のまま残る
  if (p === '/api/chat/resume' && req.method === 'POST') {
    const a = await readBody(req);
    const s = chats.get(+a.id);
    if (!s) return json(res, 404, { ok: false, error: 'chat not found' });
    if (s.chat.proc || s.chat.busy || s.chat.waiting)
      return json(res, 200, { ok: true, id: +a.id });   // 既に接続中/実行中
    if (!s.chat.savedSessionId)
      return json(res, 400, { ok: false, error: '再開できるセッションがありません' });
    try {
      await s.chat.ensureSession();
      return json(res, 200, { ok: true, id: +a.id });
    } catch (e) {
      s.chat.push('sys', `[手動再開失敗] ${e.message}`);
      return json(res, 500, { ok: false, error: e.message });
    }
  }
  // 検証ターンの手動起動（処理状況ボードの検証列カード「検証」ボタン）。
  // 実機検証 prompt（VERIFY_PROMPT）をその chat のセッションへ送る
  if (p === '/api/chat/verify' && req.method === 'POST') {
    const a = await readBody(req);
    const s = chats.get(+a.id);
    if (!s) return json(res, 404, { ok: false, error: 'chat not found' });
    if (s.chat.busy)
      return json(res, 409, { ok: false, error: 'busy' });
    s.verifyCount = 0; s.verifyBack = 0;
    startVerify(+a.id);
    return json(res, 200, { ok: true, id: +a.id });
  }
  if (p === '/api/chat/close' && req.method === 'POST') {
    const a = await readBody(req);
    if (await closeChatEntry(+a.id)) persistChats();
    sse('chats', {});
    return json(res, 200, { ok: true });
  }
  // 処理状況ボードの完了（live）列ヘッダ「すべて削除」: 完了列の chat を
  // 一括削除する。repo 指定時はそのスコープのみ（パネルの絞込と同じ意味）。
  // 完了列の条件は statusKey と同じ: 検証・実行中・待機中でなく live
  if (p === '/api/chat/close-completed' && req.method === 'POST') {
    const a = await readBody(req);
    const ids = [...chats.entries()]
      .filter(([, s]) => (!a.repo || repoSlug(s.repo) === a.repo)
        && !s.verify && !s.chat.busy && !s.chat.waiting
        && (s.chat.proc || (s.chat.transport === 'http' && s.chat.ready)))
      .map(([id]) => id);
    for (const id of ids) await closeChatEntry(id);
    if (ids.length) persistChats();
    sse('chats', {});
    return json(res, 200, { ok: true, closed: ids.length, ids });
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
  // SkillOps/AgentOps doc browser（?repo=<slug> で対象リポジトリに絞込）
  if (p === '/api/agents' && req.method === 'GET') {
    return json(res, 200, { ok: true,
      entries: agentDocs(url.searchParams.get('repo') || '') });
  }
  // /api/skills は一覧表示用: global 配置は収集・doc 解決の対象だが一覧からは除外
  if (p === '/api/skills' && req.method === 'GET') {
    return json(res, 200, { ok: true,
      entries: skillDocs(url.searchParams.get('repo') || '')
        .filter((d) => d.scope !== 'global') });
  }
  // doc が本文中で参照するファイル一覧（?path=<収集済みdoc path>）
  // 解決済みの実在・非秘匿パスは /api/doc の本文返却対象にもなる
  if (p === '/api/refs' && req.method === 'GET') {
    const target = normalize(String(url.searchParams.get('path') ?? '')).toLowerCase();
    const doc = [...agentDocs(), ...skillDocs()]
      .find((d) => normalize(d.path).toLowerCase() === target);
    if (!doc) return json(res, 404, { ok: false, error: 'not found' });
    return json(res, 200, { ok: true,
      entries: listRefDocs(doc.path, docRoots(''), doc.scope) });
  }
  if (p === '/api/doc' && req.method === 'GET') {
    // 収集済み doc + doc が参照する実在ファイルのみ本文を返す
    // （任意ファイル参照・秘匿系参照を防ぐ）
    const target = normalize(String(url.searchParams.get('path') ?? '')).toLowerCase();
    const allDocs = [...agentDocs(), ...skillDocs()];
    let hit = allDocs.find((d) => normalize(d.path).toLowerCase() === target);
    if (!hit && listAllRefPaths(allDocs, docRoots('')).has(target)) {
      const raw = String(url.searchParams.get('path'));
      hit = { name: basename(raw), kind: 'ref', path: raw, scope: 'ref' };
    }
    if (!hit) return json(res, 404, { ok: false, error: 'not found' });
    let text = '';
    try { text = readFileSync(hit.path, 'utf8'); } catch { /* noop */ }
    const truncated = text.length > 200 * 1024;
    return json(res, 200, { ok: true, ...hit,
      text: truncated ? text.slice(0, 200 * 1024) : text, truncated });
  }

  // 静的ファイル（panel/ 直下と /docs/ のマニュアルサイトを配信）
  if (p === '/docs') {                       // 相対リンクの基準を /docs/ に揃える
    res.writeHead(302, { location: '/docs/' });
    return res.end();
  }
  const DOCS = join(ROOT, 'docs');
  const isDocs = p.startsWith('/docs/');
  const base = isDocs ? DOCS : PANEL;
  const rel = p === '/' || p === '/docs/' ? '/index.html'
    : isDocs ? p.slice(5) : p;
  const file = normalize(join(base, rel));
  if (!file.startsWith(base)) return json(res, 403, { ok: false });
  if (existsSync(file) && statSync(file).isFile()) {
    res.writeHead(200, {
      'content-type': MIME[extname(file)] ?? 'application/octet-stream',
      'cache-control': 'no-store' });
    return res.end(readFileSync(file));
  }
  json(res, 404, { ok: false, error: 'not found' });
});

server.listen(PORT, '127.0.0.1', () => {
  console.log(`[kasugai_agent] control :${PORT}  http://127.0.0.1:${PORT}/`);
});
