// control.rs — HTTP/SSE ハブ（control.mjs 移植・全エンドポイント互換）
// 状態の唯一の正（store）・パネル配信・チャット管理・API を持つ。
// 状態状態遷移（課題ライフサイクル）は pipeline.rs の責務（M1 以降）。
use axum::{
    body::{Body, Bytes},
    extract::{Path as AxPath, Query, State},
    http::{header, StatusCode, Uri},
    response::{IntoResponse, Response, Sse},
    routing::{get, post},
    Json, Router,
};
use futures::stream::Stream;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;
use tokio::sync::{broadcast, mpsc::UnboundedSender, Mutex};

use crate::chat::{acp::AcpChat, http::HttpChat, Chat, CtrlEvt};
use crate::claims::{extract_paths, find_conflicts, Conflict};
use crate::conlog::{conlog, console_tail};
use crate::config::*;
use crate::registry::{agents::list_agent_docs, refs, skills::list_skill_docs};
use crate::pipeline;
use crate::proposals;
use crate::repomatch::{repo_slug, suggest_repo};
use crate::snapshots::SnapshotStore;
use crate::split::{extract_splits, split_key, PARALLEL_PREAMBLE};
use crate::util::*;
use crate::verify::{build_back_prompt, extract_verdict, VERIFY_PROMPT};

pub const VERSION: &str = "0.2.0";

// ---------- チャット管理 ----------
pub struct ChatEntry {
    pub chat: Chat,
    pub snaps: StdMutex<SnapshotStore>,
    pub label: String,
    pub repo: Option<Value>,
    pub updated_at: String,
    pub exec_mode: String,                 // direct|parallel
    pub parent: Option<i64>,
    pub depth: u64,
    pub primed_for: Primed,                // プリアンブル付与済み sessionId
    pub split_scan: usize,
    pub seen_splits: HashSet<String>,
    pub pending_splits: Vec<crate::split::Split>, // 同時実行上限超過分の待機キュー
    pub repo_declined: HashSet<String>,
    pub verify: Option<String>,            // pending|running
    pub verify_count: u64,
    pub verify_back: u64,
    pub verify_scan: usize,
    pub done_kind: Option<String>,         // 終了区分: done|ng|unverified
    pub back_prompt: Option<String>,       // 検証NG差し戻しの保留 prompt（再送待ち）
    pub needs_human: bool,                 // 差し戻し上限到達（要人対応マーク）
    pub issue_key: Option<String>,         // 委譲元課題 key（並列モードの chat 起票）
    pub attach_src: Option<String>,        // 外部セッション attach 元: "devin"|"copilot"
    pub attach_n: usize,                   // attach トランスクリプト注入済み件数（messages 先頭区間）
}

/// primedFor: NotYet=未付与 / Session(sid)=付与済み / Always(HTTP型は常に付与扱い)
#[derive(Clone, PartialEq)]
pub enum Primed {
    NotYet,
    Session(String),
    Always,
}

impl Primed {
    fn persisted(&self) -> Value {
        match self {
            Primed::NotYet => json!(false),
            Primed::Session(s) => json!(s),
            Primed::Always => Value::Null,
        }
    }
    pub fn from_persisted(v: Option<&Value>) -> Self {
        match v {
            Some(Value::String(s)) => Primed::Session(s.clone()),
            Some(Value::Null) => Primed::Always,
            _ => Primed::NotYet,
        }
    }
}

pub struct App {
    pub root: PathBuf,
    pub panel: PathBuf,
    pub uploads: PathBuf,
    pub snaps: PathBuf,
    pub ws_root: String,
    pub cfg: tokio::sync::RwLock<Value>,
    pub port: u16,
    pub chats: Mutex<BTreeMap<i64, ChatEntry>>,
    pub chat_seq: AtomicI64,
    pub sse: broadcast::Sender<(String, String)>,
    pub evt_tx: UnboundedSender<CtrlEvt>,
    pub started: Instant,
    pub restart_pending: AtomicBool,
    pub restarting: AtomicBool,
    pub busy_prev: Mutex<HashMap<i64, bool>>,
    pub assets: Mutex<crate::assets::Mgr>,
    pub http: reqwest::Client,
}

impl App {
    pub fn sse(&self, event: &str, data: &Value) {
        let _ = self
            .sse
            .send((event.to_string(), serde_json::to_string(data).unwrap_or_default()));
    }

    // ---------- 永続化パス ----------
    fn chats_path(&self) -> PathBuf {
        self.root.join("chats.json")
    }
    pub fn opts_path(&self) -> PathBuf {
        self.root.join("acp-options.json")
    }
    fn cand_path(&self) -> PathBuf {
        crate::store::candidates_path(&self.root)
    }

    // ---------- config ヘルパ ----------
    pub async fn cfg(&self) -> Value {
        self.cfg.read().await.clone()
    }

    /// chatAgents レジストリ: {active, agents}
    pub(crate) fn chat_agents(cfg: &Value) -> (Option<String>, Map<String, Value>) {
        let mut agents = cfg_obj(cfg, "chatAgents").cloned().unwrap_or_default();
        if cfg_str(cfg, "devinExe").is_some() && !agents.contains_key("devin") {
            let mut d = Map::new();
            d.insert("label".into(), json!("Devin"));
            d.insert("exe".into(), json!(cfg_str(cfg, "devinExe").unwrap()));
            let mut args = vec![json!("acp")];
            args.extend(cfg_arr(cfg, "devinExtraArgs").into_iter().cloned());
            d.insert("args".into(), json!(args));
            d.insert("auth".into(), json!("devin"));
            agents.insert("devin".into(), Value::Object(d));
        }
        let cur = cfg_str(cfg, "chatAgent").map(String::from);
        let active = match cur {
            Some(a) if agents.contains_key(&a) => Some(a),
            _ => agents.keys().next().cloned(),
        };
        (active, agents)
    }

