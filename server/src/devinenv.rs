// devinenv.rs — Devin CLI/Desktop のローカルセッション管理（環境維持）。
// README「Devin セッション管理」・diagrams/devin-sessions.mmd が正本。
//
// 対象は %APPDATA%\Devin\cli\ 配下:
//   sessions.db        … セッション台帳（SQLite/WAL）
//   session_locks/     … <id>.lock（生存プロセスがバイトロックを保持）
//   summaries/         … <id>.md（セッション要約）
//
// live 判定は <id>.lock への LockFile 試行で行う（開いているプロセスが
// バイトロックを握っていると試行が失敗する＝live）。live 中のセッションは
// 削除を拒否する。削除は sessions.db の行削除で行い、message_nodes の
// AFTER DELETE トリガーが FTS(message_search) を連動整理する。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use axum::{
    body::Bytes,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

use crate::conlog::conlog;

// ---------- パス ----------

fn cli_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("Devin").join("cli"))
}
fn db_path() -> Option<PathBuf> {
    cli_dir().map(|d| d.join("sessions.db"))
}
fn locks_dir() -> Option<PathBuf> {
    cli_dir().map(|d| d.join("session_locks"))
}
fn summaries_dir() -> Option<PathBuf> {
    cli_dir().map(|d| d.join("summaries"))
}
fn lock_file(id: &str) -> Option<PathBuf> {
    locks_dir().map(|d| d.join(format!("{id}.lock")))
}
fn summary_file(id: &str) -> Option<PathBuf> {
    summaries_dir().map(|d| d.join(format!("{id}.md")))
}

// ---------- live 判定（ロック生存試行） ----------

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

fn live_ids() -> HashSet<String> {
    let mut out = HashSet::new();
    let Some(dir) = locks_dir() else { return out };
    let Ok(rd) = fs::read_dir(&dir) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("lock") {
            continue;
        }
        if lock_held(&p) {
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                out.insert(stem.to_string());
            }
        }
    }
    out
}

// ---------- DB ----------

const SESSION_TABLES: &[&str] = &[
    "message_nodes",
    "tool_call_state",
    "live_row_times",
    "prompt_history",
    "rendered_commits",
    "subagent_heads",
];

fn open_db() -> Result<rusqlite::Connection, rusqlite::Error> {
    let p = db_path().unwrap_or_default();
    let c = rusqlite::Connection::open(&p)?;
    c.busy_timeout(std::time::Duration::from_secs(10))?;
    Ok(c)
}

fn table_exists(c: &rusqlite::Connection, name: &str) -> bool {
    c.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}

fn session_ids(c: &rusqlite::Connection) -> HashSet<String> {
    c.prepare("SELECT id FROM sessions")
        .and_then(|mut s| {
            let rows = s.query_map([], |r| r.get::<_, String>(0))?;
            Ok(rows.flatten().collect())
        })
        .unwrap_or_default()
}

/// detector（session-watch / unattributed-work）からの再利用用スナップショット。
/// 環境ビューと同じ読取筋（sessions.db + lock live 判定）を共有する
pub(crate) fn sessions_snapshot() -> Vec<Value> {
    read_sessions()
}

