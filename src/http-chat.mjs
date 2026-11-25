// HTTP 実行体チャット（type=agent / openai）。AcpChat を継承し、
// ファイルクレーム・実装待機・履歴表示は共通実装を再利用する。
// ACP セッションは持たない単発ターン型（session/load 再開・configOptions・
// 権限応答は対象外）。
//
// - type=agent:  KASUGAI Canvas ブリッジハブ互換。POST {endpoint} に
//   {provider, message, project, model?, tools} を送り {reply, actions?, error?}
//   を受ける。bridge-hub(127.0.0.1:8511) 経由の copilot/devin/antigravity/
//   汎用 acp と外部エージェントを同じ型で扱う
// - type=openai: OpenAI 互換 chat/completions。endpoint は base URL
//   （/chat/completions を補完）かフルパス。履歴は直近メッセージから組み立てる
//
// Bearer トークンは getToken(agent) で secrets.json から解決する
// （config.json・ログ・プロンプトには書かない）
import { basename } from 'node:path';
import { AcpChat } from './acp-chat.mjs';

export class HttpChat extends AcpChat {
  constructor(opts) {
    // auth:'devin' の credentials.toml 読み取りは HTTP 型では使わない
    super({ ...opts, agent: { auth: 'none', ...opts.agent } });
    // 旧形式（type 未記載で endpoint のみ）も agent 型として扱う
    this.agent.type = this.agent.type === 'openai' ? 'openai' : 'agent';
    this.getToken = opts.getToken ?? (() => null);
    this.transport = 'http';
    this.inflight = null;          // 進行中 fetch の AbortController
    this.steerQueue = [];          // busy 中に届いた追加指示（完了後に順次送信）
  }

  endpoint() {
    return String(this.agent.endpoint ?? '').trim();
  }

  completionsUrl() {
    const ep = this.endpoint();
    return /\/chat\/completions\/?$/.test(ep)
      ? ep
      : ep.replace(/\/+$/, '') + '/chat/completions';
  }

  headers() {
    const h = { 'content-type': 'application/json' };
    const t = this.getToken(this.agent);
    if (t) h.authorization = `Bearer ${t}`;
    return h;
  }

  // ACP のセッション確立に相当する入口。HTTP 型は常駐接続を持たないため
  // endpoint の存在確認のみ行い、疎通は実際のリクエストで確かめる
  async ensureSession() {
    if (!this.endpoint()) throw new Error('endpoint が未設定です');
    this.ready = true;
  }

  // openai 型の履歴: 直近の user/agent メッセージを messages 形式へ。
  // system に作業ディレクトリだけ添える（ツール実行はエージェント任せ）
  history(limit = 20) {
    const msgs = [{ role: 'system',
      content: `あなたは kasugai_agent 経由で応答する接続AI。作業ディレクトリ: ${this.cwd}` }];
    const conv = this.messages
      .filter((m) => m.role === 'user' || m.role === 'agent')
      .slice(-limit);
    // user メッセージは wire（実送信文）優先: 並列モードのプリアンブルは
    // 表示へは出さないが、単発ターン型の履歴には残す必要がある
    for (const m of conv)
      msgs.push({ role: m.role === 'agent' ? 'assistant' : 'user',
        content: m.wire ?? m.text });
    return msgs;
  }

  async request(text) {
    const isOpenAI = this.agent.type === 'openai';
    const body = isOpenAI
      ? { model: this.agent.model ?? undefined,
          messages: [...this.history(), { role: 'user', content: text }] }
      : { provider: this.agent.provider ?? 'agent',
          message: text,
          project: this.agent.project ?? basename(this.cwd),
          ...(this.agent.model ? { model: this.agent.model } : {}),
          tools: [] };
    this.inflight = new AbortController();
    try {
      const r = await fetch(isOpenAI ? this.completionsUrl() : this.endpoint(), {
        method: 'POST', headers: this.headers(),
        body: JSON.stringify(body), signal: this.inflight.signal });
      const d = await r.json().catch(() => null);
      if (!r.ok)
        throw new Error(d?.error?.message
          ?? (typeof d?.error === 'string' ? d.error : null)
          ?? `HTTP ${r.status}`);
      if (isOpenAI)
        return { reply: d?.choices?.[0]?.message?.content ?? '' };
      return { reply: d?.reply ?? '',
        actions: Array.isArray(d?.actions) ? d.actions : [],
        error: typeof d?.error === 'string' ? d.error : null };
    } finally {
      this.inflight = null;
    }
  }

