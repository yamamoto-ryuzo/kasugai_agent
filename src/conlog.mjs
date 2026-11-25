// conlog.mjs — 裏側コンソール出力の集約リングバッファ
// spawn した接続AI 実行体の stderr・非JSON stdout と、control 自身の
// stdio を {seq, ts, src, stream, text} 行として集約する。
// 履歴は GET /api/console、追加分は SSE console push でパネルの
// コンソールタブへ流す。バッファはプロセス内のみ
// （永続ログは起動元のリダイレクト＝logs/control.log が担う）
const MAX = 4000;
let seq = 0;
const buf = [];         // {seq, ts, src, stream('out'|'err'|'sys'), text}
let sink = null;        // 追加行の通知先（control が SSE へ繋ぐ）

export function onConsoleLine(fn) { sink = fn; }

// stream: out=通常 / err=stderr / sys=spawn・exit 等の制御イベント
export function conlog(src, stream, text) {
  for (const raw of String(text ?? '').split('\n')) {
    const t = raw.replace(/\r$/, '');
    if (!t) continue;
    const line = { seq: ++seq, ts: new Date().toISOString(),
      src: String(src), stream,
      text: t.length > 2000 ? t.slice(0, 2000) + '…' : t };
    buf.push(line);
    try { sink?.(line); } catch { /* noop */ }
  }
  if (buf.length > MAX) buf.splice(0, buf.length - MAX);
}

// ?after=<seq> の差分 or 直近 limit 行を返す
export function consoleTail(after = 0, limit = 800) {
  const n = Math.max(1, Math.min(+limit || 800, MAX));
  return buf.filter((l) => l.seq > after).slice(-n);
}

// ストリームのチャンクを行単位へ分割する feeder を返す
export function lineFeeder(emit) {
  let rest = '';
  return (chunk) => {
    rest += typeof chunk === 'string' ? chunk : chunk.toString('utf8');
    let i;
    while ((i = rest.indexOf('\n')) >= 0) {
      const line = rest.slice(0, i).replace(/\r$/, '');
      rest = rest.slice(i + 1);
      if (line.trim()) emit(line);
    }
  };
}

// control 自身の stdout/stderr をバッファへも写す（元の出力は維持）。
// 起動元がリダイレクト無しでも、パネルから稼働ログを追跡できるようにする
export function hookOwnConsole(src = 'control') {
  for (const [st, stream] of [[process.stdout, 'out'],
                            [process.stderr, 'err']]) {
    const orig = st.write.bind(st);
    const feed = lineFeeder((l) => conlog(src, stream, l));
    st.write = (chunk, enc, cb) => {
      try { feed(chunk); } catch { /* noop */ }
      return orig(chunk, enc, cb);
    };
  }
}