    fn agent_type(a: &Value) -> &'static str {
        match a.get("type").and_then(|v| v.as_str()) {
            Some("agent") => "agent",
            Some("openai") => "openai",
            _ => {
                if a.get("exe").and_then(|v| v.as_str()).is_some() {
                    "acp"
                } else if a.get("endpoint").and_then(|v| v.as_str()).is_some() {
                    "agent"
                } else {
                    "acp"
                }
            }
        }
    }

    fn agent_pub(&self, id: &str, a: &Value) -> Value {
        let key = a
            .get("tokenKey")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(id);
        json!({
            "id": id,
            "label": a.get("label").and_then(|v| v.as_str()).unwrap_or(id),
            "type": Self::agent_type(a),
            "exe": a.get("exe").and_then(|v| v.as_str()).unwrap_or(""),
            "args": a.get("args").and_then(|v| v.as_array()).cloned().unwrap_or_default(),
            // auth は文字列（none/devin/その他 methodId）または
            // {methodId,tokenEnv|tokenSecret} オブジェクト。表示は methodId に畳む
            "auth": match a.get("auth") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Object(o)) => o
                    .get("methodId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("custom")
                    .to_string(),
                _ => "none".to_string(),
            },
            "envKeys": a
                .get("env")
                .and_then(|v| v.as_object())
                .map(|m| m.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default(),
            "endpoint": a.get("endpoint").and_then(|v| v.as_str()).unwrap_or(""),
            "provider": a.get("provider").and_then(|v| v.as_str()).unwrap_or(""),
            "model": a.get("model").and_then(|v| v.as_str()).unwrap_or(""),
            "project": a.get("project").and_then(|v| v.as_str()).unwrap_or(""),
            "tokenKey": a.get("tokenKey").and_then(|v| v.as_str()).unwrap_or(""),
            "hasToken": secrets_token(&self.root, a, key).is_some(),
        })
    }

    // ---------- repo ----------
    fn repo_pub(r: &Value) -> Value {
        json!({
            "slug": repo_slug(Some(r)),
            "path": r.get("path").and_then(|v| v.as_str()).unwrap_or(""),
            "requirePushApproval": r.get("requirePushApproval").and_then(|v| v.as_bool()).unwrap_or(true),
        })
    }

    fn is_under_ws(&self, p: &str) -> bool {
        norm_p(p).starts_with(&format!("{}/", norm_p(&self.ws_root)))
    }

    fn under_repo(cfg: &Value, p: &str) -> bool {
        let n = norm_p(p);
        cfg_arr(cfg, "repos").iter().any(|r| {
            let root = norm_p(r.get("path").and_then(|v| v.as_str()).unwrap_or(""));
            !root.is_empty() && (n == root || n.starts_with(&format!("{}/", root)))
        })
    }

    fn doc_roots(&self, cfg: &Value, slug: &str) -> Vec<String> {
        let mut all: Vec<String> = vec![self.root.to_string_lossy().replace('\\', "/")];
        for r in cfg_arr(cfg, "repos") {
            if let Some(p) = r.get("path").and_then(|v| v.as_str()) {
                let p = p.replace('\\', "/");
                if !p.is_empty() && !all.contains(&p) {
                    all.push(p);
                }
            }
        }
        if slug.is_empty() {
            return all;
        }
        cfg_arr(cfg, "repos")
            .iter()
            .find(|r| repo_slug(Some(r)).as_deref() == Some(slug))
            .and_then(|r| r.get("path").and_then(|v| v.as_str()))
            .map(|p| vec![p.replace('\\', "/")])
            .unwrap_or_default()
    }

    fn repo_entries(&self, cfg: &Value) -> Vec<Value> {
        let ws_n = norm_p(&self.ws_root);
        let has_git = |p: &Path| p.join(".git").exists();
        let mut reg: Vec<Value> = cfg_arr(cfg, "repos")
            .iter()
            .map(|r| {
                let p = r.get("path").and_then(|x| x.as_str()).unwrap_or("");
                let mut v = Self::repo_pub(r);
                v["registered"] = json!(true);
                let ws = self.is_under_ws(p) && has_git(Path::new(p));
                v["ws"] = json!(ws);
                v["user"] = json!(!ws);
                v
            })
            .collect();
        let mut seen: HashSet<String> = cfg_arr(cfg, "repos")
            .iter()
            .map(|r| norm_p(r.get("path").and_then(|v| v.as_str()).unwrap_or("")))
            .collect();
        // 自動検出の走査根（GitHub 非依存のユーザーリポジトリも対象）:
        //   ws_root      — 全 dir。.git 付き=ワークスペース管理下、無し=ユーザー
        //   home         — .git 付きのみ（ユーザーのローカル repo）
        //   repoScanDirs — 全 dir（ユーザー指定の走査根）
        let mut scan_roots: Vec<(String, bool)> = vec![(self.ws_root.clone(), true)];
        if let Some(h) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
            scan_roots.push((norm_slashes(&h.to_string_lossy()), false));
        }
        for d in cfg_arr(cfg, "repoScanDirs") {
            if let Some(s) = d.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                scan_roots.push((norm_slashes(s), true));
            }
        }
        let mut scanned = HashSet::new();
        for (root, list_all) in scan_roots {
            if !scanned.insert(norm_p(&root)) {
                continue;
            }
            let ws_dir = norm_p(&root) == ws_n;
            if let Ok(rd) = std::fs::read_dir(&root) {
                for e in rd.flatten() {
                    let p = e.path();
                    if !p.is_dir()
                        || p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.starts_with('.'))
                    {
                        continue;
                    }
                    let git = has_git(&p);
                    if !list_all && !git {
                        continue;
                    }
                    let ps = p.to_string_lossy().replace('\\', "/");
                    if !seen.insert(norm_p(&ps)) {
                        continue;
                    }
                    let ws = ws_dir && git;
                    reg.push(json!({"slug": null, "path": ps,
                        "registered": false, "ws": ws, "user": !ws}));
                }
            }
        }
        reg
    }

    pub(crate) fn git_repo<'a>(cfg: &'a Value, slug: &str) -> Option<&'a Value> {
        cfg_arr(cfg, "repos").iter().find(|r| {
            repo_slug(Some(r)).as_deref() == Some(slug)
                || norm_p(r.get("path").and_then(|v| v.as_str()).unwrap_or("")) == norm_p(slug)
        }).copied()
    }

    // ---------- chat ----------
    fn make_chat(&self, repo: &Option<Value>, agent_id: Option<&str>, cfg: &Value) -> Chat {
        let (active, agents) = Self::chat_agents(cfg);
        let aid = match agent_id {
            Some(a) if agents.contains_key(a) => Some(a.to_string()),
            _ => active,
        };
        let spec = aid.map(|id| {
            let mut a = agents[&id].clone();
            a["id"] = json!(id);
            a
        });
        let cwd: Arc<tokio::sync::RwLock<String>> = Arc::new(tokio::sync::RwLock::new(
            repo.as_ref().and_then(|r| r.get("path").and_then(|v| v.as_str()))
                .map(String::from)
                .unwrap_or_else(|| self.root.to_string_lossy().replace('\\', "/")),
        ));
        let root = self.root.clone();
        let opts_path = self.opts_path();
        let opts_path2 = opts_path.clone();
        let root2 = self.root.clone();
        let get_options: Arc<dyn Fn() -> Map<String, Value> + Send + Sync> = Arc::new(move || {
            load_j(&opts_path, json!({}))
                .as_object()
                .cloned()
                .unwrap_or_default()
        });
        let save_option: Arc<dyn Fn(String, Value) + Send + Sync> =
            Arc::new(move |id: String, value: Value| {
                let mut o = load_j(&opts_path2, json!({}));
                o.as_object_mut()
                    .unwrap()
                    .insert(id, value);
                save_j(&opts_path2, &o);
            });
        let get_mcp: Arc<dyn Fn() -> Vec<Value> + Send + Sync> = Arc::new(|| vec![]);
        let stall_cfg = cfg_f64(cfg, "chatStallMinutes", 30.0);
        let stall: Arc<dyn Fn() -> f64 + Send + Sync> = Arc::new(move || stall_cfg);
        let spec = spec.unwrap_or_else(|| json!({"id": "none", "label": "none"}));
        if Self::agent_type(&spec) != "acp" {
            let get_token: Arc<dyn Fn(&Value, &str) -> Option<String> + Send + Sync> =
                Arc::new(move |a: &Value, id: &str| secrets_token(&root, a, id));
            Chat::Http(Arc::new(HttpChat {
                id: 0,
                agent: {
                    let mut a = spec;
                    a["type"] = json!(Self::agent_type(&a));
                    a["auth"] = json!(a.get("auth").and_then(|v| v.as_str()).unwrap_or("none"));
                    a
                },
                cwd,
                core: Arc::new(Mutex::new(crate::chat::Core::default())),
                client: self.http.clone(),
                inflight: Arc::new(Mutex::new(None)),
                get_token,
                stall_minutes: stall,
                get_options,
                save_option,
                evt: self.evt_tx.clone(),
            }))
        } else {
            // auth 解決: "devin"=credentials.toml / "none"=authenticate 省略 /
            // {methodId,tokenEnv?,tokenSecret?}=汎用（ACP authenticate へ任意 methodId）
            let api_key = match spec.get("auth") {
                Some(Value::String(s)) if s == "devin" => {
                    crate::chat::acp::devin_api_key_pub()
                }
                Some(Value::Object(o)) => o
                    .get("tokenEnv")
                    .and_then(|v| v.as_str())
                    .and_then(|n| std::env::var(n).ok())
                    .or_else(|| {
                        o.get("tokenSecret")
                            .and_then(|v| v.as_str())
                            .and_then(|k| crate::config::secrets_get(&root2, k))
                    })
                    .or_else(|| crate::config::secrets_token(&root2, &spec, "agent")),
                _ => None,
            };
            // env: {NAME: "literal" | "env:VAR" | "secret:key"} — spawn 時注入。
            // トークン等の秘匿値は config.json へ直書きせず secret: で secrets.json を引く
            let env = crate::config::agent_env(&root2, &spec);
            Chat::Acp(Arc::new(AcpChat {
                id: 0,
                agent: spec,
                cwd,
                core: Arc::new(Mutex::new(crate::chat::Core::default())),
                proc_slot: Arc::new(Mutex::new(None)),
                init_lock: Arc::new(Mutex::new(())),
                api_key,
                env,
                stall_minutes: stall,
                get_options,
                save_option,
                get_mcp,
                evt: self.evt_tx.clone(),
            }))
        }
    }

    pub async fn new_chat(&self, o: NewChatOpts) -> i64 {
        let cfg = self.cfg().await;
        let id = o
            .id
            .unwrap_or_else(|| self.chat_seq.fetch_add(1, Ordering::SeqCst) + 1);
        let cur = self.chat_seq.load(Ordering::SeqCst);
        if id > cur {
            self.chat_seq.store(id, Ordering::SeqCst);
        }
        let mut chat = self.make_chat(&o.repo, o.agent.as_deref(), &cfg);
        match &mut chat {
            Chat::Acp(c) => Arc::get_mut(c).unwrap().id = id,
            Chat::Http(c) => Arc::get_mut(c).unwrap().id = id,
        }
        if let Some(sid) = &o.session_id {
            chat.core().lock().await.saved_session_id = Some(sid.clone());
        }
        chat.core().lock().await.usage = o.usage;
        {
            // 再起動前の待機状態を復元（held prompts・resumeOnRelease も維持）
            let mut c = chat.core().lock().await;
            c.waiting = o.waiting;
            c.held_prompts = o.held_prompts;
            c.resume_on_release = o.resume_on_release;
        }
        let snaps = SnapshotStore::new(self.snaps.join(id.to_string()));
        chat.core().lock().await.snap_turn = snaps.last_turn();
        let exec_mode = if o.mode.as_deref() == Some("direct") {
            "direct".to_string()
        } else {
            "parallel".to_string()
        };
        // HTTP 型はプリアンブルが history に残るため常に付与済み扱い
        let primed_for = match o.primed_for {
            Some(p) => p,
            None => {
                if chat.transport() == "http" {
                    Primed::Always
                } else {
                    Primed::NotYet
                }
            }
        };
        let entry = ChatEntry {
            chat,
            snaps: StdMutex::new(snaps),
            label: o.label.unwrap_or_else(|| format!("chat {}", id)),
            repo: o.repo,
            updated_at: o.updated_at.unwrap_or_else(now_iso),
            exec_mode,
            parent: o.parent,
            depth: o.depth,
            primed_for,
            split_scan: 0,
            seen_splits: o.seen_splits,
            pending_splits: o.pending_splits,
            repo_declined: o.repo_declined,
            verify: match o.verify.as_deref() {
                Some("running") => Some("pending".to_string()),
                Some(v) => Some(v.to_string()),
                None => None,
            },
            verify_count: o.verify_count,
            verify_back: o.verify_back,
            verify_scan: 0,
            done_kind: o.done_kind,
            back_prompt: o.back_prompt,
            needs_human: o.needs_human,
            issue_key: o.issue_key,
            attach_src: o.attach_src,
            attach_n: 0,
        };
        self.chats.lock().await.insert(id, entry);
        self.persist_chats().await;
        id
    }

    pub async fn persist_chats(&self) {
        let chats = self.chats.lock().await;
        let mut o = Map::new();
        for (id, s) in chats.iter() {
            let c = s.chat.core().lock().await;
            let mut e = Map::new();
            e.insert("label".into(), json!(s.label));
            e.insert("repo".into(), s.repo.clone().unwrap_or(Value::Null));
            e.insert(
                "agent".into(),
                s.chat
                    .agent()
                    .get("id")
                    .cloned()
                    .unwrap_or(Value::Null),
            );
            e.insert(
                "sessionId".into(),
                c.saved_session_id
                    .clone()
                    .or(c.session_id.clone())
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
            e.insert("updatedAt".into(), json!(s.updated_at));
            e.insert("mode".into(), json!(s.exec_mode));
            e.insert(
                "parent".into(),
                s.parent.map(|p| json!(p)).unwrap_or(Value::Null),
            );
            e.insert("depth".into(), json!(s.depth));
            e.insert("primedFor".into(), s.primed_for.persisted());
            e.insert(
                "seenSplits".into(),
                json!(s.seen_splits.iter().cloned().collect::<Vec<_>>()),
            );
            e.insert(
                "pendingSplits".into(),
                json!(s
                    .pending_splits
                    .iter()
                    .map(|p| json!({"title": p.title, "prompt": p.prompt}))
                    .collect::<Vec<_>>()),
            );
            e.insert(
                "repoDeclined".into(),
                json!(s.repo_declined.iter().cloned().collect::<Vec<_>>()),
            );
            e.insert(
                "verify".into(),
                s.verify.clone().map(Value::String).unwrap_or(Value::Null),
            );
            e.insert("verifyCount".into(), json!(s.verify_count));
            e.insert("verifyBack".into(), json!(s.verify_back));
            e.insert(
                "doneKind".into(),
                s.done_kind
                    .clone()
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
            e.insert(
                "backPrompt".into(),
                s.back_prompt
                    .clone()
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
            e.insert("needsHuman".into(), json!(s.needs_human));
            e.insert(
                "issueKey".into(),
                s.issue_key
                    .clone()
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
            e.insert(
                "usage".into(),
                serde_json::to_value(&c.usage).unwrap_or(Value::Null),
            );
            // 待機状態は再起動を跨いで復元する（busy ではないため延期対象外だが
            // 保留 prompt まで失うと再送できない）
            e.insert(
                "waiting".into(),
                c.waiting
                    .as_ref()
                    .map(|w| {
                        json!({
                            "kind": w.kind, "files": w.files, "blockedBy": w.blocked_by,
                            "suggest": w.suggest, "since": w.since })
                    })
                    .unwrap_or(Value::Null),
            );
            e.insert("heldPrompts".into(), json!(c.held_prompts));
            e.insert("resumeOnRelease".into(), json!(c.resume_on_release));
            e.insert(
                "attachSrc".into(),
                s.attach_src
                    .clone()
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
            drop(c);
            o.insert(id.to_string(), Value::Object(e));
        }
        drop(chats);
        save_j(&self.chats_path(), &Value::Object(o));
    }

    pub async fn chat_list(&self) -> Vec<Value> {
        let chats = self.chats.lock().await;
        let mut out = Vec::new();
        for (id, s) in chats.iter() {
            let c = s.chat.core().lock().await;
            let children: Vec<i64> = chats
                .iter()
                .filter(|(_, x)| x.parent == Some(*id))
                .map(|(cid, _)| *cid)
                .collect();
            let proc = s.chat.has_proc().await;
            out.push(json!({
                "id": id, "label": s.label, "busy": c.busy, "ready": c.ready,
                "execMode": s.exec_mode, "parent": s.parent, "depth": s.depth,
                "children": children,
                "agent": s.chat.agent().get("id").cloned().unwrap_or(Value::Null),
                "agentLabel": s.chat.agent().get("label").cloned().unwrap_or(Value::Null),
                "agentType": if s.chat.agent().is_null() { "acp" } else { Self::agent_type(s.chat.agent()) },
                "live": proc || (s.chat.transport() == "http" && c.ready),
                "sessionId": c.session_id,
                "resumable": s.chat.transport() == "acp" && !proc && c.saved_session_id.is_some(),
                "waiting": c.waiting.is_some(),
                "heldPrompts": c.held_prompts.len(),
                "usage": c.usage,
                "verify": s.verify, "verifyBack": s.verify_back,
                "doneKind": s.done_kind,
                "backPending": s.back_prompt.is_some(),
                "needsHuman": s.needs_human,
                "issueKey": s.issue_key,
                "pendingSplits": s.pending_splits.iter()
                    .map(|p| json!({"title": p.title})).collect::<Vec<_>>(),
                "waitInfo": c.waiting.as_ref().map(|w| json!({
                    "kind": w.kind, "files": w.files, "blockedBy": w.blocked_by,
                    "held": c.held_prompts.len(), "suggest": w.suggest })),
                "updatedAt": s.updated_at,
                "repo": repo_slug(s.repo.as_ref()),
            }));
        }
        out
    }

    // ---------- 状態変化通知 ----------
    /// Changed evt → busy 終了検知 → releaseWaiters/onChatDone → SSE デバウンス
    pub async fn notify_chat(self: &Arc<Self>, id: i64) {
        let (was, now) = {
            let chats = self.chats.lock().await;
            let now = match chats.get(&id) {
                Some(s) => s.chat.core().lock().await.busy,
                None => false,
            };
            let mut bp = self.busy_prev.lock().await;
            let was = bp.get(&id).copied().unwrap_or(false);
            if now {
                bp.insert(id, true);
            } else {
                bp.remove(&id);
            }
            (was, now)
        };
        // updatedAt 更新
        {
            let mut chats = self.chats.lock().await;
            if let Some(s) = chats.get_mut(&id) {
                s.updated_at = now_iso();
            }
        }
        if was && !now {
            self.release_waiters(id).await;
            self.on_chat_done(id).await;
            // ターン終了時に usage・updatedAt 等を chats.json へ反映
            // （SessionId を出さない http 型や usage_update 更新後の保存抜けを防ぐ）
            self.persist_chats().await;
        }
        self.sse("chat", &json!({"id": id}));
    }

    // ---------- 待機解放（claims.mjs releaseWaiters 移植） ----------
    pub async fn release_waiters(self: &Arc<Self>, blocker_id: i64) {
        let chats = self.chats.lock().await;
        // busy 集合を先に確定
        let mut busy_ids = HashSet::new();
        for (id, s) in chats.iter() {
            if s.chat.core().lock().await.busy {
                busy_ids.insert(*id);
            }
        }
        let mut work: Vec<(i64, String)> = Vec::new();
        for (id, s) in chats.iter() {
            let mut c = s.chat.core().lock().await;
            let Some(w) = c.waiting.as_mut() else { continue };
            // capacity 待機は枠管理（drain_capacity）が解放する
            if w.kind == "capacity" {
                continue;
            }
            if !w.blocked_by.contains(&blocker_id) {
                continue;
            }
            w.blocked_by.retain(|b| busy_ids.contains(b));
            if !w.blocked_by.is_empty() {
                let id2 = *id;
                let _ = c;
                self.sse("chat", &json!({"id": id2}));
                continue;
            }
            if let Some(text) = c.leave_wait() {
                let cwd = s.chat.cwd().read().await.clone();
                setup_claims_core(&mut c, &text, &cwd);
                work.push((*id, text));
            }
        }
        drop(chats);
        for (id, text) in work {
            let chats2 = self.chats.lock().await;
            if let Some(s) = chats2.get(&id) {
                let chat = s.chat.clone();
                drop(chats2);
                self.send_with_slot(id, &chat, text).await;
            }
        }
        self.drain_capacity().await;
    }

    /// 解放済み prompt の送信前に同時起動枠を確保する共通経路。
    /// proc 無し chat は枠が無ければ capacity 待機へ戻す
    /// （leave_wait 済みなので user 行は再表示せず保留キューへ戻す）
    async fn send_with_slot(self: &Arc<Self>, id: i64, chat: &Chat, text: String) {
        if !chat.has_proc().await {
            if let Err(blockers) = self.acquire_proc_slot(id, true).await {
                {
                    let mut c = chat.core().lock().await;
                    c.held_prompts.push(text);
                    c.enter_wait(Vec::new(), blockers, false, "capacity", None);
                }
                self.sse("chat", &json!({"id": id}));
                return;
            }
        }
        let _ = self.evt_tx.send(CtrlEvt::Changed { id });
        if let Err(e) = chat.send(text, None).await {
            chat.push_sys(&format!("[再開失敗] {}", e)).await;
        } else {
            clear_attach_transcript(self, id).await;
        }
    }

    // ---------- 並列モード（処理状況ボード exec 列の「並列」ボタン） ----------
    /// on=true で conflictWait=false を config 永続化（以後の send・FileTouch
    /// 競合は find_chat_conflicts が空を返し待機しない）し、全ての実装待機
    /// （kind!=repo）を blocker の busy に関わらず解放して即時送信する。
    /// on=false は直列ガードへ復帰するだけ（実行中の chat は継続）。
    /// repo 確認待ちは人ゲートのため対象外。
    pub async fn set_parallel_exec(self: &Arc<Self>, on: bool) -> Value {
        {
            let mut cfg = self.cfg.write().await;
            cfg["conflictWait"] = json!(!on);
            persist_config(&self.root, &cfg);
        }
        self.sse("config", &json!({"chatParallel": on}));
        let mut released = 0usize;
        if on {
            let mut work: Vec<(i64, String)> = Vec::new();
            {
                let chats = self.chats.lock().await;
                for (id, s) in chats.iter() {
                    let mut c = s.chat.core().lock().await;
                    // capacity 待機は枠管理が解放する（並列モードでも枠は守る）
                    match c.waiting.as_ref() {
                        Some(w) if w.kind != "repo" && w.kind != "capacity" => {}
                        _ => continue,
                    }
                    if let Some(text) = c.leave_wait() {
                        let cwd = s.chat.cwd().read().await.clone();
                        setup_claims_core(&mut c, &text, &cwd);
                        work.push((*id, text));
                    }
                }
            }
            for (id, text) in work {
                let chats = self.chats.lock().await;
                if let Some(s) = chats.get(&id) {
                    let chat = s.chat.clone();
                    drop(chats);
                    self.send_with_slot(id, &chat, text).await;
                    released += 1;
                }
            }
            self.drain_capacity().await;
            conlog(
                "control",
                "sys",
                &format!(
                    "並列モードON: 実装待機 {} 件を解放し以後の競合待機を無効化",
                    released
                ),
            );
        } else {
            conlog("control", "sys", "並列モードOFF: 直列ガードへ復帰");
        }
        // waiting/held_prompts を変更したため永続化
        self.persist_chats().await;
        self.sse("chats", &json!({}));
        json!({"ok": true, "parallel": on, "released": released})
    }

    // ---------- busy 終了フック（onChatDone 移植） ----------
    async fn on_chat_done(self: &Arc<Self>, id: i64) {
        let cfg = self.cfg().await;
        let back_limit = cfg_u64(&cfg, "verifyBackLimit", 3);
        let retry_limit = cfg_u64(&cfg, "retryLimit", 3);
        let auto_verify = cfg_bool_default_true(&cfg, "autoVerify");
        let par_max_depth = cfg_u64(&cfg, "parallelMaxDepth", 1);

        enum Action {
            None,
            Back { n: u64 },
            BackLimit { n: u64 },
        }
        let mut action = Action::None;
        let mut need_persist = false;
        let mut do_aggregate = false;
        let mut parent_id = None;
        let mut do_splits = false;
        let mut do_verify = false;
        let mut emit_chats = false;
        let mut derived = None;

        {
            let mut chats = self.chats.lock().await;
            if let Some(s) = chats.get_mut(&id) {
                let c = s.chat.core().lock().await;
                if s.verify.as_deref() == Some("running") {
                    let scan_start = s.verify_scan.min(c.messages.len());
                    let mut v = None;
                    for m in &c.messages[scan_start..] {
                        if m.role != "agent" {
                            continue;
                        }
                        if let Some(x) = extract_verdict(&m.text) {
                            v = Some(x);
                        }
                    }
                    match &v {
                        Some(v) if !v.ok => {
                            s.verify = None;
                            s.verify_count = 0;
                            if s.verify_back < back_limit {
                                s.verify_back += 1;
                                // 差し戻し prompt は送信せず保留する（再送は
                                // 「処理再開」/「差し戻し」ボタン・再起動時の
                                // 自動再処理で行う）。終了区分は ng のまま:
                                // 再実装ターンが touched 無しで終わっても
                                // 「未実装」ではなく「検証NG」と表示するため
                                s.done_kind = Some("ng".to_string());
                                s.back_prompt = Some(build_back_prompt(v));
                                action = Action::Back { n: s.verify_back };
                            } else {
                                s.verify_back = 0;
                                s.done_kind = Some("ng".to_string());
                                s.needs_human = true;
                                action = Action::BackLimit { n: back_limit };
                            }
                            need_persist = true;
                            emit_chats = true;
                        }
                        _ => {
                            s.verify = if !c.touched.is_empty() && s.verify_count < retry_limit {
                                Some("pending".to_string())
                            } else {
                                None
                            };
                            if s.verify.is_none() {
                                s.verify_count = 0;
                                s.verify_back = 0;
                                // 検証フェーズ解消時の終了区分:
                                // verdict 合格=done（実装完了）/ verdict 未取得=unverified
                                s.done_kind = Some(
                                    if v.as_ref().is_some_and(|x| x.ok) {
                                        "done"
                                    } else {
                                        "unverified"
                                    }
                                    .to_string(),
                                );
                                emit_chats = true;
                            } else {
                                s.done_kind = None;
                            }
                            need_persist = true;
                        }
                    }
                } else if !c.touched.is_empty() {
                    if s.done_kind.as_deref() == Some("done") {
                        // done は終端: 変更の生じたターンは派生課題として起票し、
                        // chat 側は done を維持（検証・是正は派生課題が担う）
                        let last_prompt = c
                            .messages
                            .iter()
                            .rev()
                            .find(|m| m.role == "user")
                            .map(|m| m.text.clone())
                            .unwrap_or_default();
                        derived = Some((
                            s.label.clone(),
                            s.repo.clone(),
                            last_prompt,
                            c.touched.iter().cloned().collect::<Vec<_>>(),
                        ));
                    } else {
                        s.verify = Some("pending".to_string());
                        s.verify_count = 0;
                        s.done_kind = None;
                        need_persist = true;
                    }
                }
                do_aggregate = s.parent.is_some();
                parent_id = s.parent;
                do_splits = s.exec_mode == "parallel" && s.depth < par_max_depth;
                do_verify = s.verify.as_deref() == Some("pending") && auto_verify;
            }
        }
        if emit_chats {
            self.sse("chats", &json!({}));
        }
        if need_persist {
            self.persist_chats().await;
        }
        match action {
            Action::Back { n } => {
                let chats = self.chats.lock().await;
                if let Some(s) = chats.get(&id) {
                    s.chat
                        .push_sys(&format!(
                            "[検証] 差し戻し {}回目を保留しました（「処理再開」/「差し戻し」で再実装を送信）",
                            n
                        ))
                        .await;
                }
            }
            Action::BackLimit { n } => {
                let chats = self.chats.lock().await;
                if let Some(s) = chats.get(&id) {
                    s.chat
                        .push_sys(&format!(
                            "[検証] 差し戻し上限（{}回）に達しました。「要人対応」として人の判断を待ちます",
                            n
                        ))
                        .await;
                }
            }
            Action::None => {}
        }
        // done 済み chat 内で生じた変更は派生課題として intake へ起票する
        if let Some((label, repo, last_prompt, touched)) = derived {
            let slug = repo_slug(repo.as_ref()).unwrap_or_default();
            let head = last_prompt.lines().next().unwrap_or("").trim();
            let title = if head.is_empty() {
                format!("派生: {}", label)
            } else {
                format!("派生: {}", head.chars().take(60).collect::<String>())
            };
            let cand = json!({
                "source": "derived",
                "repo": slug,
                "slug": slug,
                "title": title,
                "body": format!(
                    "chat #{}「{}」の完了後ターンで変更が発生（touched: {}）。\n\
                     適用済みの変更を検証し、不足があれば是正する。\n\n元の依頼:\n{}",
                    id,
                    label,
                    touched.join(", "),
                    last_prompt
                ),
                "detectedAt": now_iso(),
            });
            let (issue, _created, _deferred) = crate::detectors::ingest(&self.root, &cand);
            self.sse("candidates", &json!({}));
            self.sse("issues", &json!({}));
            let chats = self.chats.lock().await;
            if let Some(s) = chats.get(&id) {
                s.chat
                    .push_sys(&format!(
                        "[派生課題] 変更を別課題として起票: {}",
                        issue.get("title").and_then(|v| v.as_str()).unwrap_or("")
                    ))
                    .await;
            }
        }
        if do_aggregate {
            self.aggregate_to_parent(id).await;
            // 子の完了で同時実行枠が空いたら親の待機キューを drain する
            if let Some(pid) = parent_id {
                self.spawn_splits(pid).await;
            }
        }
        if do_splits {
            self.spawn_splits(id).await;
        }
        if do_verify {
            self.start_verify(id).await;
        }
        // 課題委譲 chat（issueExecMode=parallel）の終了区分を課題状態へ
        // 写像する（issue-lifecycle.mmd）:
        //   done → 課題 done（chat 側検証を内包・GitHub Issue へ結果報告）
        //   unverified → 課題 queued（検証未確認・人の再判断へ）
        //   ng/未了（verify 差し戻し待ち等）は課題 running のまま継続
        let issue_map = {
            let chats = self.chats.lock().await;
            chats.get(&id).and_then(|s| {
                s.issue_key
                    .clone()
                    .and_then(|k| s.done_kind.clone().map(|d| (k, d)))
            })
        };
        if let Some((ikey, dkind)) = issue_map {
            // 委譲課題が running の時だけ写像する（chat が done を維持して
            // 後続ターンを処理する場合の再写像や、人が既に判断した課題への
            // 上書きを防ぐ）
            let cur_running = crate::runner::find_issue(&self.root, &ikey)
                .and_then(|it| {
                    it.get("status").and_then(|v| v.as_str()).map(String::from)
                })
                .as_deref()
                == Some(pipeline::ST_RUNNING);
            match (dkind.as_str(), cur_running) {
                ("done", true) => {
                    match pipeline::request_transition(&self.root, &ikey, pipeline::ST_DONE) {
                        Ok(_) => {
                            conlog(
                                "control",
                                "sys",
                                &format!("{}: 委譲 chat #{} の完了で課題 done", ikey, id),
                            );
                            let _ = pipeline::set_field(
                                &self.root,
                                &ikey,
                                "finishedAt",
                                json!(now_iso()),
                            );
                            if let Some(it) = crate::runner::find_issue(&self.root, &ikey) {
                                let reason = format!(
                                    "[kasugai-agent] 委譲 chat #{} による対応が完了しました",
                                    id
                                );
                                crate::runner::report_issue(self, &it, &reason).await;
                                // GitHub 由来の課題は完了理由を投稿した上で
                                // issue をクローズする（issue-lifecycle.mmd）
                                let cfg = self.cfg().await;
                                if let Some(repo) = App::git_repo(
                                    &cfg,
                                    it.get("repo").and_then(|v| v.as_str()).unwrap_or(""),
                                ) {
                                    let slug = it
                                        .get("slug")
                                        .and_then(|v| v.as_str())
                                        .map(String::from)
                                        .or_else(|| repo_slug(Some(repo)))
                                        .unwrap_or_default();
                                    let num = it.get("number").and_then(|v| v.as_u64());
                                    if !slug.is_empty() && num.is_some() {
                                        let dir = std::path::Path::new(
                                            repo.get("path")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or(""),
                                        );
                                        if let Err(e) = crate::github::issue_close(
                                            dir,
                                            &slug,
                                            num.unwrap(),
                                        )
                                        .await
                                        {
                                            conlog(
                                                "control",
                                                "err",
                                                &format!(
                                                    "issue #{} のクローズ失敗: {}",
                                                    num.unwrap(),
                                                    e
                                                ),
                                            );
                                        }
                                    }
                                }
                            }
                            // 完了課題はこの枠に残さず、課題レコードを削除する
                            // （完了表示は中央ペインの委譲 chat カードが担う。
                            // 監査・証跡・知見は別系統に残る）。
                            // postPush 監視中の課題は監視終了まで残し、
                            // drain の sweep が終了後に削除する
                            let pp_active = crate::runner::find_issue(&self.root, &ikey)
                                .and_then(|it| {
                                    it.get("postPush")
                                        .and_then(|p| p.get("state"))
                                        .and_then(|v| v.as_str())
                                        .map(String::from)
                                })
                                .is_some_and(|s| {
                                    matches!(s.as_str(), "checking" | "waiting" | "fixing")
                                });
                            if !pp_active {
                                // 削除前に case/eval へ同期（学習データの
                                // 取りこぼし防止）
                                crate::cases::sync_cases(&self.root);
                                crate::evals::sync_evals(&self.root);
                                crate::audit::record(
                                    &self.root,
                                    &ikey,
                                    "auto",
                                    "complete-delete",
                                    None,
                                    Map::from_iter([("chat".into(), json!(id))]),
                                );
                            }
                            if !pp_active && crate::store::delete_issue(&self.root, &ikey) {
                                let mut chats = self.chats.lock().await;
                                if let Some(s) = chats.get_mut(&id) {
                                    s.issue_key = None;
                                }
                                drop(chats);
                                self.persist_chats().await;
                            }
                        }
                        Err(e) => conlog(
                            "control",
                            "err",
                            &format!("{}: done 遷移失敗: {}", ikey, e),
                        ),
                    }
                    self.sse("issues", &json!({}));
                }
                ("unverified", true) => {
                    if let Err(e) =
                        pipeline::request_transition(&self.root, &ikey, pipeline::ST_QUEUED)
                    {
                        conlog(
                            "control",
                            "err",
                            &format!("{}: queued 差し戻し失敗: {}", ikey, e),
                        );
                    } else {
                        // queued へ戻った課題は人の再 dispatch で新規 chat を
                        // 起票するため、この chat の委譲リンクを外す
                        // （旧 chat への後続送信で誤写像しない）
                        let _ = pipeline::set_field(
                            &self.root,
                            &ikey,
                            "chatId",
                            Value::Null,
                        );
                        let mut chats = self.chats.lock().await;
                        if let Some(s) = chats.get_mut(&id) {
                            s.issue_key = None;
                        }
                        drop(chats);
                        self.persist_chats().await;
                    }
                    self.sse("issues", &json!({}));
                }
                _ => {}
            }
        }
    }

    async fn start_verify(self: &Arc<Self>, id: i64) -> bool {
        {
            let mut chats = self.chats.lock().await;
            let Some(s) = chats.get_mut(&id) else { return false };
            let c = s.chat.core().lock().await;
            if c.busy || s.verify.as_deref() == Some("running") {
                return false;
            }
            s.verify = Some("running".to_string());
            s.verify_count += 1;
            s.verify_scan = c.messages.len();
        }
        self.persist_chats().await;
        self.sse("chats", &json!({}));
        if let Err(e) = self
            .send_to_chat(
                id,
                VERIFY_PROMPT.to_string(),
                Some("[検証] 実装を実機で確認してください".to_string()),
            )
            .await
        {
            let chats = self.chats.lock().await;
            if let Some(s) = chats.get(&id) {
                s.chat.push_sys(&format!("[検証起動失敗] {}", e)).await;
            }
        }
        true
    }

    // ---------- 処理再開の共通ロジック ----------
    // 処理再開ボタン（continue_resumable=true）と起動時の自動再処理
    // （false）で共有する。仕分けの優先度: 差し戻し待ち → 検証待ち →
    // 実行待機 → 再開可/未着手。
    //   back_prompt あり（差し戻し待ち）        → 保留 prompt を再送
    //   verify=pending                         → start_verify（検証ターン起動）
    //   waiting（kind!=repo）で blocker 全非busy → leave_wait + 保留 prompt 再送
    //   resumable（acp・proc無・saved sid有）   → 再接続+中断作業の続行 prompt
    //     ※ continue_resumable=false の起動時再処理では行わない
    //   未着手（acp・proc無・saved sid無し）     → 保留 prompt があれば起動して
    //     再送（無ければ continue_resumable 時のみ session/new 起動）
    // busy 中・repo 確認待ち（人ゲート）・要人対応は触らない。
    pub async fn resume_all(
        self: &Arc<Self>,
        repo_f: Option<String>,
        continue_resumable: bool,
    ) -> Value {
        let (rework_ids, verify_ids, resume_ids, waiter_ids) = {
            let chats = self.chats.lock().await;
            let mut busy = HashSet::new();
            for (id, s) in chats.iter() {
                if s.chat.core().lock().await.busy {
                    busy.insert(*id);
                }
            }
            let (mut rw, mut v, mut r, mut w) =
                (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            for (id, s) in chats.iter() {
                if let Some(rf) = &repo_f {
                    if repo_slug(s.repo.as_ref()).as_deref() != Some(rf.as_str()) {
                        continue;
                    }
                }
                let c = s.chat.core().lock().await;
                if c.busy || s.needs_human {
                    continue;
                }
                if s.back_prompt.is_some() {
                    rw.push(*id);
                } else if s.verify.as_deref() == Some("pending") {
                    v.push(*id);
                } else if let Some(wstate) = &c.waiting {
                    if wstate.kind != "repo"
                        && !wstate.blocked_by.iter().any(|b| busy.contains(b))
                    {
                        w.push(*id);
                    }
                } else if s.chat.transport() == "acp" && !s.chat.has_proc().await {
                    // saved sid 有り=再開可 / 無し=未着手。どちらも起動対象
                    r.push(*id);
                }
            }
            (rw, v, r, w)
        };
        // 差し戻し待ち → 保留の差し戻し prompt を再送（send が session/load を兼ねる）
        let mut n_rework = 0usize;
        for id in rework_ids {
            let job = {
                let mut chats = self.chats.lock().await;
                let Some(s) = chats.get_mut(&id) else { continue };
                s.back_prompt.take().map(|p| (p, s.verify_back))
            };
            let Some((prompt, n)) = job else { continue };
            if let Err(e) = self
                .send_to_chat(
                    id,
                    prompt,
                    Some(format!("[差し戻し {}回目] 検証NG → 再実装", n)),
                )
                .await
            {
                let chats = self.chats.lock().await;
                if let Some(s) = chats.get(&id) {
                    s.chat.push_sys(&format!("[差し戻し失敗] {}", e)).await;
                }
            } else {
                n_rework += 1;
            }
        }
        // 検証待ち → 検証ターン起動（send が session/load を兼ねる）
        let mut n_verify = 0usize;
        for id in verify_ids {
            if self.start_verify(id).await {
                n_verify += 1;
            }
        }
        // 再開可 → 再接続（send 内で session/load）して中断作業の続行を促す。
        // 未着手（saved sid 無し）→ 起動失敗で保留の prompt があればそれを再送、
        // 無ければ ensure_session で session/new 起動のみ（未着手から抜け出す）
        let mut n_resumed = 0usize;
        for id in resume_ids {
            enum Job {
                Send(String, Option<String>),
                StartOnly,
                Skip,
            }
            let job = {
                let chats = self.chats.lock().await;
                let Some(s) = chats.get(&id) else { continue };
                let mut c = s.chat.core().lock().await;
                if c.saved_session_id.is_some() {
                    if continue_resumable {
                        Job::Send(
                            "セッションが切断されました。中断していた作業を続行してください。"
                                .to_string(),
                            Some("[処理再開] 中断していた作業を続行してください".to_string()),
                        )
                    } else {
                        Job::Skip
                    }
                } else {
                    let held = std::mem::take(&mut c.held_prompts);
                    if held.is_empty() {
                        if continue_resumable {
                            Job::StartOnly
                        } else {
                            Job::Skip
                        }
                    } else {
                        Job::Send(
                            held.join("\n"),
                            Some("[処理再開] 起動失敗で保留されていた送信を再送します".to_string()),
                        )
                    }
                }
            };
            match job {
                Job::Send(text, disp) => {
                    if let Err(e) = self.send_to_chat(id, text, disp).await {
                        let chats = self.chats.lock().await;
                        if let Some(s) = chats.get(&id) {
                            s.chat.push_sys(&format!("[処理再開失敗] {}", e)).await;
                        }
                    } else {
                        n_resumed += 1;
                    }
                }
                Job::StartOnly => {
                    let chat = self.chats.lock().await.get(&id).map(|s| s.chat.clone());
                    if let Some(chat) = chat {
                        // 同時起動枠が無ければセッション起動を見送る
                        // （ensure_session は proc spawn を伴うため。
                        // 開始のみの再開は idle 追い出ししない）
                        match self.acquire_proc_slot(id, false).await {
                            Err(_) => {}
                            Ok(()) => match chat.ensure_session().await {
                                Ok(()) => n_resumed += 1,
                                Err(e) => {
                                    chat.push_sys(&format!("[処理再開失敗] {}", e)).await;
                                }
                            },
                        }
                    }
                }
                Job::Skip => {}
            }
        }
        // 宙ぶらの実行待機 → 再開後の busy 集合で再判定し、解放して保留を再送
        // （release_waiters と同じ経路。解放前に再開した blocker は busy に
        //   戻っているため待機は維持される）
        let mut n_released = 0usize;
        {
            let chats = self.chats.lock().await;
            let mut busy = HashSet::new();
            for (id, s) in chats.iter() {
                if s.chat.core().lock().await.busy {
                    busy.insert(*id);
                }
            }
            let mut work: Vec<(i64, String)> = Vec::new();
            for id in &waiter_ids {
                let Some(s) = chats.get(id) else { continue };
                let mut c = s.chat.core().lock().await;
                let Some(wstate) = c.waiting.as_mut() else { continue };
                // repo 確認待ちは人ゲート、capacity 待機は枠管理が解放する
                if wstate.kind == "repo" || wstate.kind == "capacity" {
                    continue;
                }
                wstate.blocked_by.retain(|b| busy.contains(b));
                if !wstate.blocked_by.is_empty() {
                    continue;
                }
                if let Some(text) = c.leave_wait() {
                    let cwd = s.chat.cwd().read().await.clone();
                    setup_claims_core(&mut c, &text, &cwd);
                    work.push((*id, text));
                }
            }
            drop(chats);
            for (id, text) in work {
                let chats2 = self.chats.lock().await;
                if let Some(s) = chats2.get(&id) {
                    let chat = s.chat.clone();
                    drop(chats2);
                    self.send_with_slot(id, &chat, text).await;
                    n_released += 1;
                }
            }
        }
        self.drain_capacity().await;
        if n_rework + n_verify + n_resumed + n_released > 0 {
            conlog(
                "control",
                "sys",
                &format!(
                    "処理再開: 差し戻し再送 {}・続行 {}・検証起動 {}・待機解放 {}",
                    n_rework, n_resumed, n_verify, n_released
                ),
            );
        }
        // back_prompt/held_prompts/waiting を変更しているため必ず永続化
        // （未保存で落ちると差し戻し prompt が復活し二重送信され得る）
        self.persist_chats().await;
        self.sse("chats", &json!({}));
        json!({"ok": true, "reworked": n_rework, "resumed": n_resumed, "verify": n_verify, "released": n_released})
    }

    /// スリープ中（resumable: acp・proc無・savedSessionId有）の chat を
    /// 全件 ensure_session で起動する（panel-layout.mmd の「スリープ起動」）。
    /// prompt は送らず session/load のみ → busy 化しない。同時起動枠は
    /// evict_idle=false で確保し、枠超過分はスリープのまま残す
    /// （次回送信時に lazy 再開）。busy・要人対応・待機中は対象外。
    pub async fn start_suspended(self: &Arc<Self>, repo_f: Option<String>) -> Value {
        let ids: Vec<i64> = {
            let chats = self.chats.lock().await;
            let mut out = Vec::new();
            for (id, s) in chats.iter() {
                if let Some(rf) = &repo_f {
                    if repo_slug(s.repo.as_ref()).as_deref() != Some(rf.as_str()) {
                        continue;
                    }
                }
                let c = s.chat.core().lock().await;
                if s.chat.transport() == "acp"
                    && !s.chat.has_proc().await
                    && c.saved_session_id.is_some()
                    && !c.busy
                    && c.waiting.is_none()
                    && !s.needs_human
                {
                    out.push(*id);
                }
            }
            out
        };
        let mut started = 0usize;
        let mut skipped = 0usize;
        let mut failed = 0usize;
        for id in ids {
            if self.acquire_proc_slot(id, false).await.is_err() {
                skipped += 1;
                continue;
            }
            let chat = self.chats.lock().await.get(&id).map(|s| s.chat.clone());
            let Some(chat) = chat else { continue };
            match chat.ensure_session().await {
                Ok(()) => {
                    clear_attach_transcript(self, id).await;
                    started += 1;
                }
                Err(e) => {
                    failed += 1;
                    chat.push_sys(&format!("[起動失敗] {}", e)).await;
                }
            }
        }
        if started + skipped + failed > 0 {
            conlog(
                "control",
                "sys",
                &format!(
                    "スリープ一括起動: 起動 {}・枠超過skip {}・失敗 {}",
                    started, skipped, failed
                ),
            );
        }
        self.sse("chats", &json!({}));
        json!({"ok": true, "started": started, "skipped": skipped, "failed": failed})
    }

    pub async fn spawn_splits(self: &Arc<Self>, id: i64) {
        let par_max_children = cfg_u64(&self.cfg().await, "parallelMaxChildren", 4);
        let mut work: Vec<(String, String)> = Vec::new(); // (title, prompt)
        {
            let mut chats = self.chats.lock().await;
            // parallelMaxChildren は同時実行上限。アクティブな子
            // （busy／実装待機／検証中）だけが枠を占有し、完了・削除済みは解放される
            let mut n_active = 0u64;
            for x in chats.values() {
                if x.parent != Some(id) {
                    continue;
                }
                let c = x.chat.core().lock().await;
                if c.busy || c.waiting.is_some() || x.verify.is_some() {
                    n_active += 1;
                }
            }
            let Some(s) = chats.get_mut(&id) else { return };
            let c = s.chat.core().lock().await;
            let len = c.messages.len();
            if s.split_scan > len {
                s.split_scan = len;
            }
            let start = s.split_scan;
            s.split_scan = len;
            let mut sys_notes: Vec<String> = Vec::new();
            for m in &c.messages[start..] {
                if m.role != "agent" {
                    continue;
                }
                for sub in extract_splits(&m.text) {
                    let key = split_key(&sub);
                    if s.seen_splits.contains(&key)
                        || s.pending_splits.iter().any(|p| split_key(p) == key)
                    {
                        continue;
                    }
                    if n_active + work.len() as u64 >= par_max_children {
                        // 上限超過は打ち捨てず待機キューへ（子完了で順次起動）
                        sys_notes.push(format!(
                            "[並列] 同時実行上限（{}）のため「{}」は待機キューに追加しました",
                            par_max_children, sub.title
                        ));
                        s.pending_splits.push(sub);
                        continue;
                    }
                    s.seen_splits.insert(key);
                    sys_notes.push(format!(
                        "[並列] 分割タスク「{}」→ 子チャットを起動します",
                        sub.title
                    ));
                    work.push((sub.title, sub.prompt));
                }
            }
            drop(c);
            // 空き枠があれば待機キューから順次起動する
            while n_active + (work.len() as u64) < par_max_children
                && !s.pending_splits.is_empty()
            {
                let sub = s.pending_splits.remove(0);
                s.seen_splits.insert(split_key(&sub));
                sys_notes.push(format!(
                    "[並列] 待機中の分割タスク「{}」→ 子チャットを起動します",
                    sub.title
                ));
                work.push((sub.title, sub.prompt));
            }
            if !sys_notes.is_empty() {
                let mut c = s.chat.core().lock().await;
                for n in &sys_notes {
                    c.push("sys", n);
                }
            }
        }
        if work.is_empty() {
            let dirty = self
                .chats
                .lock()
                .await
                .get(&id)
                .map(|s| !s.seen_splits.is_empty() || !s.pending_splits.is_empty())
                .unwrap_or(false);
            if dirty {
                self.persist_chats().await;
                self.sse("chat", &json!({"id": id}));
                self.sse("chats", &json!({}));
            }
            return;
        }
        self.persist_chats().await;
        self.sse("chat", &json!({"id": id}));
        for (title, prompt) in work {
            let (repo, agent, depth) = {
                let chats = self.chats.lock().await;
                let s = match chats.get(&id) {
                    Some(s) => s,
                    None => return,
                };
                (
                    s.repo.clone(),
                    s.chat.agent().get("id").and_then(|v| v.as_str()).map(String::from),
                    s.depth,
                )
            };
            let cid = self
                .new_chat(NewChatOpts {
                    label: Some(title.clone()),
                    repo,
                    agent,
                    mode: Some("direct".to_string()),
                    parent: Some(id),
                    depth,
                    ..Default::default()
                })
                .await;
            {
                let chats = self.chats.lock().await;
                if let Some(s) = chats.get(&id) {
                    let mut c = s.chat.core().lock().await;
                    c.push(
                        "sys",
                        &format!("[並列] 分割タスク「{}」→ 子チャット #{} を起動しました", title, cid),
                    );
                }
            }
            if let Err(e) = self
                .send_to_chat(
                    cid,
                    format!("[親チャット #{} の分割タスク「{}」]\n{}", id, title, prompt),
                    None,
                )
                .await
            {
                let chats = self.chats.lock().await;
                if let Some(s) = chats.get(&cid) {
                    s.chat.push_sys(&format!("[起動失敗] {}", e)).await;
                }
            }
        }
        self.sse("chats", &json!({}));
    }

    async fn aggregate_to_parent(self: &Arc<Self>, id: i64) {
        let (parent, label, body) = {
            let chats = self.chats.lock().await;
            let Some(s) = chats.get(&id) else { return };
            let Some(pid) = s.parent else { return };
            let c = s.chat.core().lock().await;
            let last = c
                .messages
                .iter()
                .rev()
                .find(|m| m.role == "agent" && !m.text.trim().is_empty());
            let body = last
                .map(|m| m.text.trim().chars().take(3000).collect::<String>())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| "（応答なし）".to_string());
            (pid, s.label.clone(), body)
        };
        {
            let chats = self.chats.lock().await;
            if let Some(p) = chats.get(&parent) {
                let mut c = p.chat.core().lock().await;
                c.push(
                    "sys",
                    &format!("[並列] 子チャット #{}「{}」が完了 → 結果を統合します", id, label),
                );
            }
        }
        let text = format!(
            "[子チャット #{}「{}」の完了報告]\n以下は子チャットの最終回答です。内容を踏まえて統合・追検討を続けてください:\n\n{}",
            id, label, body
        );
        if let Err(e) = self.send_to_chat(parent, text, None).await {
            let chats = self.chats.lock().await;
            if let Some(p) = chats.get(&parent) {
                p.chat.push_sys(&format!("[集約失敗] {}", e)).await;
            }
        }
    }

    /// 内部経路の送信（sendToChat 移植）: 待機 → 保留、パス競合 → 実装待機
    pub(crate) async fn send_to_chat(
        self: &Arc<Self>,
        id: i64,
        text: String,
        display: Option<String>,
    ) -> Result<(), String> {
        let chat = {
            let chats = self.chats.lock().await;
            match chats.get(&id) {
                Some(s) => s.chat.clone(),
                None => return Ok(()),
            }
        };
        if chat.core().lock().await.waiting.is_some() {
            return chat.send(text, display).await;
        }
        let cwd = chat.cwd().read().await.clone();
        let paths = extract_paths(&text, &cwd);
        let hits = self.find_chat_conflicts(id, &paths).await;
        if !hits.is_empty() {
            let files: Vec<String> = hits.iter().flat_map(|h| h.files.clone()).collect();
            let blocked: Vec<i64> = hits.iter().map(|h| h.id).collect();
            match &chat {
                // ACP は session/cancel でターンのみ中断、HTTP 型は busy 中断も必要
                Chat::Acp(c) => c.notify_cancel().await,
                Chat::Http(h) => h.enter_wait_busy().await,
            }
            {
                let mut c = chat.core().lock().await;
                c.hold(&text, files, blocked, "conflict", None, display.as_deref());
            }
            self.sse("chat", &json!({"id": id}));
            return Ok(());
        }
        {
            let mut c = chat.core().lock().await;
            setup_claims_core(&mut c, &text, &cwd);
        }
        // 同時起動セッション枠（maxConcurrentSessions・既定3・0=無制限）:
        // proc 未保有の chat が新規起動する前に枠を確保する。満杯なら
        // 最も古い idle proc を suspend（savedSessionId 保持で自動再開可）
        // して空け、全枠 busy なら capacity 待機へ
        if !chat.has_proc().await {
            if let Err(blockers) = self.acquire_proc_slot(id, true).await {
                let mut c = chat.core().lock().await;
                c.hold(&text, Vec::new(), blockers, "capacity", None, display.as_deref());
                drop(c);
                self.sse("chat", &json!({"id": id}));
                return Ok(());
            }
        }
        let r = chat.send(text, display).await;
        if r.is_ok() {
            clear_attach_transcript(self, id).await;
        }
        r
    }

    /// 同時起動セッション枠の確保: proc 保持 chat 数が
    /// maxConcurrentSessions（0=無制限）未満なら Ok。満杯なら最も
    /// 活動時刻の古い idle（busy でない）proc を suspend して枠を空け
    /// Ok。全 proc が busy なら Err(その id 一覧) → caller は待機へ
    /// 同時起動枠の確保。evict_idle=true（送信経路）のみ満杯時に
    /// 最古の idle proc を suspend して枠を空ける。resume/開始のみの
    /// 再開要求は evict_idle=false で呼び、枠が無ければ追い出さず Err
    /// を返す（呼出側は savedSessionId を残したまま skip → 次回送信で
    /// lazy 再開）。resume が idle proc を逐次追い出す spawn→kill
    /// コンベアを防ぐための区別
    pub async fn acquire_proc_slot(
        self: &Arc<Self>,
        self_id: i64,
        evict_idle: bool,
    ) -> Result<(), Vec<i64>> {
        let max = cfg_f64(&self.cfg().await, "maxConcurrentSessions", 3.0);
        if max <= 0.0 {
            return Ok(());
        }
        let chats = self.chats.lock().await;
        let mut n_live = 0i64;
        let mut blockers = Vec::new();
        let mut oldest_idle: Option<(i64, i64)> = None; // (last_activity, id)
        for (id, s) in chats.iter() {
            if *id == self_id || !s.chat.has_proc().await {
                continue;
            }
            n_live += 1;
            let c = s.chat.core().lock().await;
            // busy または起動中（session 未確立=ready false）の proc は
            // suspend 不可 — ハンドシェイク途中で kill すると
            // 「initialize: session closed」で起動自体が失敗する
            if c.busy || !c.ready {
                blockers.push(*id);
            } else if oldest_idle.map(|(t, _)| c.last_activity < t).unwrap_or(true) {
                oldest_idle = Some((c.last_activity, *id));
            }
        }
        if n_live < max as i64 {
            return Ok(());
        }
        // idle proc を suspend（savedSessionId は残るので次回送信で自動再開）
        // — 送信経路（evict_idle）のみ。resume 経路は追い出さない
        if let Some((_, suspend_id)) = oldest_idle.filter(|_| evict_idle) {
            if let Some(s) = chats.get(&suspend_id) {
                s.chat
                    .push_sys(
                        "[一時停止] 同時起動セッション数の上限のため停止しました（次の送信で自動再開します）",
                    )
                    .await;
                s.chat.kill_proc().await;
                drop(chats);
                let _ = self.evt_tx.send(CtrlEvt::Changed { id: suspend_id });
                return Ok(());
            }
        }
        Err(blockers)
    }

    /// capacity 待機の drain: 枠が空いた順に最古の待機 chat を解放して
    /// 保留 prompt を再送する。release_waiters の末尾から呼ぶ
    pub async fn drain_capacity(self: &Arc<Self>) {
        loop {
            let next = {
                let chats = self.chats.lock().await;
                let mut best: Option<(i64, String)> = None; // (id, since)
                for (id, s) in chats.iter() {
                    let c = s.chat.core().lock().await;
                    let Some(w) = c.waiting.as_ref() else {
                        continue;
                    };
                    if w.kind != "capacity" {
                        continue;
                    }
                    if best.as_ref().map(|(_, t)| w.since < *t).unwrap_or(true) {
                        best = Some((*id, w.since.clone()));
                    }
                }
                best.map(|(id, _)| id)
            };
            let Some(id) = next else { break };
            if self.acquire_proc_slot(id, true).await.is_err() {
                break;
            }
            let send_job = {
                let chats = self.chats.lock().await;
                match chats.get(&id) {
                    Some(s) => {
                        let chat = s.chat.clone();
                        let mut c = chat.core().lock().await;
                        if c.waiting.as_ref().map(|w| w.kind.as_str()) != Some("capacity") {
                            None
                        } else {
                            match c.leave_wait() {
                                Some(text) => {
                                    let cwd = s.chat.cwd().try_read().map(|r| r.clone()).unwrap_or_default();
                                    setup_claims_core(&mut c, &text, &cwd);
                                    drop(c);
                                    Some((chat, text))
                                }
                                None => None,
                            }
                        }
                    }
                    None => None,
                }
            };
            match send_job {
                Some((chat, text)) => {
                    let _ = self.evt_tx.send(CtrlEvt::Changed { id });
                    if let Err(e) = chat.send(text, None).await {
                        chat.push_sys(&format!("[再開失敗] {}", e)).await;
                    }
                }
                None => continue,
            }
        }
    }

    /// claims.mjs findConflicts 相当: 他 busy chat のクレームとの衝突
    async fn find_chat_conflicts(&self, self_id: i64, paths: &[String]) -> Vec<Conflict> {
        // 並列モード（conflictWait=false・POST /api/chat/parallel）:
        // ファイル競合の直列化を無効化し、send 時の保留・FileTouch 時の
        // 中断待機をいずれも抑止して全実行を並列に進める
        if !cfg_bool_default_true(&self.cfg().await, "conflictWait") {
            return Vec::new();
        }
        let chats = self.chats.lock().await;
        let mut map = Vec::new();
        for (oid, s) in chats.iter() {
            let c = s.chat.core().lock().await;
            if !c.busy {
                continue;
            }
            let mut claims: HashSet<String> = c.touched.clone();
            claims.extend(c.declared.iter().cloned());
            map.push((*oid, claims));
        }
        find_conflicts(&map, self_id, paths)
    }

    async fn close_chat_entry(self: &Arc<Self>, id: i64) -> bool {
        let s = self.chats.lock().await.remove(&id);
        let existed = s.is_some();
        let parent = s.as_ref().and_then(|s| s.parent);
        let issue_key = s.as_ref().and_then(|s| s.issue_key.clone());
        if let Some(s) = s {
            let _ = s.chat.reset().await;
            let _ = std::fs::remove_dir_all(self.snaps.join(id.to_string()));
        }
        // 課題委譲 chat（issueExecMode=parallel）を消した場合、委譲元課題が
        // running のまま滞留しないよう queued へ戻してリンクを外す
        if let Some(ikey) = issue_key {
            let _ = pipeline::set_field(&self.root, &ikey, "chatId", Value::Null);
            if let Err(e) =
                pipeline::request_transition(&self.root, &ikey, pipeline::ST_QUEUED)
            {
                conlog(
                    "control",
                    "warn",
                    &format!("{}: 委譲 chat 削除時の queued 差し戻し: {}", ikey, e),
                );
            }
            self.sse("issues", &json!({}));
        }
        self.busy_prev.lock().await.remove(&id);
        self.release_waiters(id).await;
        // 子の削除で同時実行枠が空いたら親の待機キューを drain する
        if let Some(pid) = parent {
            self.spawn_splits(pid).await;
        }
        existed
    }

    // ---------- control 自己再起動（README §8） ----------
    // 延期判定は「実行中 run」（busy）のみ。waiting は実行系を占有しておらず
    // （待機状態は chats.json へ persist して再起動後に復元する）、repo 確認待ち
    // 等の人ゲート待機が残る限り drift 再起動が永久延期になるため含めない
    pub async fn busy_ids(&self) -> Vec<i64> {
        let chats = self.chats.lock().await;
        let mut out = Vec::new();
        for (id, s) in chats.iter() {
            if s.chat.core().lock().await.busy {
                out.push(*id);
            }
        }
        out
    }

    pub async fn maybe_restart(self: &Arc<Self>, force: bool) -> bool {
        if self.restarting.load(Ordering::SeqCst) {
            return true;
        }
        let ids = self.busy_ids().await;
        if !ids.is_empty() && !force {
            if !self.restart_pending.swap(true, Ordering::SeqCst) {
                conlog(
                    "control",
                    "sys",
                    &format!(
                        "再起動を延期（busy: {}）",
                        ids.iter().map(|i| format!("#{}", i)).collect::<Vec<_>>().join(",")
                    ),
                );
            }
            self.sse("restart", &json!({"pending": true, "busy": ids}));
            let tx = self.evt_tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let _ = tx.send(CtrlEvt::Restart { force: false });
            });
            return false;
        }
        self.restart_pending.store(false, Ordering::SeqCst);
        self.restarting.store(true, Ordering::SeqCst);
        conlog(
            "control",
            "sys",
            &format!(
                "control を再起動します{}",
                if force && !ids.is_empty() { "（強制・実行中を中断）" } else { "" }
            ),
        );
        self.sse(
            "restart",
            &json!({"restarting": true, "forced": force && !ids.is_empty()}),
        );
        // 実行系停止: ACP プロセスを全て畳む
        {
            let chats = self.chats.lock().await;
            for s in chats.values() {
                s.chat.kill_proc().await;
            }
        }
        self.persist_chats().await;
        // detached respawner: 自身を --respawn モードで起動し、旧 PID 終了を待って新起動。
        // spawn 失敗のまま exit すると control が死んだまま戻らないため、
        // 失敗時は再起動を中止して失敗を通知する
        let spawned = std::env::current_exe().map_err(|e| e.to_string()).and_then(|exe| {
            let pid = std::process::id();
            spawn_detached(&exe, &[
                "--respawn".to_string(),
                pid.to_string(),
                exe.to_string_lossy().into_owned(),
            ])
            .map_err(|e| e.to_string())
        });
        if let Err(e) = spawned {
            conlog(
                "control",
                "err",
                &format!("再起動中止: respawner 起動失敗: {}", e),
            );
            self.restarting.store(false, Ordering::SeqCst);
            self.sse(
                "restart",
                &json!({"restarting": false, "failed": true, "error": e}),
            );
            return false;
        }
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            std::process::exit(0);
        });
        true
    }
}

