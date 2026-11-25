// ACP チャットセッション（devin.exe acp 経由）
//
// session/new で1セッションを維持し、送受信を messages 配列に蓄積する。
// 切断後は savedSessionId を保持し、次の送信で session/load による再開を試みる。
// 権限要求（session/request_permission）は allow 系を自動選択する（自律実行前提）。
// 状態が変わるたび onChange() を呼ぶ（control が SSE push へ繋ぐ）。
import { spawn } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { homedir } from 'node:os';

export class AcpChat {
  constructor({ devinExe, devinExtraArgs = [], cwd, getPersistedOptions, saveOption,
                getMcpServers, initialSessionId, onSessionId, getStallMinutes,
                onOptions, onChange }) {
    this.devinExe = devinExe;
    this.devinExtraArgs = devinExtraArgs;
    this.cwd = cwd;
    this.getPersistedOptions = getPersistedOptions ?? (() => ({}));
    this.saveOption = saveOption ?? (() => {});
    this.getMcpServers = getMcpServers ?? (() => []);
    this.getStallMinutes = getStallMinutes ?? (() => 30);
    this.savedSessionId = initialSessionId ?? null;
    this.onSessionId = onSessionId ?? (() => {});
    this.onOptions = onOptions ?? (() => {});
    this.onChange = onChange ?? (() => {});
    this.agentCaps = null;
    this.proc = null;
    this.sessionId = null;
    this.buf = '';
    this.seq = 0;
    this.pending = new Map();
    this.messages = []; // {role:'user'|'agent'|'tool'|'sys', text, ts, done?}
    this.busy = false;
    this.ready = false;
    this.error = null;
    this.lastActivityAt = 0;
    this.stallTimer = null;
    try {
      const creds = readFileSync(
        join(homedir(), 'AppData/Roaming/devin/credentials.toml'), 'utf8');
      this.apiKey =
        creds.match(/windsurf_api_key\s*=\s*['"]([^'"]+)['"]/)?.[1] ?? null;
    } catch { this.apiKey = null; }
  }

  rpc(method, params) {
    const id = ++this.seq;
    this.proc.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
    return new Promise((res, rej) => this.pending.set(id, { res, rej }));
  }

  push(role, text) {
    this.messages.push({ role, text, ts: new Date().toISOString() });
    if (this.messages.length > 500) this.messages = this.messages.slice(-400);
    this.onChange();
  }

  agentText(t) {
    const last = this.messages.at(-1);
    if (last?.role === 'agent' && !last.done) last.text += t;
    else this.messages.push({ role: 'agent', text: t, ts: new Date().toISOString() });
  }

  // session/load 時の履歴再送（user_message_chunk）を表示へ反映する
  userText(t) {
    const last = this.messages.at(-1);
    if (last?.role === 'user' && !last.done) last.text += t;
    else this.messages.push({ role: 'user', text: t, ts: new Date().toISOString() });
  }

