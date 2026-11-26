// ci_failure.rs — GitHub Actions 失敗 run の検出（README §5.1）
// config.repos の各 slug で `gh run list --status failure` を実行し、
// 失敗 run を候補化する。検出のみ: dedupe・起動には関与しない。
use serde_json::{json, Value};
use std::path::Path;

use crate::conlog::conlog;
use crate::config::cfg_arr;
use crate::github;
use crate::util::now_iso;

const RUN_LIMIT: u32 = 10;

/// config.repos 全件分の失敗 workflow run を候補化する。
/// gh 不在・タイムアウト・JSON 失敗はその repo をスキップして conlog のみ。
pub async fn poll(cfg: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    let lim = RUN_LIMIT.to_string();
    for repo in cfg_arr(cfg, "repos") {
        let slug = repo
            .get("slug")
            .or_else(|| repo.get("github"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if slug.is_empty() {
            continue;
        }
        let path = repo.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let cwd = if path.is_empty() {
            None
        } else {
            let dir = Path::new(path);
            if !dir.is_dir() {
                continue; // path 不在は local-health の検出領分
            }
            Some(dir)
        };
        let runs = github::gh(
            &[
                "run",
                "list",
                "--repo",
                slug,
                "--status",
                "failure",
                "--limit",
                &lim,
                "--json",
                "databaseId,displayTitle,headBranch,headSha,url,workflowName,conclusion",
            ],
            cwd,
        )
        .await
        .and_then(|s| {
            serde_json::from_str::<Vec<Value>>(&s).map_err(|e| format!("gh run list json: {}", e))
        });
        let runs = match runs {
            Ok(v) => v,
            Err(e) => {
                conlog(
                    "detect",
                    "err",
                    &format!("ci-failure: {} の run 一覧を取得できない: {}", slug, e),
                );
                continue;
            }
        };
        for r in runs {
            let Some(id) = r.get("databaseId").and_then(|v| v.as_u64()) else {
                continue;
            };
            if r.get("conclusion").and_then(|v| v.as_str()) != Some("failure") {
                continue;
            }
            let wf = r
                .get("workflowName")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let disp = r
                .get("displayTitle")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let url = r.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let sha = r.get("headSha").and_then(|v| v.as_str()).unwrap_or("");
            let branch = r
                .get("headBranch")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            out.push(json!({
                "source": "ci-failure",
                "repo": slug,
                "slug": slug,
                "url": url,
                "title": format!("CI失敗: {} {}", wf, disp).trim_end(),
                "body": format!("run: {}\nbranch: {}\nsha: {}", url, branch, sha),
                "labels": ["ci"],
                "kind": wf,
                "symptomHash": format!("ci-{}", id),
                "detectedAt": now_iso(),
            }));
        }
    }
    out
}
