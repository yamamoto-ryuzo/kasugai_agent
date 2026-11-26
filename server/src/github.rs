// github.rs — gh CLI ラッパ（issue list/view/comment/label）
// detector・runner・verify から共通利用する。gh 不在・タイムアウトは Err。
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

use crate::util::hide_console_tokio;

const GH_TIMEOUT: Duration = Duration::from_secs(60);

/// `gh` を spawn して stdout を返す。非0終了は stderr を Err として返す
pub async fn gh(args: &[&str], cwd: Option<&Path>) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new("gh");
    cmd.args(args);
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    hide_console_tokio(&mut cmd);
    let out = tokio::time::timeout(GH_TIMEOUT, cmd.output())
        .await
        .map_err(|_| "gh timeout".to_string())?
        .map_err(|e| format!("gh spawn failed: {}", e))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(format!(
            "gh {} failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// open Issue を列挙（`gh issue list --repo <slug> --json ...`）
pub async fn issue_list(repo_dir: &Path, slug: &str, limit: u32) -> Result<Vec<Value>, String> {
    let lim = limit.to_string();
    let out = gh(
        &[
            "issue",
            "list",
            "--repo",
            slug,
            "--state",
            "open",
            "--json",
            "number,title,body,labels,url",
            "--limit",
            &lim,
        ],
        Some(repo_dir),
    )
    .await?;
    serde_json::from_str::<Vec<Value>>(&out).map_err(|e| format!("gh issue list json: {}", e))
}

/// Issue へコメント
pub async fn issue_comment(repo_dir: &Path, slug: &str, number: u64, body: &str) -> Result<(), String> {
    let n = number.to_string();
    gh(
        &["issue", "comment", &n, "--repo", slug, "--body", body],
        Some(repo_dir),
    )
    .await?;
    Ok(())
}
