// copilotenv.rs — GitHub Copilot CLI のローカルセッション管理（環境維持）。
// README「Copilot セッション管理」・diagrams/copilot-sessions.mmd が正本。
// 共通テンプレの契約は diagrams/agent-sessions.mmd。
//
// 対象は ~/.copilot/ 配下:
//   session-state/<uuid>/                        … セッション実体（events.jsonl 等）
//   session-state/.session-operation-locks/<uuid>.lock … 生存プロセスのロック
//   open-sessions-state.json                     … 開いているセッション（working フラグ）
//
// live 判定は lock ファイルへの LockFile 試行＋open-state エントリ。
// DB 台帳を持たない（session-state dir が実体）ため VACUUM は非対応
// （caps.vacuum=false を返し共通テンプレがボタンを隠す）。

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use axum::{
    body::Bytes,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

use crate::conlog::conlog;

const HEAD_BYTES: u64 = 64 * 1024;
const TAIL_BYTES: u64 = 256 * 1024;

// ---------- パス ----------

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
}
fn copilot_dir() -> Option<PathBuf> {
    home_dir().map(|d| d.join(".copilot"))
}
fn state_dir() -> Option<PathBuf> {
    copilot_dir().map(|d| d.join("session-state"))
}
fn oplocks_dir() -> Option<PathBuf> {
    state_dir().map(|d| d.join(".session-operation-locks"))
}
fn open_state_path() -> Option<PathBuf> {
    copilot_dir().map(|d| d.join("open-sessions-state.json"))
}
fn lock_file(id: &str) -> Option<PathBuf> {
    oplocks_dir().map(|d| d.join(format!("{id}.lock")))
}
// 完了台帳。cwd は起動時に kasugai_agent root に設定済み（main.rs）
fn done_path() -> PathBuf {
    PathBuf::from("copilot-done.json")
}

// ---------- live 判定（ロック生存試行。devinenv.rs と同手法） ----------

#[cfg(windows)]
fn lock_held(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, LockFile, UnlockFile, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    let w: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let h = CreateFileW(
            w.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        );
        if h == INVALID_HANDLE_VALUE {
            return true; // 開けない＝誰かが保持。保守的に live 扱い
        }
        let ok = LockFile(h, 0, 0, 1, 0);
        if ok != 0 {
            UnlockFile(h, 0, 0, 1, 0);
        }
        CloseHandle(h);
        ok == 0
    }
}
#[cfg(not(windows))]
fn lock_held(_path: &Path) -> bool {
    false
}

// ---------- open-sessions-state.json ----------

fn open_states() -> HashMap<String, Value> {
    let Some(p) = open_state_path() else { return HashMap::new() };
    fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .map(|m| m.into_iter().collect())
        .unwrap_or_default()
}

// ---------- events.jsonl 解析 ----------

fn read_head(path: &Path) -> String {
    let mut f = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    let mut buf = vec![0u8; HEAD_BYTES as usize];
    let n = f.read(&mut buf).unwrap_or(0);
    String::from_utf8_lossy(&buf[..n]).to_string()
}

fn read_tail(path: &Path) -> String {
    let mut f = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let off = len.saturating_sub(TAIL_BYTES);
    if f.seek(SeekFrom::Start(off)).is_err() {
        return String::new();
    }
    let mut s = String::new();
    let _ = f.read_to_string(&mut s);
    // 先頭は行途中の可能性があるので最初の改行まで捨てる
    if off > 0 {
        if let Some(i) = s.find('\n') {
            s = s[i + 1..].to_string();
        }
    }
    s
}

fn parse_lines(s: &str) -> Vec<Value> {
    s.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}

fn clip(s: &str, n: usize) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(n)
        .collect()
}

struct SessionInfo {
    title: Option<String>,
    cwd: Option<String>,
    model: Option<String>,
    start_time: Option<String>,
    turn: &'static str,
    last_note: Option<String>,
    failed_tools: i64,
}

