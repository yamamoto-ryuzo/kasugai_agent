// session_watch.rs — Devin セッション観測（diagrams/session-watch.mmd）
//
// 人が Devin Desktop/CLI で直接起動したものを含む全セッションを
// devinenv::sessions_snapshot（環境ビューと同じ読取筋）で観測し、
// live なのに活動のない停滞セッションを候補化する。観測のみで介入しない
// （再開は人が Devin 側で行う）。検出のみ。
use serde_json::{json, Value};

use crate::config::{cfg_arr, cfg_f64, cfg_obj};
use crate::devinenv;
use crate::util::{norm_slashes, now_iso};

/// workingDirectory を config.repos の path に照合し、slug（無ければ github）を返す
fn match_repo(cfg: &Value, wd: &str) -> String {
    let wd = norm_slashes(wd).to_lowercase();
    let mut best: (usize, String) = (0, String::new());
    for repo in cfg_arr(cfg, "repos") {
        let path = norm_slashes(repo.get("path").and_then(|v| v.as_str()).unwrap_or(""))
            .to_lowercase();
        if path.is_empty() {
            continue;
        }
        if wd == path || wd.starts_with(&format!("{}/", path)) {
            if path.len() > best.0 {
                let slug = repo
                    .get("slug")
                    .or_else(|| repo.get("github"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                best = (path.len(), slug.to_string());
            }
        }
    }
    best.1
}

/// live セッションの停滞を検出。閾値は sessionWatch.stallHours（既定12時間）
pub async fn poll(cfg: &Value) -> Vec<Value> {
    let stall_hours = cfg_obj(cfg, "sessionWatch")
        .and_then(|o| o.get("stallHours"))
        .and_then(|v| v.as_f64())
        .unwrap_or_else(|| cfg_f64(cfg, "sessionWatchStallHours", 12.0));
    if stall_hours <= 0.0 {
        return vec![];
    }
    let now = now_iso();
    let now_ms = crate::util::now_ms();
    let mut out = Vec::new();
    for s in devinenv::sessions_snapshot() {
        if s.get("hidden").and_then(|v| v.as_bool()).unwrap_or(false) {
            continue;
        }
        if !s.get("live").and_then(|v| v.as_bool()).unwrap_or(false) {
            continue; // sleep 中は人が明示的に閉じた状態。監視しない
        }
        let id = s.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let last = s.get("lastActivityAt").and_then(|v| v.as_str()).unwrap_or("");
        let Some(ts) = chrono::DateTime::parse_from_rfc3339(last)
            .ok()
            .map(|d| d.timestamp_millis())
        else {
            continue;
        };
        let stall_ms = now_ms - ts;
        let stall_h = stall_ms as f64 / 3_600_000.0;
        if stall_h < stall_hours {
            continue;
        }
        let title = s.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let wd = s
            .get("workingDirectory")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let slug = match_repo(cfg, wd);
        let repo_label = if slug.is_empty() { wd } else { slug.as_str() };
        out.push(json!({
            "source": "session-watch",
            "repo": repo_label,
            "slug": slug,
            "title": format!("Devin セッションが停滞: {}", if title.is_empty() { id } else { title }),
            "body": format!(
                "live セッション {}（{}）の最終活動が {}（{:.1}時間前）。\nworkingDirectory: {}\n帰属 repo: {}\n停滞セッションの再開・終了は人が Devin 側で判断する。",
                id, if title.is_empty() { "無題" } else { title }, last, stall_h, wd,
                if slug.is_empty() { "未登録" } else { slug.as_str() }
            ),
            "labels": ["oversight"],
            "kind": "stalled-session",
            "symptomHash": format!("stalled:{}", id),
            // 停滞時間を持続情報として載せる（閾値超過は確定状態なので即昇格材料）
            "durationSeconds": stall_ms as f64 / 1000.0,
            "detectedAt": now,
        }));
    }
    out
}
