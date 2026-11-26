// github_issues.rs — GitHub Issue の検出・一括取込（README §5.1）
// open issue はすべて候補化する（ラベル選別しない。起票＝本システムの課題で、
// GitHub Issue への登録は別機構として後続検討）。
// closed 済み Issue の一括取込は import_all が行い、pipeline が終端課題
// （COMPLETED→done / NOT_PLANNED→rejected）として起票する。
// 検出のみ: dedupe・起動には関与しない。
use serde_json::{json, Value};
use std::path::Path;

use crate::conlog::conlog;
use crate::config::cfg_arr;
use crate::github;
use crate::util::now_iso;

const ISSUE_LIMIT: u32 = 50;
const IMPORT_LIMIT: u32 = 500;

/// gh が返す labels（[{name:..}] or [".."]）を文字列配列へ正規化
fn label_names(it: &Value) -> Vec<String> {
    it.get("labels")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|l| l.get("name").and_then(|n| n.as_str()).or_else(|| l.as_str()))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// issue 1件を統一 candidate 形式へ（ghState/ghStateReason は取込の終端判定用）
fn issue_candidate(slug: &str, it: &Value) -> Option<Value> {
    let number = it.get("number").and_then(|v| v.as_u64())?;
    Some(json!({
        "source": "github-issues",
        "repo": slug,
        "slug": slug,
        "number": number,
        "url": it.get("url").cloned().unwrap_or(Value::Null),
        "title": it.get("title").and_then(|v| v.as_str()).unwrap_or(""),
        "body": it.get("body").and_then(|v| v.as_str()).unwrap_or(""),
        "labels": label_names(it),
        "kind": "issue",
        "ghState": it.get("state").cloned().unwrap_or(Value::Null),
        "ghStateReason": it.get("stateReason").cloned().unwrap_or(Value::Null),
        "symptomHash": format!("gh-{}", number),
        "detectedAt": now_iso(),
    }))
}

/// config.repos 全件分の issue を state 指定で候補化する。
/// gh 不在・タイムアウト・JSON 失敗はその repo をスキップして conlog のみ。
async fn poll_state(cfg: &Value, state: &str, limit: u32) -> Vec<Value> {
    let mut out = Vec::new();
    for repo in cfg_arr(cfg, "repos") {
        let slug = repo
            .get("slug")
            .or_else(|| repo.get("github"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let path = repo.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if slug.is_empty() || path.is_empty() {
            continue;
        }
        let dir = Path::new(path);
        if !dir.is_dir() {
            continue; // path 不在は local-health の検出領分
        }
        let issues = match github::issue_list(dir, slug, state, limit).await {
            Ok(v) => v,
            Err(e) => {
                conlog(
                    "detect",
                    "err",
                    &format!("github-issues: {} の issue 一覧を取得できない: {}", slug, e),
                );
                continue;
            }
        };
        for it in &issues {
            if let Some(c) = issue_candidate(slug, it) {
                out.push(c);
            }
        }
    }
    out
}

/// 定期検出: open issue は全件候補化する
pub async fn poll(cfg: &Value) -> Vec<Value> {
    poll_state(cfg, "open", ISSUE_LIMIT).await
}

/// GitHub Issue の一括取込（`--import-github` / POST /api/control/import-github）。
/// open+closed 全件を intake へ流す。closed は pipeline が終端状態で起票し、
/// open は通常通り assess → queued|gated へ。戻り値 (新規起票数, 対象件数)。
pub async fn import_all(root: &Path, cfg: &Value) -> (usize, usize) {
    let cands = poll_state(cfg, "all", IMPORT_LIMIT).await;
    let total = cands.len();
    let mut created = 0;
    for c in &cands {
        let (_i, cr, _df) = crate::detectors::ingest(root, c);
        if cr {
            created += 1;
        }
    }
    (created, total)
}