// ターン状態判定（copilot-sessions.mmd）:
//   closed      = session.shutdown が末尾（正常終了）
//   answered    = assistant.message が末尾（報告済み＝次指示待ち）
//   working     = user/tool/model 系が末尾 かつ live
//   interrupted = 上記途中状態 かつ非 live
//   unknown     = 判定不能
fn analyze(dir: &Path, live: bool, working: bool) -> SessionInfo {
    let ev = dir.join("events.jsonl");
    let head = parse_lines(&read_head(&ev));
    let tail = parse_lines(&read_tail(&ev));

    let mut info = SessionInfo {
        title: None,
        cwd: None,
        model: None,
        start_time: None,
        turn: "unknown",
        last_note: None,
        failed_tools: 0,
    };
    for e in &head {
        match e.get("type").and_then(|v| v.as_str()) {
            Some("session.start") => {
                let d = e.get("data").cloned().unwrap_or(json!({}));
                info.cwd = d
                    .pointer("/context/cwd")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                info.model = d
                    .get("selectedModel")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                info.start_time = d
                    .get("startTime")
                    .and_then(|v| v.as_str())
                    .map(String::from);
            }
            Some("user.message") if info.title.is_none() => {
                info.title = e
                    .pointer("/data/content")
                    .and_then(|v| v.as_str())
                    .map(|s| clip(s, 60));
            }
            _ => {}
        }
        if info.title.is_some() && info.cwd.is_some() {
            break;
        }
    }

    for e in &tail {
        let t = e.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if t.starts_with("tool.execution_") {
            let d = e.get("data").cloned().unwrap_or(json!({}));
            let failed = d.get("success") == Some(&json!(false))
                || d.get("status") == Some(&json!("failed"))
                || d.get("error").map(|v| !v.is_null()).unwrap_or(false);
            if failed {
                info.failed_tools += 1;
            }
        }
    }
    // 最後の assistant.message を lastNote に（content か toolRequests の message）
    for e in tail.iter().rev() {
        if e.get("type").and_then(|v| v.as_str()) == Some("assistant.message") {
            let d = e.get("data").cloned().unwrap_or(json!({}));
            let content = d.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let note = if !content.trim().is_empty() {
                Some(clip(content, 160))
            } else {
                d.pointer("/toolRequests/0/arguments/message")
                    .or_else(|| d.pointer("/toolRequests/0/arguments/input"))
                    .and_then(|v| v.as_str())
                    .map(|s| clip(s, 160))
            };
            info.last_note = note;
            break;
        }
    }

    // 末尾の意味あるイベントで判定（usage/snapshot/選択系は読み飛ばす）
    const NOISE: &[&str] = &[
        "session.usage_record",
        "session.usage_checkpoint",
        "model.messages_snapshot",
        "model.captured_assignment_context",
        "session.auto_mode_resolved",
        "session.model_change",
        "subagent.selected",
        "session.start",
    ];
    let last_ty = tail
        .iter()
        .rev()
        .filter_map(|e| e.get("type").and_then(|v| v.as_str()))
        .find(|t| !NOISE.contains(t))
        .unwrap_or("");
    let mid_turn = matches!(
        last_ty,
        "user.message"
            | "assistant.turn_start"
            | "model.turn_started"
            | "model.model_call_started"
            | "model.model_call_success"
            | "model.response"
            | "tool.execution_start"
    ) || last_ty.starts_with("model.");
    info.turn = if last_ty == "session.shutdown" {
        "closed"
    } else if mid_turn {
        if live || working {
            "working"
        } else {
            "interrupted"
        }
    } else if matches!(last_ty, "session.error" | "abort") {
        "interrupted"
    } else if matches!(
        last_ty,
        "assistant.message" | "assistant.turn_end" | "model.turn_ended"
    ) {
        "answered"
    } else {
        "unknown"
    };
    info
}

fn dir_size(p: &Path) -> u64 {
    let mut n = 0u64;
    if let Ok(rd) = fs::read_dir(p) {
        for e in rd.flatten() {
            let m = e.metadata();
            if let Ok(m) = m {
                if m.is_dir() {
                    n += dir_size(&e.path());
                } else {
                    n += m.len();
                }
            }
        }
    }
    n
}