  // ターン実行。busy 中の追加指示は steerQueue で直列化する
  // （ACP の steer 相当。同一 chat への並行リクエストは応答が混ざるため許さない）
  dispatch(text, display) {
    this.snapTurn += 1;
    this.onTurnStart(this.snapTurn, display ?? text);
    this.busy = true;
    const turn = ++this.turnSeq;
    this.lastActivityAt = Date.now();
    this.armStallWatch();
    this.onChange();
    const drain = () => {
      const next = this.steerQueue.shift();
      if (next != null) this.dispatch(next);
    };
    this.request(text).then((r) => {
      if (turn !== this.turnSeq) return;
      this.busy = false;
      this.lastActivityAt = Date.now();
      if (r.reply) { this.agentText(r.reply); this.messages.at(-1).done = true; }
      for (const a of r.actions ?? [])
        this.push('tool',
          String(typeof a === 'string' ? a : JSON.stringify(a)).slice(0, 160));
      if (r.error) this.push('sys', `[error] ${r.error}`);
      if (!r.reply && !r.error && !(r.actions ?? []).length)
        this.push('sys', '[empty reply]');
      drain();
      this.onChange();
    }).catch((e) => {
      if (turn !== this.turnSeq) return;
      this.busy = false;
      this.push('sys', `[error] ${e.message}`);
      drain();
    });
  }

  // text/display の分離は AcpChat.send と同じ（parallel プリアンブル用）。
  // HTTP 型は history() が messages から組み立てられるため、表示が display でも
  // 実送信文を wire として保持して次ターン以降の履歴に残す
  markWire(text, display) {
    if (display != null && display !== text)
      this.messages.at(-1).wire = text;
  }
  async send(text, display) {
    if (this.waiting) {          // 待機中の送信は保留キューへ積む
      this.push('user', display ?? text);
      this.markWire(text, display);
      this.heldPrompts.push(text);
      this.push('sys', this.waitNotice());
      return;
    }
    await this.ensureSession();
    this.push('user', display ?? text);
    this.markWire(text, display);
    if (this.busy) {
      this.steerQueue.push(text);
      this.push('sys', '[実行中] 現在の応答後に送信します');
      return;
    }
    this.dispatch(text, display);
  }

  // busy 中に待機へ入る場合は ACP の session/cancel に相当する中断を行い、
  // 未送信の steer は保留キューへ移す
  enterWait(w) {
    if (this.busy) {
      this.turnSeq += 1;
      this.inflight?.abort();
      this.busy = false;
      const last = this.messages.at(-1);
      if (last?.role === 'agent') last.done = true;
      if (this.steerQueue.length)
        this.heldPrompts.push(...this.steerQueue.splice(0));
    }
    super.enterWait(w);
  }

  async cancel() {
    if (this.waiting) return super.cancel();   // 待機解除（保留は破棄）
    if (!this.busy) return;
    this.turnSeq += 1;                          // 進行中応答を無効化
    this.inflight?.abort();
    this.steerQueue.length = 0;
    this.busy = false;
    const last = this.messages.at(-1);
    if (last?.role === 'agent') last.done = true;
    this.push('sys', '[中断しました]');
  }

  // 停滞検知（checkStall）からの中断。ACP の session/cancel+kill 相当
  async abortTurn() {
    this.turnSeq += 1;
    this.inflight?.abort();
  }

  async reset() {
    this.steerQueue.length = 0;
    this.inflight?.abort();
    await super.reset();
  }

  snapshot() {
    const s = super.snapshot();
    // HTTP 型は常駐プロセス・リモートセッションを持たない
    s.live = this.ready;
    s.resumable = false;
    return s;
  }
}
