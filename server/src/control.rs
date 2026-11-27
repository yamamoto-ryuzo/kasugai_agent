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
            "auth": a.get("auth").and_then(|v| v.as_str()).unwrap_or("none"),
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
                let ide = self.is_under_ws(p) && has_git(Path::new(p));
                v["ide"] = json!(ide);
                v["user"] = json!(!ide);
                v
            })
            .collect();
        let mut seen: HashSet<String> = cfg_arr(cfg, "repos")
            .iter()
            .map(|r| norm_p(r.get("path").and_then(|v| v.as_str()).unwrap_or("")))
            .collect();
        // 自動検出の走査根（GitHub 非依存のユーザーリポジトリも対象）:
        //   ws_root      — 全 dir。.git 付き=IDE管理下、無し=ユーザー
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
            let ide_root = norm_p(&root) == ws_n;
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
                    let ide = ide_root && git;
                    reg.push(json!({"slug": null, "path": ps,
                        "registered": false, "ide": ide, "user": !ide}));
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
            let api_key = if spec.get("auth").and_then(|v| v.as_str()) == Some("devin") {
                crate::chat::acp::devin_api_key_pub()
            } else {
                None
            };
            let _ = root2;
            Chat::Acp(Arc::new(AcpChat {
                id: 0,
                agent: spec,
                cwd,
                core: Arc::new(Mutex::new(crate::chat::Core::default())),
                proc_slot: Arc::new(Mutex::new(None)),
                api_key,
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
                let _ = self.evt_tx.send(CtrlEvt::Changed { id });
                if let Err(e) = chat.send(text, None).await {
                    chat.push_sys(&format!("[再開失敗] {}", e)).await;
                }
            }
        }
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
                let mut c = s.chat.core().lock().await;
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
                        match chat.ensure_session().await {
                            Ok(()) => n_resumed += 1,
                            Err(e) => {
                                chat.push_sys(&format!("[処理再開失敗] {}", e)).await;
                            }
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
                if wstate.kind == "repo" {
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
                    let _ = self.evt_tx.send(CtrlEvt::Changed { id });
                    if let Err(e) = chat.send(text, None).await {
                        chat.push_sys(&format!("[再開失敗] {}", e)).await;
                    } else {
                        n_released += 1;
                    }
                }
            }
        }
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
    async fn send_to_chat(
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
        chat.send(text, display).await
    }

    /// claims.mjs findConflicts 相当: 他 busy chat のクレームとの衝突
    async fn find_chat_conflicts(&self, self_id: i64, paths: &[String]) -> Vec<Conflict> {
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
        if let Some(s) = s {
            let _ = s.chat.reset().await;
            let _ = std::fs::remove_dir_all(self.snaps.join(id.to_string()));
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
    pub usage: Option<crate::chat::Usage>,
    pub waiting: Option<crate::chat::WaitState>,
    pub held_prompts: Vec<String>,
    pub resume_on_release: bool,
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

// git 実行（60s timeout、出力を conlog へ）
async fn run_git(cwd: &str, args: &[&str]) -> (i32, String) {
    use tokio::io::AsyncReadExt;
    let mut gcmd = tokio::process::Command::new("git");
    gcmd.args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    crate::util::hide_console_tokio(&mut gcmd);
    let child = gcmd.spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return (-1, e.to_string()),
    };
    let mut so = child.stdout.take().unwrap();
    let mut se = child.stderr.take().unwrap();
    let ho = tokio::spawn(async move {
        let mut v = Vec::new();
        let _ = so.read_to_end(&mut v).await;
        v
    });
    let he = tokio::spawn(async move {
        let mut v = Vec::new();
        let _ = se.read_to_end(&mut v).await;
        v
    });
    let res = tokio::time::timeout(std::time::Duration::from_secs(60), child.wait()).await;
    match res {
        Ok(Ok(st)) => {
            let o = ho.await.unwrap_or_default();
            let e = he.await.unwrap_or_default();
            (
                st.code().unwrap_or(-1),
                format!("{}{}", String::from_utf8_lossy(&o), String::from_utf8_lossy(&e))
                    .trim()
                    .to_string(),
            )
        }
        Ok(Err(e)) => (-1, e.to_string()),
        Err(_) => {
            let _ = child.kill().await;
            (-1, "timeout".to_string())
        }
    }
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
        entry.insert(
            "auth".into(),
            json!(if a.get("auth").and_then(|v| v.as_str()) == Some("devin") { "devin" } else { "none" }),
        );
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
        ag["auth"] = json!(if v.as_str() == Some("devin") { "devin" } else { "none" });
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
    match chat.ensure_session().await {
        Ok(()) => {
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

/// 対応/却下の適用（issues・candidates 共通の人ゲート）。
/// dispatch: queued→running（gated は承認→queued・failed は再実行→queued を
/// 兼ねた即時 dispatch）。reject: gated|queued|failed→rejected。
/// 状態遷移は pipeline・起動は runner に委譲し、ここでは状態を直接触らない
async fn issue_gate_action(app: &Shared, key: &str, action: &str) -> Result<Value, String> {
    let cur = issue_status(&app.root, key).ok_or_else(|| "issue not found".to_string())?;
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
                crate::pipeline::ST_QUEUED => {}
                s => {
                    return Err(format!("issue is {} (gated|queued|failed required)", s));
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

// POST /api/candidates/{key}/{dispatch|reject} — 候補一覧（判断待ち inbox）の
// 人ゲート。課題が未起票なら人判断で即時起票（rate-limit は人判断を優先して
// bypass）してから、対応=即時 dispatch（gated/failed は承認/再実行を兼ねる）・
// 却下=rejected へ（detect-pipeline.mmd INBOX・issue-lifecycle.mmd）
async fn api_candidates_action(
    State(app): State<Shared>,
    AxPath((key, action)): AxPath<(String, String)>,
) -> Response {
    if action != "dispatch" && action != "reject" {
        return err(StatusCode::NOT_FOUND, "unknown action");
    }
    let cands = crate::store::load_candidates(&app.root);
    let Some(cand) = cands
        .iter()
        .find(|c| c.get("key").and_then(|v| v.as_str()) == Some(key.as_str()))
    else {
        return err(StatusCode::NOT_FOUND, "candidate not found");
    };
    if issue_status(&app.root, &key).is_none() {
        crate::detectors::ingest_forced(&app.root, cand);
    }
    match issue_gate_action(&app, &key, &action).await {
        Ok(issue) => {
            app.sse("issues", &json!({}));
            app.sse("candidates", &json!({}));
            j(json!({"ok": true, "issue": issue}))
        }
        Err(e) => err(StatusCode::CONFLICT, &e),
    }
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
    let (from, to) = match action.as_str() {
        "approve" => (crate::pipeline::ST_GATED, crate::pipeline::ST_QUEUED),
        "reject" => ("", crate::pipeline::ST_REJECTED),
        "requeue" => (crate::pipeline::ST_FAILED, crate::pipeline::ST_QUEUED),
        "push-approve" => (crate::pipeline::ST_VERIFYING, ""),
        "dispatch" => ("", ""),
        "plan-approve" | "plan-reject" => ("", ""),
        _ => return err(StatusCode::NOT_FOUND, "unknown action"),
    };
    let Some(cur) = issue_status(&app.root, &key) else {
        return err(StatusCode::NOT_FOUND, "issue not found");
    };
    // reject は gated（承認前）・queued（対応待ち）・failed（再試行しない判断）
    // のどれからでも可。dispatch は queued に加え gated（承認+即時 dispatch）・
    // failed（再実行+即時 dispatch）からも可
    let ok_from = match action.as_str() {
        "reject" | "dispatch" => matches!(
            cur.as_str(),
            crate::pipeline::ST_GATED
                | crate::pipeline::ST_QUEUED
                | crate::pipeline::ST_PLAN_REVIEW
                | crate::pipeline::ST_FAILED
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
        .route("/api/control/import-github", post(api_import_github))
        .route(
            "/api/control/autostart",
            get(api_autostart_get).post(api_autostart_post),
        )
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
        .route("/api/chat/verify", post(api_chat_verify))
        .route("/api/chat/rework", post(api_chat_rework))
        .route("/api/chat/close", post(api_chat_close))
        .route("/api/chat/close-completed", post(api_chat_close_completed))
        .route("/api/chat/rename", post(api_chat_rename))
        .route("/api/chat/option", post(api_chat_option))
        .route("/api/chat/attach", post(api_chat_attach))
        .route("/api/candidates", get(api_candidates_get).post(api_candidates_post))
        .route("/api/candidates/{key}/{action}", post(api_candidates_action))
        .route("/api/issues", get(api_issues_get))
        .route("/api/issues/{key}/{action}", post(api_issues_action))
        .route("/api/issues/{key}/trace", get(api_issue_trace))
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
    // 課題検出ポーリング（detect-pipeline.mmd: detector emit → ingest → intake）
    // pollSeconds.issues（既定120s）で github-issues、pollSeconds.health
    // （既定300s）で local-health。新規課題ができたら SSE で即時反映する。
    spawn_detector_poller(app.clone(), "issues", 120.0);
    spawn_detector_poller(app.clone(), "ciFailure", 300.0);
    spawn_detector_poller(app.clone(), "specDrift", 300.0);
    spawn_detector_poller(app.clone(), "health", 300.0);
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
