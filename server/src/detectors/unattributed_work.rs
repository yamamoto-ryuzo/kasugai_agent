// unattributed_work.rs — 帰属不明作業の検出（diagrams/unattributed-work.mmd）
//
// 登録リポジトリの git 状態を走査し、
//   (a) 稼働中のセッション・chat のいずれにも帰属しない未コミット変更
//   (b) Issue: トレーラを持たない未 push コミット
// を候補化する。Copilot agent mode や人の手作業のようにエージェント本体を
// 観測できない実行経路を、成果物（git 変更）から間接監視する。検出のみ。
use serde_json::{json, Value};

use crate::config::cfg_arr;
use crate::control::App;
use crate::devinenv;
use crate::util::{issue_trailer, norm_slashes, now_iso, run_git};

/// repo の path 正規化（末尾スラッシュなし・小文字）
fn norm(p: &str) -> String {
    norm_slashes(p).trim_end_matches('/').to_lowercase()
}

fn live_sessions() -> Vec<Value> {
    devinenv::sessions_snapshot()
        .into_iter()
        .filter(|s| {
            s.get("live").and_then(|v| v.as_bool()).unwrap_or(false)
                && !s.get("hidden").and_then(|v| v.as_bool()).unwrap_or(false)
        })
        .collect()
}

/// live Devin セッションの workingDirectory が repo path 配下にあるか
fn has_live_session(repo_path: &str) -> Option<String> {
    let rp = norm(repo_path);
    for s in live_sessions() {
        let wd = norm(s.get("workingDirectory").and_then(|v| v.as_str()).unwrap_or(""));
        if !wd.is_empty() && (wd == rp || wd.starts_with(&format!("{}/", rp))) {
            return Some(
                s.get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string(),
            );
        }
    }
    None
}

/// wd が repo path の上位にある live セッション（曖昧な帰属ヒント。
/// ワークスペースルートのセッションはどの repo を触っているか分からないため
/// 帰属とはみなさないが、トリアージ用に body へ併記する）
fn parent_sessions(repo_path: &str) -> Vec<String> {
    let rp = norm(repo_path);
    live_sessions()
        .iter()
        .filter(|s| {
            let wd = norm(s.get("workingDirectory").and_then(|v| v.as_str()).unwrap_or(""));
            !wd.is_empty() && rp.starts_with(&format!("{}/", wd))
        })
        .map(|s| {
            format!(
                "{}（wd: {}）",
                s.get("title").and_then(|v| v.as_str()).unwrap_or("?"),
                s.get("workingDirectory")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
            )
        })
        .collect()
}

/// kasugai 管理下の chat がこの repo で稼働中か（busy / verify 中 / 待機中）
async fn has_active_chat(app: &App, repo_path: &str) -> Option<String> {
    let rp = norm(repo_path);
    let chats = app.chats.lock().await;
    for (id, e) in chats.iter() {
        let Some(repo) = e.repo.as_ref() else { continue };
        let cp = norm(repo.get("path").and_then(|v| v.as_str()).unwrap_or(""));
        if cp.is_empty() || (cp != rp && !cp.starts_with(&format!("{}/", rp)) && !rp.starts_with(&format!("{}/", cp))) {
            continue;
        }
        let c = e.chat.core().lock().await;
        let active = c.busy || e.verify.is_some() || c.waiting.is_some();
        if active {
            return Some(format!("chat #{} {}", id, e.label));
        }
    }
    None
}

