// assets.rs — CDN 配布アセットの取得・検証・適用（README §13 / diagrams/asset-update.mmd 正本）
//
// モデル: cdnBaseUrl の manifest.json を取得し、チャネル(panel/docs/...)ごとの
// remote version と installed(assets/state.json)を比較。差分があれば
// .staging/<ch>/ に全ファイル DL → 各 sha256 検証 → assets/<ch>/ を差替
// （現行は <ch>.bak/ へ退避し rollback で復帰可）。serve_static は
// assets/<ch>/ を panel/docs より優先して配信する（オーバーレイ）。
// prompts など行動系は assetAutoApply.<ch>=false で proposals 人ゲートへ。
// 原則: CDN をランタイムに直接配信させない（検証 → ローカル配置 → ローカル配信）
use axum::{
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use crate::conlog::conlog;
use crate::config::*;
use crate::control::{Shared, VERSION};
use crate::proposals;
use crate::util::*;

/// 実行時状態（再起動で remote は揮発。次回 check で再取得）
#[derive(Default)]
pub struct Mgr {
    pub checking: bool,
    pub last_check_at: Option<String>,
    pub last_error: Option<String>,
    pub remote: Map<String, Value>, // ch -> {version, files:{rel: sha256}}
    pub apply_errors: Map<String, Value>, // ch -> 直近の適用/起票エラー
    pub min_exe: Option<String>,    // manifest.minExeVersion
    pub needs_exe: Option<String>,  // 互換不足で適用保留した minExeVersion
}

impl Mgr {
    pub fn new() -> Self {
        Self::default()
    }
}

// ---------- パス ----------
pub fn dir(root: &Path) -> PathBuf {
    root.join("assets")
}
fn state_path(root: &Path) -> PathBuf {
    dir(root).join("state.json")
}
fn gated_path(root: &Path) -> PathBuf {
    dir(root).join("gated.json")
}
fn staging_dir(root: &Path, ch: &str) -> PathBuf {
    dir(root).join(".staging").join(ch)
}
fn live_dir(root: &Path, ch: &str) -> PathBuf {
    dir(root).join(ch)
}
fn bak_dir(root: &Path, ch: &str) -> PathBuf {
    dir(root).join(format!("{}.bak", ch))
}
fn load_state(root: &Path) -> Value {
    load_j(&state_path(root), json!({}))
}
fn save_state(root: &Path, v: &Value) {
    let _ = std::fs::create_dir_all(dir(root));
    save_j(&state_path(root), v);
}

/// 静的配信のオーバーレイ解決。assets/<ch>/<rel> が実ファイルなら返す
pub fn overlay_file(root: &Path, ch: &str, rel: &str) -> Option<PathBuf> {
    let base = live_dir(root, ch);
    let base_n = norm_slashes(&base.to_string_lossy());
    let f = normalize(&format!("{}/{}", base_n, rel));
    if !(f.starts_with(&base_n) || f.to_lowercase().starts_with(&base_n.to_lowercase())) {
        return None;
    }
    let p = PathBuf::from(f);
    p.is_file().then_some(p)
}

/// assetAutoApply.<ch>。未定義は prompts=false（人ゲート）・他=true（自動適用）
fn auto_apply(cfg: &Value, ch: &str) -> bool {
    cfg.pointer(&format!("/assetAutoApply/{}", ch))
        .and_then(|v| v.as_bool())
        .unwrap_or(ch != "prompts")
}

fn sha256_hex(b: &[u8]) -> String {
    format!("{:x}", Sha256::digest(b))
}

/// semver 比較（a.b.c の数値列・余分なセグメントは0埋め）
fn semver_ge(have: &str, need: &str) -> bool {
    let p = |s: &str| -> Vec<u64> {
        s.split('.')
            .map(|x| x.trim().parse().unwrap_or(0))
            .collect()
    };
    let (h, n) = (p(have), p(need));
    for i in 0..n.len().max(h.len()) {
        let (a, b) = (h.get(i).copied().unwrap_or(0), n.get(i).copied().unwrap_or(0));
        if a != b {
            return a > b;
        }
    }
    true
}

async fn fetch(http: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    let r = http.get(url).send().await.map_err(|e| e.to_string())?;
    if !r.status().is_success() {
        return Err(format!("GET {} → {}", url, r.status()));
    }
    r.bytes().await.map(|b| b.to_vec()).map_err(|e| e.to_string())
}

fn installed(st: &Value, ch: &str) -> Option<String> {
    st.pointer(&format!("/channels/{}/version", ch))
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// API 表示・SSE push 用の現在状態
pub async fn status(app: &Shared) -> Value {
    let cfg = app.cfg().await;
    let st = load_state(&app.root);
    let g = load_j(&gated_path(&app.root), json!({}));
    let m = app.assets.lock().await;
    let mut names: Vec<String> = m.remote.keys().cloned().collect();
    if let Some(obj) = st.get("channels").and_then(|v| v.as_object()) {
        for k in obj.keys() {
            if !names.contains(k) {
                names.push(k.clone());
            }
        }
    }
    names.sort();
    let mut channels = Map::new();
    for ch in names {
        let inst = installed(&st, &ch);
        let rem = m
            .remote
            .get(&ch)
            .and_then(|c| c.get("version"))
            .and_then(|v| v.as_str())
            .map(String::from);
        channels.insert(
            ch.clone(),
            json!({
                "installed": inst,
                "prevVersion": st.pointer(&format!("/channels/{}/prevVersion", ch)).cloned().unwrap_or(Value::Null),
                "held": st.pointer(&format!("/channels/{}/held", ch)).cloned().unwrap_or(Value::Null),
                "hasBackup": bak_dir(&app.root, &ch).is_dir(),
                "latest": rem,
                "update": rem.is_some() && rem.as_deref() != inst.as_deref(),
                "autoApply": auto_apply(&cfg, &ch),
                "error": m.apply_errors.get(&ch).cloned().unwrap_or(Value::Null),
                "gatedProposal": g.get(&ch).cloned().unwrap_or(Value::Null),
            }),
        );
    }
    json!({
        "ok": true,
        "cdnBaseUrl": cfg_str(&cfg, "cdnBaseUrl").unwrap_or(""),
        "checkMinutes": cfg_f64(&cfg, "cdnCheckMinutes", 0.0),
        "checking": m.checking,
        "lastCheckAt": m.last_check_at,
        "lastError": m.last_error,
        "exeVersion": VERSION,
        "needsExe": m.needs_exe,
        "channels": Value::Object(channels),
    })
}

/// 更新確認の入口（起動時・定期・POST /api/assets/check 共通）。
/// apply_auto=true で autoApply チャネルを自動適用・gated は proposal 起票
pub async fn check(app: &Shared, apply_auto: bool) {
    {
        let mut m = app.assets.lock().await;
        if m.checking {
            return;
        }
        m.checking = true;
    }
    let res = run(app, apply_auto).await;
    {
        let mut m = app.assets.lock().await;
        m.checking = false;
        m.last_check_at = Some(now_iso());
        m.last_error = res.err();
    }
    app.sse("assets", &status(app).await);
}

async fn run(app: &Shared, apply_auto: bool) -> Result<(), String> {
    let cfg = app.cfg().await;
    let base = cfg_str(&cfg, "cdnBaseUrl")
        .unwrap_or("")
        .trim_end_matches('/')
        .to_string();
    if base.is_empty() {
        return Ok(());
    }
    let man_bytes = fetch(&app.http, &format!("{}/manifest.json", base)).await?;
    let pin = cfg_str(&cfg, "cdnManifestSha256").unwrap_or("");
    if !pin.is_empty() && sha256_hex(&man_bytes) != pin.to_lowercase() {
        return Err("manifest.json sha256 mismatch".into());
    }
    let man: Value = serde_json::from_slice(&man_bytes).map_err(|e| e.to_string())?;
    let min_exe = man
        .get("minExeVersion")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let channels = man
        .get("channels")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    {
        let mut m = app.assets.lock().await;
        m.remote = channels.clone();
        m.min_exe = (!min_exe.is_empty()).then_some(min_exe.clone());
        m.needs_exe = (!min_exe.is_empty() && !semver_ge(VERSION, &min_exe)).then_some(min_exe.clone());
    }
    if !min_exe.is_empty() && !semver_ge(VERSION, &min_exe) {
        conlog(
            "control",
            "sys",
            &format!("assets: exe {} < minExeVersion {} → 適用保留", VERSION, min_exe),
        );
        return Ok(());
    }
    if !apply_auto {
        return Ok(());
    }
    let st = load_state(&app.root);
    for (ch, cm) in &channels {
        let remote_v = cm.get("version").and_then(|v| v.as_str()).unwrap_or("");
        if remote_v.is_empty() || installed(&st, ch).as_deref() == Some(remote_v) {
            continue;
        }
        // rollback した版は held: 自動適用を抑制（より新しい版が来れば再開）
        let held = st
            .pointer(&format!("/channels/{}/held", ch))
            .and_then(|v| v.as_str());
        let mut ch_err: Option<String> = None;
        if auto_apply(&cfg, ch) && held != Some(remote_v) {
            match apply_channel(app, &base, ch, cm).await {
                Ok(v) => conlog("control", "sys", &format!("assets: {} を {} へ更新", ch, v)),
                Err(e) => {
                    conlog("control", "err", &format!("assets: {} 適用失敗: {}", ch, e));
                    ch_err = Some(e);
                }
            }
        } else if !auto_apply(&cfg, ch) {
            match ensure_proposal(app, &base, ch, cm).await {
                Ok(Some(pid)) => conlog(
                    "control",
                    "sys",
                    &format!("assets: {} {} → proposal {} 起票（人ゲート）", ch, remote_v, pid),
                ),
                Ok(None) => {}
                Err(e) => {
                    conlog("control", "err", &format!("assets: {} proposal 起票失敗: {}", ch, e));
                    ch_err = Some(e);
                }
            }
        }
        let mut m = app.assets.lock().await;
        match ch_err {
            Some(e) => {
                m.apply_errors.insert(ch.clone(), json!(e));
            }
            None => {
                m.apply_errors.remove(ch);
            }
        }
    }
    Ok(())
}

/// チャネルの staging DL → sha256 検証 → assets/<ch>/ 差替。戻り値=適用版
async fn apply_channel(app: &Shared, base: &str, ch: &str, cm: &Value) -> Result<String, String> {
    let version = cm
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let files = cm
        .get("files")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    if version.is_empty() || files.is_empty() {
        return Err("version/files empty".into());
    }
    let stg = staging_dir(&app.root, ch);
    let _ = std::fs::remove_dir_all(&stg);
    std::fs::create_dir_all(&stg).map_err(|e| e.to_string())?;
    let res = async {
        for (rel, sha) in &files {
            let expect = sha.as_str().unwrap_or("").to_lowercase();
            let data = fetch(&app.http, &format!("{}/{}/{}", base, ch, rel)).await?;
            if sha256_hex(&data) != expect {
                return Err(format!("{} sha256 mismatch", rel));
            }
            let fp = stg.join(rel);
            if let Some(d) = fp.parent() {
                std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
            }
            std::fs::write(&fp, &data).map_err(|e| e.to_string())?;
        }
        Ok::<(), String>(())
    }
    .await;
    if let Err(e) = res {
        let _ = std::fs::remove_dir_all(&stg);
        return Err(e);
    }
    // swap: live → bak、staging → live
    let live = live_dir(&app.root, ch);
    let bak = bak_dir(&app.root, ch);
    let mut st = load_state(&app.root);
    let prev = installed(&st, ch);
    if live.exists() {
        let _ = std::fs::remove_dir_all(&bak);
        std::fs::rename(&live, &bak).map_err(|e| e.to_string())?;
    }
    if let Err(e) = std::fs::rename(&stg, &live) {
        if bak.exists() {
            let _ = std::fs::rename(&bak, &live);
        }
        return Err(e.to_string());
    }
    st["channels"][ch] = json!({"version": version, "prevVersion": prev, "held": Value::Null, "appliedAt": now_iso()});
    save_state(&app.root, &st);
    Ok(st["channels"][ch]["version"].as_str().unwrap_or("").to_string())
}

/// gated チャネル: 同 version の起票済がなければ proposal を作る（人ゲート）
async fn ensure_proposal(
    app: &Shared,
    base: &str,
    ch: &str,
    cm: &Value,
) -> Result<Option<String>, String> {
    let version = cm.get("version").and_then(|v| v.as_str()).unwrap_or("");
    let gp = gated_path(&app.root);
    let mut g = load_j(&gp, json!({}));
    if g.pointer(&format!("/{}/version", ch)).and_then(|v| v.as_str()) == Some(version) {
        return Ok(g
            .pointer(&format!("/{}/proposalId", ch))
            .and_then(|v| v.as_str())
            .map(String::from));
    }
    let files = cm
        .get("files")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let mut changes = vec![];
    for (rel, sha) in &files {
        let data = fetch(&app.http, &format!("{}/{}/{}", base, ch, rel)).await?;
        if sha256_hex(&data) != sha.as_str().unwrap_or("").to_lowercase() {
            return Err(format!("{} sha256 mismatch", rel));
        }
        let text = String::from_utf8(data).map_err(|_| format!("{} not utf8", rel))?;
        changes.push(json!({"path": format!("{}/{}", ch, rel), "content": text}));
    }
    let d = proposals::dir(&app.root);
    let p = proposals::create(
        &d,
        &json!({
            "title": format!("CDN assets: {} v{}", ch, version),
            "kind": if ch == "prompts" { "prompt" } else { "other" },
            "changes": changes,
            "rationale": format!("CDN 配布の {} 更新（version {}）。人ゲート採否対象", ch, version),
        }),
    );
    let pid = p.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    g[ch] = json!({"version": version, "proposalId": pid});
    save_j(&gp, &g);
    app.sse("proposals", &json!({}));
    Ok(Some(pid))
}

/// 手動適用（autoApply off のチャネルも人の明示操作で適用可）
pub async fn apply_pending(app: &Shared, ch: &str) -> (u16, Value) {
    let cfg = app.cfg().await;
    let base = cfg_str(&cfg, "cdnBaseUrl")
        .unwrap_or("")
        .trim_end_matches('/')
        .to_string();
    if base.is_empty() {
        return (400, json!({"ok": false, "error": "cdnBaseUrl not set"}));
    }
    let cm = app.assets.lock().await.remote.get(ch).cloned();
    let Some(cm) = cm else {
        return (404, json!({"ok": false, "error": "no remote info (run check first)"}));
    };
    match apply_channel(app, &base, ch, &cm).await {
        Ok(v) => {
            app.sse("assets", &status(app).await);
            (200, json!({"ok": true, "applied": v}))
        }
        Err(e) => (500, json!({"ok": false, "error": e})),
    }
}

/// 1世代前（<ch>.bak/）へロールバック
pub async fn rollback(app: &Shared, ch: &str) -> (u16, Value) {
    let live = live_dir(&app.root, ch);
    let bak = bak_dir(&app.root, ch);
    if !bak.is_dir() {
        return (404, json!({"ok": false, "error": "no backup"}));
    }
    let tmp = dir(&app.root).join(format!(".swap-{}", ch));
    let _ = std::fs::remove_dir_all(&tmp);
    if live.exists() {
        if let Err(e) = std::fs::rename(&live, &tmp) {
            return (500, json!({"ok": false, "error": e.to_string()}));
        }
    }
    if let Err(e) = std::fs::rename(&bak, &live) {
        if tmp.exists() {
            let _ = std::fs::rename(&tmp, &live);
        }
        return (500, json!({"ok": false, "error": e.to_string()}));
    }
    if tmp.exists() {
        let _ = std::fs::rename(&tmp, &bak);
    }
    let mut st = load_state(&app.root);
    let c = &mut st["channels"][ch];
    let (cur, prev) = (c["version"].clone(), c["prevVersion"].clone());
    c["version"] = prev;
    c["prevVersion"] = cur.clone();
    c["held"] = cur; // 巻き戻した版を hold（自動適用の抑制。新版が来れば解除）
    c["appliedAt"] = json!(now_iso());
    save_state(&app.root, &st);
    app.sse("assets", &status(app).await);
    (200, json!({"ok": true}))
}

// ---------- API ハンドラ ----------
fn jr(v: Value) -> Response {
    Json(v).into_response()
}

pub async fn api_get(State(app): State<Shared>) -> Response {
    jr(status(&app).await)
}

pub async fn api_check(State(app): State<Shared>) -> Response {
    check(&app, true).await;
    jr(status(&app).await)
}

pub async fn api_apply(State(app): State<Shared>, b: Bytes) -> Response {
    let a: Value = serde_json::from_slice(&b).unwrap_or(json!({}));
    let ch = a.get("channel").and_then(|v| v.as_str()).unwrap_or("");
    if ch.is_empty() {
        return jr(json!({"ok": false, "error": "channel required"}));
    }
    let (code, body) = apply_pending(&app, ch).await;
    (StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), Json(body)).into_response()
}

pub async fn api_rollback(State(app): State<Shared>, b: Bytes) -> Response {
    let a: Value = serde_json::from_slice(&b).unwrap_or(json!({}));
    let ch = a.get("channel").and_then(|v| v.as_str()).unwrap_or("");
    if ch.is_empty() {
        return jr(json!({"ok": false, "error": "channel required"}));
    }
    let (code, body) = rollback(&app, ch).await;
    (StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), Json(body)).into_response()
}