// setupClaims: 新ターンなら過去クレームを捨て、prompt 文中パスを declared へ
fn setup_claims_core(c: &mut crate::chat::Core, text: &str, cwd: &str) {
    if !c.busy {
        c.touched.clear();
        c.declared.clear();
    }
    for d in extract_paths(text, cwd) {
        c.declared.insert(d);
    }
}

#[derive(Default)]
pub struct NewChatOpts {
    pub label: Option<String>,
    pub repo: Option<Value>,
    pub agent: Option<String>,
    pub id: Option<i64>,
    pub session_id: Option<String>,
    pub updated_at: Option<String>,
    pub mode: Option<String>,
    pub parent: Option<i64>,
    pub depth: u64,
    pub primed_for: Option<Primed>,
    pub seen_splits: HashSet<String>,
    pub pending_splits: Vec<crate::split::Split>,
    pub repo_declined: HashSet<String>,
    pub verify: Option<String>,
    pub verify_count: u64,
    pub verify_back: u64,
    pub done_kind: Option<String>,
    pub back_prompt: Option<String>,
    pub needs_human: bool,
    pub issue_key: Option<String>,
    pub usage: Option<crate::chat::Usage>,
    pub waiting: Option<crate::chat::WaitState>,
    pub held_prompts: Vec<String>,
    pub resume_on_release: bool,
    pub attach_src: Option<String>,
}

// detached spawn（respawner 用）
fn spawn_detached(exe: &Path, args: &[String]) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        // 隠し console 付きで起動（respawner の子 tasklist や、その先に
        // 起き上がる新 control の孫プロセスへ継承されて窓が出ない）
        std::process::Command::new(exe)
            .args(args)
            .creation_flags(crate::util::CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
            .spawn()?;
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new(exe).args(args).spawn()?;
    }
    Ok(())
}

// 環境起動・プロトコル・タスク常駐の共有ロジックは crate::launch へ移管
use crate::launch::{launch_env, LaunchResult};

// ---------- HTTP ハンドラ ----------

pub type Shared = Arc<App>;

fn j(v: Value) -> Response {
    Json(v).into_response()
}
fn err(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({"ok": false, "error": msg}))).into_response()
}

async fn body_json(b: Bytes) -> Value {
    serde_json::from_slice(&b).unwrap_or(json!({}))
}

fn qi(a: &Value, k: &str) -> Option<i64> {
    a.get(k).and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
}

