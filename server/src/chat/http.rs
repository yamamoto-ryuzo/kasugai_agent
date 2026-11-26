// chat/http.rs — HTTP 実行体チャット（http-chat.mjs 移植）
// - type=agent:  bridge-hub 互換 POST {endpoint} → {reply, actions?, error?}
// - type=openai: OpenAI 互換 chat/completions（履歴は直近メッセージから組立）
// 常駐接続を持たない単発ターン型。busy 中の追加指示は steer_queue で直列化
use serde_json::{json, Map, Value};
use std::sync::Arc;
use tokio::sync::{mpsc::UnboundedSender, Mutex, RwLock};
use tokio_util::sync::CancellationToken;

use super::{Core, CtrlEvt};
use crate::util::{basename, now_iso};

#[derive(Clone)]
pub struct HttpChat {
    pub id: i64,
    pub agent: Value,
    pub cwd: Arc<RwLock<String>>,
    pub core: Arc<Mutex<Core>>,
    pub client: reqwest::Client,
    pub inflight: Arc<Mutex<Option<CancellationToken>>>,
    pub get_token: Arc<dyn Fn(&Value, &str) -> Option<String> + Send + Sync>,
    pub stall_minutes: Arc<dyn Fn() -> f64 + Send + Sync>,
    pub get_options: Arc<dyn Fn() -> Map<String, Value> + Send + Sync>,
    pub save_option: Arc<dyn Fn(String, Value) + Send + Sync>,
    pub evt: UnboundedSender<CtrlEvt>,
}

impl HttpChat {
    fn is_openai(&self) -> bool {
        self.agent.get("type").and_then(|v| v.as_str()) == Some("openai")
    }
    fn endpoint(&self) -> String {
        self.agent
            .get("endpoint")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    }
    fn completions_url(&self) -> String {
        let ep = self.endpoint();
        if regex::Regex::new(r"/chat/completions/?$").unwrap().is_match(&ep) {
            ep
        } else {
            format!("{}/chat/completions", ep.trim_end_matches('/'))
        }
    }
    fn token(&self) -> Option<String> {
        let id = self
            .agent
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        (self.get_token)(&self.agent, &id)
    }

    async fn history(&self, limit: usize) -> Vec<Value> {
        let cwd = self.cwd.read().await.clone();
        let c = self.core.lock().await;
        let mut msgs = vec![json!({
            "role": "system",
            "content": format!("あなたは kasugai_agent 経由で応答する接続AI。作業ディレクトリ: {}", cwd)
        })];
        let conv: Vec<&super::Msg> = c
            .messages
            .iter()
            .filter(|m| m.role == "user" || m.role == "agent")
            .collect();
        for m in conv.iter().skip(conv.len().saturating_sub(limit)) {
            msgs.push(json!({
                "role": if m.role == "agent" { "assistant" } else { "user" },
                "content": m.wire.clone().unwrap_or_else(|| m.text.clone()),
            }));
        }
        msgs
    }

