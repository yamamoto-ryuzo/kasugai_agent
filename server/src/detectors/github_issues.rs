// github_issues.rs — GitHub open Issue の検出（README §5.1）
// repo.autoTrigger==true なら全 open issue、そうでなければ triggerLabel
// （既定 "devin"）付きのみ候補化する。検出のみ: dedupe・起動には関与しない。
use serde_json::{json, Value};
use std::path::Path;

use crate::conlog::conlog;
use crate::config::cfg_arr;
use crate::github;
use crate::util::now_iso;

const ISSUE_LIMIT: u32 = 50;

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

/// config.repos 全件分の open issue を候補化する。
/// gh 不在・タイムアウト・JSON 失敗はその repo をスキップして conlog のみ。
pub async fn poll(cfg: &Value) -> Vec<Value> {
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
        let auto = repo
            .get("autoTrigger")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let trigger = repo
            .get("triggerLabel")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("devin")
            .to_string();
        let issues = match github::issue_list(dir, slug, ISSUE_LIMIT).await {
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
        for it in issues {
            let labels = label_names(&it);
            if !auto && !labels.iter().any(|l| l.eq_ignore_ascii_case(&trigger)) {
                continue;
            }
            let Some(number) = it.get("number").and_then(|v| v.as_u64()) else {
                continue;
            };
            out.push(json!({
                "source": "github-issues",
                "repo": slug,
                "slug": slug,
                "number": number,
                "url": it.get("url").cloned().unwrap_or(Value::Null),
                "title": it.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                "body": it.get("body").and_then(|v| v.as_str()).unwrap_or(""),
                "labels": labels,
                "symptomHash": format!("gh-{}", number),
                "detectedAt": now_iso(),
            }));
        }
    }
    out
}