async fn api_events(
    State(app): State<Shared>,
) -> Sse<impl Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>> {
    let rx = app.sse.subscribe();
    let stream = async_stream::stream! {
        yield Ok(axum::response::sse::Event::default().comment("retry: 2000"));
        let mut rx = rx;
        let mut hb = tokio::time::interval(std::time::Duration::from_secs(15));
        loop {
            tokio::select! {
                _ = hb.tick() => {
                    yield Ok(axum::response::sse::Event::default().comment("hb"));
                }
                r = rx.recv() => {
                    match r {
                        Ok((ev, data)) => {
                            yield Ok(axum::response::sse::Event::default().event(ev).data(data));
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            }
        }
    };
    Sse::new(stream)
}

async fn api_console(Query(q): Query<HashMap<String, String>>) -> Response {
    let after = q.get("after").and_then(|s| s.parse().ok()).unwrap_or(0);
    let limit = q.get("limit").and_then(|s| s.parse().ok()).unwrap_or(800);
    j(json!({"ok": true, "lines": console_tail(after, limit)}))
}

// 開発インスタンス判定: KASUGAI_DEV=1 が付与されているか、
// 実行 exe が server/target/dev|debug/ 配下（cargo run 等の dev ビルド）の場合
fn is_dev() -> bool {
    if std::env::var("KASUGAI_DEV").is_ok_and(|v| !v.is_empty() && v != "0") {
        return true;
    }
    std::env::current_exe()
        .map(|p| {
            let s = p.to_string_lossy().replace('\\', "/");
            s.contains("/target/dev/") || s.contains("/target/debug/")
        })
        .unwrap_or(false)
}

async fn api_state(State(app): State<Shared>) -> Response {
    let cfg = app.cfg().await;
    let repos: Vec<Value> = cfg_arr(&cfg, "repos").iter().map(|r| App::repo_pub(r)).collect();
    j(json!({
        "ok": true, "version": VERSION, "port": app.port,
        "dev": is_dev(),
        "uptime": app.started.elapsed().as_secs_f64(),
        "restartPending": app.restart_pending.load(Ordering::SeqCst),
        "repos": repos,
        "chats": app.chat_list().await,
        "assets": crate::assets::status(&app).await,
        "autostart": crate::launch::autostart_status(),
        "heartbeat": crate::heartbeat::status(),
        "detectorHealth": crate::detector_health::status(&app.root, &cfg),
        "issueExecMode": cfg_str(&cfg, "issueExecMode").unwrap_or("parallel"),
        "chatParallel": !cfg_bool_default_true(&cfg, "conflictWait"),
        "maxConcurrentSessions": cfg_f64(&cfg, "maxConcurrentSessions", 3.0),
    }))
}

async fn api_repos_get(State(app): State<Shared>) -> Response {
    let cfg = app.cfg().await;
    j(json!({"ok": true, "workspace": app.ws_root, "entries": app.repo_entries(&cfg)}))
}

async fn find_repo(cfg: &Value, slug: &str, path: &str) -> Option<usize> {
    cfg_arr(cfg, "repos")
        .iter()
        .position(|r| {
            repo_slug(Some(r)).as_deref() == Some(slug)
                || norm_p(r.get("path").and_then(|v| v.as_str()).unwrap_or("")) == norm_p(path)
        })
        .filter(|_| !slug.is_empty() || !path.is_empty())
}

async fn api_repos_update(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let slug = a.get("slug").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let path = a.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mut cfg = app.cfg.write().await;
    let Some(i) = find_repo(&cfg, &slug, &path).await else {
        return err(StatusCode::NOT_FOUND, "repo not found");
    };
    let repo = &mut cfg["repos"][i];
    if let Some(v) = a.get("requirePushApproval") {
        repo["requirePushApproval"] = json!(v.as_bool().unwrap_or(false));
    }
    let pub_r = App::repo_pub(&cfg["repos"][i]);
    persist_config(&app.root, &cfg);
    drop(cfg);
    app.sse("repos", &json!({}));
    j(json!({"ok": true, "repo": pub_r}))
}

async fn api_repos_remove(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let slug = a.get("slug").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let path = a.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mut cfg = app.cfg.write().await;
    let Some(i) = find_repo(&cfg, &slug, &path).await else {
        return err(StatusCode::NOT_FOUND, "repo not found");
    };
    let removed = cfg["repos"].as_array_mut().unwrap().remove(i);
    persist_config(&app.root, &cfg);
    drop(cfg);
    app.sse("repos", &json!({}));
    j(json!({"ok": true, "removed": App::repo_pub(&removed)}))
}

async fn api_repos_post(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let rpath = a
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .replace('\\', "/");
    let rpath = rpath.trim_end_matches('/').to_string();
    if rpath.is_empty() {
        return err(StatusCode::BAD_REQUEST, "path required");
    }
    let p = Path::new(&rpath);
    if !p.is_dir() {
        return err(StatusCode::BAD_REQUEST, "not a directory");
    }
    let mut slug = a.get("slug").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if slug.is_empty() {
        let mut gcmd = tokio::process::Command::new("git");
        gcmd.args(["remote", "get-url", "origin"]).current_dir(&rpath);
        crate::util::hide_console_tokio(&mut gcmd);
        if let Ok(out) = gcmd.output().await {
            let u = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if let Some(m) = regex::Regex::new(r"[:/]([^/:\s]+/[^/\s]+?)(?:\.git)?$")
                .unwrap()
                .captures(&u)
            {
                slug = m[1].to_string();
            }
        }
        if slug.is_empty() {
            slug = basename(&rpath);
        }
    }
    let mut cfg = app.cfg.write().await;
    let repos = cfg_arr(&cfg, "repos");
    let dup = repos.iter().find(|r| {
        repo_slug(Some(r)).as_deref() == Some(slug.as_str())
            || norm_p(r.get("path").and_then(|v| v.as_str()).unwrap_or("")) == norm_p(&rpath)
    });
    if let Some(d) = dup {
        return (
            StatusCode::CONFLICT,
            Json(json!({"ok": false, "error": "already registered",
                "slug": repo_slug(Some(*d))})),
        )
            .into_response();
    }
    let repo = json!({"slug": slug, "path": rpath, "requirePushApproval": true});
    if cfg.get("repos").and_then(|v| v.as_array()).is_none() {
        cfg["repos"] = json!([]);
    }
    cfg["repos"].as_array_mut().unwrap().push(repo.clone());
    persist_config(&app.root, &cfg);
    drop(cfg);
    app.sse("repos", &json!({}));
    j(json!({"ok": true, "repo": {"slug": repo["slug"], "path": repo["path"]}}))
}

async fn api_fs_dirs(Query(q): Query<HashMap<String, String>>) -> Response {
    let raw = q.get("path").cloned().unwrap_or_default();
    let qp = regex::Regex::new(r"[/]{2,}").unwrap().replace_all(&raw.replace('\\', "/"), "/").trim().to_string();
    if qp.is_empty() {
        #[cfg(windows)]
        let roots: Vec<String> = ('A'..='Z')
            .map(|d| format!("{}:/", d))
            .filter(|d| Path::new(d).exists())
            .collect();
        #[cfg(not(windows))]
        let roots: Vec<String> = vec!["/".to_string()];
        let dirs: Vec<Value> = roots
            .iter()
            .map(|d| json!({"name": d, "path": d, "git": false}))
            .collect();
        return j(json!({"ok": true, "path": "", "parent": "", "dirs": dirs}));
    }
    let dir = if regex::Regex::new(r"^[A-Za-z]:/$").unwrap().is_match(&qp) || qp == "/" {
        qp.clone()
    } else {
        qp.trim_end_matches('/').to_string()
    };
    let p = Path::new(&dir);
    if !p.is_dir() {
        return err(StatusCode::BAD_REQUEST, "not a directory");
    }
    let mut dirs = Vec::new();
    match std::fs::read_dir(p) {
        Ok(rd) => {
            for e in rd.flatten() {
                if !e.path().is_dir() {
                    continue;
                }
                let dp = format!(
                    "{}{}",
                    if dir.ends_with('/') { dir.clone() } else { format!("{}/", dir) },
                    e.file_name().to_string_lossy()
                );
                dirs.push(json!({"name": e.file_name().to_string_lossy(),
                    "path": dp, "git": Path::new(&dp).join(".git").exists()}));
                if dirs.len() >= 500 {
                    break;
                }
            }
        }
        Err(e2) => return err(StatusCode::BAD_REQUEST, &e2.to_string()),
    }
    dirs.sort_by(|a, b| {
        (b["git"].as_bool().unwrap_or(false) as u8)
            .cmp(&(a["git"].as_bool().unwrap_or(false) as u8))
            .then_with(|| {
                a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or(""))
            })
    });
    let parent = dirname(&dir).replace('\\', "/");
    j(json!({"ok": true, "path": dir,
        "parent": if parent == dir { "" } else { parent.as_str() }, "dirs": dirs}))
}

async fn api_fs_tree(State(app): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
    let raw = q.get("path").cloned().unwrap_or_default();
    let qp = norm_slashes(&normalize(&raw));
    let qp = qp.trim_end_matches('/').trim().to_string();
    let cfg = app.cfg().await;
    if qp.is_empty() || !App::under_repo(&cfg, &qp) {
        return err(StatusCode::FORBIDDEN, "outside repos");
    }
    let p = Path::new(&qp);
    if !p.is_dir() {
        return err(StatusCode::BAD_REQUEST, "not a directory");
    }
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    match std::fs::read_dir(p) {
        Ok(rd) => {
            for e in rd.flatten() {
                let ep = format!("{}/{}", qp, e.file_name().to_string_lossy());
                if e.path().is_dir() {
                    if refs::skip_dirs().contains(e.file_name().to_string_lossy().as_ref()) {
                        continue;
                    }
                    dirs.push(json!({"name": e.file_name().to_string_lossy(), "path": ep}));
                } else if e.path().is_file() {
                    files.push(json!({"name": e.file_name().to_string_lossy(),
                        "path": ep, "denied": refs::denied(&ep)}));
                }
                if dirs.len() + files.len() >= 1000 {
                    break;
                }
            }
        }
        Err(e2) => return err(StatusCode::BAD_REQUEST, &e2.to_string()),
    }
    dirs.sort_by(|a: &Value, b: &Value| a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or("")));
    files.sort_by(|a: &Value, b: &Value| a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or("")));
    j(json!({"ok": true, "path": qp, "dirs": dirs, "files": files}))
}

async fn api_fs_file(State(app): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
    let raw = q.get("path").cloned().unwrap_or_default();
    let qp = norm_slashes(&normalize(&raw));
    let qp = qp.trim_end_matches('/').trim().to_string();
    let cfg = app.cfg().await;
    if qp.is_empty() || !App::under_repo(&cfg, &qp) {
        return err(StatusCode::FORBIDDEN, "outside repos");
    }
    if refs::denied(&qp) {
        return err(StatusCode::FORBIDDEN, "denied");
    }
    let p = Path::new(&qp);
    let Ok(st) = p.metadata() else {
        return err(StatusCode::NOT_FOUND, "not found");
    };
    if !st.is_file() {
        return err(StatusCode::NOT_FOUND, "not found");
    }
    let head = json!({"ok": true, "name": qp.split('/').last().unwrap_or(""),
        "path": qp, "scope": "file"});
    if st.len() > 2 * 1024 * 1024 {
        let mut v = head;
        v["binary"] = json!(true);
        v["text"] = json!("");
        v["truncated"] = json!(false);
        return j(v);
    }
    let text = std::fs::read_to_string(p).unwrap_or_default();
    if text.as_bytes().iter().take(8192).any(|&b| b == 0) {
        let mut v = head;
        v["binary"] = json!(true);
        v["text"] = json!("");
        v["truncated"] = json!(false);
        return j(v);
    }
    let truncated = text.chars().count() > 200 * 1024;
    let mut v = head;
    v["text"] = json!(if truncated {
        text.chars().take(200 * 1024).collect::<String>()
    } else {
        text.clone()
    });
    v["truncated"] = json!(truncated);
    j(v)
}

fn git_log(slug: &str, code: i32, out: &str) {
    for l in out.split('\n').take(30).filter(|l| !l.trim().is_empty()) {
        conlog("git", if code != 0 { "err" } else { "out" }, &format!("[{}] {}", slug, l.trim_end()));
    }
}