    async fn request(&self, text: &str, project: &str, cancel: CancellationToken) -> Result<Value, String> {
        let is_openai = self.is_openai();
        let body = if is_openai {
            let mut b = json!({"messages": self.history(20).await});
            if let Some(m) = self.agent.get("model").and_then(|v| v.as_str()) {
                b["model"] = json!(m);
            }
            b["messages"]
                .as_array_mut()
                .unwrap()
                .push(json!({"role": "user", "content": text}));
            b
        } else {
            let mut b = json!({
                "provider": self.agent.get("provider").and_then(|v| v.as_str()).unwrap_or("agent"),
                "message": text,
                "project": project,
                "tools": [],
            });
            if let Some(m) = self.agent.get("model").and_then(|v| v.as_str()) {
                b["model"] = json!(m);
            }
            b
        };
        let url = if is_openai { self.completions_url() } else { self.endpoint() };
        let mut rb = self.client.post(&url).json(&body);
        if let Some(t) = self.token() {
            rb = rb.bearer_auth(t);
        }
        let r = tokio::select! {
            _ = cancel.cancelled() => return Err("cancelled".to_string()),
            r = rb.send() => r.map_err(|e| e.to_string())?,
        };
        let status = r.status();
        let d: Value = r.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            let msg = d
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .or_else(|| d.get("error").and_then(|v| v.as_str()))
                .map(String::from)
                .unwrap_or_else(|| format!("HTTP {}", status));
            return Err(msg);
        }
        if is_openai {
            return Ok(json!({
                "reply": d.pointer("/choices/0/message/content")
                    .and_then(|v| v.as_str()).unwrap_or(""),
                "usage": d.get("usage").cloned().unwrap_or(Value::Null),
            }));
        }
        Ok(json!({
            "reply": d.get("reply").and_then(|v| v.as_str()).unwrap_or(""),
            "actions": d.get("actions").and_then(|v| v.as_array()).cloned().unwrap_or_default(),
            "error": d.get("error").and_then(|v| v.as_str()),
        }))
    }

    /// ターン実行。完了後 steer_queue を drain する
    fn dispatch(self: &Arc<Self>, text: String, display: Option<String>) {
        tokio::spawn({
            let this = self.clone();
            async move {
                let turn = {
                    let mut c = this.core.lock().await;
                    c.snap_turn += 1;
                    let t = c.snap_turn;
                    let dt = display.clone().unwrap_or_else(|| text.clone());
                    c.busy = true;
                    c.turn_seq += 1;
                    c.last_activity = now_ms();
                    (t, dt, c.turn_seq)
                };
                let _ = this.evt.send(CtrlEvt::TurnStart {
                    id: this.id,
                    turn: turn.0,
                    text: turn.1,
                });
                this.changed();
                let project = this
                    .agent
                    .get("project")
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .unwrap_or_else(|| {
                        basename(&this.cwd.try_read().map(|c| c.clone()).unwrap_or_default())
                    });
                let token = CancellationToken::new();
                *this.inflight.lock().await = Some(token.clone());
                let res = this.request(&text, &project, token).await;
                *this.inflight.lock().await = None;
                let next = {
                    let mut c = this.core.lock().await;
                    if c.turn_seq != turn.2 {
                        None
                    } else {
                        c.busy = false;
                        c.last_activity = now_ms();
                        match res {
                            Ok(r) => {
                                let reply = r["reply"].as_str().unwrap_or("").to_string();
                                if !reply.is_empty() {
                                    c.agent_text(&reply);
                                    c.mark_last_done(&["agent"]);
                                }
                                let actions = r["actions"].as_array().cloned().unwrap_or_default();
                                for a in &actions {
                                    let s = a.as_str().map(String::from).unwrap_or_else(|| a.to_string());
                                    c.push("tool", &s.chars().take(160).collect::<String>());
                                }
                                if let Some(e) = r["error"].as_str() {
                                    c.push("sys", &format!("[error] {}", e));
                                }
                                // openai 応答の usage.prompt_tokens をコンテキスト
                                // 消費量として保持（送信コンテキストサイズ。窓上限は不明）
                                if let Some(used) = r
                                    .pointer("/usage/prompt_tokens")
                                    .and_then(|v| v.as_u64())
                                {
                                    c.usage = Some(super::Usage {
                                        used,
                                        size: None,
                                        cost: None,
                                    });
                                }
                                if reply.is_empty() && r["error"].is_null() && actions.is_empty() {
                                    c.push("sys", "[empty reply]");
                                }
                            }
                            Err(e) if e != "cancelled" => {
                                c.push("sys", &format!("[error] {}", e));
                            }
                            Err(_) => {}
                        }
                        c.steer_queue.pop_front()
                    }
                };
                this.changed();
                if let Some(n) = next {
                    this.dispatch(n, None);
                }
            }
        });
    }

    fn changed(&self) {
        let _ = self.evt.send(CtrlEvt::Changed { id: self.id });
    }

    fn mark_wire(&self, c: &mut Core, text: &str, display: &Option<String>) {
        if let Some(d) = display {
            if *d != text {
                if let Some(m) = c.messages.last_mut() {
                    m.wire = Some(text.to_string());
                }
            }
        }
    }

    pub async fn send(self: &Arc<Self>, text: String, display: Option<String>) -> Result<(), String> {
        {
            let mut c = self.core.lock().await;
            if c.waiting.is_some() {
                let d = display.clone().unwrap_or_else(|| text.clone());
                let notice = c.wait_notice();
                c.push("user", &d);
                self.mark_wire(&mut c, &text, &display);
                c.held_prompts.push(text);
                c.push("sys", notice);
                drop(c);
                self.changed();
                return Ok(());
            }
        }
        self.ensure_session().await?;
        {
            let mut c = self.core.lock().await;
            let d = display.clone().unwrap_or_else(|| text.clone());
            c.push("user", &d);
            self.mark_wire(&mut c, &text, &display);
            if c.busy {
                c.steer_queue.push_back(text);
                c.push("sys", "[実行中] 現在の応答後に送信します");
                drop(c);
                self.changed();
                return Ok(());
            }
        }
        self.changed();
        self.dispatch(text, display);
        Ok(())
    }

    /// busy 中に待機へ入る場合の中断（ACP session/cancel 相当）
    pub async fn enter_wait_busy(&self) {
        let mut c = self.core.lock().await;
        if c.busy {
            c.turn_seq += 1;
            drop(c);
            if let Some(t) = self.inflight.lock().await.take() {
                t.cancel();
            }
            let mut c = self.core.lock().await;
            c.busy = false;
            c.mark_last_done(&["agent"]);
            let steer: Vec<String> = c.steer_queue.drain(..).collect();
            c.held_prompts.extend(steer);
        }
    }

    pub async fn cancel(&self) {
        let mut c = self.core.lock().await;
        if c.waiting.is_some() {
            let held = std::mem::take(&mut c.held_prompts);
            c.waiting = None;
            if !held.is_empty() {
                c.push("sys", &format!("[待機解除] 保留 {} 件を破棄", held.len()));
            }
            c.busy = false;
            c.resume_on_release = false;
            drop(c);
            self.changed();
            return;
        }
        if !c.busy {
            return;
        }
        c.turn_seq += 1;
        drop(c);
        if let Some(t) = self.inflight.lock().await.take() {
            t.cancel();
        }
        let mut c = self.core.lock().await;
        c.steer_queue.clear();
        c.busy = false;
        c.mark_last_done(&["agent"]);
        c.push("sys", "[中断しました]");
        drop(c);
        self.changed();
    }

    pub async fn abort_turn(&self) {
        self.core.lock().await.turn_seq += 1;
        if let Some(t) = self.inflight.lock().await.take() {
            t.cancel();
        }
    }

    pub async fn reset(&self) {
        self.abort_turn().await;
        let mut c = self.core.lock().await;
        c.steer_queue.clear();
        c.busy = false;
        c.waiting = None;
        c.held_prompts.clear();
        c.ready = false;
        c.push("sys", "[リセット] セッションを閉じました");
        drop(c);
        self.changed();
    }

    pub async fn ensure_session(&self) -> Result<(), String> {
        if self.endpoint().is_empty() {
            return Err("endpoint が未設定です".to_string());
        }
        self.core.lock().await.ready = true;
        Ok(())
    }

    pub async fn set_option(&self, id: &str, value: &str) -> Result<Value, String> {
        (self.save_option)(id.to_string(), json!(value));
        Ok(json!(value))
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

// unused: now_iso は mod 側で利用
#[allow(dead_code)]
fn _keep() {
    let _ = now_iso;
}