fn to_iso(t: std::time::SystemTime) -> Option<String> {
    let d: chrono::DateTime<chrono::Utc> = t.into();
    Some(d.to_rfc3339())
}

// ---------- 完了台帳（copilot-done.json） ----------

fn done_map() -> HashMap<String, Value> {
    fs::read_to_string(done_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("done")?.as_object().cloned())
        .map(|m| m.into_iter().collect())
        .unwrap_or_default()
}

fn save_done(map: &HashMap<String, Value>) -> std::io::Result<()> {
    let obj: serde_json::Map<String, Value> = map.clone().into_iter().collect();
    fs::write(
        done_path(),
        serde_json::to_string_pretty(&json!({"done": obj})).unwrap_or_default(),
    )
}

// ---------- セッション列挙 ----------

fn is_uuidish(name: &str) -> bool {
    name.len() == 36
        && name
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == '-')
}

fn read_sessions() -> Vec<Value> {
    let Some(dir) = state_dir() else { return vec![] };
    let opens = open_states();
    let done = done_map();
    let Ok(rd) = fs::read_dir(&dir) else { return vec![] };
    let mut out = vec![];
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if !p.is_dir() || !is_uuidish(&name) {
            continue;
        }
        let lock = lock_file(&name);
        let held = lock.as_deref()
            .map(|p| p.is_file() && lock_held(p))
            .unwrap_or(false);
        let open = opens.get(&name);
        let working = open
            .and_then(|v| v.get("working"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let live = held || open.is_some();
        let info = analyze(&p, live, working);
        let mtime = fs::metadata(p.join("events.jsonl"))
            .and_then(|m| m.modified())
            .ok()
            .and_then(to_iso)
            .or_else(|| {
                fs::metadata(&p).and_then(|m| m.modified()).ok().and_then(to_iso)
            });
        let mut badges = vec![];
        if let Some(m) = &info.model {
            badges.push(json!(m));
        }
        let mut v = json!({
            "id": name,
            "title": info.title,
            "workingDirectory": info.cwd,
            "lastActivityAt": mtime.or(info.start_time),
            "live": live,
            "working": working,
            "turn": info.turn,
            "lastNote": info.last_note,
            "failedTools": info.failed_tools,
            "badges": badges,
        });
        if let Some(d) = done.get(&name) {
            v["done"] = json!(true);
            if let Some(t) = d.get("doneAt") {
                v["doneAt"] = t.clone();
            }
        }
        out.push(v);
    }
    out
}

/// spawn_watchers の外部セッション監視が使う読み取り専用一覧
/// （id/lastActivityAt/live の差分検出に利用）
pub(crate) fn sessions_list() -> Vec<Value> {
    read_sessions()
}

// ---------- 孤立ファイル ----------

fn session_dir_ids() -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    if let Some(dir) = state_dir() {
        if let Ok(rd) = fs::read_dir(&dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if e.path().is_dir() && is_uuidish(&name) {
                    out.insert(name);
                }
            }
        }
    }
    out
}

fn orphan_locks(existing: &std::collections::HashSet<String>) -> Vec<PathBuf> {
    let mut out = vec![];
    let Some(dir) = oplocks_dir() else { return out };
    if let Ok(rd) = fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("lock") {
                continue;
            }
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if is_uuidish(stem) && !existing.contains(stem) && !lock_held(&p) {
                out.push(p);
            }
        }
    }
    out
}

// ---------- 応答 ----------

fn jr(v: Value) -> Response {
    (StatusCode::OK, Json(v)).into_response()
}
fn je(msg: &str) -> Response {
    jr(json!({"ok": false, "error": msg}))
}

