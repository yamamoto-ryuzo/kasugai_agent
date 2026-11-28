// chat/acp.rs — ACP (Agent Client Protocol) 対応 CLI を spawn して
// 会話を駆動する実行体（acp-chat.mjs 移植・最大リスク部）
//
// - 子プロセスの stdin/stdout へ ndjson の JSON-RPC を打ち、
//   initialize / authenticate / session/new|load / session/prompt を扱う
// - devin acp は実親ではなく WINDSURF_EXT_HOST_PID の pid を親として監視する。
//   control が IDE 配下で起動された経緯で死んだホスト pid を継承していると
//   spawn した実行体が「親死亡」を検知して即自死するため env から除去する
// - session/cancel は必ず notification（id なし）で送る。rpc で送ると
//   devin 側が -32601 を返して効かない
// - 権限要求（session/request_permission）は allow 系を自動選択する（自律実行前提）
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc::UnboundedSender, oneshot, Mutex, RwLock};

use super::{Core, CtrlEvt};
use crate::util::now_ms;

// auth 指定はホスト固有でなく汎用: "devin" は credentials.toml の
// windsurf_api_key、"none" は authenticate 省略、
// {methodId,tokenEnv|tokenSecret} は任意の ACP authenticate methodId
fn devin_api_key() -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"windsurf_api_key\s*=\s*['"]([^'"]+)['"]"#).unwrap()
    });
    let path = crate::util::appdata()
        .join("devin")
        .join("credentials.toml");
    let s = std::fs::read_to_string(path).ok()?;
    re.captures(&s).map(|m| m[1].to_string())
}
pub fn devin_api_key_pub() -> Option<String> {
    devin_api_key()
}

// 子プロセス共有部
pub struct ProcShared {
    pub stdin: Mutex<ChildStdin>,
    pub child: Mutex<Child>,
    pub pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    pub seq: AtomicU64,
}

#[derive(Clone)]
pub struct AcpChat {
    pub id: i64,
    pub agent: Value,
    pub cwd: Arc<RwLock<String>>,
    pub core: Arc<Mutex<Core>>,
    pub proc_slot: Arc<Mutex<Option<Arc<ProcShared>>>>,
    /// ensure_proc の check-spawn-store を直列化する起動ロック。
    /// （proc()==None の同時通過→二重 spawn→proc_slot 上書きで
    /// 先に spawn した子が kill_on_drop される競合の防止）
    pub init_lock: Arc<Mutex<()>>,
    pub api_key: Option<String>,
    pub env: Vec<(String, String)>,
    pub stall_minutes: Arc<dyn Fn() -> f64 + Send + Sync>,
    pub get_options: Arc<dyn Fn() -> serde_json::Map<String, Value> + Send + Sync>,
    pub save_option: Arc<dyn Fn(String, Value) + Send + Sync>,
    pub get_mcp: Arc<dyn Fn() -> Vec<Value> + Send + Sync>,
    pub evt: UnboundedSender<CtrlEvt>,
}

fn is_write_call(u: &Value) -> bool {
    static RE_KIND: OnceLock<Regex> = OnceLock::new();
    static RE_TITLE: OnceLock<Regex> = OnceLock::new();
    let rk = RE_KIND.get_or_init(|| Regex::new(r"(?i)edit|delete|move|write").unwrap());
    let rt = RE_TITLE.get_or_init(|| {
        Regex::new(r"(?i)edit|write|create|replace|patch|apply|insert|delete|remove|rename|move")
            .unwrap()
    });
    if let Some(k) = u.get("kind").and_then(|v| v.as_str()) {
        return rk.is_match(k);
    }
    rt.is_match(u.get("title").and_then(|v| v.as_str()).unwrap_or(""))
}

