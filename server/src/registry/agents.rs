// registry/agents.rs — AgentOps: agent 定義・ルール文書の台帳
// （read-only 文書スキャン。agents.mjs 移植）
// AGENTS.md 系・.devin/rules・.devin/agents 定義・グローバル配置を収集する。
// 収集した path だけが GET /api/doc の本文返却対象になる（secrets 等は拾わない）。
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::util::{appdata, basename, home_dir, is_dir, is_file};

const RULE_FILES: &[&str] = &[
    "AGENTS.md",
    "AGENTS.local.md",
    "AGENT.md",
    "CLAUDE.md",
    ".windsurfrules",
];
// agents/<name>/ 配下の定義ファイル名（優先順。先に見つかった方を採用）
const AGENT_DEF_NAMES: &[&str] = &["AGENT.md", "AGENTS.md", "agent.md", "agents.md"];

fn md_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file()
                && p.extension()
                    .map(|x| x.to_string_lossy().to_lowercase() == "md")
                    .unwrap_or(false)
            {
                out.push(p);
            }
        }
    }
    out
}

fn entry(name: String, kind: &str, path: PathBuf, scope: &str) -> Value {
    json!({"name": name, "kind": kind, "path": path.to_string_lossy().replace('\\', "/"), "scope": scope})
}

// agents/ 配下: フラット agents/<name>.md と ディレクトリ agents/<name>/AGENT.md
fn agent_defs(dir: &Path, scope: &str) -> Vec<Value> {
    let mut out: Vec<Value> = md_files(dir)
        .into_iter()
        .map(|p| {
            entry(
                p.file_stem().unwrap().to_string_lossy().into_owned(),
                "agent",
                p,
                scope,
            )
        })
        .collect();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let sub = e.path();
            if !sub.is_dir() {
                continue;
            }
            if let Some(def) = AGENT_DEF_NAMES
                .iter()
                .map(|n| sub.join(n))
                .find(|p| p.is_file())
            {
                out.push(entry(
                    e.file_name().to_string_lossy().into_owned(),
                    "agent",
                    def,
                    scope,
                ));
            }
        }
    }
    out
}

// .devin/ 相当ディレクトリ内のルール + agents 定義を収集
fn scan_conf_dir(dir: &Path, scope: &str, prefix: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let gr = dir.join("global_rules.md");
    if gr.is_file() {
        out.push(entry(
            format!("{}global_rules.md", prefix),
            "rules",
            gr,
            scope,
        ));
    }
    for p in md_files(&dir.join("rules")) {
        out.push(entry(
            format!("rules/{}", p.file_name().unwrap().to_string_lossy()),
            "rules",
            p,
            scope,
        ));
    }
    out.extend(agent_defs(&dir.join("agents"), scope));
    out
}

// ルート直下のルールファイル + .devin/.agents 配下を収集
fn scan_root(root: &Path, scope: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for f in RULE_FILES {
        let p = root.join(f);
        if p.is_file() {
            out.push(entry(f.to_string(), "rules", p, scope));
        }
    }
    out.extend(scan_conf_dir(&root.join(".devin"), scope, ".devin/"));
    out.extend(agent_defs(&root.join(".agents").join("agents"), scope));
    out
}

// グローバル配置: %APPDATA%/devin, ~/.config/devin, ~/.devin, ~/.agents
fn scan_global() -> Vec<Value> {
    let mut out = Vec::new();
    let home = home_dir();
    for conf in [
        appdata().join("devin"),
        home.join(".config").join("devin"),
    ] {
        for f in ["AGENTS.md", "AGENTS.local.md", "AGENT.md"] {
            let p = conf.join(f);
            if p.is_file() {
                out.push(entry(f.to_string(), "rules", p, "global"));
            }
        }
        out.extend(scan_conf_dir(&conf, "global", ""));
    }
    out.extend(scan_conf_dir(&home.join(".devin"), "global", "~/.devin/"));
    out.extend(agent_defs(&home.join(".agents").join("agents"), "global"));
    out
}

// roots: [workspace ROOT, ...config.repos[].path]
pub fn list_agent_docs(roots: &[String]) -> Vec<Value> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let roots_uniq: HashSet<String> = roots.iter().filter(|r| !r.is_empty()).cloned().collect();
    for root in roots_uniq {
        let scope = basename(&root);
        let scope = if scope.is_empty() { root.clone() } else { scope };
        for d in scan_root(Path::new(&root), &scope) {
            let p = d["path"].as_str().unwrap_or("").to_string();
            if seen.insert(p) {
                out.push(d);
            }
        }
    }
    for d in scan_global() {
        let p = d["path"].as_str().unwrap_or("").to_string();
        if seen.insert(p) {
            out.push(d);
        }
    }
    let _ = is_dir;
    let _ = is_file;
    out
}
