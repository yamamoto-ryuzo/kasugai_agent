// audit.rs — run 単位の構造化監査タイムライン（audit-timeline.mmd・README §5.10）
//
// Azure SRE Agent の action log / PagerDuty incident timeline を参考に、
// 「誰が dispatch した・何を実行した・根拠・verify 結果・push 有無」を
// 課題単位の時系列イベントとして evidence/<slug>/audit.jsonl へ追記する。
// 人の操作（actor=human）は control.rs の API ハンドラが、システム動作
// （actor=system）は runner.rs が記録する。証跡（run.log 等の成果物）とは
// 別に「行動の記録」だけを持つ薄い台帳。
use serde_json::{json, Map, Value};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::util::now_iso;

/// 課題の監査ログパス（evidence/<slug>/audit.jsonl。課題単位で run を
/// またぐ時系列を持つ）
fn audit_path(root: &Path, key: &str) -> PathBuf {
    root.join("evidence")
        .join(crate::knowledge::key_slug(key))
        .join("audit.jsonl")
}

/// 監査イベントを1行追記する。
/// actor: "human"（パネル/API の人操作）| "system"（runner/pipeline の自動動作）
/// action: dispatch/reject/approve/push-approve/plan-approve/followup/split/
///         run.start/exec/verify/send_back/push/done/failed/aborted 等
/// detail: 実行内容・根拠・verify 結果・push 有無等の構造化メタ
/// run: イベントが属する run idx（課題操作など run に紐付かないものは None）
pub fn record(
    root: &Path,
    key: &str,
    actor: &str,
    action: &str,
    run: Option<usize>,
    detail: Map<String, Value>,
) {
    let path = audit_path(root, key);
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let ev = json!({
        "at": now_iso(),
        "actor": actor,
        "action": action,
        "run": run.map(|i| json!(i)).unwrap_or(Value::Null),
        "detail": Value::Object(detail),
    });
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        // secret redaction（manual-a3a60c51）: detail は verify 理由等の
        // 出力由来文字列を含み得るため、監査台帳への書き込み点でマスクする
        let _ = writeln!(f, "{}", crate::redact::scrub(&ev.to_string()));
    }
}

/// 課題の最新 run idx を調べて record へ渡す短縮形（runner の run 内
/// イベント用。issue を既に持っている呼出元は detail.run を直接入れてよい）
pub fn record_run(root: &Path, key: &str, actor: &str, action: &str, detail: Map<String, Value>) {
    let idx = crate::store::load_issues(root)
        .into_iter()
        .find(|i| i.get("key").and_then(|v| v.as_str()) == Some(key))
        .and_then(|i| crate::trajectory::latest_run_idx(&i));
    record(root, key, actor, action, idx, detail);
}

/// 監査イベント一覧を読む（API 用）
pub fn read(root: &Path, key: &str) -> Vec<Value> {
    let mut events = Vec::new();
    if let Ok(text) = std::fs::read_to_string(audit_path(root, key)) {
        for l in text.lines() {
            if let Ok(v) = serde_json::from_str::<Value>(l) {
                events.push(v);
            }
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_appends_chronological_events() {
        let root = std::env::temp_dir().join(format!(
            "kasugai-audit-test-{}",
            crate::util::rand_hex(4)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let key = "src:repo:h";
        record(&root, key, "human", "dispatch", None, Map::new());
        let mut d = Map::new();
        d.insert("ok".into(), json!(true));
        d.insert("level".into(), json!(2));
        record(&root, key, "system", "verify", Some(0), d);
        let evs = read(&root, key);
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0]["actor"], json!("human"));
        assert_eq!(evs[0]["action"], json!("dispatch"));
        assert_eq!(evs[1]["run"], json!(0));
        assert_eq!(evs[1]["detail"]["level"], json!(2));
        let _ = std::fs::remove_dir_all(&root);
    }
}