/// 未 push コミットのうち Issue: トレーラを持たないものを列挙。
/// upstream が無ければ None。戻り値は (総数, 先頭件名リスト)
async fn unpushed_without_trailer(path: &str) -> Option<(usize, Vec<String>)> {
    let (code, _) = run_git(path, &["rev-parse", "--abbrev-ref", "@{u}"]).await;
    if code != 0 {
        return None; // upstream なし = 判定不能（ローカル専用 repo）
    }
    // %H%x1f%B%x1e: レコード sep=0x1e、フィールド sep=0x1f
    let (code, out) = run_git(path, &["log", "@{u}..HEAD", "--format=%H%x1f%B%x1e"]).await;
    if code != 0 {
        return None;
    }
    let mut count = 0usize;
    let mut subjects = Vec::new();
    for rec in out.split('\x1e') {
        let rec = rec.trim_matches('\n');
        if rec.trim().is_empty() {
            continue;
        }
        let mut parts = rec.splitn(2, '\x1f');
        let _sha = parts.next().unwrap_or("");
        let body = parts.next().unwrap_or("");
        if issue_trailer(body).is_none() {
            count += 1;
            if subjects.len() < 5 {
                subjects.push(body.lines().next().unwrap_or("").trim().to_string());
            }
        }
    }
    Some((count, subjects))
}

pub async fn poll(cfg: &Value, app: &App) -> Vec<Value> {
    let now = now_iso();
    let mut out = Vec::new();
    for repo in cfg_arr(cfg, "repos") {
        let path = repo.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if path.is_empty() || !crate::util::is_dir(path) {
            continue;
        }
        let slug = repo
            .get("slug")
            .or_else(|| repo.get("github"))
            .and_then(|v| v.as_str())
            .unwrap_or(path);

        // (a) 未コミット変更: 帰属ヒント（live セッション / 稼働 chat）が無い時のみ
        let (scode, status) = run_git(path, &["status", "--porcelain"]).await;
        if scode == 0 && !status.trim().is_empty() {
            let files: Vec<&str> = status.lines().filter(|l| !l.trim().is_empty()).collect();
            let live = has_live_session(path);
            let chat = has_active_chat(app, path).await;
            if live.is_none() && chat.is_none() {
                let parents = parent_sessions(path);
                let hint = if parents.is_empty() {
                    String::new()
                } else {
                    format!(
                        "\n\n参考: repo 上位を wd に持つ live セッション（帰属は曖昧・抑止はしない）:\n{}",
                        parents.iter().map(|p| format!("  {}", p)).collect::<Vec<_>>().join("\n")
                    )
                };
                out.push(json!({
                    "source": "unattributed-work",
                    "repo": slug,
                    "slug": slug,
                    "title": format!("帰属不明の未コミット変更: {}（{}件）", slug, files.len()),
                    "body": format!(
                        "{} に {} 件の未コミット変更があるが、live の Devin セッション・稼働中の chat のいずれにも帰属しない。Copilot agent mode・人の手作業・放置された変更の可能性。\n\n先頭:\n{}{}",
                        path, files.len(),
                        files.iter().take(10).map(|l| format!("  {}", l)).collect::<Vec<_>>().join("\n"),
                        hint
                    ),
                    "labels": ["oversight"],
                    "kind": "uncommitted-changes",
                    "symptomHash": format!("uncommitted:{}", slug),
                    "detectedAt": now,
                }));
            }
        }

        // (b) Issue: トレーラを持たない未 push コミット
        if let Some((count, subjects)) = unpushed_without_trailer(path).await {
            if count > 0 {
                out.push(json!({
                    "source": "unattributed-work",
                    "repo": slug,
                    "slug": slug,
                    "title": format!("未帰属の未 push コミット: {}（{}件）", slug, count),
                    "body": format!(
                        "{} に Issue: トレーラを持たない未 push コミットが {} 件ある。課題に帰属しない作業（人の直接コミット等）か、帰属記録漏れ。\n\n先頭:\n{}",
                        path, count,
                        subjects.iter().map(|s| format!("  {}", s)).collect::<Vec<_>>().join("\n")
                    ),
                    "labels": ["oversight"],
                    "kind": "unattributed-commits",
                    "symptomHash": format!("unpushed:{}", slug),
                    "detectedAt": now,
                }));
            }
        }
    }
    out
}