fn overview() -> Value {
    let Some(dir) = state_dir() else {
        return json!({"ok": false, "installed": false, "error": "~/.copilot がありません"});
    };
    if !dir.is_dir() {
        return json!({"ok": false, "installed": false, "error": "session-state がありません"});
    }
    let sessions = read_sessions();
    let existing = session_dir_ids();
    let locks = orphan_locks(&existing);
    // dir 無しの open-state エントリ数
    let orphan_states = open_states()
        .keys()
        .filter(|k| !existing.contains(*k))
        .count();
    let live = sessions
        .iter()
        .filter(|s| s.get("live").and_then(|v| v.as_bool()).unwrap_or(false))
        .count();
    let bytes: u64 = fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| dir_size(&e.path()))
                .sum()
        })
        .unwrap_or(0);
    json!({
        "ok": true,
        "agent": "copilot",
        "dir": dir.to_string_lossy(),
        "sessions": sessions,
        "counts": {
            "sessions": existing.len(),
            "live": live,
            "sleeping": existing.len().saturating_sub(live),
            "orphanLocks": locks.len(),
            "orphanStates": orphan_states,
        },
        "store": { "bytes": bytes },
        "caps": { "vacuum": false, "done": true },
    })
}

// ---------- API ----------

pub async fn api_list() -> Response {
    conlog("env", "api", "GET /api/copilot-sessions");
    jr(overview())
}

pub async fn api_delete(b: Bytes) -> Response {
    let v: Value = serde_json::from_slice(&b).unwrap_or(json!({}));
    let ids: Vec<String> = v
        .get("ids")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    if ids.is_empty() {
        return je("ids required");
    }
    conlog("env", "api", &format!("delete copilot sessions: {:?}", ids));
    let opens = open_states();
    let mut deleted = vec![];
    let mut skipped = vec![];
    let mut done = done_map();
    for id in &ids {
        if !is_uuidish(id) {
            skipped.push(json!({"id": id, "reason": "invalid id"}));
            continue;
        }
        let held = lock_file(id)
            .map(|p| p.is_file() && lock_held(&p))
            .unwrap_or(false);
        if held || opens.contains_key(id) {
            skipped.push(json!({"id": id, "reason": "live"}));
            continue;
        }
        let Some(dir) = state_dir() else { continue };
        let p = dir.join(id);
        match fs::remove_dir_all(&p) {
            Ok(_) => {
                if let Some(l) = lock_file(id) {
                    let _ = fs::remove_file(l);
                }
                done.remove(id);
                deleted.push(id.clone());
            }
            Err(e) => skipped.push(json!({"id": id, "reason": e.to_string()})),
        }
    }
    let _ = save_done(&done);
    jr(json!({"ok": true, "deleted": deleted, "skipped": skipped}))
}

pub async fn api_cleanup() -> Response {
    conlog("env", "api", "cleanup copilot sessions");
    let existing = session_dir_ids();
    let locks = orphan_locks(&existing);
    let mut removed_locks = 0;
    for p in &locks {
        if fs::remove_file(p).is_ok() {
            removed_locks += 1;
        }
    }
    // dir 無しの open-state エントリを除去
    let mut removed_states = 0;
    if let Some(p) = open_state_path() {
        if let Ok(s) = fs::read_to_string(&p) {
            if let Ok(mut v) = serde_json::from_str::<Value>(&s) {
                if let Some(m) = v.as_object_mut() {
                    let before = m.len();
                    m.retain(|k, _| existing.contains(k));
                    removed_states = before - m.len();
                    if removed_states > 0 {
                        let _ = fs::write(&p, serde_json::to_string_pretty(&v).unwrap_or_default());
                    }
                }
            }
        }
    }
    jr(json!({
        "ok": true,
        "removedLocks": removed_locks,
        "removedStates": removed_states,
    }))
}

fn set_done(b: &Bytes, mark: bool) -> Response {
    let v: Value = serde_json::from_slice(b).unwrap_or(json!({}));
    let ids: Vec<String> = v
        .get("ids")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    if ids.is_empty() {
        return je("ids required");
    }
    let opens = open_states();
    let mut map = done_map();
    let mut done = vec![];
    let mut skipped = vec![];
    for id in &ids {
        if mark {
            let held = lock_file(id)
            .map(|p| p.is_file() && lock_held(&p))
            .unwrap_or(false);
            if held || opens.contains_key(id) {
                skipped.push(json!({"id": id, "reason": "live"}));
                continue;
            }
            map.insert(
                id.clone(),
                json!({"doneAt": chrono::Utc::now().to_rfc3339()}),
            );
        } else {
            map.remove(id);
        }
        done.push(id.clone());
    }
    let _ = save_done(&map);
    jr(json!({"ok": true, "done": done, "skipped": skipped}))
}

