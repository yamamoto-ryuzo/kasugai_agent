// conlog.rs — 裏側コンソール出力の集約リングバッファ（conlog.mjs 移植）
// spawn した接続AI 実行体の stderr・非JSON stdout と、control 自身の
// stdio を {seq, ts, src, stream, text} 行として集約する。
// 履歴は GET /api/console、追加分は SSE console push でパネルの
// コンソールタブへ流す。バッファはプロセス内のみ。
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use tokio::sync::mpsc::UnboundedSender;

use crate::util::now_iso;

const MAX: usize = 4000;

#[derive(Clone, Serialize)]
pub struct Line {
    pub seq: u64,
    pub ts: String,
    pub src: String,
    pub stream: String, // out=通常 / err=stderr / sys=spawn・exit 等の制御イベント
    pub text: String,
}

static SEQ: AtomicU64 = AtomicU64::new(0);
static BUF: OnceLock<Mutex<VecDeque<Line>>> = OnceLock::new();
static SINK: OnceLock<UnboundedSender<Line>> = OnceLock::new();

fn buf() -> &'static Mutex<VecDeque<Line>> {
    BUF.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// 追加分の通知先（control が SSE へ繋ぐ）
pub fn on_console_line(tx: UnboundedSender<Line>) {
    let _ = SINK.set(tx);
}

/// stream: out=通常 / err=stderr / sys=spawn・exit 等の制御イベント
pub fn conlog(src: &str, stream: &str, text: &str) {
    for raw in text.split('\n') {
        let t = raw.trim_end_matches('\r');
        if t.is_empty() {
            continue;
        }
        let line = Line {
            seq: SEQ.fetch_add(1, Ordering::Relaxed) + 1,
            ts: now_iso(),
            src: src.to_string(),
            stream: stream.to_string(),
            text: if t.chars().count() > 2000 {
                let s: String = t.chars().take(2000).collect();
                format!("{}…", s)
            } else {
                t.to_string()
            },
        };
        {
            let mut b = buf().lock().unwrap();
            b.push_back(line.clone());
            while b.len() > MAX {
                b.pop_front();
            }
        }
        if let Some(tx) = SINK.get() {
            let _ = tx.send(line);
        }
    }
}

/// ?after=<seq> の差分 or 直近 limit 行を返す
pub fn console_tail(after: u64, limit: usize) -> Vec<Line> {
    let n = limit.clamp(1, MAX);
    let b = buf().lock().unwrap();
    b.iter()
        .filter(|l| l.seq > after)
        .rev()
        .take(n)
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

/// ストリームのチャンクを行単位へ分割する feeder
pub struct LineFeeder<F: FnMut(String)> {
    rest: String,
    emit: F,
}

impl<F: FnMut(String)> LineFeeder<F> {
    pub fn new(emit: F) -> Self {
        Self { rest: String::new(), emit }
    }
    pub fn feed(&mut self, chunk: &[u8]) {
        self.rest.push_str(&String::from_utf8_lossy(chunk));
        while let Some(i) = self.rest.find('\n') {
            let line = self.rest[..i].trim_end_matches('\r').to_string();
            self.rest = self.rest[i + 1..].to_string();
            if !line.trim().is_empty() {
                (self.emit)(line);
            }
        }
    }
}