  ensureProc() {
    if (this.proc) return;
    this.proc = spawn(this.devinExe, ['acp', ...(this.devinExtraArgs ?? [])], {
      cwd: this.cwd, stdio: ['pipe', 'pipe', 'inherit'],
    });
    this.proc.stdout.on('data', (d) => {
      this.buf += d;
      let i;
      while ((i = this.buf.indexOf('\n')) >= 0) {
        const line = this.buf.slice(0, i).trim();
        this.buf = this.buf.slice(i + 1);
        if (!line) continue;
        let m;
        try { m = JSON.parse(line); } catch { continue; }
        if (m.id != null && this.pending.has(m.id)) {
          const p = this.pending.get(m.id);
          this.pending.delete(m.id);
          m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result);
        } else if (m.method === 'session/update') {
          this.onUpdate(m.params?.update);
        } else if (m.method === 'session/request_permission' && m.id != null) {
          const opts = m.params?.options ?? [];
          const opt = opts.find((o) => /always/i.test(String(o.optionId ?? '')))
            ?? opts.find((o) => /allow/i.test(String(o.optionId ?? o.name ?? '')))
            ?? opts[0];
          const optionId = opt?.optionId ?? 'allow_once';
          this.proc.stdin.write(JSON.stringify({
            jsonrpc: '2.0', id: m.id,
            result: { outcome: { outcome: 'selected', optionId } },
          }) + '\n');
        }
      }
    });
    this.proc.on('close', () => {
      this.push('sys', '[セッション切断]');
      this.proc = null; this.sessionId = null; this.ready = false; this.busy = false;
      for (const p of this.pending.values()) p.rej(new Error('session closed'));
      this.pending.clear();
      this.onChange();
    });
    this.proc.on('error', (e) => {
      this.error = e.message;
      for (const p of this.pending.values()) p.rej(e);
      this.pending.clear();
      this.onChange();
    });
  }

  onUpdate(u) {
    if (!u) return;
    this.lastActivityAt = Date.now();
    if (u.sessionUpdate === 'agent_message_chunk' && u.content?.text) {
      this.agentText(u.content.text);
    } else if (u.sessionUpdate === 'user_message_chunk' && u.content?.text) {
      this.userText(u.content.text);
    } else if (u.sessionUpdate === 'agent_thought_chunk' && u.content?.text) {
      const last = this.messages.at(-1);
      if (last?.role === 'thought' && !last.done) last.text += u.content.text;
      else this.push('thought', u.content.text);
      return; // push 済み
    } else if (u.sessionUpdate === 'tool_call') {
      const title = u.title ?? 'tool';
      this.push('tool', `${title}: ${JSON.stringify(u.rawInput ?? '').slice(0, 160)}`);
      return;
    } else if (u.sessionUpdate === 'tool_call_update') {
      if (u.status === 'failed') this.push('tool', `✗ ${u.title ?? ''}`);
      return;
    } else if (u.sessionUpdate === 'plan') {
      const entries = (u.entries ?? [])
        .map((e) => `${e.status === 'completed' ? '☑' : e.status === 'in_progress' ? '▶' : '☐'} ${e.content ?? ''}`)
        .join('\n');
      if (entries) { this.push('plan', entries); return; }
    } else if (u.sessionUpdate === 'turn_complete') {
      this.busy = false;
      const last = this.messages.at(-1);
      if (last?.role === 'agent' || last?.role === 'user' || last?.role === 'thought')
        last.done = true;
    } else if (u.sessionUpdate === 'config_option_update' && u.configOptions) {
      this.options = u.configOptions;
      this.onOptions(this.options);
    }
    this.onChange();
  }

  async authenticate() {
    if (!this.apiKey) throw new Error('windsurf_api_key not found');
    await this.rpc('authenticate', {
      methodId: 'devin-browser', _meta: { api_key: this.apiKey } });
  }

  async ensureSession() {
    this.ensureProc();
    if (this.sessionId) return;
    const init = await this.rpc('initialize', {
      protocolVersion: 1,
      clientInfo: { name: 'kasugai-agent', version: '0.2.0' },
      // terminal/fs を true にすると実行がクライアントへ委任され未実装のまま
      // 応答が返らずターンが停止する。エージェント側のツールに任せる
      clientCapabilities: { fs: { readTextFile: false, writeTextFile: false }, terminal: false },
    });
    this.agentCaps = init?.agentCapabilities ?? null;
    await this.authenticate();
    if (this.savedSessionId && this.agentCaps?.loadSession) {
      try {
        const s = await this.rpc('session/load',
          { sessionId: this.savedSessionId, cwd: this.cwd,
            mcpServers: this.getMcpServers() });
        this.sessionId = this.savedSessionId;
        this.ready = true;
        this.options = s?.configOptions ?? this.options ?? [];
        this.modes = s?.modes ?? this.modes ?? null;
        this.onOptions(this.options);
        this.push('sys', `セッション再開 (${this.sessionId})`);
        await this.applyPersistedOptions();
        return;
      } catch (e) {
        this.push('sys', `[再開失敗] ${e.message} → 新規セッションを開始します`);
        this.savedSessionId = null;
        this.onSessionId(null);
      }
    }
    const s = await this.rpc('session/new',
      { cwd: this.cwd, mcpServers: this.getMcpServers() });
    this.sessionId = s.sessionId;
    this.savedSessionId = s.sessionId;
    this.onSessionId(s.sessionId);
    this.ready = true;
    this.options = s.configOptions ?? [];
    this.modes = s.modes ?? null;
    this.onOptions(this.options);
    this.push('sys', `セッション開始 (${s.sessionId})`);
    await this.applyPersistedOptions();
  }

  // 永続化済みオプションのうち現在値と異なるものだけ session/set_config_option で適用
  async applyPersistedOptions() {
    for (const [oid, v] of Object.entries(this.getPersistedOptions())) {
      const cur = (this.options ?? []).find((o) => o.id === oid);
      if (cur && cur.currentValue === v) continue;
      try {
        await this.rpc('session/set_config_option',
          { sessionId: this.sessionId, configId: oid, value: v });
        if (cur) cur.currentValue = v;
        this.push('sys', `[option復元] ${oid} → ${v}`);
      } catch (e) {
        this.push('sys', `[option復元失敗] ${oid}: ${e.message}`);
      }
    }
  }

  async send(text) {
    await this.ensureSession();
    this.push('user', text);
    this.busy = true;
    this.lastActivityAt = Date.now();
    this.armStallWatch();
    this.onChange();
    // 応答は session/update で流れる。prompt の完了は待たない（UIは busy で追跡）
    this.rpc('session/prompt', {
      sessionId: this.sessionId,
      prompt: [{ type: 'text', text }],
    }).then(() => { this.busy = false; this.onChange(); })
      .catch((e) => {
        this.busy = false;
        this.push('sys', `[error] ${e.message}`);
      });
  }

  // ターン停滞検知: busy のまま stallMinutes 無応答ならターンを中断する
  armStallWatch() {
    if (this.stallTimer) return;
    this.stallTimer = setInterval(() => this.checkStall(), 30 * 1000);
    this.stallTimer.unref?.();
  }

  checkStall() {
    if (!this.busy) return;
    const mins = +(this.getStallMinutes?.() ?? 30);
    if (!(mins > 0)) return;
    const idle = Date.now() - (this.lastActivityAt || 0);
    if (idle < mins * 60 * 1000) return;
    this.push('sys', `[応答なし ${Math.round(idle / 60000)}分] ターンを中断します`);
    this.busy = false;
    const last = this.messages.at(-1);
    if (last?.role === 'agent') last.done = true;
    this.abortTurn();
  }

  // session/cancel を試み、それでも止まらない場合はプロセスごと畳む。
  // savedSessionId は残るため次回送信時に session/load で会話を再開できる
  async abortTurn() {
    try {
      if (this.sessionId) {
        await Promise.race([
          this.rpc('session/cancel', { sessionId: this.sessionId }),
          new Promise((_, rej) => setTimeout(() => rej(new Error('cancel timeout')), 10000)),
        ]);
      }
    } catch { /* noop */ }
    try { this.proc?.kill(); } catch { /* noop */ }
  }

  async setOption(id, value) {
    // 未接続時は永続化のみ（次の session/new・load で適用）
    const cur = (this.options ?? []).find((o) => o.id === id);
    if (cur) cur.currentValue = value;
    this.saveOption(id, value);
    this.onChange();
    if (!this.sessionId) {
      this.push('sys', `${id} → ${value}`);
      return null;
    }
    const res = await this.rpc('session/set_config_option',
      { sessionId: this.sessionId, configId: id, value });
    this.push('sys', `${id} → ${value}`);
    return res;
  }

  clear() {
    this.messages = [];
    this.push('sys', '履歴をクリアしました（セッションは継続中）');
  }

  async cancel() {
    if (!this.sessionId) return;
    try { await this.rpc('session/cancel', { sessionId: this.sessionId }); }
    catch { /* noop */ }
    this.busy = false;
    const last = this.messages.at(-1);
    if (last?.role === 'agent') last.done = true;
    this.push('sys', '[中断しました]');
  }

  // devin 側セッション一覧に残さないよう session/close を best-effort で通知。
  // abortTurn は再開用に savedSessionId を保持するため送らない
  async closeRemote() {
    if (this.proc && this.proc.exitCode === null && this.sessionId
        && this.agentCaps?.sessionCapabilities?.close) {
      try {
        await Promise.race([
          this.rpc('session/close', { sessionId: this.sessionId }),
          new Promise((_, rej) => setTimeout(() => rej(new Error('timeout')), 2000)),
        ]);
      } catch { /* noop */ }
    }
  }

  async reset() {
    await this.closeRemote();
    try { this.proc?.kill(); } catch { /* noop */ }
    this.proc = null; this.sessionId = null; this.ready = false;
    if (this.savedSessionId) {
      this.savedSessionId = null;
      this.onSessionId(null);
    }
    this.push('sys', 'セッションをリセットしました');
  }

  snapshot() {
    const p = this.getPersistedOptions();
    const options = (this.options ?? []).map((o) =>
      p[o.id] != null && p[o.id] !== o.currentValue
        ? { ...o, currentValue: p[o.id] } : o);
    return {
      ready: this.ready, busy: this.busy, error: this.error,
      live: !!this.proc,
      sessionId: this.sessionId ?? this.savedSessionId,
      resumable: !this.proc && !!this.savedSessionId,
      messages: this.messages,
      options, modes: this.modes,
    };
  }
}
