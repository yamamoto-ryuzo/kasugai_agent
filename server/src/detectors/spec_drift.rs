// spec_drift.rs — 図・正本ドキュメントと実装の乖離検出（architecture.mmd 検出層）
// (a) tools/sync-diagrams.mjs / sync-docs.mjs の --check（非0終了で乖離）
// (b) README §10 のモジュール一覧と server/src 実ファイルの対応
// node 不在時は (a) をスキップ。検出のみ: dedupe・起動には関与しない。
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

use crate::config::cfg_arr;
use crate::repomatch::repo_slug;
use crate::util::{hide_console_tokio, norm_p, now_iso};

const SYNC_TIMEOUT: Duration = Duration::from_secs(60);

/// app root に対応する登録 repo の slug（自リポジトリ監視用。無ければ ""）
fn self_slug(cfg: &Value, root: &Path) -> String {
    let root_n = norm_p(&root.to_string_lossy());
    cfg_arr(cfg, "repos")
        .iter()
        .find(|r| {
            norm_p(r.get("path").and_then(|v| v.as_str()).unwrap_or("")) == root_n
        })
        .and_then(|r| repo_slug(Some(*r)))
        .unwrap_or_default()
}

fn cand(repo: &str, title: String, body: String, hash: String) -> Value {
    json!({
        "source": "spec-drift",
        "repo": repo,
        "slug": repo,
        "title": title,
        "body": body,
        "labels": ["spec-drift"],
        "kind": hash.clone(),
        "symptomHash": hash,
        "detectedAt": now_iso(),
    })
}

/// `node tools/<tool> --check` を app root で実行。非0終了なら candidate。
/// node 不在（spawn NotFound）・タイムアウトはスキップ（None）。
async fn sync_check(root: &Path, repo: &str, tool: &str, hash: &str) -> Option<Value> {
    let script = format!("tools/{}", tool);
    let mut cmd = tokio::process::Command::new("node");
    cmd.args([&script, "--check"]).current_dir(root);
    hide_console_tokio(&mut cmd);
    let out = match tokio::time::timeout(SYNC_TIMEOUT, cmd.output()).await {
        Ok(Ok(o)) => o,
        _ => return None,
    };
    if out.status.success() {
        return None;
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let detail: String = stderr.trim().lines().take(12).collect::<Vec<_>>().join("\n");
    let code = out
        .status
        .code()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "?".into());
    Some(cand(
        repo,
        format!("正本ドキュメントと写しが乖離（{}）", tool),
        format!(
            "`node {} --check` が非0終了（code: {}）。`node {}` を実行して正本と写しを同期する。\n\n```\n{}\n```",
            script, code, script, detail
        ),
        hash.to_string(),
    ))
}

/// README §10 のコードブロックから server/src/ 配下の設計エントリを抽出。
/// 戻り値は (src/ からの相対パス, is_dir)。dirs は `name/`、files は `*.rs`。
fn readme_src_entries(readme: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let Some(sec) = readme.split("## 10.").nth(1) else {
        return out;
    };
    let sec = sec.split("\n## ").next().unwrap_or(sec);
    let mut in_block = false;
    let mut src_indent: Option<usize> = None;
    let mut dirs: Vec<(usize, String)> = Vec::new();
    for line in sec.lines() {
        let t = line.trim();
        if t.starts_with("```") {
            if in_block {
                break;
            }
            in_block = true;
            continue;
        }
        if !in_block {
            continue;
        }
        // コメントに 未実装 と明記された設計予定項目は乖離ではない（locks.rs 等）
        let planned = line.split('#').nth(1).is_some_and(|c| c.contains("未実装"));
        let code = line.split('#').next().unwrap_or("");
        let indent = code.len() - code.trim_start().len();
        let name = code.trim();
        if name.is_empty() {
            continue;
        }
        match src_indent {
            None => {
                if name == "src/" {
                    src_indent = Some(indent);
                }
            }
            Some(si) => {
                if indent <= si {
                    break; // src/ サブツリーから出た
                }
                while dirs.last().is_some_and(|(di, _)| *di >= indent) {
                    dirs.pop();
                }
                let is_dir = name.ends_with('/');
                let base = name.trim_end_matches('/');
                let rel = dirs
                    .iter()
                    .map(|(_, d)| d.as_str())
                    .chain(std::iter::once(base))
                    .collect::<Vec<_>>()
                    .join("/");
                if !planned && (is_dir || rel.ends_with(".rs")) {
                    out.push((rel.clone(), is_dir));
                }
                if is_dir {
                    dirs.push((indent, base.to_string()));
                }
            }
        }
    }
    out
}