/// message_nodes の最終ノードからターン状態を導出する（diagrams/devin-sessions.mmd）。
/// answered    = 応答完了（最終が assistant かつ finish=stop。指示への報告済み＝次指示待ち）
/// working     = 実行中（最終が user、または tool_calls 途中。live のみ）
/// interrupted = 中断（割込マーカー / finish=length 系 / 非 live の途中状態）
/// unknown     = ノード無し・判定不能
/// lastNote は最終 assistant 応答の冒頭（報告内容を人が確認するための抜粋）、
/// failedTools は tool_call_state の失敗数（警告指標）。
fn turn_state(c: &rusqlite::Connection, sid: &str, live: bool) -> Value {
    // 末尾数行を遡る: 末尾の system ノード（割込マーカー以外）はターン状態を
    // 変えない通知類なので、最初に出る user/tool/assistant ノードで判定する。
    // 割込マーカーの system ノードはそれ自体が中断を表す
    let mut st = match c.prepare(
        "SELECT chat_message FROM message_nodes WHERE session_id=?1 \
         ORDER BY row_id DESC LIMIT 8",
    ) {
        Ok(s) => s,
        Err(_) => return json!({"turn": "unknown", "lastNote": null, "failedTools": 0}),
    };
    let msgs: Vec<Value> = st
        .query_map([sid], |r| r.get::<_, String>(0))
        .map(|it| {
            it.flatten()
                .filter_map(|m| serde_json::from_str(&m).ok())
                .collect()
        })
        .unwrap_or_default();
    let mut turn = "unknown";
    for msg in &msgs {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let finish = msg
            .pointer("/metadata/finish_reason")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
        if content.contains("[Response interrupted")
            || matches!(finish, "length" | "max_tokens" | "content_filter")
        {
            turn = "interrupted";
            break;
        }
        if role == "system" {
            continue; // 通知類は読み飛ばして手前のノードで判定
        }
        turn = if role == "assistant" && finish == "stop" {
            "answered"
        } else if live {
            "working"
        } else {
            "interrupted"
        };
        break;
    }
    let note: Option<String> = c
        .query_row(
            "SELECT chat_message FROM message_nodes WHERE session_id=?1 \
             AND json_extract(chat_message,'$.role')='assistant' \
             AND length(json_extract(chat_message,'$.content')) > 0 \
             ORDER BY row_id DESC LIMIT 1",
            [sid],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|m| serde_json::from_str::<Value>(&m).ok())
        .and_then(|m| {
            m.get("content")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .map(|s| {
            s.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(160)
                .collect()
        });
    let failed: i64 = if table_exists(c, "tool_call_state") {
        c.query_row(
            "SELECT COUNT(*) FROM tool_call_state WHERE session_id=?1 \
             AND json_extract(tool_call_update_json,'$.status')='failed'",
            [sid],
            |r| r.get(0),
        )
        .unwrap_or(0)
    } else {
        0
    };
    json!({"turn": turn, "lastNote": note, "failedTools": failed})
}

fn read_sessions() -> Vec<Value> {
    let live = live_ids();
    let Ok(c) = open_db() else { return vec![] };
    let has_nodes = table_exists(&c, "message_nodes");
    let mut st = match c.prepare(
        "SELECT id, working_directory, title, last_activity_at, hidden FROM sessions",
    ) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    let rows = st
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })
        .map(|it| it.flatten().collect::<Vec<_>>())
        .unwrap_or_default();
    rows.iter()
        .map(|(id, wd, title, ts, hidden)| {
            let live_f = live.contains(id);
            let mut v = json!({
                "id": id,
                "title": title,
                "workingDirectory": wd,
                "lastActivityAt": ts
                    .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                    .map(|d| d.to_rfc3339()),
                "hidden": *hidden != 0,
                "live": live_f,
            });
            if has_nodes {
                if let Some(obj) = v.as_object_mut() {
                    for (k, val) in turn_state(&c, id, live_f)
                        .as_object()
                        .map(|o| o.clone())
                        .unwrap_or_default()
                    {
                        obj.insert(k, val);
                    }
                }
            }
            v
        })
        .collect()
}

/// spawn_watchers の外部セッション監視が使う読み取り専用一覧
/// （id/lastActivityAt/live の差分検出に利用）
pub(crate) fn sessions_list() -> Vec<Value> {
    read_sessions()
}

// ---------- 孤立ファイル ----------

fn orphan_locks(existing: &HashSet<String>) -> Vec<PathBuf> {
    let mut out = vec![];
    let Some(dir) = locks_dir() else { return out };
    if let Ok(rd) = fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("lock") {
                continue;
            }
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if !existing.contains(stem) && !lock_held(&p) {
                out.push(p);
            }
        }
    }
    out
}

// セッション由来の summary は <adjective>-<noun>.md 形のみ対象にする
// （history_* 等の別用途ファイルは残す）
fn is_session_name(stem: &str) -> bool {
    let mut parts = stem.split('-');
    matches!(parts.next(), Some(a) if !a.is_empty() && a.chars().all(|c| c.is_ascii_lowercase()))
        && matches!(parts.next(), Some(b) if !b.is_empty() && b.chars().all(|c| c.is_ascii_lowercase()))
        && parts.next().is_none()
}

fn orphan_summaries(existing: &HashSet<String>) -> Vec<PathBuf> {
    let mut out = vec![];
    let Some(dir) = summaries_dir() else { return out };
    if let Ok(rd) = fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if is_session_name(stem) && !existing.contains(stem) {
                out.push(p);
            }
        }
    }
    out
}

