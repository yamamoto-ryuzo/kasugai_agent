// ACP チャットセッション（ACP 対応エージェント CLI 経由）
//
// session/new で1セッションを維持し、送受信を messages 配列に蓄積する。
// 切断後は savedSessionId を保持し、次の送信で session/load による再開を試みる。
// 権限要求（session/request_permission）は allow 系を自動選択する（自律実行前提）。
// 状態が変わるたび onChange() を呼ぶ（control が SSE push へ繋ぐ）。
//
// 実行体は agent（{id, label, exe, args, auth}）で指定する。
// auth: 'devin' は credentials.toml のキーで authenticate、'none' は省略。
// agent 未指定時は devinExe/devinExtraArgs から従来の devin 実行体を組み立てる。
import { spawn } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { homedir } from 'node:os';

export class AcpChat {
  constructor({ agent, devinExe, devinExtraArgs = [], cwd, getPersistedOptions,
                saveOption, getMcpServers, initialSessionId, onSessionId,
                getStallMinutes, onFileTouch, onTurnStart, onOptions, onChange,
                onConsole }) {
    this.agent = agent ?? { id: 'devin', label: 'Devin', exe: devinExe,
      args: ['acp', ...devinExtraArgs], auth: 'devin' };
    this.cwd = cwd;
    this.getPersistedOptions = getPersistedOptions ?? (() => ({}));
    this.saveOption = saveOption ?? (() => {});
    this.getMcpServers = getMcpServers ?? (() => []);
    this.getStallMinutes = getStallMinutes ?? (() => 30);
    this.savedSessionId = initialSessionId ?? null;
    this.onSessionId = onSessionId ?? (() => {});
    this.onOptions = onOptions ?? (() => {});
    this.onChange = onChange ?? (() => {});
    // 実行体プロセスの stderr・非JSON stdout・spawn/exit を1行ずつ通知する
    // （control が conlog リングバッファ → パネルのコンソールタブへ繋ぐ）
    this.onConsole = onConsole ?? (() => {});
    // 編集系 tool_call がファイルパスを掴んだ時に呼ばれる（control が競合判定）
    this.onFileTouch = onFileTouch ?? (() => {});
    // 新しいターン（busy でない状態からの send）開始時に呼ばれる。
    // control がスナップショットの turn 採取に使う
    this.onTurnStart = onTurnStart ?? (() => {});
    this.snapTurn = 0;              // スナップショット用ターン番号（send ごとに+1）
    // ファイルクレーム: declared = prompt 文中のパス（control が登録）、
    // touched = 編集系 tool_call が扱ったパス。busy 中のみ有効
    this.declared = new Set();
    this.touched = new Set();
    // 実装待機: {files, blockedBy, since}。heldPrompts は解除時にまとめて再送
    this.waiting = null;
    this.heldPrompts = [];
    this.resumeOnRelease = false;   // cancel されたターンの再開指示が必要か
    this.turnSeq = 0;               // ターン世代（古い prompt 応答の busy 上書き防止）
    this.agentCaps = null;
    this.proc = null;
    this.sessionId = null;
    this.buf = '';
    this.errBuf = '';               // stderr の行分割バッファ
    this.seq = 0;
    this.pending = new Map();
    this.messages = []; // {role:'user'|'agent'|'tool'|'sys', text, ts, done?}
    this.busy = false;
    this.ready = false;
    this.error = null;
    this.lastActivityAt = 0;
    this.stallTimer = null;
    try {
      const creds = this.agent.auth === 'devin' ? readFileSync(
        join(homedir(), 'AppData/Roaming/devin/credentials.toml'), 'utf8') : '';
      this.apiKey =
        creds.match(/windsurf_api_key\s*=\s*['"]([^'"]+)['"]/)?.[1] ?? null;
    } catch { this.apiKey = null; }
  }

  rpc(method, params) {
    const id = ++this.seq;
    this.proc.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
    return new Promise((res, rej) => this.pending.set(id, { res, rej }));
  }