/// README §10 に記載のモジュールが server/src に無いものを候補化。
/// .rs 名の `-` は `_` に正規化して対応付ける（README は kebab 表記）。
fn missing_modules(root: &Path, repo: &str) -> Vec<Value> {
    let Ok(readme) = std::fs::read_to_string(root.join("README.md")) else {
        return Vec::new();
    };
    let src = root.join("server").join("src");
    let mut missing_dirs: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for (rel, is_dir) in readme_src_entries(&readme) {
        if missing_dirs
            .iter()
            .any(|d| rel.starts_with(&format!("{}/", d)))
        {
            continue;
        }
        let norm = rel
            .split('/')
            .map(|s| s.replace('-', "_"))
            .collect::<Vec<_>>()
            .join("/");
        let p = src.join(&norm);
        if if is_dir { p.is_dir() } else { p.is_file() } {
            continue;
        }
        if is_dir {
            missing_dirs.push(rel.clone());
        }
        let stem = rel
            .rsplit('/')
            .next()
            .unwrap_or(&rel)
            .trim_end_matches(".rs");
        out.push(cand(
            repo,
            format!("README §10 の設計モジュールが実装に無い: {}", rel),
            format!(
                "README.md §10（ディレクトリ構成）に `{}` が記載されているが `server/src/{}` が存在しない。実装追加か README 側の修正（図・正本を先に直す規約に従い README 更新）が必要。",
                rel, norm
            ),
            format!("drift-module-{}", stem),
        ));
    }
    out
}

pub async fn poll(cfg: &Value, root: &Path) -> Vec<Value> {
    let repo = self_slug(cfg, root);
    let mut out = Vec::new();
    for (tool, hash) in [
        ("sync-diagrams.mjs", "drift-diagrams"),
        ("sync-docs.mjs", "drift-docs"),
    ] {
        if let Some(c) = sync_check(root, &repo, tool, hash).await {
            out.push(c);
        }
    }
    out.extend(missing_modules(root, &repo));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_root() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf()
    }

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("kasugai-specdrift-{}-{}", tag, std::process::id()))
    }

    #[test]
    fn readme_entries_cover_src_tree_and_skip_planned() {
        let readme = std::fs::read_to_string(app_root().join("README.md")).unwrap();
        let entries = readme_src_entries(&readme);
        let names: Vec<&str> = entries.iter().map(|(r, _)| r.as_str()).collect();
        assert!(names.contains(&"control.rs"));
        assert!(names.contains(&"chat/acp.rs"));
        assert!(names.contains(&"detectors/spec_drift.rs"));
        assert!(names.contains(&"registry/refs.rs"));
        // locks.rs は実装済みで §10 に記載あり → entries に含まれる
        assert!(names.contains(&"locks.rs"));
    }

    #[test]
    fn missing_modules_flags_absent_files() {
        let tmp = tmp_dir("mod");
        let src = tmp.join("server").join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(
            tmp.join("README.md"),
            "## 10. ディレクトリ構成（設計）\n\n```\napp/\n  server/\n    src/\n      main.rs\n      ghost.rs\n      planned.rs # M2 予定・未実装\n```\n",
        )
        .unwrap();
        let out = missing_modules(&tmp, "me/app");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["source"], json!("spec-drift"));
        assert_eq!(out[0]["repo"], json!("me/app"));
        assert_eq!(out[0]["symptomHash"], json!("drift-module-ghost"));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn missing_modules_empty_on_synced_repo() {
        assert!(missing_modules(&app_root(), "me/app").is_empty());
    }

    #[tokio::test]
    async fn sync_check_reports_failing_tool() {
        // tools/ が無い root では node が非0終了 → drift-diagrams 候補
        // （node 不在環境ではスキップ=None になるため Some の場合のみ検証）
        let tmp = tmp_dir("sync");
        std::fs::create_dir_all(&tmp).unwrap();
        if let Some(c) = sync_check(&tmp, "", "sync-diagrams.mjs", "drift-diagrams").await {
            assert_eq!(c["source"], json!("spec-drift"));
            assert_eq!(c["symptomHash"], json!("drift-diagrams"));
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn poll_clean_on_synced_repo() {
        let out = poll(&json!({}), &app_root()).await;
        assert!(
            out.is_empty(),
            "unexpected candidates: {}",
            serde_json::to_string_pretty(&out).unwrap()
        );
    }
}