async fn api_git_commit(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let cfg = app.cfg().await;
    let slug = a
        .get("slug")
        .or_else(|| a.get("repo"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let Some(repo) = App::git_repo(&cfg, &slug) else {
        return err(StatusCode::NOT_FOUND, "repo not found");
    };
    let path = repo.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let rslug = repo_slug(Some(repo)).unwrap_or_default();
    let message = a.get("message").and_then(|v| v.as_str()).unwrap_or("").trim().chars().take(300).collect::<String>();
    if message.is_empty() {
        return err(StatusCode::BAD_REQUEST, "message required");
    }
    let (code, out) = run_git(&path, &["status", "--porcelain"]).await;
    if code != 0 {
        git_log(&rslug, code, &out);
        return j(json!({"ok": false, "error": out.chars().take(400).collect::<String>()}));
    }
    if out.is_empty() {
        return j(json!({"ok": true, "noop": true}));
    }
    conlog("git", "sys", &format!("[{}] git commit: {}", rslug, message));
    let (acode, aout) = run_git(&path, &["add", "-A"]).await;
    if acode != 0 {
        git_log(&rslug, acode, &aout);
        return j(json!({"ok": false, "error": aout.chars().take(400).collect::<String>()}));
    }
    let (rcode, rout) = run_git(&path, &["commit", "-m", &message]).await;
    git_log(&rslug, rcode, &rout);
    if rcode != 0 {
        return j(json!({"ok": false, "error": rout.chars().take(400).collect::<String>()}));
    }
    j(json!({"ok": true, "out": rout.chars().take(400).collect::<String>()}))
}

async fn api_git_push(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let cfg = app.cfg().await;
    let slug = a
        .get("slug")
        .or_else(|| a.get("repo"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let Some(repo) = App::git_repo(&cfg, &slug) else {
        return err(StatusCode::NOT_FOUND, "repo not found");
    };
    let path = repo.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let rslug = repo_slug(Some(repo)).unwrap_or_default();
    let (_, br_out) = run_git(&path, &["branch", "--show-current"]).await;
    let br = br_out.lines().next().unwrap_or("").trim().to_string();
    let mut args: Vec<String> = vec!["push".to_string()];
    if !br.is_empty() {
        let (ucode, _) = run_git(
            &path,
            &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
        )
        .await;
        if ucode != 0 {
            let (_, rem) = run_git(&path, &["remote"]).await;
            let rems: Vec<&str> = rem.lines().filter(|l| !l.trim().is_empty()).collect();
            let remote = if rems.contains(&"origin") {
                "origin"
            } else {
                rems.first().copied().unwrap_or("")
            };
            if !remote.is_empty() {
                args = vec!["push".to_string(), "-u".to_string(), remote.to_string(), br.clone()];
            }
        }
    }
    conlog("git", "sys", &format!("[{}] git {}", rslug, args.join(" ")));
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let (code, out) = run_git(&path, &arg_refs).await;
    git_log(&rslug, code, &out);
    if code != 0 {
        return j(json!({"ok": false, "error": out.chars().take(400).collect::<String>()}));
    }
    j(json!({"ok": true, "out": out.chars().take(400).collect::<String>()}))
}

async fn api_restart(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let force = a.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
    let pending = !app.maybe_restart(force).await;
    j(json!({"ok": true, "pending": pending, "busy": app.busy_ids().await}))
}

// POST /api/control/import-github — config.repos の GitHub Issue を
// 一括取込する（open+closed 全件。closed は pipeline が終端状態で起票）。
// `kasugai-agent --import-github` と同等の操作を API から行える
async fn api_import_github(State(app): State<Shared>) -> Response {
    let cfg = app.cfg().await;
    let (created, total) =
        crate::detectors::github_issues::import_all(&app.root, &cfg).await;
    if created > 0 {
        app.sse("candidates", &json!({}));
        app.sse("issues", &json!({}));
    }
    conlog(
        "control",
        "sys",
        &format!("github issue 取込: {} 件中 {} 件を新規起票", total, created),
    );
    j(json!({"ok": true, "total": total, "created": created}))
}

// POST /api/control/launch — 環境起動（docs ナビの起動ボタンが呼ぶ。
// 正本: diagrams/env-launch.mmd → README §8）
//   {target:"prod", port?:8586} → server/target/release/kasugai-agent.exe
//                                （無ければ現行 exe）を detached 起動
//   対象ポートが既に応答していれば起動せず already:true を返す
async fn api_launch(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let target = a.get("target").and_then(|v| v.as_str()).unwrap_or("prod");
    let port: u16 = a
        .get("port")
        .and_then(|v| v.as_u64())
        .map(|v| v as u16)
        .unwrap_or(8586);
    match launch_env(&app.root, target, port).await {
        Ok(LaunchResult::Already) => j(json!({"ok": true, "already": true,
            "url": format!("http://127.0.0.1:{}/", port)})),
        Ok(LaunchResult::Launched) => {
            conlog(
                "control",
                "sys",
                &format!("{} 環境を起動要求（:{}）", target, port),
            );
            j(json!({"ok": true, "already": false,
                "url": format!("http://127.0.0.1:{}/", port)}))
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

// ---------- 開発環境の起動管理（dev-envs） ----------
// 正本: diagrams/dev-envs.mmd → README §8「開発環境の起動管理」
// config.devEnvironments[] を対象に、起動（detached spawn・出力は
// logs/devenv-<id>.log）と停止（port 占有 PID の taskkill）を行う

// GET /api/dev-envs — 登録環境の一覧（port 応答で alive 判定）
async fn api_dev_envs(State(app): State<Shared>) -> Response {
    let cfg = app.cfg().await;
    let mut out = vec![];
    for e in crate::devenvs::envs(&cfg) {
        out.push(crate::devenvs::describe(&app.root, &e).await);
    }
    j(json!({"ok": true, "envs": out}))
}

// POST /api/dev-envs/launch — {id} を起動（既に応答中なら already:true）
async fn api_dev_envs_launch(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let id = a.get("id").and_then(|v| v.as_str()).unwrap_or_default();
    let cfg = app.cfg().await;
    let Some(e) = crate::devenvs::find(&cfg, id) else {
        return err(StatusCode::NOT_FOUND, &format!("unknown env: {}", id));
    };
    match crate::devenvs::launch(&app.root, &e).await {
        Ok(LaunchResult::Already) => {
            j(json!({"ok": true, "already": true, "url": e.url_or_default()}))
        }
        Ok(LaunchResult::Launched) => {
            conlog(
                "control",
                "sys",
                &format!("開発環境を起動: {}（{} / :{}）", e.id, e.command, e.port),
            );
            app.sse("devenvs", &json!({"id": e.id}));
            j(json!({"ok": true, "already": false, "url": e.url_or_default()}))
        }
        Err(e2) => err(StatusCode::INTERNAL_SERVER_ERROR, &e2),
    }
}

// POST /api/dev-envs/stop — {id} の port 占有プロセスを taskkill
async fn api_dev_envs_stop(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let id = a.get("id").and_then(|v| v.as_str()).unwrap_or_default();
    let cfg = app.cfg().await;
    let Some(e) = crate::devenvs::find(&cfg, id) else {
        return err(StatusCode::NOT_FOUND, &format!("unknown env: {}", id));
    };
    match crate::devenvs::stop(&e) {
        Ok(killed) => {
            conlog(
                "control",
                "sys",
                &format!("開発環境を停止: {}（:{}, PID {:?}）", e.id, e.port, killed),
            );
            app.sse("devenvs", &json!({"id": e.id}));
            j(json!({"ok": true, "killed": killed}))
        }
        Err(e2) => err(StatusCode::BAD_REQUEST, &e2),
    }
}

// GET /api/control/autostart — 自動起動（HKCU Run キー）の登録状態
// POST /api/control/autostart — {action:"install"|"uninstall"} で登録/解除
async fn api_autostart_get() -> Response {
    j(json!({"ok": true, "autostart": crate::launch::autostart_status()}))
}

async fn api_autostart_post(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let action = a.get("action").and_then(|v| v.as_str()).unwrap_or("");
    let r = match action {
        "install" => match std::env::current_exe() {
            Ok(exe) => crate::launch::install_task(&exe),
            Err(e) => Err(format!("current_exe failed: {}", e)),
        },
        "uninstall" => crate::launch::uninstall_task(),
        _ => return err(StatusCode::BAD_REQUEST, "unknown action"),
    };
    match r {
        Ok(msg) => {
            conlog(
                "control",
                "sys",
                &format!("自動起動 {}: {}", action, msg),
            );
            let st = crate::launch::autostart_status();
            app.sse("autostart", &st);
            j(json!({"ok": true, "autostart": st, "out": msg.chars().take(400).collect::<String>()}))
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

// POST /api/control/issue-mode — 課題の処理方法 issueExecMode の切替
// （run-control.mmd。{mode:"direct"|"parallel"}・既定 parallel）。
// direct=runner パイプラインで直接実行（同一 repo の active 課題が
// ある間 dispatch を止める直列・maxConcurrentRuns クォーラム有効）、
// parallel=課題を chat 起票で委譲し通常のチャットと同じ流れで並列処理
async fn api_issue_mode(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let mode = a.get("mode").and_then(|v| v.as_str()).unwrap_or("");
    if mode != "direct" && mode != "parallel" {
        return err(StatusCode::BAD_REQUEST, "mode must be direct|parallel");
    }
    let mut cfg = app.cfg.write().await;
    cfg["issueExecMode"] = json!(mode);
    persist_config(&app.root, &cfg);
    drop(cfg);
    conlog(
        "control",
        "sys",
        &format!("課題の処理方法を {} に切替", mode),
    );
    app.sse("config", &json!({"issueExecMode": mode}));
    j(json!({"ok": true, "issueExecMode": mode}))
}

// POST /api/control/session-limit — 同時起動セッション数
// maxConcurrentSessions の変更（chat-sequence.mmd。{max:number}・
// 既定5・0=無制限）。ACP 実行体を保持する chat の同時数を制限し、
// 超過起動は最古の idle proc suspend or capacity 待機で調停する
async fn api_session_limit(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let max = a.get("max").and_then(|v| v.as_f64()).unwrap_or(-1.0);
    if !(0.0..=64.0).contains(&max) || max.fract() != 0.0 {
        return err(StatusCode::BAD_REQUEST, "max must be integer 0..=64");
    }
    {
        let mut cfg = app.cfg.write().await;
        cfg["maxConcurrentSessions"] = json!(max);
        persist_config(&app.root, &cfg);
    }
    conlog(
        "control",
        "sys",
        &format!("同時起動セッション数を {} に設定", max),
    );
    app.sse("config", &json!({"maxConcurrentSessions": max}));
    // 引上げ時に滞留した capacity 待機を即座に解放する
    app.drain_capacity().await;
    j(json!({"ok": true, "maxConcurrentSessions": max}))
}

async fn api_chat_agents_get(State(app): State<Shared>) -> Response {
    let cfg = app.cfg().await;
    let (active, agents) = App::chat_agents(&cfg);
    let list: Vec<Value> = agents
        .iter()
        .map(|(id, a)| app.agent_pub(id, a))
        .collect();
    j(json!({"ok": true, "active": active, "agents": list}))
}

async fn api_chat_agents_post(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let exe = a.get("exe").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let endpoint = a.get("endpoint").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let ty = match a.get("type").and_then(|v| v.as_str()) {
        Some("openai") => "openai",
        Some("agent") => "agent",
        _ => {
            if !exe.is_empty() {
                "acp"
            } else if !endpoint.is_empty() {
                "agent"
            } else {
                "acp"
            }
        }
    };
    if ty == "acp" && exe.is_empty() {
        return err(StatusCode::BAD_REQUEST, "exe required");
    }
    if ty != "acp" && endpoint.is_empty() {
        return err(StatusCode::BAD_REQUEST, "endpoint required");
    }
    let mut cfg = app.cfg.write().await;
    let (_, mut agents) = App::chat_agents(&cfg);
    let re_id = regex::Regex::new(r"[^\w-]+").unwrap();
    let id_raw = a
        .get("id")
        .or_else(|| a.get("label"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_lowercase();
    let mut id = re_id.replace_all(&id_raw, "-").trim_matches('-').to_string();
    if id.is_empty() {
        id = basename(&(if exe.is_empty() { endpoint.clone() } else { exe.clone() }))
            .to_lowercase();
        // 拡張子除去
        if let Some(dot) = id.rfind('.') {
            id = id[..dot].to_string();
        }
    }
    if agents.contains_key(&id) {
        return (
            StatusCode::CONFLICT,
            Json(json!({"ok": false, "error": "already registered", "id": id})),
        )
            .into_response();
    }
    let mut entry = Map::new();
    entry.insert("label".into(), json!(a.get("label").and_then(|v| v.as_str()).unwrap_or(&id)));
    if ty == "acp" {
        let args: Vec<Value> = match a.get("args").and_then(|v| v.as_array()) {
            Some(arr) => arr.iter().filter_map(|v| v.as_str()).map(|s| json!(s.to_string())).collect(),
            None => a
                .get("args")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .split_whitespace()
                .map(|s| json!(s.to_string()))
                .collect(),
        };
        entry.insert("exe".into(), json!(exe));
        entry.insert("args".into(), json!(if args.is_empty() { vec![json!("acp")] } else { args }));
        // auth は文字列（none/devin/任意 methodId）または
        // {methodId,tokenEnv|tokenSecret} オブジェクトをそのまま保持
        entry.insert(
            "auth".into(),
            match a.get("auth") {
                Some(v @ Value::Object(_)) => v.clone(),
                Some(v) => json!(v.as_str().unwrap_or("none")),
                None => json!("none"),
            },
        );
        if let Some(env) = a.get("env").and_then(|v| v.as_object()) {
            entry.insert("env".into(), Value::Object(env.clone()));
        }
    } else {
        entry.insert("type".into(), json!(ty));
        entry.insert("endpoint".into(), json!(endpoint));
        for k in ["provider", "model", "project", "tokenKey"] {
            if let Some(v) = a.get(k).and_then(|v| v.as_str()).map(|s| s.trim()).filter(|s| !s.is_empty()) {
                entry.insert(k.into(), json!(v));
            }
        }
    }
    agents.insert(id.clone(), Value::Object(entry));
    cfg["chatAgents"] = Value::Object(agents.clone());
    let key = agents[&id]
        .get("tokenKey")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&id)
        .to_string();
    if let Some(t) = a.get("token").and_then(|v| v.as_str()) {
        secrets_set_token(&app.root, &key, Some(t));
    }
    if cfg.get("chatAgent").is_none() || cfg["chatAgent"].is_null() {
        cfg["chatAgent"] = json!(id);
    }
    persist_config(&app.root, &cfg);
    drop(cfg);
    app.sse("chatAgents", &json!({}));
    j(json!({"ok": true, "agent": app.agent_pub(&id, &agents[&id])}))
}

async fn api_chat_agents_update(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let id = a.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mut cfg = app.cfg.write().await;
    let (_, mut agents) = App::chat_agents(&cfg);
    let Some(ag) = agents.get_mut(&id) else {
        return err(StatusCode::NOT_FOUND, "agent not found");
    };
    if let Some(v) = a.get("label").and_then(|v| v.as_str()) {
        ag["label"] = json!(if v.trim().is_empty() { ag["label"].clone() } else { json!(v.trim()) });
    }
    if let Some(v) = a.get("exe").and_then(|v| v.as_str()) {
        ag["exe"] = json!(v.trim());
    }
    if let Some(v) = a.get("args") {
        let args: Vec<String> = match v.as_array() {
            Some(arr) => arr.iter().filter_map(|x| x.as_str()).map(String::from).collect(),
            None => v.as_str().unwrap_or("").split_whitespace().map(String::from).collect(),
        };
        ag["args"] = json!(args);
    }
    if let Some(v) = a.get("auth") {
        ag["auth"] = if v.is_object() { v.clone() } else { json!(v.as_str().unwrap_or("none")) };
    }
    if let Some(v) = a.get("env") {
        ag["env"] = v.clone();
    }
    for k in ["endpoint", "provider", "model", "project", "tokenKey"] {
        if let Some(v) = a.get(k) {
            let s = v.as_str().unwrap_or("").trim();
            if s.is_empty() {
                ag.as_object_mut().unwrap().remove(k);
            } else {
                ag[k] = json!(s);
            }
        }
    }
    let agv = agents[&id].clone();
    cfg["chatAgents"] = Value::Object(agents.clone());
    let key = agv
        .get("tokenKey")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&id)
        .to_string();
    if let Some(t) = a.get("token") {
        secrets_set_token(&app.root, &key, t.as_str());
    }
    persist_config(&app.root, &cfg);
    drop(cfg);
    app.sse("chatAgents", &json!({}));
    j(json!({"ok": true, "agent": app.agent_pub(&id, &agv)}))
}

async fn api_chat_agents_remove(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let id = a.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mut cfg = app.cfg.write().await;
    let (active, mut agents) = App::chat_agents(&cfg);
    if !agents.contains_key(&id) {
        return err(StatusCode::NOT_FOUND, "agent not found");
    }
    if active.as_deref() == Some(id.as_str()) {
        return err(StatusCode::BAD_REQUEST, "active agent cannot be removed");
    }
    agents.remove(&id);
    cfg["chatAgents"] = Value::Object(agents);
    persist_config(&app.root, &cfg);
    drop(cfg);
    app.sse("chatAgents", &json!({}));
    j(json!({"ok": true, "removed": id}))
}

async fn api_chat_agents_active(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let id = a.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mut cfg = app.cfg.write().await;
    let (_, agents) = App::chat_agents(&cfg);
    if !agents.contains_key(&id) {
        return err(StatusCode::NOT_FOUND, "agent not found");
    }
    cfg["chatAgents"] = Value::Object(agents);
    cfg["chatAgent"] = json!(id);
    persist_config(&app.root, &cfg);
    drop(cfg);
    app.sse("chatAgents", &json!({}));
    j(json!({"ok": true, "active": id}))
}

async fn api_chat_sessions(State(app): State<Shared>) -> Response {
    j(json!({"ok": true, "sessions": app.chat_list().await}))
}

// POST /api/<agent>-sessions/attach {id} — 外部エージェントのセッションを
// kasugai chat として開く。saved_session_id に外部セッション id を立て、
// 初回送信時に ACP session/load で実セッションへ接続する。
// 既に同じセッションにアタッチ済みの chat があればそれを再利用する。
// （共通テンプレ agent-sessions.mmd）
/// attach_info の messages[] を Msg 列へ変換（注入・再同期の共通形）
fn attach_msgs(info: &Value) -> Vec<crate::chat::Msg> {
    info["messages"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|m| crate::chat::Msg {
            role: m["role"].as_str().unwrap_or("sys").to_string(),
            text: m["text"].as_str().unwrap_or("").to_string(),
            ts: m["ts"].as_str().map(String::from).unwrap_or_else(now_iso),
            done: true,
            wire: None,
        })
        .collect()
}

/// attach chat のトランスクリプトを外部セッションと再同期する。
/// messages 先頭 attach_n 件が注入区間なので最新版で splice 置換する
///（セッション成長で追記・枝分かれ巻戻しで縮小の両方を吸収）。
/// 再起動後は messages が空・attach_n=0 なので全件再注入になる。
async fn sync_attach(app: &App, id: i64) {
    let (src, sid) = {
        let chats = app.chats.lock().await;
        let Some(s) = chats.get(&id) else { return };
        let Some(src) = s.attach_src.clone() else { return };
        let c = s.chat.core().lock().await;
        let sid = c
            .saved_session_id
            .clone()
            .or(c.session_id.clone())
            .unwrap_or_default();
        (src, sid)
    };
    if sid.is_empty() {
        return;
    }
    let info = match src.as_str() {
        "devin" => crate::devinenv::attach_info(&sid),
        "copilot" => crate::copilotenv::attach_info(&sid),
        _ => None,
    };
    let Some(info) = info else { return };
    let msgs = attach_msgs(&info);
    let mut chats = app.chats.lock().await;
    let Some(s) = chats.get_mut(&id) else { return };
    let mut c = s.chat.core().lock().await;
    let k = s.attach_n.min(c.messages.len());
    let changed = c.messages[..k]
        .iter()
        .zip(msgs.iter())
        .any(|(a, b)| a.role != b.role || a.text != b.text)
        || k != msgs.len();
    if !changed {
        return;
    }
    c.messages.splice(0..k, msgs.iter().cloned());
    s.attach_n = msgs.len();
    drop(c);
    drop(chats);
    app.sse("chat", &json!({"id": id}));
}

/// session/load 成功後、注入済みトランスクリプト（messages 先頭 attach_n 件）を
/// 除去する。ACP replay が正本となるため注入分は二重表示になる。
/// attach_src も解除し以後の sync_attach 再注入を止める
///（この時点で chat は実セッションと同一になる）。
async fn clear_attach_transcript(app: &App, id: i64) {
    let mut chats = app.chats.lock().await;
    let Some(s) = chats.get_mut(&id) else { return };
    if s.attach_n == 0 && s.attach_src.is_none() {
        return;
    }
    let mut c = s.chat.core().lock().await;
    // session/load 済み（session_id 確立）のときだけ除去する
    if c.session_id.is_none() {
        return;
    }
    let k = s.attach_n.min(c.messages.len());
    c.messages.splice(0..k, std::iter::empty());
    s.attach_n = 0;
    s.attach_src = None;
    drop(c);
    drop(chats);
    app.persist_chats().await;
    app.sse("chat", &json!({"id": id}));
}

async fn agent_session_attach(
    app: &App,
    b: Bytes,
    agent_keys: &[&str],
    attach_info: fn(&str) -> Option<Value>,
    fallback_agent: Option<(&str, Value)>,
) -> Response {
    let a = body_json(b).await;
    let sid = a.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if sid.is_empty() {
        return err(StatusCode::BAD_REQUEST, "id required");
    }
    let Some(info) = attach_info(sid) else {
        return err(StatusCode::NOT_FOUND, "session not found");
    };
    // 既アタッチ chat の再利用（同一外部セッションへの重複 chat を防ぐ）
    let reused = {
        let chats = app.chats.lock().await;
        let mut hit = None;
        for (cid, s) in chats.iter() {
            let c = s.chat.core().lock().await;
            if c.session_id.as_deref() == Some(sid)
                || c.saved_session_id.as_deref() == Some(sid)
            {
                hit = Some((*cid, repo_slug(s.repo.as_ref())));
                break;
            }
        }
        hit
    };
    if let Some((cid, repo)) = reused {
        // 旧版で作成された attach chat は attach_src 未設定 → 既存
        // トランスクリプトとの共通 prefix 長で注入件数を採用する。
        // chats ガードは sync_attach（内部で再ロック）前に必ず解放する
        {
            let chats = app.chats.lock().await;
            if let Some(s) = chats.get(&cid) {
                if s.attach_src.is_none() {
                    let fetched = attach_msgs(&info);
                    let c = s.chat.core().lock().await;
                    let mut k = 0usize;
                    while k < fetched.len()
                        && k < c.messages.len()
                        && c.messages[k].role == fetched[k].role
                        && c.messages[k].text == fetched[k].text
                    {
                        k += 1;
                    }
                    drop(c);
                    drop(chats);
                    let mut chats = app.chats.lock().await;
                    if let Some(s) = chats.get_mut(&cid) {
                        s.attach_src = Some(agent_keys[0].to_string());
                        s.attach_n = k;
                    }
                }
            }
        }
        app.persist_chats().await;
        sync_attach(app, cid).await;
        return j(json!({
            "ok": true, "id": cid, "reused": true,
            "repo": repo,
        }));
    }
    let cwd = info["cwd"].as_str().unwrap_or("").to_string();
    let cfg = app.cfg().await;
    // セッション cwd から所属 repo を前方一致で解決（最長一致）。
    // 登録 repo 外なら None（スコープ外 chat として全 repo で表示する）
    let repo = {
        let ncwd = norm_p(&cwd);
        cfg_arr(&cfg, "repos")
            .iter()
            .filter(|r| {
                let rp = norm_p(r.get("path").and_then(|v| v.as_str()).unwrap_or(""));
                !rp.is_empty() && (ncwd == rp || ncwd.starts_with(&format!("{}/", rp)))
            })
            .max_by_key(|r| norm_p(r.get("path").and_then(|v| v.as_str()).unwrap_or("")).len())
            .map(|r| (**r).clone())
    };
    // chatAgents の実キーを解決。無ければ ACP 直結の既定 spec を
    // 自動登録する（hub-copilot 等 HTTP 型は session/load 不可のため
    // attach には使わない）
    let mut registered = false;
    let agent = {
        let mut cfgw = app.cfg.write().await;
        let (_, agents) = App::chat_agents(&cfgw);
        match agent_keys.iter().find(|k| agents.contains_key(**k)) {
            Some(k) => Some(k.to_string()),
            None => fallback_agent.map(|(id, spec)| {
                let mut a2 = agents.clone();
                a2.insert(id.to_string(), spec.clone());
                cfgw["chatAgents"] = Value::Object(a2);
                persist_config(&app.root, &cfgw);
                registered = true;
                id.to_string()
            }),
        }
    };
    if registered {
        app.sse("chatAgents", &json!({}));
    }
    let label = format!(
        "[{}] {}",
        agent_keys[0],
        info["title"].as_str().unwrap_or(sid)
    );
    let chat_id = app
        .new_chat(NewChatOpts {
            label: Some(label),
            repo: repo.clone(),
            agent,
            session_id: Some(sid.to_string()),
            ..Default::default()
        })
        .await;
    // cwd・トランスクリプトを chat へ流し込む（履歴として表示し、
    // 実セッションの継続は送信時の session/load が担う。
    // attach_src/attach_n を記録し GET /api/chat 時の差分再同期に使う）
    {
        let msgs = attach_msgs(&info);
        let n = msgs.len();
        let mut chats = app.chats.lock().await;
        if let Some(s) = chats.get_mut(&chat_id) {
            if !cwd.is_empty() {
                *s.chat.cwd().write().await = cwd.clone();
            }
            s.attach_src = Some(agent_keys[0].to_string());
            s.attach_n = n;
            let mut c = s.chat.core().lock().await;
            c.messages.extend(msgs);
        }
    }
    app.persist_chats().await;
    app.sse("chat", &json!({"id": chat_id}));
    j(json!({"ok": true, "id": chat_id, "repo": repo_slug(repo.as_ref())}))
}

async fn api_devin_attach(State(app): State<Shared>, b: Bytes) -> Response {
    agent_session_attach(
        &app, b, &["devin"], crate::devinenv::attach_info,
        Some(("devin", json!({
            "label": "Devin", "exe": "devin",
            "args": ["acp"], "auth": "devin",
        }))),
    ).await
}
async fn api_copilot_attach(State(app): State<Shared>, b: Bytes) -> Response {
    agent_session_attach(
        &app, b, &["copilot"], crate::copilotenv::attach_info,
        Some(("copilot", json!({
            "label": "Copilot", "exe": "copilot",
            "args": ["--acp"], "auth": "none",
        }))),
    ).await
}

async fn api_chat_options(State(app): State<Shared>) -> Response {
    let persisted = load_j(&app.opts_path(), json!({}));
    let chats = app.chats.lock().await;
    let mut options: Vec<Value> = Vec::new();
    for s in chats.values() {
        let c = s.chat.core().lock().await;
        if !c.options.is_empty() {
            let c_opts = c.options.clone();
            drop(c);
            let po = persisted.as_object().cloned().unwrap_or_default();
            // snapshot の persisted 差し替えと同じ処理
            options = c_opts
                .iter()
                .map(|o| {
                    let oid = o.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    match po.get(oid) {
                        Some(v) if *v != o["currentValue"] => {
                            let mut o = o.clone();
                            o["currentValue"] = v.clone();
                            o
                        }
                        _ => o.clone(),
                    }
                })
                .collect();
            break;
        }
    }
    j(json!({"ok": true, "options": options, "persisted": persisted}))
}

async fn api_chat_get(State(app): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
    let id: i64 = q.get("id").and_then(|s| s.parse().ok()).unwrap_or(0);
    sync_attach(&app, id).await;
    let persisted = load_j(&app.opts_path(), json!({}));
    let chats = app.chats.lock().await;
    let Some(s) = chats.get(&id) else {
        return j(json!({"ok": true, "messages": [], "options": persisted}));
    };
    let mut snap = s
        .chat
        .snapshot(persisted.as_object().unwrap_or(&Map::new()))
        .await;
    let children: Vec<i64> = chats
        .iter()
        .filter(|(_, x)| x.parent == Some(id))
        .map(|(cid, _)| *cid)
        .collect();
    let points = s.snaps.lock().unwrap().points();
    snap["ok"] = json!(true);
    snap["id"] = json!(id);
    snap["label"] = json!(s.label);
    snap["snapPoints"] = json!(points);
    snap["repo"] = json!(repo_slug(s.repo.as_ref()));
    snap["execMode"] = json!(s.exec_mode);
    snap["parent"] = s.parent.map(|p| json!(p)).unwrap_or(Value::Null);
    snap["depth"] = json!(s.depth);
    snap["verify"] = s.verify.clone().map(Value::String).unwrap_or(Value::Null);
    snap["verifyBack"] = json!(s.verify_back);
    snap["doneKind"] = s
        .done_kind
        .clone()
        .map(Value::String)
        .unwrap_or(Value::Null);
    snap["backPending"] = json!(s.back_prompt.is_some());
    snap["needsHuman"] = json!(s.needs_human);
    snap["issueKey"] = s
        .issue_key
        .clone()
        .map(Value::String)
        .unwrap_or(Value::Null);
    snap["children"] = json!(children);
    j(snap)
}

async fn api_chat_send(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let mut id = qi(&a, "id");
    let cfg = app.cfg().await;
    if id.is_none() || !app.chats.lock().await.contains_key(&id.unwrap_or(-1)) {
        let repos = cfg_arr(&cfg, "repos");
        let repo = repos
            .iter()
            .find(|r| repo_slug(Some(r)).as_deref() == a.get("repo").and_then(|v| v.as_str()))
            .or_else(|| repos.first())
            .map(|r| (**r).clone());
        id = Some(
            app.new_chat(NewChatOpts {
                label: a
                    .get("text")
                    .and_then(|v| v.as_str())
                    .map(|t| t.chars().take(24).collect::<String>())
                    .filter(|s| !s.is_empty()),
                repo,
                mode: a.get("mode").and_then(|v| v.as_str()).map(String::from),
                ..Default::default()
            })
            .await,
        );
    }
    let id = id.unwrap();
    // mode 指定があれば既存 chat の execMode も更新
    let mode = a.get("mode").and_then(|v| v.as_str()).unwrap_or("");
    if mode == "direct" || mode == "parallel" {
        let mut chats = app.chats.lock().await;
        if let Some(s) = chats.get_mut(&id) {
            if s.exec_mode != mode {
                s.exec_mode = mode.to_string();
                drop(chats);
                app.persist_chats().await;
                app.sse("chats", &json!({}));
            }
        }
    }
    let mut text = a.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mut display: Option<String> = None;
    // parallel ではセッションごとに1回だけ分割指示プリアンブルを前置
    let cur_sid = {
        let chats = app.chats.lock().await;
        let s = chats.get(&id).unwrap();
        let c = s.chat.core().lock().await;
        c.session_id.clone().or(c.saved_session_id.clone())
    };
    let need_prime = {
        let chats = app.chats.lock().await;
        let s = chats.get(&id).unwrap();
        s.exec_mode == "parallel"
            && match &s.primed_for {
                Primed::Always => false,
                Primed::NotYet => true,
                Primed::Session(p) => Some(p.as_str()) != cur_sid.as_deref(),
            }
    };
    if need_prime {
        display = Some(text.clone());
        text = format!("{}\n\n{}", PARALLEL_PREAMBLE, text);
    }
    let mark_primed = |app: &Arc<App>, id: i64| {
        let app = app.clone();
        async move {
            let mut chats = app.chats.lock().await;
            if let Some(s) = chats.get_mut(&id) {
                let c = s.chat.core().lock().await;
                s.primed_for = c
                    .session_id
                    .clone()
                    .map(Primed::Session)
                    .unwrap_or(if matches!(s.chat, Chat::Http(_)) { Primed::Always } else { s.primed_for.clone() });
            }
            drop(chats);
            app.persist_chats().await;
        }
    };
    // 待機中 → 保留キュー
    {
        let chats = app.chats.lock().await;
        let s = chats.get(&id).unwrap();
        if s.chat.core().lock().await.waiting.is_some() {
            let chat = s.chat.clone();
            drop(chats);
            let _ = chat.send(text, display.clone()).await;
            if display.is_some() {
                mark_primed(&app, id).await;
            }
            return j(json!({"ok": true, "id": id, "waiting": true}));
        }
    }
    // リポジトリ不整合チェック
    {
        let chats = app.chats.lock().await;
        let s = chats.get(&id).unwrap();
        let busy = s.chat.core().lock().await.busy;
        if !busy {
            let user_text = a.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let sug = suggest_repo(user_text, s.repo.as_ref(), &cfg_arr(&cfg, "repos").iter().map(|r| (*r).clone()).collect::<Vec<_>>());
            if let Some(sug) = sug {
                let slug = sug.slug.clone().unwrap_or_default();
                if !s.repo_declined.contains(&slug) {
                    let chat = s.chat.clone();
                    drop(chats);
                    {
                        let mut c = chat.core().lock().await;
                        c.hold(&text, vec![], vec![], "repo", Some(json!(sug)), display.as_deref());
                    }
                    if display.is_some() {
                        mark_primed(&app, id).await;
                    }
                    return j(json!({"ok": true, "id": id, "waiting": true, "repoAsk": sug}));
                }
            }
        }
    }
    // ファイル競合 → 実装待機
    {
        let chats = app.chats.lock().await;
        let s = chats.get(&id).unwrap();
        let cwd = s.chat.cwd().read().await.clone();
        let paths = extract_paths(&text, &cwd);
        drop(chats);
        let hits = app.find_chat_conflicts(id, &paths).await;
        if !hits.is_empty() {
            let files: Vec<String> = hits.iter().flat_map(|h| h.files.clone()).collect();
            let blocked: Vec<i64> = hits.iter().map(|h| h.id).collect();
            let chats = app.chats.lock().await;
            let chat = chats.get(&id).unwrap().chat.clone();
            drop(chats);
            match &chat {
                Chat::Acp(c) => c.notify_cancel().await,
                Chat::Http(h) => h.enter_wait_busy().await,
            }
            let mut c = chat.core().lock().await;
            c.hold(&text, files, blocked, "conflict", None, display.as_deref());
            drop(c);
            app.sse("chat", &json!({"id": id}));
            return j(json!({"ok": true, "id": id, "waiting": true}));
        }
    }
    // 新規ターン: verify/verifyBack/doneKind をリセット
    // （doneKind=done は終端のため消さない。実装完了 chat は live列に留まる）
    {
        let mut chats = app.chats.lock().await;
        if let Some(s) = chats.get_mut(&id) {
            if s.done_kind.as_deref() != Some("done")
                && (s.verify.is_some()
                    || s.verify_back > 0
                    || s.done_kind.is_some()
                    || s.back_prompt.is_some()
                    || s.needs_human)
            {
                s.verify = None;
                s.verify_back = 0;
                s.done_kind = None;
                s.back_prompt = None;
                s.needs_human = false;
                drop(chats);
                app.persist_chats().await;
                app.sse("chats", &json!({}));
            }
        }
    }
    // claims + send
    let chat = {
        let chats = app.chats.lock().await;
        chats.get(&id).unwrap().chat.clone()
    };
    {
        let mut c = chat.core().lock().await;
        let cwd = chat.cwd().read().await.clone();
        setup_claims_core(&mut c, &text, &cwd);
    }
    match chat.send(text, display.clone()).await {
        Ok(()) => {
            clear_attach_transcript(&app, id).await;
            if display.is_some() {
                mark_primed(&app, id).await;
            }
            j(json!({"ok": true, "id": id}))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "id": id, "error": e})),
        )
            .into_response(),
    }
}

async fn api_chat_mode(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let id = qi(&a, "id").unwrap_or(0);
    let mode = a.get("mode").and_then(|v| v.as_str()).unwrap_or("");
    if mode != "direct" && mode != "parallel" {
        return err(StatusCode::BAD_REQUEST, "mode must be direct|parallel");
    }
    let mut chats = app.chats.lock().await;
    let Some(s) = chats.get_mut(&id) else {
        return err(StatusCode::NOT_FOUND, "chat not found");
    };
    if s.exec_mode != mode {
        s.exec_mode = mode.to_string();
        let mut c = s.chat.core().lock().await;
        c.push(
            "sys",
            &format!("[処理モード] {} に切り替えました", if mode == "parallel" { "並列" } else { "直接" }),
        );
        drop(c);
        drop(chats);
        app.persist_chats().await;
        app.sse("chats", &json!({}));
        app.sse("chat", &json!({"id": id}));
    }
    j(json!({"ok": true, "id": id, "mode": mode}))
}

async fn api_chat_repo_answer(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let id = qi(&a, "id").unwrap_or(0);
    let action = a.get("action").and_then(|v| v.as_str()).unwrap_or("");
    let sug = {
        let chats = app.chats.lock().await;
        let Some(s) = chats.get(&id) else {
            return err(StatusCode::NOT_FOUND, "chat not found");
        };
        let c = s.chat.core().lock().await;
        match c.waiting.as_ref() {
            Some(w) if w.kind == "repo" => w.suggest.clone().unwrap_or(json!({})),
            _ => return err(StatusCode::BAD_REQUEST, "リポジトリ確認待ちではありません"),
        }
    };
    if action == "keep" {
        let slug = sug.get("slug").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let text = {
            let mut chats = app.chats.lock().await;
            let s = chats.get_mut(&id).unwrap();
            s.repo_declined.insert(slug);
            let mut c = s.chat.core().lock().await;
            c.leave_wait()
        };
        app.persist_chats().await;
        if let Some(t) = text {
            let _ = app.send_to_chat(id, t, None).await;
        }
        return j(json!({"ok": true, "id": id, "action": "keep"}));
    }
    if action == "switch" {
        let cfg = app.cfg().await;
        let repos = cfg_arr(&cfg, "repos");
        let target = repos
            .iter()
            .find(|r| repo_slug(Some(r)).as_deref() == sug.get("slug").and_then(|v| v.as_str()))
            .or_else(|| {
                repos.iter().find(|r| {
                    norm_p(r.get("path").and_then(|v| v.as_str()).unwrap_or(""))
                        == norm_p(sug.get("path").and_then(|v| v.as_str()).unwrap_or(""))
                })
            })
            .map(|r| (**r).clone());
        let Some(target) = target else {
            return err(
                StatusCode::BAD_REQUEST,
                &format!("repo not found: {}", sug.get("slug").and_then(|v| v.as_str()).unwrap_or("")),
            );
        };
        let (chat, from_slug) = {
            let mut chats = app.chats.lock().await;
            let s = chats.get_mut(&id).unwrap();
            let from = repo_slug(s.repo.as_ref());
            s.repo = Some(target.clone());
            (s.chat.clone(), from)
        };
        let from = match from_slug {
            Some(f) => f,
            None => chat.cwd().read().await.clone(),
        };
        let text = chat.core().lock().await.leave_wait();
        *chat.cwd().write().await = target.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
        chat.close_remote().await;
        chat.kill_proc().await;
        {
            let mut c = chat.core().lock().await;
            c.session_id = None;
            c.saved_session_id = None;
            c.push(
                "sys",
                &format!(
                    "[リポジトリ切替] {} → {}。新規セッションで継続します",
                    if from.is_empty() { "(未設定)" } else { &from },
                    repo_slug(Some(&target)).unwrap_or_else(|| target["path"].as_str().unwrap_or("").to_string())
                ),
            );
        }
        let _ = app.evt_tx.send(CtrlEvt::SessionId { id });
        app.persist_chats().await;
        app.sse("chats", &json!({}));
        if let Some(t) = text {
            let _ = app.send_to_chat(id, t, None).await;
        }
        return j(json!({"ok": true, "id": id, "action": "switch",
            "repo": repo_slug(Some(&target))}));
    }
    err(StatusCode::BAD_REQUEST, "action must be keep|switch")
}

async fn api_chat_revise(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let id = qi(&a, "id").unwrap_or(0);
    let text = a.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if text.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "text required");
    }
    let chat = {
        let chats = app.chats.lock().await;
        match chats.get(&id) {
            Some(s) => s.chat.clone(),
            None => return err(StatusCode::NOT_FOUND, "chat not found"),
        }
    };
    match chat.revise(&text).await {
        Ok(()) => j(json!({"ok": true, "id": id})),
        Err(e) => err(StatusCode::BAD_REQUEST, &e),
    }
}

async fn api_chat_restore(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let id = qi(&a, "id").unwrap_or(0);
    let chats = app.chats.lock().await;
    let Some(s) = chats.get(&id) else {
        return err(StatusCode::NOT_FOUND, "chat not found");
    };
    if s.chat.core().lock().await.busy {
        return err(StatusCode::CONFLICT, "実行中です。先に中断してください");
    }
    let turn = a.get("turn").and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())));
    let r = s.snaps.lock().unwrap().restore(turn);
    if !r.ok {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::to_value(&r).unwrap()),
        )
            .into_response();
    }
    let names = |arr: &[String]| {
        arr.iter()
            .map(|f| f.split('/').rev().take(2).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("/"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut parts = Vec::new();
    if !r.restored.is_empty() {
        parts.push(format!("復元 {} 件: {}", r.restored.len(), names(&r.restored)));
    }
    if !r.deleted.is_empty() {
        parts.push(format!("削除 {} 件: {}", r.deleted.len(), names(&r.deleted)));
    }
    if !r.failed.is_empty() {
        parts.push(format!("失敗 {} 件: {}", r.failed.len(), names(&r.failed)));
    }
    s.chat
        .push_sys(&format!(
            "[変更復元] turn {} 以降の変更を元へ戻しました（{}）",
            r.turn,
            if parts.is_empty() { "対象なし".to_string() } else { parts.join(" / ") }
        ))
        .await;
    drop(chats);
    app.sse("chat", &json!({"id": id}));
    j(serde_json::to_value(&r).unwrap())
}

async fn api_chat_cancel(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let id = qi(&a, "id").unwrap_or(0);
    let mut chats = app.chats.lock().await;
    if let Some(s) = chats.get_mut(&id) {
        let chat = s.chat.clone();
        drop(chats);
        chat.cancel().await;
        let mut chats = app.chats.lock().await;
        if let Some(s) = chats.get_mut(&id) {
            if s.verify.as_deref() == Some("running") && !s.chat.core().lock().await.busy {
                s.verify = Some("pending".to_string());
                drop(chats);
                app.persist_chats().await;
            }
        }
    }
    j(json!({"ok": true}))
}

async fn api_chat_clear(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let id = qi(&a, "id").unwrap_or(0);
    let chats = app.chats.lock().await;
    if let Some(s) = chats.get(&id) {
        s.chat.clear().await;
    }
    j(json!({"ok": true}))
}

async fn api_chat_reset(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let id = qi(&a, "id").unwrap_or(0);
    let mut chats = app.chats.lock().await;
    if let Some(s) = chats.get_mut(&id) {
        s.primed_for = Primed::NotYet;
        let chat = s.chat.clone();
        drop(chats);
        chat.reset().await;
        app.persist_chats().await;
        app.sse("chat", &json!({"id": id}));
    }
    j(json!({"ok": true}))
}

// POST /api/chat/resume — 再開可 chat の session/load 再接続に加え、
// 未着手（saved sid 無し・起動失敗滞留）chat の session/new 起動も担う。
// 未送信の保留 prompt（held_prompts）があれば接続後に再送する
async fn api_chat_resume(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let id = qi(&a, "id").unwrap_or(0);
    let (chat, held) = {
        let chats = app.chats.lock().await;
        let Some(s) = chats.get(&id) else {
            return err(StatusCode::NOT_FOUND, "chat not found");
        };
        let mut c = s.chat.core().lock().await;
        if s.chat.has_proc().await || c.busy || c.waiting.is_some() {
            return j(json!({"ok": true, "id": id}));
        }
        (s.chat.clone(), std::mem::take(&mut c.held_prompts))
    };
    // 同時起動枠（maxConcurrentSessions）が取れなければ再開しない
    // （開始のみの再開は idle proc を追い出さない）
    if let Err(_busy) = app.acquire_proc_slot(id, false).await {
        chat.core().lock().await.held_prompts.extend(held);
        return err(
            StatusCode::CONFLICT,
            "同時起動セッション数の上限に達しています（実行中のセッション終了後に再試行）",
        );
    }
    match chat.ensure_session().await {
        Ok(()) => {
            clear_attach_transcript(&app, id).await;
            if !held.is_empty() {
                if let Err(e) = app.send_to_chat(id, held.join("\n"), None).await {
                    chat.push_sys(&format!("[保留再送失敗] {}", e)).await;
                }
            }
            j(json!({"ok": true, "id": id}))
        }
        Err(e) => {
            // 失敗時は保留を戻す（再送手段を失わない）
            chat.core().lock().await.held_prompts.extend(held);
            chat.push_sys(&format!("[手動再開失敗] {}", e)).await;
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"ok": false, "error": e})))
                .into_response()
        }
    }
}

