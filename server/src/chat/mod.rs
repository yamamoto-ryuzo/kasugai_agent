// chat/ — 接続AI 実行体の抽象（acp-chat.mjs / http-chat.mjs 移植）
//
// - AcpChat: ACP 対応 CLI を spawn し ndjson JSON-RPC で session/* を駆動
// - HttpChat: type=agent（bridge-hub /agent）・type=openai（chat/completions）
//   の単発 HTTP 実行体
// 両者で共通の表示状態（messages/busy/waiting/クレーム等）は Core に集約する。
pub mod acp;
pub mod http;

use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use tokio::sync::{mpsc::UnboundedSender, Mutex};

use crate::util::now_iso;

/// control への状態変化通知（SSE push・クレーム判定・永続化の契機）
#[derive(Debug)]
pub enum CtrlEvt {
    Changed { id: i64 },
    SessionId { id: i64 },
    TurnStart { id: i64, turn: u64, text: String },
    FileTouch { id: i64, paths: Vec<String> },
    Restart { force: bool },
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Clone, Serialize)]
pub struct Msg {
    pub role: String, // user|agent|tool|sys|thought|plan
    pub text: String,
    pub ts: String,
    #[serde(skip_serializing_if = "is_false")]
    pub done: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wire: Option<String>, // HTTP 型のみ: 実送信文（display と異なる場合）
}

/// コンテキスト消費量（ACP usage_update / openai 応答 usage 由来）。
/// used=現在のコンテキスト内トークン数、size=コンテキスト窓サイズ
/// （不明な場合は None）、cost=累積コスト {amount,currency}（任意）
#[derive(Clone, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub used: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub cost: Option<Value>,
}

#[derive(Clone, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitState {
    pub kind: String, // conflict|repo
    pub files: Vec<String>,
    #[serde(rename = "blockedBy")]
    pub blocked_by: Vec<i64>,
    pub suggest: Option<Value>,
    pub since: String,
}

#[derive(Default)]
pub struct Core {
    pub messages: Vec<Msg>,
    pub busy: bool,
    pub ready: bool,
    pub error: Option<String>,
    pub options: Vec<Value>,
    pub modes: Value,
    pub waiting: Option<WaitState>,
    pub held_prompts: Vec<String>,
    pub declared: HashSet<String>,
    pub touched: HashSet<String>,
    pub snap_turn: u64,
    pub turn_seq: u64,
    pub last_activity: i64,
    pub session_id: Option<String>,
    pub saved_session_id: Option<String>,
    pub usage: Option<Usage>,
    pub resume_on_release: bool,
    pub steer_queue: VecDeque<String>,
    pub agent_caps: Option<Value>,
}

impl Core {
    pub fn push(&mut self, role: &str, text: &str) {
        self.messages.push(Msg {
            role: role.to_string(),
            text: text.to_string(),
            ts: now_iso(),
            done: false,
            wire: None,
        });
        if self.messages.len() > 500 {
            self.messages = self.messages.split_off(self.messages.len() - 400);
        }
    }

    fn last_mut(&mut self) -> Option<&mut Msg> {
        self.messages.last_mut()
    }

    /// 連続 agent chunk を末尾メッセージへ連結（done でなければ追記）
    pub fn agent_text(&mut self, t: &str) {
        match self.last_mut() {
            Some(m) if m.role == "agent" && !m.done => m.text.push_str(t),
            _ => self.push("agent", t),
        }
    }

    pub fn user_text(&mut self, t: &str) {
        match self.last_mut() {
            Some(m) if m.role == "user" && !m.done => m.text.push_str(t),
            _ => self.push("user", t),
        }
    }

    pub fn mark_last_done(&mut self, roles: &[&str]) {
        if let Some(m) = self.last_mut() {
            if roles.contains(&m.role.as_str()) {
                m.done = true;
            }
        }
    }

