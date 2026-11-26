// detectors/ — 課題候補（candidate）の検出層（README §5.1、detect-pipeline.mmd）
//
// detector は検出のみを行い、統一 candidate 形式を返す:
//   {source, repo, title, body, symptomHash, detectedAt,
//    slug?, number?, url?, labels?}
// dedupe・起動判定には関与しない。入口の ingest が candidates.json への
// dedupe 記録と pipeline::intake を一括で担う（dedupe はこの入口1箇所のみ）。
pub mod github_issues;
pub mod local_health;

use serde_json::{json, Value};
use std::path::Path;
use std::sync::Mutex;

use crate::store;
use crate::util::now_iso;

/// ingest の直列化（issues/candidates の load→save 間の lost update 防止）
static INGEST_LOCK: Mutex<()> = Mutex::new(());

/// emitted candidate の入口: candidates.json に dedupe 記録してから
/// pipeline::intake へ渡す。戻り値は intake の (issue, created?)。
pub fn ingest(root: &Path, cand: &Value) -> (Value, bool) {
    let _g = INGEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    record_candidate(root, cand);
    crate::pipeline::intake(root, cand)
}

/// candidates.json は検出の受信箱。issue key（source+repo+symptomHash）で
/// 既出判定し、既出は lastSeen のみ更新（detect-pipeline.mmd と同規則）。
fn record_candidate(root: &Path, cand: &Value) {
    let source = cand.get("source").and_then(|v| v.as_str()).unwrap_or("manual");
    let repo = cand.get("repo").and_then(|v| v.as_str()).unwrap_or("");
    let title = cand.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let body = cand.get("body").and_then(|v| v.as_str()).unwrap_or("");
    let sh = cand
        .get("symptomHash")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| store::symptom_hash(title, body));
    let key = store::issue_key(source, repo, &sh);
    let now = now_iso();

    let mut cands = store::load_candidates(root);
    if let Some(ex) = cands.iter_mut().find(|c| {
        c.get("key").and_then(|v| v.as_str()) == Some(key.as_str())
    }) {
        let o = ex.as_object_mut().unwrap();
        o.insert("lastSeen".into(), json!(now));
        store::save_candidates(root, &cands);
        return;
    }
    let mut c = cand.clone();
    let o = c.as_object_mut().unwrap();
    o.insert("key".into(), json!(key));
    o.insert("symptomHash".into(), json!(sh));
    if o.get("detectedAt").is_none() {
        o.insert("detectedAt".into(), json!(now.clone()));
    }
    o.insert("lastSeen".into(), json!(now));
    cands.push(c);
    store::save_candidates(root, &cands);
}