// POST /api/chat/resume-all — 処理状況ボード上部の「処理再開」ボタン。
// 実処理は App::resume_all（起動時の自動再処理と共通）へ委譲。
async fn api_chat_resume_all(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let repo_f = a.get("repo").and_then(|v| v.as_str()).map(String::from);
    j(app.resume_all(repo_f, true).await)
}

// POST /api/chat/start-suspended — ボード上部の「スリープ起動」ボタン。
// スリープ中（resumable）セッションを prompt 無しで一括起動
// （panel-layout.mmd の SUSP ノード）
async fn api_chat_start_suspended(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let repo_f = a.get("repo").and_then(|v| v.as_str()).map(String::from);
    j(app.start_suspended(repo_f).await)
}

// POST /api/chat/parallel — ボード exec 列の「並列」ボタン。{on:bool}（既定
// true）で並列モードを切替。on→conflictWait=false 永続化＋全ての実装待機を
// 即時解放、off→直列ガードへ復帰（実処理は App::set_parallel_exec）
async fn api_chat_parallel(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let on = a.get("on").and_then(|v| v.as_bool()).unwrap_or(true);
    j(app.set_parallel_exec(on).await)
}

async fn api_chat_verify(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let id = qi(&a, "id").unwrap_or(0);
    {
        let mut chats = app.chats.lock().await;
        let Some(s) = chats.get_mut(&id) else {
            return err(StatusCode::NOT_FOUND, "chat not found");
        };
        if s.chat.core().lock().await.busy {
            return err(StatusCode::CONFLICT, "busy");
        }
        s.verify_count = 0;
        s.verify_back = 0;
        s.done_kind = None;
        s.back_prompt = None;
        s.needs_human = false;
    }
    app.start_verify(id).await;
    j(json!({"ok": true, "id": id}))
}

// POST /api/chat/rework — 要対応列の差し戻し。保留の差し戻し prompt
// （無ければ終了区分に応じた再作業 prompt）を送り、doneKind/verify 系・
// 要人対応をリセットして実行状況列へ戻す
async fn api_chat_rework(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let id = qi(&a, "id").unwrap_or(0);
    let (kind, stored) = {
        let mut chats = app.chats.lock().await;
        let Some(s) = chats.get_mut(&id) else {
            return err(StatusCode::NOT_FOUND, "chat not found");
        };
        let c = s.chat.core().lock().await;
        if c.busy {
            return err(StatusCode::CONFLICT, "busy");
        }
        drop(c);
        let k = s.done_kind.take();
        s.verify = None;
        s.verify_count = 0;
        s.verify_back = 0;
        s.needs_human = false;
        (k, s.back_prompt.take())
    };
    app.persist_chats().await;
    app.sse("chats", &json!({}));
    let (prompt, disp) = match stored {
        Some(p) => (p, "[差し戻し] 検証NG → 再実装".to_string()),
        None => {
            let reason = match kind.as_deref() {
                Some("ng") => {
                    "前回の実装は実機検証でNGが解消しませんでした（差し戻し上限到達）。"
                }
                Some("unverified") => {
                    "前回の実装は検証結果を取得できず、実装完了を確認できていません。"
                }
                _ => "このセッションでは実装が完了していません。",
            };
            (
                format!(
                    "[差し戻し] {}作業内容を確認し、残りの実装・修正を行って完了させてください。",
                    reason
                ),
                "[差し戻し] 実装完了に至っていないため再作業".to_string(),
            )
        }
    };
    if let Err(e) = app.send_to_chat(id, prompt, Some(disp)).await {
        let chats = app.chats.lock().await;
        if let Some(s) = chats.get(&id) {
            s.chat.push_sys(&format!("[差し戻し失敗] {}", e)).await;
        }
    }
    j(json!({"ok": true, "id": id}))
}

async fn api_chat_close(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let id = qi(&a, "id").unwrap_or(0);
    if app.close_chat_entry(id).await {
        app.persist_chats().await;
    }
    app.sse("chats", &json!({}));
    j(json!({"ok": true}))
}

async fn api_chat_close_completed(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let repo_f = a.get("repo").and_then(|v| v.as_str()).map(String::from);
    let ids: Vec<i64> = {
        let chats = app.chats.lock().await;
        let mut out = Vec::new();
        for (id, s) in chats.iter() {
            if let Some(rf) = &repo_f {
                if repo_slug(s.repo.as_ref()).as_deref() != Some(rf.as_str()) {
                    continue;
                }
            }
            let c = s.chat.core().lock().await;
            let proc = s.chat.has_proc().await;
            // 一括削除の対象は live 列（doneKind=done・無印）の停止中 chat。
            // 要対応（ng/unverified・差し戻し待ち・要人対応）は残す
            if matches!(s.done_kind.as_deref(), Some("done") | None)
                && s.verify.is_none()
                && s.back_prompt.is_none()
                && !s.needs_human
                && !c.busy
                && c.waiting.is_none()
                && (proc || (s.chat.transport() == "http" && c.ready))
            {
                out.push(*id);
            }
        }
        out
    };
    for id in &ids {
        app.close_chat_entry(*id).await;
    }
    if !ids.is_empty() {
        app.persist_chats().await;
    }
    app.sse("chats", &json!({}));
    j(json!({"ok": true, "closed": ids.len(), "ids": ids}))
}

async fn api_chat_rename(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let id = qi(&a, "id").unwrap_or(0);
    let mut chats = app.chats.lock().await;
    if let Some(s) = chats.get_mut(&id) {
        if let Some(l) = a.get("label").and_then(|v| v.as_str()) {
            s.label = l.chars().take(60).collect();
            s.updated_at = now_iso();
            drop(chats);
            app.persist_chats().await;
        }
    }
    app.sse("chats", &json!({}));
    j(json!({"ok": true}))
}

async fn api_chat_option(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let mut o = load_j(&app.opts_path(), json!({}));
    if !o.is_object() {
        o = json!({});
    }
    if let Some(id) = a.get("id").and_then(|v| v.as_str()) {
        o.as_object_mut()
            .unwrap()
            .insert(id.to_string(), a.get("value").cloned().unwrap_or(Value::Null));
    }
    save_j(&app.opts_path(), &o);
    let sid = qi(&a, "sid");
    if let Some(sid) = sid {
        let chat = {
            let chats = app.chats.lock().await;
            chats.get(&sid).map(|s| s.chat.clone())
        };
        if let Some(chat) = chat {
            let oid = a.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let val = a.get("value").and_then(|v| v.as_str()).unwrap_or("");
            if let Err(e) = chat.set_option(oid, val).await {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"ok": false, "error": e}))).into_response();
            }
        }
    }
    app.sse("chat", &json!({"id": sid}));
    j(json!({"ok": true}))
}

async fn api_chat_attach(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let _ = std::fs::create_dir_all(&app.uploads);
    let mut files = Vec::new();
    let re_name = regex::Regex::new(r"[^\w.-]").unwrap();
    let re_data = regex::Regex::new(r"(?s)^data:.*?;base64,(.*)$").unwrap();
    use base64::Engine;
    if let Some(arr) = a.get("attachments").and_then(|v| v.as_array()) {
        for f in arr.iter().take(8) {
            let name = re_name
                .replace_all(f.get("name").and_then(|v| v.as_str()).unwrap_or("file"), "_")
                .chars()
                .take(80)
                .collect::<String>();
            let data = f.get("data").and_then(|v| v.as_str()).unwrap_or("");
            let Some(m) = re_data.captures(data) else { continue };
            let Ok(bin) = base64::engine::general_purpose::STANDARD.decode(m[1].as_bytes()) else {
                continue;
            };
            let fname = format!(
                "{}-{}-{}",
                crate::util::now_ms(),
                rand_hex(3),
                name
            );
            let dest = app.uploads.join(fname);
            if std::fs::write(&dest, bin).is_ok() {
                files.push(json!({"name": name, "path": dest.to_string_lossy().replace('\\', "/")}));
            }
        }
    }
    j(json!({"ok": true, "files": files}))
}

async fn api_candidates_get(State(app): State<Shared>) -> Response {
    let cands = load_j(&app.cand_path(), json!([]));
    let issues = crate::store::load_issues(&app.root);
    // 起票済み課題の status を各行に付与（candidate 行に課題状態を併記する表示用。
    // 保存データではなく応答にだけ載せる）
    let out: Vec<Value> = cands
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|mut c| {
            if let Some(key) = c.get("key").and_then(|v| v.as_str()) {
                let st = issues
                    .iter()
                    .find(|i| i.get("key").and_then(|v| v.as_str()) == Some(key))
                    .and_then(|i| i.get("status").and_then(|v| v.as_str()))
                    .unwrap_or("");
                c.as_object_mut()
                    .unwrap()
                    .insert("issueStatus".into(), json!(st));
            }
            c
        })
        .collect();
    j(json!({"ok": true, "candidates": out}))
}

// POST /api/candidates — manual detector 入口。{repo,title,body} を intake する
async fn api_candidates_post(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let title = a.get("title").and_then(|v| v.as_str()).unwrap_or("").trim();
    if title.is_empty() {
        return err(StatusCode::BAD_REQUEST, "title required");
    }
    // repo は slug 文字列 or repo オブジェクトを許容し slug に正規化
    let slug = match a.get("repo") {
        Some(Value::String(s)) => s.clone(),
        other => repo_slug(other).unwrap_or_default(),
    };
    let cand = json!({
        "source": "manual",
        "repo": slug,
        "slug": slug,
        "title": title,
        "body": a.get("body").and_then(|v| v.as_str()).unwrap_or(""),
        "symptomHash": format!("manual-{}", rand_hex(4)),
        "detectedAt": now_iso(),
    });
    let (issue, created, deferred) = crate::detectors::ingest(&app.root, &cand);
    if created {
        app.sse("candidates", &json!({}));
        app.sse("issues", &json!({}));
    } else if deferred {
        // レート制限の deferred ピル表示を即時反映
        app.sse("candidates", &json!({}));
    }
    j(json!({"ok": true, "created": created, "issue": issue, "candidate": cand}))
}

// ---------- issues（課題ライフサイクル。状態遷移は pipeline.rs のみ） ----------