fn extract_tool_paths(u: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for loc in u.get("locations").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
        if let Some(p) = loc.get("path").and_then(|v| v.as_str()) {
            out.push(p.to_string());
        }
    }
    if let Some(ri) = u.get("rawInput").and_then(|v| v.as_object()) {
        for k in [
            "file_path",
            "filePath",
            "path",
            "target_file",
            "filename",
            "notebook_path",
        ] {
            if let Some(p) = ri.get(k).and_then(|v| v.as_str()) {
                out.push(p.to_string());
            }
        }
    }
    out
}

impl AcpChat {
    fn console(&self, stream: &str, text: &str) {
        let label = self
            .agent
            .get("label")
            .and_then(|v| v.as_str())
            .unwrap_or("agent")
            .to_string();
        crate::conlog::conlog(&format!("#{} {}", self.id, label), stream, text);
    }
    fn changed(&self) {
        let _ = self.evt.send(CtrlEvt::Changed { id: self.id });
    }

    async fn proc(&self) -> Option<Arc<ProcShared>> {
        self.proc_slot.lock().await.clone()
    }

    async fn rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        let proc = match self.proc().await {
            Some(p) => p,
            None => return Err("プロセスなし".to_string()),
        };
        let id = proc.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = oneshot::channel();
        proc.pending.lock().await.insert(id, tx);
        let line = format!(
            "{}\n",
            serde_json::to_string(&json!({
                "jsonrpc": "2.0", "id": id, "method": method, "params": params
            }))
            .map_err(|e| e.to_string())?
        );
        {
            let mut stdin = proc.stdin.lock().await;
            if let Err(e) = stdin.write_all(line.as_bytes()).await {
                return Err(e.to_string());
            }
            let _ = stdin.flush().await;
        }
        match rx.await {
            Ok(r) => r,
            Err(_) => Err("session closed".to_string()),
        }
    }

    /// notification（id 無し・応答なし）。session/cancel 等の通知系メソッド用
    async fn notify(&self, method: &str, params: Value) {
        if let Some(proc) = self.proc().await {
            let line = format!(
                "{}\n",
                serde_json::to_string(&json!({
                    "jsonrpc": "2.0", "method": method, "params": params
                }))
                .unwrap_or_default()
            );
            let mut stdin = proc.stdin.lock().await;
            let _ = stdin.write_all(line.as_bytes()).await;
            let _ = stdin.flush().await;
        }
    }

    /// 実装待機に入る際のターン中断（enterWait 内の session/cancel 相当）
    pub async fn notify_cancel(&self) {
        let c = self.core.lock().await;
        if c.busy {
            if let Some(sid) = c.session_id.clone() {
                drop(c);
                self.notify("session/cancel", json!({"sessionId": sid})).await;
            }
        }
    }

    async fn handle_msg(self: &Arc<Self>, m: Value, proc: Arc<ProcShared>) {
        if let Some(id) = m.get("id").and_then(|v| v.as_u64()) {
            // pending 応答（id + error/result）
            if m.get("method").is_none() {
                if let Some(tx) = proc.pending.lock().await.remove(&id) {
                    let r = if let Some(e) = m.get("error") {
                        Err(format!("{}", e))
                    } else {
                        Ok(m.get("result").cloned().unwrap_or(Value::Null))
                    };
                    let _ = tx.send(r);
                }
                return;
            }
            // 要求: session/request_permission は allow 系を自動選択（自律実行前提）
            if m["method"].as_str() == Some("session/request_permission") {
                let this = self.clone();
                tokio::spawn(async move {
                    let opts = m["params"]["options"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default();
                    let re_always = Regex::new(r"(?i)always").unwrap();
                    let re_allow = Regex::new(r"(?i)allow").unwrap();
                    let label = |o: &Value| {
                        o.get("optionId")
                            .or_else(|| o.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string()
                    };
                    let opt = opts
                        .iter()
                        .find(|o| re_always.is_match(&label(o)))
                        .or_else(|| opts.iter().find(|o| re_allow.is_match(&label(o))))
                        .or_else(|| opts.first());
                    let option_id = opt
                        .and_then(|o| o.get("optionId").and_then(|v| v.as_str()))
                        .unwrap_or("allow_once")
                        .to_string();
                    if let Some(p) = this.proc().await {
                        let line = format!(
                            "{}\n",
                            serde_json::to_string(&json!({
                                "jsonrpc": "2.0", "id": m["id"],
                                "result": {"outcome": {"outcome": "selected", "optionId": option_id}}
                            }))
                            .unwrap_or_default()
                        );
                        let mut stdin = p.stdin.lock().await;
                        let _ = stdin.write_all(line.as_bytes()).await;
                        let _ = stdin.flush().await;
                    }
                });
            }
            return;
        }
        // notification: session/update は受信順に直列処理する。
        // spawn すると worker thread で実行順が入れ替わり、chunk 追記が
        // 乱れて表示本文（日本語等）が壊れる
        if m.get("method").and_then(|v| v.as_str()) == Some("session/update") {
            self.on_update(&m["params"]["update"]).await;
        }
    }

    async fn on_update(self: &Arc<Self>, u: &Value) {
        if u.is_null() {
            return;
        }
        {
            let mut c = self.core.lock().await;
            c.last_activity = now_ms();
        }
        let ty = u.get("sessionUpdate").and_then(|v| v.as_str()).unwrap_or("");
        match ty {
            "agent_message_chunk" => {
                if let Some(t) = u.pointer("/content/text").and_then(|v| v.as_str()) {
                    self.core.lock().await.agent_text(t);
                }
            }
            "user_message_chunk" => {
                if let Some(t) = u.pointer("/content/text").and_then(|v| v.as_str()) {
                    self.core.lock().await.user_text(t);
                }
            }
            "agent_thought_chunk" => {
                if let Some(t) = u.pointer("/content/text").and_then(|v| v.as_str()) {
                    let mut c = self.core.lock().await;
                    if let Some(m) = c.messages.last_mut() {
                        if m.role == "thought" && !m.done {
                            m.text.push_str(t);
                            drop(c);
                            self.changed();
                            return;
                        }
                    }
                    c.push("thought", t);
                }
            }
            "tool_call" => {
                let title = u.get("title").and_then(|v| v.as_str()).unwrap_or("tool");
                let raw = u.get("rawInput").cloned().unwrap_or(Value::Null);
                let raw_s = if raw.is_string() {
                    raw.as_str().unwrap().to_string()
                } else {
                    raw.to_string()
                };
                {
                    let mut c = self.core.lock().await;
                    c.push(
                        "tool",
                        &format!("{}: {}", title, raw_s.chars().take(160).collect::<String>()),
                    );
                }
                if is_write_call(u) {
                    let paths = extract_tool_paths(u);
                    if !paths.is_empty() {
                        let _ = self.evt.send(CtrlEvt::FileTouch {
                            id: self.id,
                            paths,
                        });
                    }
                }
            }
            "tool_call_update" => {
                if u.get("status").and_then(|v| v.as_str()) == Some("failed") {
                    let mut c = self.core.lock().await;
                    c.push(
                        "tool",
                        &format!("✗ {}", u.get("title").and_then(|v| v.as_str()).unwrap_or("")),
                    );
                }
            }
            "plan" => {
                let entries: Vec<String> = u
                    .get("entries")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|e| {
                        let mark = match e.get("status").and_then(|v| v.as_str()) {
                            Some("completed") => "☑",
                            Some("in_progress") => "▶",
                            _ => "☐",
                        };
                        format!(
                            "{} {}",
                            mark,
                            e.get("content").and_then(|v| v.as_str()).unwrap_or("")
                        )
                    })
                    .collect();
                if !entries.is_empty() {
                    let mut c = self.core.lock().await;
                    c.push("plan", &entries.join("\n"));
                }
            }
            "turn_complete" => {
                let mut c = self.core.lock().await;
                c.busy = false;
                c.mark_last_done(&["agent", "user", "thought"]);
            }
            "usage_update" => {
                if let Some(used) = u.get("used").and_then(|v| v.as_u64()) {
                    let mut c = self.core.lock().await;
                    c.usage = Some(super::Usage {
                        used,
                        size: u.get("size").and_then(|v| v.as_u64()),
                        cost: u.get("cost").cloned().filter(|v| !v.is_null()),
                    });
                }
            }
            "config_option_update" => {
                if let Some(opts) = u.get("configOptions").and_then(|v| v.as_array()) {
                    self.core.lock().await.options = opts.clone();
                }
            }
            _ => {}
        }
        self.changed();
    }

    /// プロセス起動 + reader/waiter
    async fn ensure_proc(self: &Arc<Self>) -> Result<(), String> {
        if self.proc().await.is_some() {
            return Ok(());
        }
        let _init = self.init_lock.lock().await;
        if self.proc().await.is_some() {
            return Ok(());
        }
        let exe = self.agent.get("exe").and_then(|v| v.as_str()).unwrap_or("");
        if exe.is_empty() {
            return Err("agent.exe 未設定".to_string());
        }
        let args: Vec<String> = self
            .agent
            .get("args")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        let cwd = self.cwd.read().await.clone();

        let mut cmd = Command::new(exe);
        cmd.args(&args)
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // WINDSURF_EXT_HOST_PID は devin acp が監視親として使うため除去
            // （他ホストでは無害な防御）
            .env_remove("WINDSURF_EXT_HOST_PID")
            .kill_on_drop(true);
        // agent 定義の env マップを子プロセスへ注入（secret:/env: は control 側で解決済み）
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        // control が console 無しで動いていても実行体へ窓を出さない
        crate::util::hide_console_tokio(&mut cmd);
        let mut child = cmd.spawn().map_err(|e| format!("{}: {}", exe, e))?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        self.console(
            "sys",
            &format!("[spawn] {} {} (pid {})", exe, args.join(" "), child.id().unwrap_or(0)),
        );
        let proc = Arc::new(ProcShared {
            stdin: Mutex::new(stdin),
            child: Mutex::new(child),
            pending: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(0),
        });
        // stdout reader: ndjson 行分割 → JSON-RPC 応答/通知 or コンソール
        {
            let this = self.clone();
            let p = proc.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let line = line.trim().to_string();
                    if line.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<Value>(&line) {
                        Err(_) => this.console("out", &line),
                        Ok(m) => this.handle_msg(m, p.clone()).await,
                    }
                }
            });
        }
        // stderr reader: 実行体ログを1行ずつコンソールへ
        {
            let this = self.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if !line.trim().is_empty() {
                        this.console("err", &line);
                    }
                }
            });
        }
        // waiter: 終了検知 → 状態リセット + pending を全て失敗返し
        {
            let this = self.clone();
            let p = proc.clone();
            tokio::spawn(async move {
                let status = loop {
                    {
                        let mut c = p.child.lock().await;
                        match c.try_wait() {
                            Ok(Some(s)) => break format!("code={:?}", s.code()),
                            Ok(None) => {}
                            Err(e) => break format!("error={}", e),
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                };
                this.console("sys", &format!("[exit] {}", status));
                let pend: Vec<oneshot::Sender<Result<Value, String>>> = {
                    let mut pm = p.pending.lock().await;
                    pm.drain().map(|(_, s)| s).collect()
                };
                for s in pend {
                    let _ = s.send(Err("session closed".to_string()));
                }
                *this.proc_slot.lock().await = None;
                let mut c = this.core.lock().await;
                c.session_id = None;
                c.ready = false;
                c.busy = false;
                c.push("sys", "[セッション切断]");
                drop(c);
                this.changed();
            });
        }
        *self.proc_slot.lock().await = Some(proc);
        Ok(())
    }

    async fn authenticate(&self) -> Result<(), String> {
        // methodId: "devin"→devin-browser、オブジェクトは methodId 指定、
        // それ以外の文字列はそのまま methodId として解釈（ホスト汎用）
        let method = match self.agent.get("auth") {
            Some(Value::String(s)) if s == "none" => return Ok(()),
            Some(Value::String(s)) if s == "devin" => "devin-browser".to_string(),
            Some(Value::String(s)) => s.clone(),
            Some(Value::Object(o)) => o
                .get("methodId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            _ => return Ok(()),
        };
        if method.is_empty() {
            return Ok(());
        }
        let Some(key) = &self.api_key else {
            return Err(format!("api key not found (methodId={})", method));
        };
        self.rpc(
            "authenticate",
            json!({"methodId": method, "_meta": {"api_key": key}}),
        )
        .await
        .map(|_| ())
    }

    pub async fn ensure_session(self: &Arc<Self>) -> Result<(), String> {
        self.ensure_proc().await?;
        if self.core.lock().await.session_id.is_some() {
            return Ok(());
        }
        let init = self
            .rpc(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientInfo": {"name": "kasugai-agent", "version": "0.2.0"},
                    // terminal/fs を true にすると実行がクライアントへ委任され
                    // 応答が返らずターンが停止する。エージェント側のツールに任せる
                    "clientCapabilities": {
                        "fs": {"readTextFile": false, "writeTextFile": false},
                        "terminal": false
                    }
                }),
            )
            .await
            .map_err(|e| format!("initialize: {}", e))?;
        self.core.lock().await.agent_caps = init.get("agentCapabilities").cloned();
        self.authenticate().await?;
        let cwd = self.cwd.read().await.clone();
        // session/load（agentCapabilities.loadSession 対応時のみ）
        let saved = self.core.lock().await.saved_session_id.clone();
        let can_load = self
            .core
            .lock()
            .await
            .agent_caps
            .as_ref()
            .and_then(|c| c.get("loadSession"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if let Some(sid) = saved.filter(|_| can_load) {
            match self
                .rpc(
                    "session/load",
                    json!({"sessionId": sid, "cwd": cwd, "mcpServers": (self.get_mcp)()}),
                )
                .await
            {
                Ok(s) => {
                    let mut c = self.core.lock().await;
                    c.session_id = Some(sid.clone());
                    c.ready = true;
                    c.options = s
                        .get("configOptions")
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default();
                    c.modes = s.get("modes").cloned().unwrap_or(Value::Null);
                    c.push("sys", &format!("セッション再開 ({})", sid));
                    drop(c);
                    self.changed();
                    self.apply_persisted_options().await;
                    return Ok(());
                }
                Err(e) => {
                    let mut c = self.core.lock().await;
                    c.push(
                        "sys",
                        &format!("[再開失敗] {} → 新規セッションを開始します", e),
                    );
                    c.saved_session_id = None;
                    drop(c);
                    let _ = self.evt.send(CtrlEvt::SessionId { id: self.id });
                    self.changed();
                }
            }
        }
        let s = self
            .rpc(
                "session/new",
                json!({"cwd": cwd, "mcpServers": (self.get_mcp)()}),
            )
            .await
            .map_err(|e| format!("session/new: {}", e))?;
        {
            let sid = s.get("sessionId").and_then(|v| v.as_str()).map(String::from);
            let mut c = self.core.lock().await;
            c.session_id = sid.clone();
            c.saved_session_id = sid.clone();
            c.usage = None;   // 新セッションはコンテキスト消費量を持ち越さない
            c.ready = true;
            c.options = s
                .get("configOptions")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            c.modes = s.get("modes").cloned().unwrap_or(Value::Null);
            c.push(
                "sys",
                &format!("セッション開始 ({})", sid.clone().unwrap_or_default()),
            );
        }
        let _ = self.evt.send(CtrlEvt::SessionId { id: self.id });
        self.changed();
        self.apply_persisted_options().await;
        Ok(())
    }

    /// 永続化済みオプションのうち現在値と異なるものだけ適用
    async fn apply_persisted_options(&self) {
        for (oid, v) in (self.get_options)() {
            let c = self.core.lock().await;
            let sid = match c.session_id.clone() {
                Some(s) => s,
                None => return,
            };
            let cur = c
                .options
                .iter()
                .find(|o| o.get("id").and_then(|x| x.as_str()) == Some(oid.as_str()))
                .cloned();
            if let Some(cur) = &cur {
                if cur["currentValue"] == v {
                    continue;
                }
            }
            drop(c);
            match self
                .rpc(
                    "session/set_config_option",
                    json!({"sessionId": sid, "configId": oid, "value": v}),
                )
                .await
            {
                Ok(_) => {
                    let mut c = self.core.lock().await;
                    if let Some(o) = c.options.iter_mut().find(|o| {
                        o.get("id").and_then(|x| x.as_str()) == Some(oid.as_str())
                    }) {
                        o["currentValue"] = v.clone();
                    }
                    c.push("sys", &format!("[option復元] {} → {}", oid, v));
                }
                Err(e) => {
                    let mut c = self.core.lock().await;
                    c.push("sys", &format!("[option復元失敗] {}: {}", oid, e));
                }
            }
        }
        self.changed();
    }

    /// text は実際に送る prompt、display は履歴へ出す表示文（省略時は text）。
    /// parallel モードの分割プリアンブルのように、送るが見せたくない前置に使う
    pub async fn send(self: &Arc<Self>, text: String, display: Option<String>) -> Result<(), String> {
        {
            let mut c = self.core.lock().await;
            if c.waiting.is_some() {
                c.push("user", &display.clone().unwrap_or_else(|| text.clone()));
                c.held_prompts.push(text);
                let notice = c.wait_notice();
                c.push("sys", notice);
                drop(c);
                self.changed();
                return Ok(());
            }
        }
        if let Err(e) = self.ensure_session().await {
            // 起動失敗: prompt を保留キューへ退避する（未着手に滞留させない。
            // 「開始」/「処理再開」/次の send で ensure_session 成功時に併送）
            let mut c = self.core.lock().await;
            c.push("user", &display.clone().unwrap_or_else(|| text.clone()));
            c.held_prompts.push(text);
            c.push(
                "sys",
                &format!("[起動失敗] {}（「開始」/「処理再開」で再送します）", e),
            );
            drop(c);
            self.changed();
            return Err(e);
        }
        // セッション確立。起動失敗で滞留した保留 prompt があれば併せて送る
        let text = {
            let mut c = self.core.lock().await;
            let held = std::mem::take(&mut c.held_prompts);
            if held.is_empty() {
                text
            } else {
                let mut v = held;
                v.push(text);
                v.join("\n")
            }
        };
        let (new_turn, seq, session_id) = {
            let mut c = self.core.lock().await;
            let d = display.clone().unwrap_or_else(|| text.clone());
            c.push("user", &d);
            let mut t = None;
            if !c.busy {
                c.snap_turn += 1;
                t = Some((c.snap_turn, d));
            }
            c.busy = true;
            c.turn_seq += 1;
            c.last_activity = now_ms();
            let seq = c.turn_seq;
            (t, seq, c.session_id.clone().unwrap_or_default())
        };
        if let Some((turn, ttext)) = new_turn {
            let _ = self.evt.send(CtrlEvt::TurnStart {
                id: self.id,
                turn,
                text: ttext,
            });
        }
        self.changed();
        // 応答は session/update で流れる。prompt の完了は待たない（UIは busy で追跡）
        let this = self.clone();
        tokio::spawn(async move {
            let res = this
                .rpc(
                    "session/prompt",
                    json!({
                        "sessionId": session_id,
                        "prompt": [{"type": "text", "text": text}],
                    }),
                )
                .await;
            let mut c = this.core.lock().await;
            if c.turn_seq == seq {
                c.busy = false;
                if let Err(e) = res {
                    c.push("sys", &format!("[error] {}", e));
                }
            }
            drop(c);
            this.changed();
        });
        Ok(())
    }

    /// session/cancel + それでも止まらない場合はプロセスごと畳む。
    /// savedSessionId は残るため次回送信時に session/load で会話を再開できる
    pub async fn abort_turn(&self) {
        let sid = self.core.lock().await.session_id.clone();
        if let Some(sid) = sid {
            self.notify("session/cancel", json!({"sessionId": sid})).await;
        }
        self.kill_proc().await;
    }

    pub async fn kill_proc(&self) {
        if let Some(p) = self.proc().await {
            let mut c = p.child.lock().await;
            let _ = c.kill().await;
        }
        *self.proc_slot.lock().await = None;
    }

    pub async fn set_option(&self, id: &str, value: &str) -> Result<Value, String> {
        {
            let mut c = self.core.lock().await;
            if let Some(o) = c
                .options
                .iter_mut()
                .find(|o| o.get("id").and_then(|x| x.as_str()) == Some(id))
            {
                o["currentValue"] = json!(value);
            }
        }
        (self.save_option)(id.to_string(), json!(value));
        let sid = self.core.lock().await.session_id.clone();
        if sid.is_none() {
            let mut c = self.core.lock().await;
            c.push("sys", &format!("{} → {}", id, value));
            drop(c);
            self.changed();
            return Ok(Value::Null);
        }
        let res = self
            .rpc(
                "session/set_config_option",
                json!({"sessionId": sid.unwrap(), "configId": id, "value": value}),
            )
            .await;
        let mut c = self.core.lock().await;
        c.push("sys", &format!("{} → {}", id, value));
        drop(c);
        self.changed();
        res
    }

    /// waiting 解除（保留破棄） or ターン中断
    pub async fn cancel(&self) {
        {
            let mut c = self.core.lock().await;
            if c.waiting.is_some() {
                c.waiting = None;
                c.held_prompts.clear();
                c.resume_on_release = false;
                c.push("sys", "[待機を解除しました（保留 prompt は破棄）]");
                drop(c);
                self.changed();
                return;
            }
            if c.session_id.is_none() {
                return;
            }
        }
        let sid = self.core.lock().await.session_id.clone().unwrap();
        self.notify("session/cancel", json!({"sessionId": sid})).await;
        let mut c = self.core.lock().await;
        c.busy = false;
        c.mark_last_done(&["agent"]);
        c.push("sys", "[中断しました]");
        drop(c);
        self.changed();
    }

    /// devin 側セッション一覧に残さないよう session/close を best-effort で通知。
    /// abortTurn は再開用に savedSessionId を保持するため送らない
    pub async fn close_remote(&self) {
        let ok = {
            let c = self.core.lock().await;
            c.session_id.is_some()
                && c
                    .agent_caps
                    .as_ref()
                    .and_then(|x| x.pointer("/sessionCapabilities/close"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                && self.proc().await.is_some()
        };
        if !ok {
            return;
        }
        let sid = self.core.lock().await.session_id.clone().unwrap();
        let f = self.rpc("session/close", json!({"sessionId": sid}));
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), f).await;
    }

    pub async fn reset(&self) {
        self.close_remote().await;
        self.kill_proc().await;
        let mut c = self.core.lock().await;
        let had_saved = c.saved_session_id.take().is_some();
        c.session_id = None;
        c.ready = false;
        c.busy = false;
        c.waiting = None;
        c.usage = None;
        c.held_prompts.clear();
        c.resume_on_release = false;
        c.push("sys", "セッションをリセットしました");
        drop(c);
        if had_saved {
            let _ = self.evt.send(CtrlEvt::SessionId { id: self.id });
        }
        self.changed();
    }
}