    /// 待機中に send された時の通知文（kind 別）
    pub fn wait_notice(&self) -> &'static str {
        if self.waiting.as_ref().map(|w| w.kind.as_str()) == Some("repo") {
            "[リポジトリ確認待ち] 回答後にまとめて送信します"
        } else {
            "[実行待機中] 解除後にまとめて送信します"
        }
    }

    /// 実装待機へ入れる。kind: conflict=同一ファイル競合 / repo=リポジトリ不整合の確認待ち
    pub fn enter_wait(
        &mut self,
        files: Vec<String>,
        blocked_by: Vec<i64>,
        resume: bool,
        kind: &str,
        suggest: Option<Value>,
    ) {
        self.waiting = Some(WaitState {
            kind: kind.to_string(),
            files: files.clone(),
            blocked_by: blocked_by.clone(),
            suggest: suggest.clone(),
            since: now_iso(),
        });
        if resume {
            self.resume_on_release = true;
        }
        let text = if kind == "repo" {
            let slug = suggest
                .as_ref()
                .and_then(|s| s.get("slug").and_then(|v| v.as_str()))
                .unwrap_or("?");
            let reason = suggest
                .as_ref()
                .and_then(|s| s.get("reason").and_then(|v| v.as_str()))
                .unwrap_or("");
            format!(
                "[リポジトリ確認] {} 向けの内容に見えます{}。切り替えて送信するか、このまま送信するか選んでください",
                slug,
                if reason.is_empty() { String::new() } else { format!("（{}）", reason) }
            )
        } else {
            format!(
                "[実行待機] {} を {} が操作中のため待機します",
                files
                    .iter()
                    .map(|f| crate::util::basename(f))
                    .collect::<Vec<_>>()
                    .join(", "),
                blocked_by.iter().map(|i| format!("#{}", i)).collect::<Vec<_>>().join(",")
            )
        };
        self.push("sys", &text);
    }

    /// send 前の待機: prompt を保留キューへ積んで waiting に入る
    pub fn hold(&mut self, text: &str, files: Vec<String>, blocked_by: Vec<i64>, kind: &str, suggest: Option<Value>, display: Option<&str>) {
        self.push("user", display.unwrap_or(text));
        self.held_prompts.push(text.to_string());
        self.enter_wait(files, blocked_by, false, kind, suggest);
    }

    /// 待機解除。戻り値は再送すべき prompt（無ければ None）
    pub fn leave_wait(&mut self) -> Option<String> {
        let held = std::mem::take(&mut self.held_prompts);
        let resume = self.resume_on_release;
        self.waiting = None;
        self.resume_on_release = false;
        self.push("sys", "[待機解除] 処理を再開します");
        if !held.is_empty() {
            return Some(held.join("\n"));
        }
        if resume {
            return Some("編集競合が解消しました。中断した作業を続行してください。".to_string());
        }
        None
    }
}

/// 実行体の公開インターフェース（control が呼ぶ共通面）
#[derive(Clone)]
pub enum Chat {
    Acp(Arc<acp::AcpChat>),
    Http(Arc<http::HttpChat>),
}

