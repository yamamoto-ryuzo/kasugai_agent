// registry/skills.rs — SkillOps: skill 定義の台帳（read-only 文書スキャン。
// skills.mjs 移植）。各種 skills/ ディレクトリ配下の SKILL.md を収集する。
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::util::{appdata, basename, home_dir};

// プロジェクト内で走査する skills ディレクトリ（devin 規約 + 他ツール互換）
const SKILL_DIRS: &[&str] = &[
    ".devin/skills",
    ".agents/skills",
    ".windsurf/skills",
    ".kiro/skills",
    ".claude/skills",
    ".github/skills",
    ".codex/skills",
];

// SKILL.md の frontmatter から name/description/status を軽く拾う（YAML 非依存）
// status は skill-lifecycle.mmd の draft|active|deprecated
fn read_meta(path: &Path) -> (Option<String>, Option<String>, Option<String>) {
    let Ok(head) = std::fs::read_to_string(path) else {
        return (None, None, None);
    };
    let head = &head[..head.len().min(4000)];
    let re = regex::Regex::new(r"(?s)^---\r?\n(.*?)\r?\n---").unwrap();
    let Some(m) = re.captures(head) else {
        return (None, None, None);
    };
    let pick = |k: &str| {
        let r = regex::Regex::new(&format!(r#"(?m)^{}:\s*["']?(.+?)["']?\s*$"#, k)).unwrap();
        r.captures(&m[1]).map(|c| c[1].to_string())
    };
    (pick("name"), pick("description"), pick("status"))
}

// <dir>/<name>/SKILL.md を収集
fn scan_skills_dir(dir: &Path, scope: &str, source: &str) -> Vec<Value> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path().join("SKILL.md");
            if !p.is_file() {
                continue;
            }
            let (name, desc, status) = read_meta(&p);
            let dir_name = e.file_name().to_string_lossy().into_owned();
            out.push(json!({
                "name": name.unwrap_or_else(|| dir_name.clone()),
                "kind": "skill",
                "path": p.to_string_lossy().replace('\\', "/"),
                "scope": scope,
                "source": source,
                "dir": dir_name,
                "desc": desc.unwrap_or_default(),
                "status": status.unwrap_or_default(),
            }));
        }
    }
    out
}

// グローバル配置: %APPDATA%/devin/skills, ~/.config/devin/skills,
// ~/.devin/skills, ~/.agents/skills, ~/.codeium/<channel>/skills,
// Devin アプリ同梱 session skills, ~/.devin/extensions/<ext>/<SKILL_DIRS>
fn scan_global(devin_exe: Option<&str>) -> Vec<Value> {
    let mut out = Vec::new();
    let home = home_dir();
    out.extend(scan_skills_dir(&appdata().join("devin").join("skills"), "global", "devin"));
    out.extend(scan_skills_dir(&home.join(".config/devin/skills"), "global", "devin"));
    out.extend(scan_skills_dir(&home.join(".devin/skills"), "global", "devin"));
    out.extend(scan_skills_dir(&home.join(".agents/skills"), "global", ".agents"));
    let codeium = home.join(".codeium");
    if let Ok(rd) = std::fs::read_dir(&codeium) {
        for e in rd.flatten() {
            out.extend(scan_skills_dir(
                &e.path().join("skills"),
                "global",
                &format!("codeium/{}", e.file_name().to_string_lossy()),
            ));
        }
    }
    // Devin アプリ同梱の session skills（devin.exe の位置から resources/app を逆算）
    let app_root: PathBuf = devin_exe
        .map(|e| {
            Path::new(e)
                .parent()
                .map(|p| p.join("../../../../").to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."))
        })
        .unwrap_or_else(|| {
            std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join("Programs/Devin/resources/app")
        });
    out.extend(scan_skills_dir(
        &app_root.join("out/vs/sessions/skills"),
        "global",
        "devin-app",
    ));
    // Devin 拡張（VSIX）同梱の skills
    let ext_root = home.join(".devin/extensions");
    if let Ok(rd) = std::fs::read_dir(&ext_root) {
        for e in rd.flatten() {
            for d in SKILL_DIRS {
                out.extend(scan_skills_dir(
                    &e.path().join(d),
                    "global",
                    &format!("ext/{}", e.file_name().to_string_lossy()),
                ));
            }
        }
    }
    out
}

// roots: [workspace ROOT, ...config.repos[].path]
// opts.devinExe: アプリ同梱 skills の探索起点（config.devinExe）
pub fn list_skill_docs(roots: &[String], devin_exe: Option<&str>) -> Vec<Value> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let roots_uniq: HashSet<String> = roots.iter().filter(|r| !r.is_empty()).cloned().collect();
    for root in roots_uniq {
        let scope = {
            let b = basename(&root);
            if b.is_empty() { root.clone() } else { b }
        };
        for dir in SKILL_DIRS {
            for d in scan_skills_dir(&Path::new(&root).join(dir), &scope, dir) {
                let p = d["path"].as_str().unwrap_or("").to_string();
                if seen.insert(p) {
                    out.push(d);
                }
            }
        }
    }
    for d in scan_global(devin_exe) {
        let p = d["path"].as_str().unwrap_or("").to_string();
        if seen.insert(p) {
            out.push(d);
        }
    }
    out
}