// ---------- 応答 ----------

fn jr(v: Value) -> Response {
    Json(v).into_response()
}
fn je(msg: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"ok": false, "error": msg})),
    )
        .into_response()
}

fn fsize(p: &Path) -> u64 {
    fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

fn overview() -> Value {
    let Some(db) = db_path() else {
        return json!({"ok": false, "error": "Devin cli dir not found"});
    };
    if !db.exists() {
        return json!({"ok": true, "installed": false, "dir": cli_dir()});
    }
    let sessions = read_sessions();
    let existing: HashSet<String> = sessions
        .iter()
        .filter_map(|s| s.get("id").and_then(|v| v.as_str()).map(String::from))
        .collect();
    let live_n = sessions.iter().filter(|s| s["live"] == true).count();
    let wal = db.with_file_name("sessions.db-wal");
    json!({
        "ok": true,
        "installed": true,
        "dir": cli_dir(),
        "sessions": sessions,
        "counts": {
            "sessions": existing.len(),
            "live": live_n,
            "sleeping": existing.len() - live_n,
            "orphanLocks": orphan_locks(&existing).len(),
            "orphanSummaries": orphan_summaries(&existing).len(),
        },
        "db": { "bytes": fsize(&db), "walBytes": fsize(&wal) },
    })
}

// ---------- API ----------

pub async fn api_list() -> Response {
    jr(overview())
}

pub async fn api_delete(b: Bytes) -> Response {
    let a: Value = serde_json::from_slice(&b).unwrap_or(json!({}));
    let ids: Vec<String> = a
        .get("ids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() {
        return jr(json!({"ok": false, "error": "ids required"}));
    }
    let live = live_ids();
    let (skip, targets): (Vec<String>, Vec<String>) =
        ids.into_iter().partition(|id| live.contains(id));
    if targets.is_empty() {
        return jr(json!({"ok": true, "deleted": 0, "skipped": skip}));
    }
    let Ok(mut c) = open_db() else {
        return je("sessions.db open failed");
    };
    // 存在するテーブルだけ対象にする（バージョン差で欠けることがある）
    let ph = targets.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let mut total = 0usize;
    let res = (|| -> Result<(), rusqlite::Error> {
        let tx = c.transaction()?;
        for t in SESSION_TABLES {
            if table_exists(&tx, t) {
                tx.execute(
                    &format!("DELETE FROM {t} WHERE session_id IN ({ph})"),
                    rusqlite::params_from_iter(targets.iter()),
                )?;
            }
        }
        total = tx.execute(
            &format!("DELETE FROM sessions WHERE id IN ({ph})"),
            rusqlite::params_from_iter(targets.iter()),
        )?;
        tx.commit()
    })();
    if let Err(e) = res {
        return je(&format!("delete failed: {e}"));
    }
    // 対象セッションの lock/summary ファイルも除去
    let mut files = 0usize;
    for id in &targets {
        for p in [lock_file(id), summary_file(id)].into_iter().flatten() {
            if p.exists() && fs::remove_file(&p).is_ok() {
                files += 1;
            }
        }
    }
    conlog("devinenv", "sys", &format!("sessions deleted: {total} (skipped live: {})", skip.len()));
    jr(json!({"ok": true, "deleted": total, "filesRemoved": files, "skipped": skip}))
}

pub async fn api_cleanup() -> Response {
    let existing = {
        let Ok(c) = open_db() else {
            return je("sessions.db open failed");
        };
        session_ids(&c)
    };
    let locks = orphan_locks(&existing);
    let sums = orphan_summaries(&existing);
    let (mut rl, mut rs) = (0usize, 0usize);
    for p in &locks {
        if fs::remove_file(p).is_ok() {
            rl += 1;
        }
    }
    for p in &sums {
        if fs::remove_file(p).is_ok() {
            rs += 1;
        }
    }
    conlog("devinenv", "sys", &format!("cleanup: locks={rl} summaries={rs}"));
    jr(json!({"ok": true, "locksRemoved": rl, "summariesRemoved": rs}))
}

pub async fn api_vacuum() -> Response {
    let Some(db) = db_path() else {
        return jr(json!({"ok": false, "error": "Devin cli dir not found"}));
    };
    if !db.exists() {
        return jr(json!({"ok": false, "error": "sessions.db not found"}));
    }
    let before = fsize(&db);
    let Ok(c) = open_db() else {
        return je("sessions.db open failed");
    };
    match c.execute("VACUUM", []) {
        Ok(_) => {
            let after = fsize(&db);
            conlog("devinenv", "sys", &format!("vacuum: {before} -> {after} bytes"));
            jr(json!({"ok": true, "bytesBefore": before, "bytesAfter": after}))
        }
        Err(e) => je(&format!("vacuum failed: {e}")),
    }
}


fn parse_id(b: &Bytes) -> Option<String> {
    serde_json::from_slice::<Value>(b)
        .ok()
        .and_then(|v| v.get("id").and_then(|x| x.as_str()).map(String::from))
        .filter(|s| is_session_name(s))
}

// message_nodes を row_id 順に読み、user/assistant の本文を
// チャット形式で抽出する。system・空本文・ツール呼出のみのノードは除外
fn session_messages(id: &str) -> Vec<Value> {
    let Ok(c) = open_db() else { return vec![] };
    let Ok(mut st) = c.prepare(
        "SELECT chat_message, created_at FROM message_nodes          WHERE session_id=?1 ORDER BY row_id",
    ) else { return vec![] };
    let rows = match st.query_map([id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    }) {
        Ok(r) => r,
        Err(_) => return vec![],
    };
    let mut msgs = Vec::new();
    let ts_of = |t: i64| chrono::DateTime::from_timestamp(t, 0).map(|d| d.to_rfc3339());
    // message_nodes は編集/枝分かれで同じ本文のノードを持ち得るので
    // 直前と同じ role+text は重複として読み飛ばす
    let push = |msgs: &mut Vec<Value>, role: &str, text: String, t: i64| {
        if text.trim().is_empty() {
            return;
        }
        if msgs.last().map(|m: &Value| m["role"] == role && m["text"] == text)
            == Some(true)
        {
            return;
        }
        msgs.push(json!({"role": role, "text": text, "ts": ts_of(t)}));
    };
    for r in rows.flatten() {
        let Ok(m) = serde_json::from_str::<Value>(&r.0) else { continue };
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let text = m.get("content").and_then(|v| v.as_str()).unwrap_or("");
        match role {
            "user" => push(&mut msgs, "user", text.to_string(), r.1),
            "assistant" => {
                // thinking → thought / 本文 → agent / tool_calls → tool
                if let Some(tk) = m
                    .get("thinking")
                    .and_then(|t| t.get("thinking"))
                    .and_then(|v| v.as_str())
                {
                    push(&mut msgs, "thought", tk.to_string(), r.1);
                }
                push(&mut msgs, "agent", text.to_string(), r.1);
                for tc in m.get("tool_calls").and_then(|v| v.as_array()).cloned()
                    .unwrap_or_default()
                {
                    let name = tc.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
                    let args = tc.get("arguments").map(|a| {
                        let s = if a.is_string() {
                            a.as_str().unwrap_or("").to_string()
                        } else {
                            a.to_string()
                        };
                        s.chars().take(160).collect::<String>()
                    });
                    let line = match args.filter(|a| !a.is_empty()) {
                        Some(a) => format!("{} {}", name, a),
                        None => name.to_string(),
                    };
                    push(&mut msgs, "tool", line, r.1);
                }
            }
            _ => {} // system/tool 結果行は出さない（結果本文は巨大なため call 行のみ表示）
        }
    }
    msgs
}

pub async fn api_messages(b: Bytes) -> Response {
    let Some(id) = parse_id(&b) else { return je("id required") };
    jr(json!({"ok": true, "messages": session_messages(&id)}))
}

/// kasugai chat へのアタッチ情報（control の /attach が使う）。
/// {id,title,cwd,messages[]}。存在しない id は None
pub(crate) fn attach_info(id: &str) -> Option<Value> {
    let s = read_sessions().into_iter().find(|s| s["id"] == id)?;
    Some(json!({
        "id": id,
        "title": s["title"].as_str().unwrap_or(id),
        "cwd": s["workingDirectory"].as_str().unwrap_or(""),
        "live": s["live"].as_bool().unwrap_or(false),
        "messages": session_messages(id),
    }))
}


