// local_health.rs — ローカル環境の健全性検出（最小実装）
// 登録 repo の path 不在・gh CLI 未認証/不在を候補化する。検出のみ。
use serde_json::{json, Value};
use std::path::Path;

use crate::config::cfg_arr;
use crate::github;
use crate::store;
use crate::util::now_iso;

/// config.repos の path 不在を検出
fn missing_paths(cfg: &Value) -> Vec<Value> {
    let now = now_iso();
    let mut out = Vec::new();
    for repo in cfg_arr(cfg, "repos") {
        let path = repo.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if path.is_empty() || Path::new(path).is_dir() {
            continue;
        }
        let slug = repo
            .get("slug")
            .or_else(|| repo.get("github"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let name = if slug.is_empty() { path } else { slug };
        out.push(json!({
            "source": "local-health",
            "repo": name,
            "slug": slug,
            "title": format!("登録リポジトリの path が見つからない: {}", path),
            "body": format!(
                "config.repos の {} の path {} が存在しない。path 修正または登録解除が必要。",
                name, path
            ),
            "labels": ["health"],
            "kind": "missing-path",
            "symptomHash": format!("health-missing-path-{}", store::symptom_hash(path, "")),
            "detectedAt": now,
        }));
    }
    out
}

/// gh CLI の認証状態を確認（未認証/不在を検出。起動不可・非0終了は Err）
async fn gh_auth() -> Option<Value> {
    github::gh(&["auth", "status"], None).await.err().map(|e| {
        json!({
            "source": "local-health",
            "repo": "",
            "title": "gh CLI が未認証または未インストール",
            "body": format!(
                "`gh auth status` が失敗: {}。github-issues detector が動作しない。`gh auth login` またはインストールが必要。",
                e
            ),
            "labels": ["health"],
            "kind": "gh-auth",
            "symptomHash": "health-gh-auth",
            "detectedAt": now_iso(),
        })
    })
}

pub async fn poll(cfg: &Value) -> Vec<Value> {
    let mut out = missing_paths(cfg);
    if let Some(c) = gh_auth().await {
        out.push(c);
    }
    out
}