impl Chat {
    pub fn core(&self) -> &Arc<Mutex<Core>> {
        match self {
            Chat::Acp(c) => &c.core,
            Chat::Http(c) => &c.core,
        }
    }
    pub fn agent(&self) -> &Value {
        match self {
            Chat::Acp(c) => &c.agent,
            Chat::Http(c) => &c.agent,
        }
    }
    pub fn cwd(&self) -> Arc<tokio::sync::RwLock<String>> {
        match self {
            Chat::Acp(c) => c.cwd.clone(),
            Chat::Http(c) => c.cwd.clone(),
        }
    }
    pub fn evt(&self) -> &UnboundedSender<CtrlEvt> {
        match self {
            Chat::Acp(c) => &c.evt,
            Chat::Http(c) => &c.evt,
        }
    }
    pub fn transport(&self) -> &'static str {
        match self {
            Chat::Acp(_) => "acp",
            Chat::Http(_) => "http",
        }
    }
    pub fn changed(&self) {
        let id = self.chat_id();
        let _ = self.evt().send(CtrlEvt::Changed { id });
    }
    pub fn chat_id(&self) -> i64 {
        match self {
            Chat::Acp(c) => c.id,
            Chat::Http(c) => c.id,
        }
    }
    pub fn console(&self, stream: &str, text: &str) {
        let label = self
            .agent()
            .get("label")
            .and_then(|v| v.as_str())
            .unwrap_or("agent")
            .to_string();
        crate::conlog::conlog(&format!("#{} {}", self.chat_id(), label), stream, text);
    }

    pub async fn push_sys(&self, text: &str) {
        self.core().lock().await.push("sys", text);
        self.changed();
    }

    pub async fn send(&self, text: String, display: Option<String>) -> Result<(), String> {
        match self {
            Chat::Acp(c) => c.send(text, display).await,
            Chat::Http(c) => c.send(text, display).await,
        }
    }
    pub async fn cancel(&self) {
        match self {
            Chat::Acp(c) => c.cancel().await,
            Chat::Http(c) => c.cancel().await,
        }
    }
    pub async fn abort_turn(&self) {
        match self {
            Chat::Acp(c) => c.abort_turn().await,
            Chat::Http(c) => c.abort_turn().await,
        }
    }
    pub async fn reset(&self) {
        match self {
            Chat::Acp(c) => c.reset().await,
            Chat::Http(c) => c.reset().await,
        }
    }
    pub async fn ensure_session(&self) -> Result<(), String> {
        match self {
            Chat::Acp(c) => c.ensure_session().await,
            Chat::Http(c) => c.ensure_session().await,
        }
    }
    /// 直前の user 送信以降の表示を破棄し、修正文で最終ターンをやり直す。
    /// リモートへは修正文を新規 prompt として送る
    pub async fn revise(&self, text: &str) -> Result<(), String> {
        if self.is_busy().await {
            self.cancel().await;
        }
        {
            let mut c = self.core().lock().await;
            let i = match c.messages.iter().rposition(|m| m.role == "user") {
                Some(i) => i,
                None => return Err("修正対象の送信がありません".to_string()),
            };
            c.messages.truncate(i);
            c.held_prompts.clear();
            c.push("sys", "[修正再送] 直前の送信を取り消し、やり直します");
        }
        self.send(text.to_string(), None).await
    }
    pub async fn clear(&self) {
        let mut c = self.core().lock().await;
        c.messages.clear();
        c.push("sys", "履歴をクリアしました（セッションは継続中）");
        drop(c);
        self.changed();
    }
    pub async fn set_option(&self, id: &str, value: &str) -> Result<Value, String> {
        match self {
            Chat::Acp(c) => c.set_option(id, value).await,
            Chat::Http(c) => c.set_option(id, value).await,
        }
    }
    pub async fn close_remote(&self) {
        if let Chat::Acp(c) = self {
            c.close_remote().await;
        }
    }
    pub async fn kill_proc(&self) {
        if let Chat::Acp(c) = self {
            c.kill_proc().await;
        }
    }
    pub async fn is_busy(&self) -> bool {
        self.core().lock().await.busy
    }
    pub async fn has_proc(&self) -> bool {
        match self {
            Chat::Acp(c) => c.proc_slot.lock().await.is_some(),
            Chat::Http(_) => false,
        }
    }

    /// snapshot(): パネル GET /api/chat 応答の本体
    pub async fn snapshot(&self, persisted_opts: &serde_json::Map<String, Value>) -> Value {
        let c = self.core().lock().await;
        let options: Vec<Value> = c
            .options
            .iter()
            .map(|o| {
                let oid = o.get("id").and_then(|v| v.as_str()).unwrap_or("");
                match persisted_opts.get(oid) {
                    Some(v) if *v != o["currentValue"] => {
                        let mut o = o.clone();
                        o["currentValue"] = v.clone();
                        o
                    }
                    _ => o.clone(),
                }
            })
            .collect();
        let live = if self.transport() == "http" {
            c.ready
        } else {
            self.has_proc().await
        };
        json!({
            "ready": c.ready,
            "busy": c.busy,
            "error": c.error,
            "agent": {
                "id": self.agent().get("id").and_then(|v| v.as_str()).unwrap_or(""),
                "label": self.agent().get("label").and_then(|v| v.as_str()).unwrap_or(""),
                "type": self.transport(),
            },
            "live": live,
            "sessionId": c.session_id.clone().or(c.saved_session_id.clone()),
            "resumable": self.transport() == "acp" && !live && c.saved_session_id.is_some(),
            "waiting": c.waiting,
            "usage": c.usage,
            "heldPrompts": c.held_prompts.len(),
            "messages": c.messages,
            "options": options,
            "modes": c.modes,
        })
    }
}