pub async fn api_done(b: Bytes) -> Response {
    set_done(&b, true)
}
pub async fn api_undone(b: Bytes) -> Response {
    set_done(&b, false)
}


fn is_live(id: &str) -> bool {
    // lock ファイルが無いなら保持者もいない（ファイル不在を「保持中」と
    // 誤判定しないよう存在確認してから LockFile 試行する）
    let held = lock_file(id)
        .map(|p| p.is_file() && lock_held(&p))
        .unwrap_or(false);
    held || open_states().contains_key(id)
}

fn session_dir(id: &str) -> Option<PathBuf> {
    let d = state_dir()?.join(id);
    d.is_dir().then_some(d)
}

// events.jsonl を走査して会話イベントを抽出する
// （中央ペインのトランスクリプト展開用）。
// user.message → user / assistant.message → agent /
// tool.execution_start → tool（toolName+toolTitle）。他イベントは除外
fn session_messages(dir: &Path) -> Vec<Value> {
    let p = dir.join("events.jsonl");
    let Ok(body) = fs::read_to_string(&p) else {
        return vec![];
    };
    let mut msgs = Vec::new();
    for l in body.lines() {
        let Ok(e) = serde_json::from_str::<Value>(l) else { continue };
        let ty = e.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let ts = e.get("timestamp").cloned().unwrap_or(Value::Null);
        let d = e.get("data").cloned().unwrap_or(json!({}));
        match ty {
            "user.message" => {
                let text = d.get("content").and_then(|v| v.as_str()).unwrap_or("");
                if !text.trim().is_empty() {
                    msgs.push(json!({"role": "user", "text": text, "ts": ts}));
                }
            }
            "assistant.message" => {
                let text = d.get("content").and_then(|v| v.as_str()).unwrap_or("");
                if !text.trim().is_empty() {
                    msgs.push(json!({"role": "agent", "text": text, "ts": ts}));
                }
                if let Some(reqs) = d.get("toolRequests").and_then(|v| v.as_array()) {
                    for rq in reqs {
                        let name = rq.get("name").and_then(|v| v.as_str())
                            .unwrap_or("tool");
                        let sum = rq.get("intentionSummary")
                            .and_then(|v| v.as_str()).unwrap_or("");
                        msgs.push(json!({
                            "role": "tool",
                            "text": if sum.is_empty() { name.to_string() }
                                    else { format!("{} {}", name, clip(sum, 80)) },
                            "ts": ts,
                        }));
                    }
                }
            }
            "tool.execution_start" => {
                let name = d.get("toolName").and_then(|v| v.as_str())
                    .unwrap_or("tool");
                let title = d.get("toolTitle").and_then(|v| v.as_str())
                    .unwrap_or("");
                let text = if title.is_empty() { name.to_string() }
                    else { format!("{}（{}）", name, title) };
                msgs.push(json!({"role": "tool", "text": text, "ts": ts}));
            }
            _ => {}
        }
    }
    msgs
}

pub async fn api_messages(b: Bytes) -> Response {
    let v: Value = serde_json::from_slice(&b).unwrap_or(json!({}));
    let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("");
    if !is_uuidish(id) {
        return je("id required");
    }
    let Some(dir) = session_dir(id) else { return je("session not found") };
    jr(json!({"ok": true, "messages": session_messages(&dir)}))
}

/// kasugai chat へのアタッチ情報（control の /attach が使う）。
/// {id,title,cwd,live,messages[]}。存在しない id は None
pub(crate) fn attach_info(id: &str) -> Option<Value> {
    let dir = session_dir(id)?;
    let info = analyze(&dir, false, false);
    Some(json!({
        "id": id,
        "title": info.title.clone().unwrap_or_else(|| id.to_string()),
        "cwd": info.cwd.clone().unwrap_or_default(),
        "live": is_live(id),
        "messages": session_messages(&dir),
    }))
}


