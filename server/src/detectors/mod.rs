// detectors/ — 課題候補（candidate）の検出層（README §5.1、detect-pipeline.mmd）
//
// detector は検出のみを行い、統一 candidate 形式を返す:
//   {source, repo, title, body, symptomHash, detectedAt,
//    slug?, number?, url?, labels?, kind?}
// kind は課題の種別（ci-failure は workflow 名等）。learn の SKILL draft 名
// （source+repo+kind の安定 slug）に使う。揮発 id（run id 等）は入れない。
// dedupe・起動判定には関与しない。入口の ingest が candidates.json への
// dedupe 記録と pipeline::intake を一括で担う（dedupe はこの入口1箇所のみ）。
pub mod ci_failure;
pub mod github_issues;
pub mod local_health;
pub mod spec_drift;

use serde_json::{json, Value};
use std::path::Path;
use std::sync::Mutex;

use crate::store;
use crate::util::now_iso;

/// ingest の直列化（issues/candidates の load→save 間の lost update 防止）
static INGEST_LOCK: Mutex<()> = Mutex::new(());

/// candidate から issue key を導出（record_candidate と intake が同じ規則で
/// 使う。source+repo+symptomHash）
fn candidate_key(cand: &Value) -> String {
    let source = cand.get("source").and_then(|v| v.as_str()).unwrap_or("manual");
    let repo = cand.get("repo").and_then(|v| v.as_str()).unwrap_or("");
    let sh = cand
        .get("symptomHash")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| {
            store::symptom_hash(
                cand.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                cand.get("body").and_then(|v| v.as_str()).unwrap_or(""),
            )
        });
    store::issue_key(source, repo, &sh)
}

/// candidates.json の candidate に deferred マークを付け/外す
/// （起票レート制限で留め置いたことを一覧へ表示するため。課題状態ではなく
/// 受信箱の表示用メタ）
fn set_candidate_deferred(root: &Path, key: &str, deferred: Option<&str>) {
    let mut cands = store::load_candidates(root);
    let mut touched = false;
    if let Some(c) = cands
        .iter_mut()
        .find(|c| c.get("key").and_then(|v| v.as_str()) == Some(key))
    {
        let o = c.as_object_mut().unwrap();
        match deferred {
            Some(reason) => {
                if o.get("deferred").and_then(|v| v.as_str()) != Some(reason) {
                    o.insert("deferred".into(), json!(reason));
                    o.insert("deferredAt".into(), json!(now_iso()));
                    touched = true;
                }
            }
            None => {
                if o.remove("deferred").is_some() | o.remove("deferredAt").is_some() {
                    touched = true;
                }
            }
        }
    }
    if touched {
        store::save_candidates(root, &cands);
    }
}

/// emitted candidate の入口: candidates.json に dedupe 記録してから
/// pipeline::intake へ渡す。戻り値は (issue, created?, deferred?)。
/// レート制限で留め置かれた場合は (null, false, true) で、candidate に
/// deferred マークが付く（deferred は candidates の SSE push 判定に使う）。
pub fn ingest(root: &Path, cand: &Value) -> (Value, bool, bool) {
    let _g = INGEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    record_candidate(root, cand);
    let key = candidate_key(cand);
    match crate::pipeline::intake(root, cand) {
        crate::pipeline::IntakeOutcome::Created(i) => {
            set_candidate_deferred(root, &key, None);
            (i, true, false)
        }
        crate::pipeline::IntakeOutcome::Existing(i) => (i, false, false),
        crate::pipeline::IntakeOutcome::Deferred(reason) => {
            set_candidate_deferred(root, &key, Some(&reason));
            (Value::Null, false, true)
        }
    }
}

/// 人操作（candidates 一覧の対応/却下ボタン）による即時起票。
/// pipeline::intake_forced 経由で起票レート制限を bypass する点以外は
/// ingest と同じ（人判断はレート制限より優先。detect-pipeline.mmd）
pub fn ingest_forced(root: &Path, cand: &Value) -> (Value, bool, bool) {
    let _g = INGEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    record_candidate(root, cand);
    let key = candidate_key(cand);
    match crate::pipeline::intake_forced(root, cand) {
        crate::pipeline::IntakeOutcome::Created(i) => {
            set_candidate_deferred(root, &key, None);
            (i, true, false)
        }
        crate::pipeline::IntakeOutcome::Existing(i) => (i, false, false),
        crate::pipeline::IntakeOutcome::Deferred(reason) => {
            set_candidate_deferred(root, &key, Some(&reason));
            (Value::Null, false, true)
        }
    }
}

/// 起票の reconcile（起票は自動が既定。detect-pipeline.mmd）:
/// candidates.json にあって issues.json に課題の無い candidate を
/// intake で補起票する。戻り値は (新規起票数, deferred マーク数)。
/// deferred 分は窓が空いた次回以降の reconcile で起票される
/// （detect-pipeline.mmd RATE → DEFER）。
pub fn backfill_issues(root: &Path) -> (usize, usize) {
    let _g = INGEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let issues = store::load_issues(root);
    let have: std::collections::HashSet<String> = issues
        .iter()
        .filter_map(|i| i.get("key").and_then(|v| v.as_str()).map(String::from))
        .collect();
    let mut n = 0;
    let mut deferred = 0;
    for c in store::load_candidates(root) {
        let key = c
            .get("key")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| candidate_key(&c));
        if have.contains(&key) {
            continue;
        }
        match crate::pipeline::intake(root, &c) {
            crate::pipeline::IntakeOutcome::Created(_) => {
                set_candidate_deferred(root, &key, None);
                n += 1;
            }
            crate::pipeline::IntakeOutcome::Deferred(reason) => {
                set_candidate_deferred(root, &key, Some(&reason));
                deferred += 1;
            }
            crate::pipeline::IntakeOutcome::Existing(_) => {}
        }
    }
    (n, deferred)
}

/// candidates.json は検出の受信箱。issue key（source+repo+symptomHash）で
/// 既出判定し、既出は lastSeen のみ更新（detect-pipeline.mmd と同規則）。
fn record_candidate(root: &Path, cand: &Value) {
    let title = cand.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let body = cand.get("body").and_then(|v| v.as_str()).unwrap_or("");
    let key = candidate_key(cand);
    let sh = cand
        .get("symptomHash")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| store::symptom_hash(title, body));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "kasugai-detectors-test-{}-{}",
            tag,
            crate::util::rand_hex(4)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn backfill_issues_files_unfiled_candidates_only() {
        let root = temp_root("backfill");
        // candidate を直接保存（課題未作成 = 起票漏れの状態）
        store::save_candidates(
            &root,
            &[json!({
                "source": "manual", "repo": "me/repo", "slug": "me/repo",
                "title": "stray candidate", "body": "b",
                "symptomHash": "h1", "detectedAt": "t",
            })],
        );
        assert_eq!(backfill_issues(&root).0, 1);
        let issues = store::load_issues(&root);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0]["status"], json!(crate::pipeline::ST_QUEUED));
        // 課題作成済みの candidate は再起票しない
        assert_eq!(backfill_issues(&root).0, 0);
        let _ = std::fs::remove_dir_all(&root);
    }
}