  // notification（id 無し・応答なし）。session/cancel 等の通知系メソッド用。
  // rpc() で送ると request 扱いになり devin 側が -32601 を返して効かない
  notify(method, params) {
    this.proc?.stdin.write(JSON.stringify({ jsonrpc: '2.0', method, params }) + '\n');
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
    // devin acp は実親ではなく WINDSURF_EXT_HOST_PID の pid を親として監視する。
    // control が IDE 配下で起動された経緯で死んだホスト pid を継承していると、
    // spawn した実行体が「親死亡」を検知して即自死するため env から除去する
    const env = { ...process.env };
    delete env.WINDSURF_EXT_HOST_PID;
    this.proc = spawn(this.agent.exe, this.agent.args ?? [], {
      cwd: this.cwd, stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true, env,
    });
    this.onConsole('sys',
      `[spawn] ${this.agent.exe} ${(this.agent.args ?? []).join(' ')}` +
      ` (pid ${this.proc.pid})`);
    // stderr は JSON-RPC ではない実行体ログ。行分割してコンソールタブへ流す
    this.proc.stderr.on('data', (d) => {
      this.errBuf += d;
      let i;
      while ((i = this.errBuf.indexOf('\n')) >= 0) {
        const line = this.errBuf.slice(0, i).replace(/\r$/, '');
        this.errBuf = this.errBuf.slice(i + 1);
        if (line.trim()) this.onConsole('err', line);
      }
    });
    this.proc.stdout.on('data', (d) => {
      this.buf += d;
      let i;
      while ((i = this.buf.indexOf('\n')) >= 0) {
        const line = this.buf.slice(0, i).trim();
        this.buf = this.buf.slice(i + 1);
        if (!line) continue;
        let m;
        try { m = JSON.parse(line); }
        catch { this.onConsole('out', line); continue; }
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
    this.proc.on('close', (code, signal) => {
      this.onConsole('sys', `[exit] code=${code} signal=${signal ?? '-'}`);
      this.push('sys', '[セッション切断]');
      this.proc = null; this.sessionId = null; this.ready = false; this.busy = false;
      for (const p of this.pending.values()) p.rej(new Error('session closed'));
      this.pending.clear();
      this.onChange();
    });
    this.proc.on('error', (e) => {
      this.error = e.message;
      this.onConsole('err', `[spawn error] ${e.message}`);
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
      // 編集系 tool_call の対象パスを touched へ（競合判定は control 側）
      const paths = this.isWriteCall(u) ? this.extractToolPaths(u) : [];
      if (paths.length) this.onFileTouch(paths);
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

  // 編集系 tool_call かの判定。ACP の kind があればそれを使い、
  // 無ければ title の動詞で推定する
  isWriteCall(u) {
    if (u.kind) return /edit|delete|move|write/i.test(String(u.kind));
    return /edit|write|create|replace|patch|apply|insert|delete|remove|rename|move/i
      .test(String(u.title ?? ''));
  }

  // tool_call から対象ファイルパスを抽出（locations / rawInput の両方を見る）
  extractToolPaths(u) {
    const out = [];
    for (const loc of u.locations ?? []) if (loc?.path) out.push(loc.path);
    const ri = u.rawInput;
    if (ri && typeof ri === 'object') {
      for (const k of ['file_path', 'filePath', 'path', 'target_file',
                       'filename', 'notebook_path'])
        if (typeof ri[k] === 'string') out.push(ri[k]);
    }
    return out;
  }

  // 実装待機へ入れる。busy 中なら session/cancel でターンを止める。
  // kind: 'conflict'=同一ファイル競合 / 'repo'=リポジトリ不整合の確認待ち
  // （suggest に切替候補 {slug, path, reason}。回答は control の
  // repo-answer で受け、leaveWait → 確定先へ再送する）
  enterWait({ files = [], blockedBy = [], resume = false,
              kind = 'conflict', suggest = null } = {}) {
    this.waiting = { files, blockedBy, kind, suggest,
      since: new Date().toISOString() };
    if (resume) this.resumeOnRelease = true;
    this.push('sys', kind === 'repo'
      ? `[リポジトリ確認] ${suggest?.slug ?? '?'} 向けの内容に見えます` +
        (suggest?.reason ? `（${suggest.reason}）` : '') +
        '。切り替えて送信するか、このまま送信するか選んでください'
      : `[実行待機] ${files.map((f) => f.split('/').pop()).join(', ')} を ` +
        `${blockedBy.map((i) => '#' + i).join(',')} が操作中のため待機します`);
    if (this.busy && this.sessionId) {
      try { this.notify('session/cancel', { sessionId: this.sessionId }); }
      catch { /* noop */ }
    }
  }

  // 待機中に send された時の通知文（kind 別）
  waitNotice() {
    return this.waiting?.kind === 'repo'
      ? '[リポジトリ確認待ち] 回答後にまとめて送信します'
      : '[実行待機中] 解除後にまとめて送信します';
  }

  // send 前の待機: prompt を保留キューへ積んで waiting に入る。
  // display は send と同じく表示文（保留キューには text が入る）
  hold(text, reason, display) {
    this.push('user', display ?? text);
    this.heldPrompts.push(text);
    this.enterWait(reason);
  }

  // 待機解除。戻り値は再送すべき prompt（無ければ null）
  leaveWait() {
    const held = this.heldPrompts.splice(0);
    const resume = this.resumeOnRelease;
    this.waiting = null;
    this.resumeOnRelease = false;
    this.push('sys', '[待機解除] 処理を再開します');
    if (held.length) return held.join('\n');
    return resume ? '編集競合が解消しました。中断した作業を続行してください。' : null;
  }

  async authenticate() {
    if (this.agent.auth === 'none') return;
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

  // text は実際に送る prompt、display は履歴へ出す表示文（省略時は text）。
  // parallel モードの分割プリアンブルのように、送るが見せたくない前置に使う
  async send(text, display) {
    if (this.waiting) {          // 待機中の送信は保留キューへ積む
      this.push('user', display ?? text);
      this.heldPrompts.push(text);
      this.push('sys', this.waitNotice());
      return;
    }
    await this.ensureSession();
    this.push('user', display ?? text);
    if (!this.busy) {               // steer（busy 中の追加指示）は同じ turn に含める
      this.snapTurn += 1;
      this.onTurnStart(this.snapTurn, display ?? text);
    }
    this.busy = true;
    const turn = ++this.turnSeq;
    this.lastActivityAt = Date.now();
    this.armStallWatch();
    this.onChange();
    // 応答は session/update で流れる。prompt の完了は待たない（UIは busy で追跡）
    this.rpc('session/prompt', {
      sessionId: this.sessionId,
      prompt: [{ type: 'text', text }],
    }).then(() => {
      if (turn === this.turnSeq) { this.busy = false; this.onChange(); }
    }).catch((e) => {
      if (turn !== this.turnSeq) return;
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
    if (this.sessionId) {
      try { this.notify('session/cancel', { sessionId: this.sessionId }); }
      catch { /* noop */ }
    }
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

  // 直前の user 送信以降の表示を破棄し、修正文で最終ターンをやり直す。
  // ACP に履歴の巻き戻しはないため、リモートへは修正文を新規 prompt として送る
  async revise(text) {
    if (this.busy) await this.cancel();
    const i = this.messages.map((m) => m.role).lastIndexOf('user');
    if (i < 0) throw new Error('修正対象の送信がありません');
    this.messages.length = i;
    this.heldPrompts = [];           // 待機中の保留もやり直し対象に含める
    this.push('sys', '[修正再送] 直前の送信を取り消し、やり直します');
    await this.send(text);
  }

  clear() {
    this.messages = [];
    this.push('sys', '履歴をクリアしました（セッションは継続中）');
  }

  async cancel() {
    if (this.waiting) {          // 待機中の中断 = 待機解除（保留は破棄）
      this.waiting = null;
      this.heldPrompts = [];
      this.resumeOnRelease = false;
      this.push('sys', '[待機を解除しました（保留 prompt は破棄）]');
      return;
    }
    if (!this.sessionId) return;
    // session/cancel は notification。中断の確定は session/prompt 応答の
    // stopReason:"cancelled" が担うのでここでは送信のみ行う
    try { this.notify('session/cancel', { sessionId: this.sessionId }); }
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
    this.busy = false;
    this.waiting = null; this.heldPrompts = []; this.resumeOnRelease = false;
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
      agent: { id: this.agent.id, label: this.agent.label,
        type: this.agent.type ?? 'acp' },
      live: !!this.proc,
      sessionId: this.sessionId ?? this.savedSessionId,
      resumable: !this.proc && !!this.savedSessionId,
      waiting: this.waiting,
      heldPrompts: this.heldPrompts.length,
      messages: this.messages,
      options, modes: this.modes,
    };
  }
}