fn issue_status(root: &Path, key: &str) -> Option<String> {
    crate::store::load_issues(root)
        .iter()
        .find(|i| i.get("key").and_then(|v| v.as_str()) == Some(key))
        .and_then(|i| {
            i.get("status")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
}

// GET /api/issues?repo=<slug|path> — 課題一覧 + status 別件数。
// ?repo= は登録 repo の slug/path どちらでもよく、解決できれば slug+path 両方で絞込
async fn api_issues_get(
    State(app): State<Shared>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let issues = crate::store::load_issues(&app.root);
    let sel = q.get("repo").map(|s| s.trim().to_string()).unwrap_or_default();
    let issues = if sel.is_empty() {
        issues
    } else {
        let cfg = app.cfg().await;
        let (slug, path) = App::git_repo(&cfg, &sel)
            .map(|r| {
                (
                    repo_slug(Some(r)).unwrap_or_default(),
                    norm_p(r.get("path").and_then(|v| v.as_str()).unwrap_or("")),
                )
            })
            .unwrap_or_default();
        let sel_n = norm_p(&sel);
        issues
            .into_iter()
            .filter(|i| {
                let ir = i.get("repo").and_then(|v| v.as_str()).unwrap_or("");
                let is = i.get("slug").and_then(|v| v.as_str()).unwrap_or("");
                ir == sel
                    || is == sel
                    || (!slug.is_empty() && (ir == slug || is == slug))
                    || (!path.is_empty() && (norm_p(ir) == path || norm_p(is) == path))
                    || norm_p(ir) == sel_n
            })
            .collect()
    };
    let mut counts = Map::new();
    for i in &issues {
        let st = i.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let e = counts.entry(st.to_string()).or_insert(json!(0));
        *e = json!(e.as_u64().unwrap_or(0) + 1);
    }
    j(json!({"ok": true, "issues": issues, "counts": counts}))
}

// GET /api/issues/{key}/audit — 課題単位の監査タイムライン
// （audit-timeline.mmd・README §5.10。evidence/<slug>/audit.jsonl を返す）
async fn api_issue_audit(State(app): State<Shared>, AxPath(key): AxPath<String>) -> Response {
    if issue_status(&app.root, &key).is_none() {
        return err(StatusCode::NOT_FOUND, "issue not found");
    }
    j(json!({"ok": true, "events": crate::audit::read(&app.root, &key)}))
}

// GET /api/issues/{key}/trace?run=N — run の OTel GenAI 風 span 一覧
// （trace-waterfall.mmd・README §5.9。run 省略時は最新 run）
async fn api_issue_trace(
    State(app): State<Shared>,
    AxPath(key): AxPath<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let Some(issue) = crate::store::load_issues(&app.root)
        .into_iter()
        .find(|i| i.get("key").and_then(|v| v.as_str()) == Some(key.as_str()))
    else {
        return err(StatusCode::NOT_FOUND, "issue not found");
    };
    let idx = q
        .get("run")
        .and_then(|s| s.parse::<usize>().ok())
        .or_else(|| crate::trajectory::latest_run_idx(&issue))
        .unwrap_or(0);
    j(json!({"ok": true, "run": idx,
             "spans": crate::trace::read_spans(&app.root, &key, idx)}))
}

// GET /api/metrics — KPI 集計（metrics.mmd・README §5.12。
// issues.json から決定的に算出する読み取り専用エンドポイント）
async fn api_metrics(State(app): State<Shared>) -> Response {
    let mut m = crate::metrics::compute(&app.root);
    if let Some(o) = m.as_object_mut() {
        o.insert("ok".into(), json!(true));
    }
    j(m)
}

/// 対応/却下の適用（issues・candidates 共通の人ゲート）。
/// dispatch: queued→running（gated は承認→queued・failed は再実行→queued を
/// 兼ねた即時 dispatch）。reject: gated|queued|failed→rejected。
/// 状態遷移は pipeline・起動は runner に委譲し、ここでは状態を直接触らない
async fn issue_gate_action(app: &Shared, key: &str, action: &str) -> Result<Value, String> {
    let cur = issue_status(&app.root, key).ok_or_else(|| "issue not found".to_string())?;
    // 監査（audit-timeline.mmd）: 人の dispatch/reject 判断を記録
    crate::audit::record(
        &app.root,
        key,
        "human",
        action,
        None,
        Map::from_iter([("from".into(), json!(cur))]),
    );
    match action {
        "dispatch" => {
            match cur.as_str() {
                crate::pipeline::ST_GATED | crate::pipeline::ST_FAILED => {
                    crate::pipeline::request_transition(
                        &app.root,
                        key,
                        crate::pipeline::ST_QUEUED,
                    )?;
                }
                // 課題キー単位の並行制御（manual-5dabae8b）: running|
                // verifying への再 trigger は dispatch_issue 側が旧 run を
                // cancel → queued へ戻してから起動する
                crate::pipeline::ST_QUEUED
                | crate::pipeline::ST_RUNNING
                | crate::pipeline::ST_VERIFYING => {}
                s => {
                    return Err(format!(
                        "issue is {} (gated|queued|failed|running|verifying required)",
                        s
                    ));
                }
            }
            crate::runner::dispatch_issue(app, key).await
        }
        "reject" => match cur.as_str() {
            crate::pipeline::ST_GATED
            | crate::pipeline::ST_QUEUED
            | crate::pipeline::ST_PLAN_REVIEW
            | crate::pipeline::ST_FAILED => {
                crate::pipeline::request_transition(&app.root, key, crate::pipeline::ST_REJECTED)
            }
            s => Err(format!("issue is {} (gated|queued|plan_review|failed required)", s)),
        },
        "plan-approve" => crate::runner::plan_approve(app, key).await,
        "plan-reject" => crate::runner::plan_reject(app, key).await,
        _ => Err("unknown action".into()),
    }
}

/// candidate 1件への人ゲート操作（個別・一括 dispatch の共用）。
/// 課題が未起票なら人判断で即時起票（rate-limit は人判断を優先して
/// bypass）してから issue_gate_action へ渡す
async fn candidate_gate_action(app: &Shared, key: &str, action: &str) -> Result<Value, String> {
    let cands = crate::store::load_candidates(&app.root);
    let cand = cands
        .iter()
        .find(|c| c.get("key").and_then(|v| v.as_str()) == Some(key))
        .cloned()
        .ok_or_else(|| "candidate not found".to_string())?;
    if issue_status(&app.root, key).is_none() {
        crate::detectors::ingest_forced(&app.root, &cand);
    }
    issue_gate_action(app, key, action).await
}

// POST /api/candidates/{key}/{dispatch|reject} — 候補一覧（判断待ち inbox）の
// 人ゲート。対応=即時 dispatch（gated/failed は承認/再実行を兼ねる）・
// 却下=rejected へ（detect-pipeline.mmd INBOX・issue-lifecycle.mmd）
async fn api_candidates_action(
    State(app): State<Shared>,
    AxPath((key, action)): AxPath<(String, String)>,
) -> Response {
    if action != "dispatch" && action != "reject" {
        return err(StatusCode::NOT_FOUND, "unknown action");
    }
    match candidate_gate_action(&app, &key, &action).await {
        Ok(issue) => {
            app.sse("issues", &json!({}));
            app.sse("candidates", &json!({}));
            j(json!({"ok": true, "issue": issue}))
        }
        Err(e) if e == "candidate not found" => err(StatusCode::NOT_FOUND, &e),
        Err(e) => err(StatusCode::CONFLICT, &e),
    }
}

// POST /api/candidates/dispatch-all — 候補一覧の「一括対応」
// （detect-pipeline.mmd INBOX）。body {keys:[...]} の候補を順に
// dispatch（未起票は即時起票）。同一 repo 競合・同時実行上限で
// 起動できない課題は autoDispatch:true を立てて待機扱いにし、drain が
// repo 解放時に自動 dispatch する（issues 一括対応と同じ扱い）
async fn api_candidates_dispatch_all(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let keys: Vec<String> = a
        .get("keys")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if keys.is_empty() {
        return err(StatusCode::BAD_REQUEST, "keys required");
    }
    let mut dispatched = Vec::new();
    let mut waiting = Vec::new();
    let mut errors = Vec::new();
    for k in keys {
        match candidate_gate_action(&app, &k, "dispatch").await {
            Ok(_) => dispatched.push(k),
            Err(e)
                if e.starts_with("blocked by") || e.starts_with("maxConcurrentRuns") =>
            {
                let _ =
                    crate::pipeline::set_field(&app.root, &k, "autoDispatch", json!(true));
                waiting.push(k);
            }
            Err(e) => errors.push(json!({"key": k, "error": e})),
        }
    }
    app.sse("issues", &json!({}));
    app.sse("candidates", &json!({}));
    j(json!({"ok": true, "dispatched": dispatched, "waiting": waiting, "errors": errors}))
}

// POST /api/issues/{key}/{approve|reject|requeue|push-approve|dispatch|followup|split}
// gated→queued(承認) / gated|queued|failed→rejected(却下) / failed→queued(再実行) /
// verifying の課題へ pushApproved:true 付与（requirePushApproval repo の人ゲート） /
// queued→running の即時 dispatch（runner の dispatch lock/can_dispatch 経由。
// dispatch はこの人操作のみが行い、drain は queued を自動起動しない。
// gated/failed からの dispatch は承認・再実行を兼ねる）。
// followup は {text} を課題の followUps へ追記する（intake-snapshot.mmd。
// 起票時 freeze の intakeSnapshot には混入せず、次回 run の prompt で
// 「フォローアップ入力」として注入される）
// split は {items:[{title,body}]}（省略時は splitProposal.items）で親課題を
// 子課題群へ分割起票する（manual-0577d650。子=source:split・splitFrom=親key、
// 親=splitInto 記録→rejected）
// 遷移・フィールド更新は pipeline に委譲し、ここでは状態を直接触らない
async fn api_issues_action(
    State(app): State<Shared>,
    AxPath((key, action)): AxPath<(String, String)>,
    b: Bytes,
) -> Response {
    // followup は body {text} を受ける追記操作（状態遷移ではない）
    if action == "followup" {
        let a = body_json(b).await;
        let text = a
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if text.is_empty() {
            return err(StatusCode::BAD_REQUEST, "text required");
        }
        return match crate::pipeline::add_followup(
            &app.root,
            &key,
            "manual",
            text,
            None,
        ) {
            Ok(_) => {
                app.sse("issues", &json!({}));
                j(json!({"ok": true}))
            }
            Err(e) => err(StatusCode::NOT_FOUND, &e),
        };
    }
    // push 後フォローアップループの人操作（postpush-loop.mmd・manual-55878990）
    // pause: checking|waiting|fixing → paused、resume: paused → checking
    if action == "postpush-pause" || action == "postpush-resume" {
        return match crate::postpush::manual(&app.root, &key, &action) {
            Ok(issue) => {
                crate::audit::record(
                    &app.root,
                    &key,
                    "human",
                    &action,
                    None,
                    Map::new(),
                );
                app.sse("issues", &json!({}));
                j(json!({"ok": true, "issue": issue}))
            }
            Err(e) => err(StatusCode::CONFLICT, &e),
        };
    }
    // split は body {items:[{title,body}]} を受ける分割起票操作
    // （manual-0577d650。items 省略時は課題の splitProposal.items を使う）
    if action == "split" {
        let a = body_json(b).await;
        let items = a.get("items").and_then(|v| v.as_array());
        return match crate::pipeline::split_issue(&app.root, &key, items) {
            Ok(issue) => {
                app.sse("issues", &json!({}));
                j(json!({"ok": true, "issue": issue}))
            }
            Err(e) => err(StatusCode::CONFLICT, &e),
        };
    }
    // cancel は running|verifying の run 中断（manual-5dabae8b。
    // RunCtl.cancel → child kill → queued 差し戻し。verifyBacks 不消費）
    if action == "cancel" {
        return match crate::runner::cancel_issue(&app, &key).await {
            Ok(issue) => {
                app.sse("issues", &json!({}));
                j(json!({"ok": true, "issue": issue}))
            }
            Err(e) => err(StatusCode::CONFLICT, &e),
        };
    }
    // steer は body {text} を受ける run 中の方針修正入力
    // （manual-4929fc13。in-flight run の stdin へ best-effort 注入 +
    // followUps kind=steer へ記録して次回 prompt に入れる）
    if action == "steer" {
        let a = body_json(b).await;
        let text = a
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if text.is_empty() {
            return err(StatusCode::BAD_REQUEST, "text required");
        }
        return match crate::runner::steer(&app, &key, text).await {
            Ok(_) => {
                app.sse("issues", &json!({}));
                j(json!({"ok": true}))
            }
            Err(e) => err(StatusCode::CONFLICT, &e),
        };
    }
    // extend は body {cycleCap?: 追加分, budget?: {maxSteps?,
    // maxWallMinutes?, maxCostUsd?}} で上限を引き上げて再 dispatch
    // （manual-94172864・manual-175e7229。cycleCapExtend / budgetExtend
    // に加算される）
    if action == "extend" {
        let a = body_json(b).await;
        let cfg = app.cfg().await;
        let Some(it) = crate::store::load_issues(&app.root)
            .into_iter()
            .find(|i| i.get("key").and_then(|v| v.as_str()) == Some(key.as_str()))
        else {
            return err(StatusCode::NOT_FOUND, "issue not found");
        };
        let cur_cycle = it
            .get("cycleCapExtend")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let add_cycle = a
            .get("cycleCap")
            .and_then(|v| v.as_u64())
            .unwrap_or_else(|| crate::config::cfg_u64(&cfg, "runCycleCap", 3));
        let _ = crate::pipeline::set_field(
            &app.root,
            &key,
            "cycleCapExtend",
            json!(cur_cycle + add_cycle),
        );
        if let Some(b) = a.get("budget").and_then(|v| v.as_object()) {
            let mut ext = it
                .get("budgetExtend")
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();
            for k in ["maxSteps", "maxWallMinutes", "maxCostUsd"] {
                if let Some(add) = b.get(k).and_then(|v| v.as_f64()) {
                    let cur = ext
                        .get(k)
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0);
                    ext.insert(k.to_string(), json!(cur + add));
                }
            }
            let _ = crate::pipeline::set_field(
                &app.root,
                &key,
                "budgetExtend",
                Value::Object(ext),
            );
        }
        crate::audit::record(
            &app.root,
            &key,
            "human",
            "extend",
            None,
            Map::from_iter([("cycleCap".into(), json!(add_cycle))]),
        );
        return match crate::runner::dispatch_issue(&app, &key).await {
            Ok(issue) => {
                app.sse("issues", &json!({}));
                j(json!({"ok": true, "issue": issue}))
            }
            Err(e) => err(StatusCode::CONFLICT, &e),
        };
    }
    // risk-ack は body {decision:"allow"|"deny", reason?} で riskHold の
    // HIGH リスク行動を人が採否する（manual-cf669d09）。
    // allow → riskAllowed に記録して即 dispatch、
    // deny → riskRejected に記録して queued で待機（次回 prompt に
    // 却下理由を注入して別アプローチを促す）
    if action == "risk-ack" {
        let a = body_json(b).await;
        let decision = a
            .get("decision")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let Some(it) = crate::store::load_issues(&app.root)
            .into_iter()
            .find(|i| i.get("key").and_then(|v| v.as_str()) == Some(key.as_str()))
        else {
            return err(StatusCode::NOT_FOUND, "issue not found");
        };
        let hold = it.get("riskHold").cloned().unwrap_or(Value::Null);
        if hold.is_null() {
            return err(StatusCode::CONFLICT, "issue has no riskHold");
        }
        crate::audit::record(
            &app.root,
            &key,
            "human",
            "risk-ack",
            None,
            Map::from_iter([("decision".into(), json!(decision))]),
        );
        match decision {
            "allow" => {
                let _ = crate::pipeline::set_field(
                    &app.root,
                    &key,
                    "riskAllowed",
                    json!({"at": crate::util::now_iso(), "hold": hold}),
                );
                let _ = crate::pipeline::set_field(
                    &app.root,
                    &key,
                    "riskHold",
                    Value::Null,
                );
                return match crate::runner::dispatch_issue(&app, &key).await {
                    Ok(issue) => {
                        app.sse("issues", &json!({}));
                        j(json!({"ok": true, "issue": issue}))
                    }
                    Err(e) => err(StatusCode::CONFLICT, &e),
                };
            }
            "deny" => {
                let reason = a
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let _ = crate::pipeline::set_field(
                    &app.root,
                    &key,
                    "riskRejected",
                    json!({"at": crate::util::now_iso(), "reason": reason, "hold": hold}),
                );
                let _ = crate::pipeline::set_field(
                    &app.root,
                    &key,
                    "riskHold",
                    Value::Null,
                );
                app.sse("issues", &json!({}));
                return match crate::store::load_issues(&app.root)
                    .into_iter()
                    .find(|i| {
                        i.get("key").and_then(|v| v.as_str()) == Some(key.as_str())
                    }) {
                    Some(issue) => j(json!({"ok": true, "issue": issue})),
                    None => err(StatusCode::NOT_FOUND, "issue not found"),
                };
            }
            _ => return err(StatusCode::BAD_REQUEST, "decision must be allow|deny"),
        }
    }
    let (from, to) = match action.as_str() {
        "approve" => (crate::pipeline::ST_GATED, crate::pipeline::ST_QUEUED),
        "reject" => ("", crate::pipeline::ST_REJECTED),
        "requeue" => (crate::pipeline::ST_FAILED, crate::pipeline::ST_QUEUED),
        "push-approve" => (crate::pipeline::ST_VERIFYING, ""),
        "plan-approve" | "plan-reject" => ("", ""),
        "dispatch" => ("", ""),
        _ => return err(StatusCode::NOT_FOUND, "unknown action"),
    };
    let Some(cur) = issue_status(&app.root, &key) else {
        return err(StatusCode::NOT_FOUND, "issue not found");
    };
    // reject は gated（承認前）・queued（対応待ち）・failed（再試行しない判断）
    // のどれからでも可。dispatch は queued に加え gated（承認+即時 dispatch）・
    // failed（再実行+即時 dispatch）からも可
    let ok_from = match action.as_str() {
        "reject" => matches!(
            cur.as_str(),
            crate::pipeline::ST_GATED
                | crate::pipeline::ST_QUEUED
                | crate::pipeline::ST_PLAN_REVIEW
                | crate::pipeline::ST_FAILED
        ),
        // dispatch は課題キー単位並行制御（manual-5dabae8b）で
        // running|verifying からの再 trigger も可（旧 run を cancel）
        "dispatch" => matches!(
            cur.as_str(),
            crate::pipeline::ST_GATED
                | crate::pipeline::ST_QUEUED
                | crate::pipeline::ST_PLAN_REVIEW
                | crate::pipeline::ST_FAILED
                | crate::pipeline::ST_RUNNING
                | crate::pipeline::ST_VERIFYING
        ),
        "plan-approve" | "plan-reject" => cur == crate::pipeline::ST_PLAN_REVIEW,
        _ => cur == from,
    };
    if !ok_from {
        let want = match action.as_str() {
            "reject" | "dispatch" => "gated|queued|plan_review|failed",
            "plan-approve" | "plan-reject" => "plan_review",
            _ => from,
        };
        return err(
            StatusCode::CONFLICT,
            &format!("issue is {} ({} required)", cur, want),
        );
    }
    let res = if action == "push-approve" {
        crate::pipeline::set_field(&app.root, &key, "pushApproved", json!(true))
    } else if action == "dispatch" || action == "reject" {
        issue_gate_action(&app, &key, &action).await
    } else if action == "plan-approve" {
        crate::runner::plan_approve(&app, &key).await
    } else if action == "plan-reject" {
        crate::runner::plan_reject(&app, &key).await
    } else {
        crate::pipeline::request_transition(&app.root, &key, to)
    };
    match res {
        Ok(issue) => {
            // 監査: dispatch/reject は issue_gate_action 側で記録済みのため
            // ここでは approve/requeue/push-approve/plan-* を記録する
            if !matches!(action.as_str(), "dispatch" | "reject") {
                crate::audit::record(
                    &app.root,
                    &key,
                    "human",
                    &action,
                    None,
                    Map::from_iter([("from".into(), json!(cur))]),
                );
            }
            app.sse("issues", &json!({}));
            j(json!({"ok": true, "issue": issue}))
        }
        Err(e) => err(StatusCode::CONFLICT, &e),
    }
}

// ---------- incidents（事案集約。incidents.mmd・manual-343b5d37） ----------
// 事案は detector 横断の関連課題グループ（incidents.json）。課題は
// incident フィールドで所属先 id を指す。merge/unmerge は人操作、
// dispatch は事案の queued メンバーを一括起動する

// GET /api/incidents — 事案一覧（memberIssues に課題の最小情報を解決済み）
async fn api_incidents_get(State(app): State<Shared>) -> Response {
    j(json!({"ok": true, "incidents": crate::incidents::list(&app.root)}))
}

// POST /api/incidents/merge — {keys:[issueKey,...], into?:<incidentId>}
// keys の課題群を1事案へ結合（into 省略時は既存所属 or 新規事案）
async fn api_incidents_merge(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let keys: Vec<String> = a
        .get("keys")
        .and_then(|v| v.as_array())
        .map(|ks| {
            ks.iter()
                .filter_map(|k| k.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let into = a.get("into").and_then(|v| v.as_str());
    match crate::incidents::merge(&app.root, &keys, into) {
        Ok(inc) => {
            app.sse("issues", &json!({}));
            j(json!({"ok": true, "incident": inc}))
        }
        Err(e) => err(StatusCode::CONFLICT, &e),
    }
}

// POST /api/incidents/unmerge — {key} 課題を事案から外す（空の事案は削除）
async fn api_incidents_unmerge(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let key = a.get("key").and_then(|v| v.as_str()).unwrap_or("");
    if key.is_empty() {
        return err(StatusCode::BAD_REQUEST, "key required");
    }
    match crate::incidents::unmerge(&app.root, key) {
        Ok(v) => {
            app.sse("issues", &json!({}));
            j(v)
        }
        Err(e) => err(StatusCode::CONFLICT, &e),
    }
}

// POST /api/incidents/{id}/dispatch — 事案の queued メンバーを一括 dispatch。
// 各課題の起動は既存 dispatch 経路（can_dispatch・runner）に委譲し、
// 起動できなかったメンバーは errors に集めて返す
async fn api_incidents_action(
    State(app): State<Shared>,
    AxPath((id, action)): AxPath<(String, String)>,
) -> Response {
    if action != "dispatch" {
        return err(StatusCode::NOT_FOUND, "unknown action");
    }
    match crate::incidents::dispatchable(&app.root, &id) {
        Err(e) => err(StatusCode::NOT_FOUND, &e),
        Ok(keys) => {
            let mut dispatched = Vec::new();
            let mut errors = Vec::new();
            for k in keys {
                match issue_gate_action(&app, &k, "dispatch").await {
                    Ok(_) => dispatched.push(k),
                    Err(e) => errors.push(json!({"key": k, "error": e})),
                }
            }
            app.sse("issues", &json!({}));
            j(json!({"ok": true, "dispatched": dispatched, "errors": errors}))
        }
    }
}

// POST /api/issues/dispatch-all — 一括対応（issue-lifecycle.mmd）。
// body {keys:[...]} の課題を順に dispatch。同一 repo 競合・同時実行上限で
// 起動できない課題は autoDispatch:true を立てて待機扱いにし、drain が
// repo 解放時に自動 dispatch する（人の一括承認の延長。フラグは起動時に消費）
async fn api_issues_dispatch_all(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let keys: Vec<String> = a
        .get("keys")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if keys.is_empty() {
        return err(StatusCode::BAD_REQUEST, "keys required");
    }
    let mut dispatched = Vec::new();
    let mut waiting = Vec::new();
    let mut errors = Vec::new();
    for k in keys {
        match issue_gate_action(&app, &k, "dispatch").await {
            Ok(_) => dispatched.push(k),
            Err(e)
                if e.starts_with("blocked by") || e.starts_with("maxConcurrentRuns") =>
            {
                let _ =
                    crate::pipeline::set_field(&app.root, &k, "autoDispatch", json!(true));
                waiting.push(k);
            }
            Err(e) => errors.push(json!({"key": k, "error": e})),
        }
    }
    app.sse("issues", &json!({}));
    j(json!({"ok": true, "dispatched": dispatched, "waiting": waiting, "errors": errors}))
}

// ---------- silences（起票ミュート。matcher+期限・期限切れ自動解除） ----------
// silences.json の 1レコード:
//   {id, comment, createdBy, createdAt, startsAt?, endsAt,
//    matchers: {source?, repo?, slug?, kind?, severity?}}
// endsAt は必須（無期限 silence は置かない）。照合は intake の SIL ゲート。

/// silence が照合対象（startsAt<=now<endsAt）か
fn silence_active(s: &Value, now: chrono::DateTime<chrono::Utc>) -> bool {
    let ts = |v: &str| {
        chrono::DateTime::parse_from_rfc3339(v)
            .map(|t| t.with_timezone(&chrono::Utc))
            .ok()
    };
    if let Some(t) = s.get("startsAt").and_then(|v| v.as_str()).and_then(ts) {
        if now < t {
            return false;
        }
    }
    matches!(
        s.get("endsAt").and_then(|v| v.as_str()).and_then(ts),
        Some(t) if now < t
    )
}

async fn api_silences_get(State(app): State<Shared>) -> Response {
    let now = chrono::Utc::now();
    let mut sils = crate::store::load_silences(&app.root);
    for s in sils.iter_mut() {
        s["active"] = json!(silence_active(s, now));
    }
    j(json!({"ok": true, "silences": sils}))
}

// POST /api/silences — {matchers:{...}, comment, durationMinutes} または
// {endsAt, startsAt?}（RFC3339）。matcher は1属性以上必須
async fn api_silences_post(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let mut m = serde_json::Map::new();
    if let Some(mm) = a.get("matchers").and_then(|v| v.as_object()) {
        for attr in ["source", "repo", "slug", "kind", "severity"] {
            if let Some(v) = mm.get(attr).and_then(|v| v.as_str()) {
                if !v.trim().is_empty() {
                    m.insert(attr.to_string(), json!(v.trim()));
                }
            }
        }
    }
    if m.is_empty() {
        return err(StatusCode::BAD_REQUEST, "matchers required");
    }
    let parse_ts = |v: Option<&Value>| {
        v.and_then(|v| v.as_str())
            .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
            .map(|t| t.with_timezone(&chrono::Utc))
    };
    let ends = parse_ts(a.get("endsAt")).or_else(|| {
        a.get("durationMinutes")
            .and_then(|v| v.as_i64())
            .map(|min| chrono::Utc::now() + chrono::Duration::minutes(min.max(0)))
    });
    let Some(ends) = ends else {
        return err(
            StatusCode::BAD_REQUEST,
            "endsAt or durationMinutes required",
        );
    };
    if ends <= chrono::Utc::now() {
        return err(StatusCode::BAD_REQUEST, "endsAt must be in the future");
    }
    let starts = parse_ts(a.get("startsAt"));
    let mut sil = json!({
        "id": rand_hex(4),
        "comment": a.get("comment").and_then(|v| v.as_str()).unwrap_or(""),
        "createdBy": a.get("createdBy").and_then(|v| v.as_str())
            .unwrap_or("panel"),
        "createdAt": now_iso(),
        "endsAt": ends.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "matchers": Value::Object(m),
    });
    if let Some(t) = starts {
        sil["startsAt"] =
            json!(t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
    }
    let mut sils = crate::store::load_silences(&app.root);
    sils.push(sil.clone());
    crate::store::save_silences(&app.root, &sils);
    app.sse("silences", &json!({}));
    j(json!({"ok": true, "silence": sil}))
}

// POST /api/silences/remove — {id}
async fn api_silences_remove(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let id = a.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let sils = crate::store::load_silences(&app.root);
    let kept: Vec<Value> = sils
        .iter()
        .filter(|s| s.get("id").and_then(|v| v.as_str()) != Some(id))
        .cloned()
        .collect();
    if kept.len() == sils.len() {
        return err(StatusCode::NOT_FOUND, "silence not found");
    }
    crate::store::save_silences(&app.root, &kept);
    app.sse("silences", &json!({}));
    j(json!({"ok": true}))
}

// ---------- proposals（採否は人ゲート・vet が前提検査） ----------

async fn api_proposals_get(State(app): State<Shared>) -> Response {
    let d = proposals::dir(&app.root);
    let base = norm_slashes(&app.root.to_string_lossy());
    let list: Vec<Value> = proposals::list(&d)
        .iter()
        .map(|p| proposals::with_vet(p, &base))
        .collect();
    j(json!({"ok": true, "proposals": list}))
}

async fn api_proposals_post(State(app): State<Shared>, b: Bytes) -> Response {
    let a = body_json(b).await;
    let no_title = a.get("title").and_then(|v| v.as_str()).map(|s| s.trim().is_empty()).unwrap_or(true);
    if no_title { return err(StatusCode::BAD_REQUEST, "title required"); }
    let no_changes = a.get("changes").and_then(|v| v.as_array()).map(|v| v.is_empty()).unwrap_or(true);
    if no_changes { return err(StatusCode::BAD_REQUEST, "changes required"); }
    let d = proposals::dir(&app.root);
    let p = proposals::create(&d, &a);
    app.sse("proposals", &json!({}));
    j(json!({"ok": true, "proposal": proposals::with_vet(&p, &norm_slashes(&app.root.to_string_lossy()))}))
}

// POST /api/proposals/{id}/{accept|decline} — accept は vet.ok のみ
async fn api_proposals_decide(
    State(app): State<Shared>,
    AxPath((id, decision)): AxPath<(String, String)>,
) -> Response {
    let accept = match decision.as_str() {
        "accept" => true,
        "decline" => false,
        _ => return err(StatusCode::NOT_FOUND, "unknown decision"),
    };
    let d = proposals::dir(&app.root);
    let base = norm_slashes(&app.root.to_string_lossy());
    let (code, body) = proposals::decide(&d, &id, accept, &base);
    if code == 200 {
        if accept {
            // 採用=適用: vet 合格の changes を root 配下へ書込む
            // （CDN gated チャネル・知見還流の proposals がここで実効化する）
            if let Some(changes) = body
                .get("proposal")
                .and_then(|p| p.get("changes"))
                .and_then(|v| v.as_array())
            {
                let mut wrote = 0;
                let mut cfg_touched = false;
                for c in changes {
                    let rel = c.get("path").and_then(|v| v.as_str()).unwrap_or("");
                    let content = c.get("content").and_then(|v| v.as_str()).unwrap_or("");
                    let fp = normalize(&format!("{}/{}", base, rel));
                    if !fp.to_lowercase().starts_with(&format!("{}/", base.to_lowercase())) {
                        continue;
                    }
                    let fp = PathBuf::from(fp);
                    if let Some(pd) = fp.parent() {
                        let _ = std::fs::create_dir_all(pd);
                    }
                    if std::fs::write(&fp, content).is_ok() {
                        wrote += 1;
                        cfg_touched |= rel == "config.json";
                    }
                }
                if cfg_touched {
                    *app.cfg.write().await = load_config(&app.root);
                }
                conlog(
                    "control",
                    "sys",
                    &format!("proposal {} を適用（{} ファイル書込）", id, wrote),
                );
            }
        }
        app.sse("proposals", &json!({}));
    }
    (StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), Json(body)).into_response()
}

async fn api_agents(State(app): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
    let cfg = app.cfg().await;
    let slug = q.get("repo").cloned().unwrap_or_default();
    let roots = app.doc_roots(&cfg, &slug);
    j(json!({"ok": true, "entries": list_agent_docs(&roots)}))
}

async fn api_skills(State(app): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
    let cfg = app.cfg().await;
    let slug = q.get("repo").cloned().unwrap_or_default();
    let roots = app.doc_roots(&cfg, &slug);
    let exe = cfg_str(&cfg, "devinExe")
        .map(String::from)
        .or_else(|| {
            cfg_obj(&cfg, "chatAgents")
                .and_then(|a| a.get("devin"))
                .and_then(|d| d.get("exe"))
                .and_then(|v| v.as_str())
                .map(String::from)
        });
    let entries: Vec<Value> = list_skill_docs(&roots, exe.as_deref())
        .into_iter()
        .filter(|d| d.get("scope").and_then(|v| v.as_str()) != Some("global"))
        .collect();
    j(json!({"ok": true, "entries": entries}))
}

async fn api_refs(State(app): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
    let cfg = app.cfg().await;
    let target = normalize(q.get("path").cloned().unwrap_or_default().as_str()).to_lowercase();
    let roots_all = app.doc_roots(&cfg, "");
    let exe = cfg_str(&cfg, "devinExe").map(String::from);
    let docs: Vec<Value> = list_agent_docs(&roots_all)
        .into_iter()
        .chain(list_skill_docs(&roots_all, exe.as_deref()))
        .collect();
    let Some(doc) = docs.iter().find(|d| {
        normalize(d.get("path").and_then(|v| v.as_str()).unwrap_or("")).to_lowercase() == target
    }) else {
        return err(StatusCode::NOT_FOUND, "not found");
    };
    let scope = doc.get("scope").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let path = doc.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
    j(json!({"ok": true, "entries": refs::list_ref_docs(&path, &roots_all, &scope)}))
}

async fn api_doc(State(app): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
    let cfg = app.cfg().await;
    let raw = q.get("path").cloned().unwrap_or_default();
    let target = normalize(&raw).to_lowercase();
    let roots = app.doc_roots(&cfg, "");
    let exe = cfg_str(&cfg, "devinExe").map(String::from);
    let docs: Vec<Value> = list_agent_docs(&roots)
        .into_iter()
        .chain(list_skill_docs(&roots, exe.as_deref()))
        .collect();
    let mut hit = docs.iter().find(|d| {
        normalize(d.get("path").and_then(|v| v.as_str()).unwrap_or("")).to_lowercase() == target
    }).cloned();
    if hit.is_none() && refs::list_all_ref_paths(&docs, &roots).contains(&target) {
        hit = Some(json!({"name": basename(&raw), "kind": "ref",
            "path": raw, "scope": "ref"}));
    }
    let Some(hit) = hit else {
        return err(StatusCode::NOT_FOUND, "not found");
    };
    let path = hit.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let truncated = text.chars().count() > 200 * 1024;
    let mut v = hit.clone();
    v["ok"] = json!(true);
    v["text"] = json!(if truncated {
        text.chars().take(200 * 1024).collect::<String>()
    } else {
        text.clone()
    });
    v["truncated"] = json!(truncated);
    j(v)
}

// ---------- 静的ファイル配信（panel/ + /docs/） ----------
async fn serve_static(State(app): State<Shared>, uri: Uri) -> Response {
    let p = uri.path().to_string();
    if p == "/docs" {
        return Response::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, "/docs/")
            .body(Body::empty())
            .unwrap();
    }
    let is_docs = p.starts_with("/docs/");
    let rel = if p == "/" || p == "/docs/" {
        "index.html".to_string()
    } else if is_docs {
        p[6..].to_string()
    } else {
        p[1..].to_string()
    };
    let base = if is_docs {
        app.root.join("docs")
    } else {
        app.panel.clone()
    };
    // 配信候補: CDN オーバーレイ(assets/<ch>/) → ローカル(panel/・docs/)
    let ch = if is_docs { "docs" } else { "panel" };
    let mut cands: Vec<PathBuf> = vec![];
    if let Some(f) = crate::assets::overlay_file(&app.root, ch, &rel) {
        cands.push(f);
    }
    let base_n = norm_slashes(&base.to_string_lossy());
    let file = normalize(&format!("{}/{}", base_n, rel));
    if !(file.starts_with(&base_n) || file.to_lowercase().starts_with(&base_n.to_lowercase())) {
        return err(StatusCode::FORBIDDEN, "");
    }
    cands.push(PathBuf::from(file));
    for fp in &cands {
        if !fp.is_file() {
            continue;
        }
        let mime = match fp.extension().and_then(|e| e.to_str()).map(|s| s.to_lowercase()) {
            Some(e) => match e.as_str() {
                "html" => "text/html; charset=utf-8",
                "js" => "text/javascript",
                "css" => "text/css",
                "json" => "application/json",
                "png" => "image/png",
                "svg" => "image/svg+xml",
                "ico" => "image/x-icon",
                _ => "application/octet-stream",
            },
            None => "application/octet-stream",
        };
        if let Ok(data) = std::fs::read(fp) {
            return Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime)
                .header(header::CACHE_CONTROL, "no-store")
                .body(Body::from(data))
                .unwrap();
        }
    }
    if p.starts_with("/api/") {
        // 未登録の API パス（実行中バイナリがディスク上の panel/ より古く
        // ルート未実装のケース）を静的ファイルの 404 と区別できるよう明示する
        return err(
            StatusCode::NOT_FOUND,
            &format!("api not found: {}（実行中バイナリが古い可能性）", p),
        );
    }
    err(StatusCode::NOT_FOUND, "not found")
}

// ---------- ルータ構築 ----------
pub fn router(app: Shared) -> Router {
    Router::new()
        .route("/api/events", get(api_events))
        .route("/api/console", get(api_console))
        .route("/api/state", get(api_state))
        .route("/api/repos", get(api_repos_get).post(api_repos_post))
        .route("/api/repos/update", post(api_repos_update))
        .route("/api/repos/remove", post(api_repos_remove))
        .route("/api/fs/dirs", get(api_fs_dirs))
        .route("/api/fs/tree", get(api_fs_tree))
        .route("/api/fs/file", get(api_fs_file))
        .route("/api/git/commit", post(api_git_commit))
        .route("/api/git/push", post(api_git_push))
        .route("/api/control/restart", post(api_restart))
        .route("/api/control/launch", post(api_launch))
        .route("/api/dev-envs", get(api_dev_envs))
        .route("/api/dev-envs/launch", post(api_dev_envs_launch))
        .route("/api/dev-envs/stop", post(api_dev_envs_stop))
        .route("/api/control/import-github", post(api_import_github))
        .route(
            "/api/control/autostart",
            get(api_autostart_get).post(api_autostart_post),
        )
        .route("/api/control/issue-mode", post(api_issue_mode))
        .route("/api/control/session-limit", post(api_session_limit))
        .route("/api/chat-agents", get(api_chat_agents_get).post(api_chat_agents_post))
        .route("/api/chat-agents/update", post(api_chat_agents_update))
        .route("/api/chat-agents/remove", post(api_chat_agents_remove))
        .route("/api/chat-agents/active", post(api_chat_agents_active))
        .route("/api/chat/sessions", get(api_chat_sessions))
        .route("/api/chat/options", get(api_chat_options))
        .route("/api/chat", get(api_chat_get))
        .route("/api/chat/send", post(api_chat_send))
        .route("/api/chat/mode", post(api_chat_mode))
        .route("/api/chat/repo-answer", post(api_chat_repo_answer))
        .route("/api/chat/revise", post(api_chat_revise))
        .route("/api/chat/restore", post(api_chat_restore))
        .route("/api/chat/cancel", post(api_chat_cancel))
        .route("/api/chat/clear", post(api_chat_clear))
        .route("/api/chat/reset", post(api_chat_reset))
        .route("/api/chat/resume", post(api_chat_resume))
        .route("/api/chat/resume-all", post(api_chat_resume_all))
        .route("/api/chat/start-suspended", post(api_chat_start_suspended))
        .route("/api/chat/parallel", post(api_chat_parallel))
        .route("/api/chat/verify", post(api_chat_verify))
        .route("/api/chat/rework", post(api_chat_rework))
        .route("/api/chat/close", post(api_chat_close))
        .route("/api/chat/close-completed", post(api_chat_close_completed))
        .route("/api/chat/rename", post(api_chat_rename))
        .route("/api/chat/option", post(api_chat_option))
        .route("/api/chat/attach", post(api_chat_attach))
        .route("/api/candidates", get(api_candidates_get).post(api_candidates_post))
        .route("/api/candidates/dispatch-all", post(api_candidates_dispatch_all))
        .route("/api/candidates/{key}/{action}", post(api_candidates_action))
        .route("/api/issues", get(api_issues_get))
        .route("/api/issues/dispatch-all", post(api_issues_dispatch_all))
        .route("/api/issues/{key}/audit", get(api_issue_audit))
        .route("/api/issues/{key}/trace", get(api_issue_trace))
        .route("/api/issues/{key}/{action}", post(api_issues_action))
        .route("/api/metrics", get(api_metrics))
        .route("/api/incidents", get(api_incidents_get))
        .route("/api/incidents/merge", post(api_incidents_merge))
        .route("/api/incidents/unmerge", post(api_incidents_unmerge))
        .route("/api/incidents/{id}/{action}", post(api_incidents_action))
        .route("/api/silences", get(api_silences_get).post(api_silences_post))
        .route("/api/silences/remove", post(api_silences_remove))
        .route("/api/proposals", get(api_proposals_get).post(api_proposals_post))
        .route("/api/proposals/{id}/{decision}", post(api_proposals_decide))
        .route("/api/agents", get(api_agents))
        .route("/api/skills", get(api_skills))
        .route("/api/refs", get(api_refs))
        .route("/api/doc", get(api_doc))
        .route("/api/devin-sessions", get(crate::devinenv::api_list))
        .route("/api/devin-sessions/delete", post(crate::devinenv::api_delete))
        .route("/api/devin-sessions/cleanup", post(crate::devinenv::api_cleanup))
        .route("/api/devin-sessions/vacuum", post(crate::devinenv::api_vacuum))
        .route("/api/devin-sessions/messages", post(crate::devinenv::api_messages))
        .route("/api/devin-sessions/attach", post(api_devin_attach))
        .route("/api/copilot-sessions", get(crate::copilotenv::api_list))
        .route("/api/copilot-sessions/delete", post(crate::copilotenv::api_delete))
        .route("/api/copilot-sessions/cleanup", post(crate::copilotenv::api_cleanup))
        .route("/api/copilot-sessions/done", post(crate::copilotenv::api_done))
        .route("/api/copilot-sessions/undone", post(crate::copilotenv::api_undone))
        .route("/api/copilot-sessions/messages", post(crate::copilotenv::api_messages))
        .route("/api/copilot-sessions/attach", post(api_copilot_attach))
        .route("/api/assets", get(crate::assets::api_get))
        .route("/api/assets/check", post(crate::assets::api_check))
        .route("/api/assets/apply", post(crate::assets::api_apply))
        .route("/api/assets/rollback", post(crate::assets::api_rollback))
        .fallback(serve_static)
        .with_state(app)
        .layer(tower_http::limit::RequestBodyLimitLayer::new(64 * 1024 * 1024))
}

// ---------- イベントループ（CtrlEvt 消費） ----------
pub async fn event_loop(app: Shared, mut rx: tokio::sync::mpsc::UnboundedReceiver<CtrlEvt>) {
    while let Some(ev) = rx.recv().await {
        match ev {
            CtrlEvt::Changed { id } => {
                let app2 = app.clone();
                app2.notify_chat(id).await;
            }
            CtrlEvt::SessionId { .. } => {
                app.persist_chats().await;
            }
            CtrlEvt::TurnStart { id, turn, text } => {
                // busy 立ち上がりを即時記録する。瞬時応答する実行体では
                // Changed 到着前に busy=false へ戻り、notify_chat の
                // busy_prev エッジ検出が立ち上がりを見ないまま
                // on_chat_done（親への集約等）を取りこぼすため
                // （同一チャネルの順序で TurnStart は Changed より先に処理される）
                app.busy_prev.lock().await.insert(id, true);
                let chats = app.chats.lock().await;
                if let Some(s) = chats.get(&id) {
                    s.snaps.lock().unwrap().begin_turn(turn, &text);
                    let cwd = s.chat.cwd().read().await.clone();
                    for n in extract_paths(&text, &cwd) {
                        s.snaps.lock().unwrap().capture(&n, turn);
                    }
                }
                drop(chats);
                app.sse("chat", &json!({"id": id}));
            }
            CtrlEvt::Restart { force } => {
                app.maybe_restart(force).await;
            }
            CtrlEvt::FileTouch { id, paths } => {
                let mut chats = app.chats.lock().await;
                if let Some(s) = chats.get(&id) {
                    let cwd = s.chat.cwd().read().await.clone();
                    let norm: Vec<String> = paths
                        .iter()
                        .map(|p| norm_path(p, &cwd))
                        .filter(|p| !p.is_empty())
                        .collect();
                    {
                        let mut c = s.chat.core().lock().await;
                        for n in &norm {
                            c.touched.insert(n.clone());
                        }
                        let (busy, snap_turn, waiting) =
                            (c.busy, c.snap_turn, c.waiting.is_some());
                        drop(c);
                        if busy {
                            let mut sn = s.snaps.lock().unwrap();
                            for n in &norm {
                                sn.capture(n, snap_turn);
                            }
                        }
                        if !waiting && !norm.is_empty() {
                            drop(chats);
                            let hits = app.find_chat_conflicts(id, &norm).await;
                            if !hits.is_empty() {
                                let files: Vec<String> =
                                    hits.iter().flat_map(|h| h.files.clone()).collect();
                                let blocked: Vec<i64> = hits.iter().map(|h| h.id).collect();
                                let chats2 = app.chats.lock().await;
                                if let Some(s2) = chats2.get(&id) {
                                    match &s2.chat {
                                        // ACP: ターンだけ session/cancel で止める
                                        // （プロセス・セッションは維持）
                                        Chat::Acp(c) => c.notify_cancel().await,
                                        Chat::Http(h) => h.enter_wait_busy().await,
                                    }
                                    let mut c2 = s2.chat.core().lock().await;
                                    c2.enter_wait(files, blocked, true, "conflict", None);
                                }
                            }
                            chats = app.chats.lock().await;
                        }
                    }
                }
                drop(chats);
                app.sse("chat", &json!({"id": id}));
            }
        }
    }
}

// ---------- 停滞検知・drift 監視・コンソール batch ----------
pub fn spawn_watchers(app: Shared) {
    // コンソール行 → 250ms バッチで SSE 'console'
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::conlog::Line>();
    crate::conlog::on_console_line(tx);
    {
        let app = app.clone();
        tokio::spawn(async move {
            let mut q = Vec::new();
            loop {
                match rx.recv().await {
                    Some(l) => {
                        q.push(l);
                        // 250ms 間に溜まった分をまとめる
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                        while let Ok(l2) = rx.try_recv() {
                            q.push(l2);
                        }
                        app.sse("console", &json!({"lines": q}));
                        q.clear();
                    }
                    None => break,
                }
            }
        });
    }
    // 停滞検知（30s 周期）
    {
        let app = app.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                iv.tick().await;
                let cfg = app.cfg().await;
                let stall_min = cfg_f64(&cfg, "chatStallMinutes", 30.0);
                let chats = app.chats.lock().await;
                for (id, s) in chats.iter() {
                    let mut c = s.chat.core().lock().await;
                    if c.busy {
                        let idle_ms = crate::util::now_ms() - c.last_activity;
                        if idle_ms > (stall_min * 60_000.0) as i64 {
                            c.push(
                                "sys",
                                &format!("[停滞検知] {:.0}分応答なし → 中断します", stall_min),
                            );
                            c.busy = false;
                            c.turn_seq += 1;
                            let chat = s.chat.clone();
                            let id2 = *id;
                            drop(c);
                            drop(chats);
                            chat.abort_turn().await;
                            let _ = app.evt_tx.send(CtrlEvt::Changed { id: id2 });
                            break;
                        }
                    }
                }
            }
        });
    }
    // 外部セッション監視（5s 周期。agent-sessions.mmd の ENVWATCH ノード）
    // - devin/copilot のセッション一覧を id+lastActivityAt で diff →
    //   変化時に SSE 'envsessions'（パネルのセッション画面が自動再描画）
    // - attach 済み chat は対象セッションの lastActivityAt 変化時のみ
    //   sync_attach（外部で進行中の会話が開いたまま追従する）
    {
        let app = app.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(std::time::Duration::from_secs(5));
            let mut first = true;
            // agent -> {sid -> (lastActivityAt, title)}
            let mut prev: HashMap<String, HashMap<String, (String, String)>> = HashMap::new();
            // chat_id -> 前回 sync_attach した lastActivityAt
            let mut seen: HashMap<i64, String> = HashMap::new();
            // 同時編集検出: chat_id -> (基準 lastActivityAt, busy 解除時刻ms)
            // ・警告済み集合。proc 保持 chat が busy でないのに外部一覧上の
            //   lastActivityAt が進んだら外部書き込みと判定（自分の書き込みは
            //   busy 中のみ発生）。busy 解除後 60s は最終書き込みの settle
            //   期間として除外し、警告は chat ごとに1回まで
            let mut ext_base: HashMap<i64, (String, i64)> = HashMap::new();
            let mut ext_warned: HashSet<i64> = HashSet::new();
            loop {
                iv.tick().await;
                for (agent, list) in [
                    ("devin", crate::devinenv::sessions_list()),
                    ("copilot", crate::copilotenv::sessions_list()),
                ] {
                    let cur: HashMap<String, (String, String)> = list
                        .iter()
                        .filter_map(|s| {
                            Some((
                                s["id"].as_str()?.to_string(),
                                (
                                    s["lastActivityAt"].as_str().unwrap_or("").to_string(),
                                    s["title"].as_str().unwrap_or("").to_string(),
                                ),
                            ))
                        })
                        .collect();
                    let changed = prev.get(agent).map(|o| *o != cur).unwrap_or(true);
                    if !first && changed {
                        app.sse("envsessions", &json!({"agent": agent}));
                    }
                    prev.insert(agent.to_string(), cur);
                }
                // attach 済み chat の追従（lastActivityAt が変わったものだけ）
                let targets: Vec<(i64, String, String)> = {
                    let chats = app.chats.lock().await;
                    let mut out = Vec::new();
                    for (id, s) in chats.iter() {
                        let Some(src) = s.attach_src.clone() else { continue };
                        let c = s.chat.core().lock().await;
                        let sid = c
                            .saved_session_id
                            .clone()
                            .or(c.session_id.clone())
                            .unwrap_or_default();
                        if !sid.is_empty() {
                            out.push((*id, src, sid));
                        }
                    }
                    out
                };
                let live_ids: HashSet<i64> = targets.iter().map(|t| t.0).collect();
                seen.retain(|id, _| live_ids.contains(id));
                for (id, src, sid) in targets {
                    let Some(sess) = prev.get(&src).and_then(|m| m.get(&sid)) else {
                        // 一覧から消失 = 外部側で削除。一覧自体が空の周回は
                        // 一時的な読み取り失敗とみなして伝播しない
                        if prev.get(&src).map(|m| !m.is_empty()).unwrap_or(false) {
                            seen.remove(&id);
                            let mut chats = app.chats.lock().await;
                            if let Some(s) = chats.get_mut(&id) {
                                s.attach_src = None;
                                // 外部削除済み sid への session/load 試行と
                                // resumable 誤表示を防ぐため解除
                                s.chat.core().lock().await.saved_session_id = None;
                                drop(chats);
                                let chat = {
                                    let chats = app.chats.lock().await;
                                    chats.get(&id).map(|s| s.chat.clone())
                                };
                                if let Some(chat) = chat {
                                    chat.push_sys(
                                        "[外部セッション削除] 元のセッションが削除されました（表示履歴は残ります）",
                                    )
                                    .await;
                                }
                                app.persist_chats().await;
                                app.sse("chat", &json!({"id": id}));
                            }
                        }
                        continue;
                    };
                    // 外部側のタイトル変更を chat ラベルへ反映
                    // （「[agent] title」形のままの場合のみ = 手動改名は守る）
                    let want = format!("[{}] {}", src, sess.1);
                    {
                        let mut chats = app.chats.lock().await;
                        if let Some(s) = chats.get_mut(&id) {
                            if !sess.1.is_empty()
                                && s.label.starts_with(&format!("[{}] ", src))
                                && s.label != want
                            {
                                s.label = want;
                                drop(chats);
                                app.persist_chats().await;
                                app.sse("chats", &json!({}));
                            }
                        }
                    }
                    if seen.get(&id).map(|a| a == &sess.0).unwrap_or(false) {
                        continue;
                    }
                    seen.insert(id, sess.0.clone());
                    sync_attach(&app, id).await;
                }
                // 同時編集検出: proc 保持中（session/load 済み）の sid が
                // 外部一覧にあり、非 busy のまま活動時刻が進んだら警告
                let live_targets: Vec<(i64, String, bool)> = {
                    let chats = app.chats.lock().await;
                    let mut out = Vec::new();
                    for (id, s) in chats.iter() {
                        if !s.chat.has_proc().await {
                            continue;
                        }
                        let c = s.chat.core().lock().await;
                        if let Some(sid) = c.session_id.clone() {
                            out.push((*id, sid, c.busy));
                        }
                    }
                    out
                };
                let live2: HashSet<i64> = live_targets.iter().map(|t| t.0).collect();
                ext_base.retain(|id, _| live2.contains(id));
                ext_warned.retain(|id| live2.contains(id));
                for (id, sid, busy) in live_targets {
                    let Some((act, _)) =
                        prev.values().find_map(|m| m.get(&sid)) else {
                        continue;
                    };
                    let now = crate::util::now_ms();
                    if busy {
                        // 自身の書き込みが走る期間。基準を現在値へ
                        ext_base.insert(id, (act.clone(), now));
                        continue;
                    }
                    match ext_base.get(&id).cloned() {
                        None => {
                            ext_base.insert(id, (act.clone(), now));
                        }
                        Some((prev_act, t)) => {
                            if prev_act != *act {
                                ext_base.insert(id, (act.clone(), t));
                                if now - t > 60_000 && ext_warned.insert(id) {
                                    let chat = {
                                        let chats = app.chats.lock().await;
                                        chats.get(&id).map(|s| s.chat.clone())
                                    };
                                    if let Some(chat) = chat {
                                        chat.push_sys(
                                            "[警告] このセッションは外部（CLI/Desktop）からも変更されています。同時編集は last-writer-wins になるため注意してください",
                                        )
                                        .await;
                                        app.sse("chat", &json!({"id": id}));
                                    }
                                }
                            }
                        }
                    }
                }
                first = false;
            }
        });
    }
    // CDN アセット更新: 起動時に 1 回確認し、cdnCheckMinutes>0 なら定期化
    {
        let app = app.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            loop {
                let cfg = app.cfg().await;
                let base_set = cfg_str(&cfg, "cdnBaseUrl").map(|s| !s.is_empty()).unwrap_or(false);
                let mins = cfg_f64(&cfg, "cdnCheckMinutes", 0.0);
                if base_set {
                    crate::assets::check(&app, true).await;
                }
                if mins <= 0.0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs_f64(mins * 60.0)).await;
            }
        });
    }
    // code drift: 実行 exe の mtime 変化 → 自動再起動
    {
        let app = app.clone();
        tokio::spawn(async move {
            let cfg = app.cfg().await;
            if !cfg_bool_default_true(&cfg, "autoRestartOnDrift") {
                return;
            }
            let Ok(exe) = std::env::current_exe() else { return };
            let mut last = exe.metadata().ok().and_then(|m| m.modified().ok());
            let mut iv = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                iv.tick().await;
                let cur = exe.metadata().ok().and_then(|m| m.modified().ok());
                if cur != last && last.is_some() {
                    conlog("control", "sys", "code drift（exe 更新）を検知 → 自動再起動");
                    last = cur;
                    let app = app.clone();
                    app.maybe_restart(false).await;
                } else {
                    last = cur;
                }
            }
        });
    }
    // learn（知見還流。learn-flow.mmd）: runs/evidence → knowledge/ 蓄積 +
    // SKILL draft の proposal 起票。pollSeconds.learn（既定60s、<=0 で無効）。
    {
        let app = app.clone();
        tokio::spawn(async move {
            // verify リプレイの最終実行（初回は即実行可能に 1h 前で初期化）
            let mut last_replay = std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(3600))
                .unwrap_or_else(std::time::Instant::now);
            loop {
                let cfg = app.cfg().await;
                let (_, props) = crate::knowledge::learn_pass(&app.root);
                // CBR（manual-654b11c2）: 終端済み課題を knowledge/cases.json
                // の事例ベースへ蓄積（決定的スコアの検索元）
                crate::cases::sync_cases(&app.root);
                // sleep-time consolidation（manual-fd394ace・consolidation.mmd）:
                // アイドル時のみ knowledge/ をクラスタ統合し上位レッスンを
                // runbook proposal として起票（出力は proposals のみ）
                let cons = crate::consolidate::consolidate_pass(&app.root, &cfg);
                // eval セット（manual-a2de7417・eval-set.mmd）: 終端課題を
                // eval case 化し、assess 回帰レポートを更新（実機不要・安い）。
                // verify リプレイは重いのでアイドル時・1h 間隔に限定
                crate::evals::sync_evals(&app.root);
                let reg = crate::evals::assess_regression(&app.root, 0);
                crate::evals::save_report(&app.root, &reg);
                if reg.get("diffCount").and_then(|v| v.as_u64()).unwrap_or(0) > 0 {
                    conlog(
                        "control",
                        "err",
                        &format!(
                            "assess 回帰で {} 件の退化を検出（knowledge/eval-report.json）",
                            reg.get("diffCount").and_then(|v| v.as_u64()).unwrap_or(0)
                        ),
                    );
                }
                if crate::consolidate::is_idle(&crate::store::load_issues(&app.root), &cfg)
                    && last_replay.elapsed().as_secs() >= 3600
                {
                    let rep = crate::evals::verify_replay(&app.root).await;
                    crate::evals::save_report(&app.root, &rep);
                    last_replay = std::time::Instant::now();
                }
                if props > 0 || cons > 0 {
                    app.sse("proposals", &json!({}));
                }
                let secs = cfg_obj(&cfg, "pollSeconds")
                    .and_then(|p| p.get("learn"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(60.0);
                if secs <= 0.0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs_f64(secs)).await;
            }
        });
    }
    // 課題パイプの定期処理（issue-lifecycle.mmd）。dispatch は人判断のみで
    // queued の自動起動はしない —— drain は起票 reconcile・中断した
    // verifying の復帰・pushApproved 待ちの再開・孤児 running の failed 化を
    // 担い、起動した課題の run→verify→push は runner.rs 側で完結する。
    // pollSeconds.runner（既定15s、<=0 で無効）。
    {
        let app = app.clone();
        tokio::spawn(async move {
            loop {
                crate::runner::drain(&app).await;
                let cfg = app.cfg().await;
                let secs = cfg_obj(&cfg, "pollSeconds")
                    .and_then(|p| p.get("runner"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(15.0);
                if secs <= 0.0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs_f64(secs)).await;
            }
        });
    }
    // push 後フォローアップループ（postpush-loop.mmd・manual-55878990）:
    // postPush サブ状態が checking|waiting|fixing の課題を監視し、CI失敗・
    // レビュー指摘・コンフリクトの検出を修復課題として起票する。
    // pollSeconds.postPush（既定300s、<=0 で無効）
    {
        let app = app.clone();
        tokio::spawn(async move {
            loop {
                let touched = crate::postpush::poll(&app).await;
                if touched > 0 {
                    app.sse("issues", &json!({}));
                    app.sse("candidates", &json!({}));
                }
                let cfg = app.cfg().await;
                let secs = cfg_obj(&cfg, "pollSeconds")
                    .and_then(|p| p.get("postPush"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(300.0);
                if secs <= 0.0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs_f64(secs)).await;
            }
        });
    }
    // 課題検出ポーリング（detect-pipeline.mmd: detector emit → ingest → intake）
    // pollSeconds.issues（既定120s）で github-issues、pollSeconds.health
    // （既定300s）で local-health。新規課題ができたら SSE で即時反映する。
    spawn_detector_poller(app.clone(), "issues", 120.0);
    spawn_detector_poller(app.clone(), "ciFailure", 300.0);
    spawn_detector_poller(app.clone(), "specDrift", 300.0);
    spawn_detector_poller(app.clone(), "health", 300.0);
    spawn_detector_poller(app.clone(), "sessionWatch", 300.0);
    spawn_detector_poller(app.clone(), "unattributedWork", 600.0);
    // dead man's switch heartbeat（heartbeat.mmd・§11）: agent 自身の死の
    // 外部通知。監視対象パイプラインと別経路の独立タスク
    crate::heartbeat::spawn(app);
}

/// kind は pollSeconds のキー（"issues"→github-issues / "health"→local-health /
/// "ciFailure"→ci-failure / "specDrift"→spec-drift）。
/// secs <= 0 なら起動時の1回のみ実行して終了する。
fn spawn_detector_poller(app: Shared, kind: &'static str, default_secs: f64) {
    tokio::spawn(async move {
        loop {
            let cfg = app.cfg().await;
            let cands = match kind {
                "health" => crate::detectors::local_health::poll(&cfg).await,
                "ciFailure" => crate::detectors::ci_failure::poll(&cfg).await,
                "specDrift" => crate::detectors::spec_drift::poll(&cfg, &app.root).await,
                "sessionWatch" => crate::detectors::session_watch::poll(&cfg).await,
                "unattributedWork" => {
                    crate::detectors::unattributed_work::poll(&cfg, &app).await
                }
                _ => crate::detectors::github_issues::poll(&cfg).await,
            };
            // detector 沈黙検知（detector-health.mmd・§11）: 最終実行/最終 emit
            // を台帳に記録し、沈黙/停止した detector を local-health 候補として
            // emit する（返却済み = 実行成功。panic は poller 死亡として検出）
            crate::detector_health::record_run(&app.root, kind, cands.len());
            let mut created = false;
            let mut deferred = false;
            for c in &cands {
                let (_i, cr, df) = crate::detectors::ingest(&app.root, c);
                created |= cr;
                deferred |= df;
            }
            for c in crate::detector_health::check_silent(&app.root, &cfg) {
                let (_i, cr, df) = crate::detectors::ingest(&app.root, &c);
                created |= cr;
                deferred |= df;
            }
            if created {
                app.sse("candidates", &json!({}));
                app.sse("issues", &json!({}));
            } else if deferred {
                // レート制限の deferred ピル表示を即時反映
                app.sse("candidates", &json!({}));
            }
            app.sse("detector-health", &json!({}));
            let secs = cfg_obj(&cfg, "pollSeconds")
                .and_then(|p| p.get(kind))
                .and_then(|v| v.as_f64())
                .unwrap_or(default_secs);
            if secs <= 0.0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs_f64(secs)).await;
        }
    });
}
