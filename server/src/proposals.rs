// proposals.rs — proposals の格納・機械検査（vet）
//
// 採否は人ゲートのまま。vet は採否の「前提検査」を機械化した第一歩:
// vet.ok != true の提案は accept を拒否する（decline は常に可）。
// 判定はすべて決定的な静的検査で、AI 評価は含めない。
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::util::*;

/// 提案が変更対象にできるパス（ws_root 相対プレフィックス）。
/// 生成物（docs/*.html・state等）・コード（server/, src/）・secrets は対象外。
const ALLOWED_PREFIXES: &[&str] = &["prompts/", "diagrams/", "skills/", ".devin/skills/"];
const ALLOWED_FILES: &[&str] = &["AGENTS.md", "README.md", "config.json"];

const KINDS: &[&str] = &["skill", "prompt", "config", "doc", "other"];

/// シークレット混入検査パターン（保守的に落とす）
const SECRET_PATTERNS: &[&str] = &[
    r"sk-[A-Za-z0-9_-]{20,}",
    r"ghp_[A-Za-z0-9]{20,}",
    r"github_pat_[A-Za-z0-9_]{20,}",
    r"xox[baprs]-[A-Za-z0-9-]{10,}",
    r"AKIA[0-9A-Z]{16}",
    r"AIza[0-9A-Za-z_-]{35}",
    r"-----BEGIN [A-Z ]*PRIVATE KEY-----",
    r#"(?i)(api[_-]?key|secret|token|password)\s*[:=]\s*["'][^"'\s]{12,}"#,
];

fn secret_hit(content: &str) -> Option<String> {
    for pat in SECRET_PATTERNS {
        if let Ok(re) = regex::Regex::new(pat) {
            if let Some(m) = re.find(content) {
                return Some(format!("pattern {} matched at {}", pat, m.start()));
            }
        }
    }
    None
}

/// ws_root 相対の安全なパスなら正規化結果を返す。絶対パス・..・~・NUL は拒否。
fn safe_rel(p: &str) -> Option<String> {
    let s = norm_slashes(p.trim());
    if s.is_empty() || s.contains('\0') || s.contains('~') {
        return None;
    }
    if s.starts_with('/') || s.get(1..2) == Some(":") {
        return None;
    }
    if s.split('/').any(|seg| seg == "..") {
        return None;
    }
    let n = normalize_lexical(&s);
    if n.is_empty() {
        return None;
    }
    Some(n)
}

/// frontmatter `---` ブロック内に `title:` があるか（.mmd / diagrams/docs の md）
fn frontmatter_title_ok(c: &str) -> bool {
    let t = c
        .trim_start_matches('\u{feff}')
        .trim_start_matches(['\r', '\n']);
    let Some(rest) = t.strip_prefix("---") else {
        return false;
    };
    let end = rest.find("\n---").unwrap_or(rest.len());
    rest[..end]
        .lines()
        .any(|l| l.trim_start().starts_with("title:"))
}

fn chk(name: &str, path: &str, ok: bool, detail: &str) -> Value {
    json!({"name": name, "path": path, "ok": ok, "detail": detail})
}

/// proposal を機械検査する。戻り値 {ok, checks[], advisories[]}
pub fn vet(p: &Value, ws_root: &str) -> Value {
    let mut checks: Vec<Value> = vec![];
    let mut advisories: Vec<String> = vec![];

    // shape: 提案の外形（title/kind/changes）
    let title_ok = p
        .get("title")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.trim().is_empty());
    let kind = p.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    let kind_ok = KINDS.contains(&kind);
    let changes = p
        .get("changes")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let ch_shape_ok = !changes.is_empty()
        && changes.iter().all(|c| {
            c.get("path").and_then(|v| v.as_str()).is_some_and(|s| !s.trim().is_empty())
                && c.get("content").map(|v| v.is_string()).unwrap_or(false)
        });
    checks.push(chk(
        "shape",
        "-",
        title_ok && kind_ok && ch_shape_ok,
        &format!(
            "title:{} kind:{} changes:{}",
            if title_ok { "ok" } else { "ng" },
            if kind_ok { kind } else { "invalid" },
            changes.len()
        ),
    ));
    if !ch_shape_ok {
        return json!({"ok": false, "checks": checks, "advisories": advisories});
    }

    let ws_n = normalize_lexical(&norm_slashes(ws_root)).to_lowercase();
    for c in &changes {
        let raw_path = c.get("path").and_then(|v| v.as_str()).unwrap_or("");
        let content = c.get("content").and_then(|v| v.as_str()).unwrap_or("");

        // path-traversal: 相対パスのみ・..禁止
        let rel = safe_rel(raw_path);
        checks.push(chk(
            "path-traversal",
            raw_path,
            rel.is_some(),
            if rel.is_some() { "relative, no .." } else { "absolute/../~/NUL rejected" },
        ));
        let Some(rel) = rel else { continue };

        // within-root: 解決後が ws_root 配下に留まる
        let abs = normalize_lexical(&format!("{}/{}", norm_slashes(ws_root), rel)).to_lowercase();
        checks.push(chk(
            "within-root",
            &rel,
            abs.starts_with(&format!("{}/", ws_n)) || abs == ws_n,
            &abs,
        ));

        // path-allowlist: 変更対象の許可
        let lower = rel.to_lowercase();
        let allowed = ALLOWED_PREFIXES.iter().any(|pre| lower.starts_with(pre))
            || ALLOWED_FILES.iter().any(|f| rel == *f);
        checks.push(chk(
            "path-allowlist",
            &rel,
            allowed,
            if allowed { "allowed target" } else { "not a prompts/diagrams/skills/正本 target" },
        ));

        // no-secrets: 内容にシークレットらしき文字列を含まない
        let hit = secret_hit(content);
        checks.push(chk(
            "no-secrets",
            &rel,
            hit.is_none(),
            hit.as_deref().unwrap_or("clean"),
        ));

        // json-parse: .json 対象は構文が通る
        if rel.to_lowercase().ends_with(".json") {
            let ok = serde_json::from_str::<Value>(content).is_ok();
            checks.push(chk("json-parse", &rel, ok, if ok { "valid json" } else { "json syntax error" }));
        }

        // frontmatter: .mmd / diagrams/docs/*.md は title frontmatter 必須
        let need_fm = rel.to_lowercase().ends_with(".mmd")
            || (rel.to_lowercase().starts_with("diagrams/docs/") && rel.to_lowercase().ends_with(".md"));
        if need_fm {
            let ok = frontmatter_title_ok(content);
            checks.push(chk("frontmatter", &rel, ok, if ok { "title frontmatter" } else { "missing title frontmatter" }));
        }

        // advisory（vet 結果には影響しない適用後の注意）
        if rel.to_lowercase().starts_with("diagrams/") {
            advisories.push(format!("{}: sync 要（sync-diagrams/sync-docs を実行）", rel));
        }
        if rel == "config.json" || rel.to_lowercase().starts_with("prompts/") {
            advisories.push(format!("{}: 反映には再起動/再読込が必要", rel));
        }
    }

    let ok = checks.iter().all(|c| c.get("ok").and_then(|v| v.as_bool()).unwrap_or(false));
    json!({"ok": ok, "checks": checks, "advisories": advisories})
}

/// vet 結果を付与した proposal（API 返却用）。
/// 決定済みは保存時の vet 結果を使い、pending は都度再評価する
pub fn with_vet(p: &Value, ws_root: &str) -> Value {
    let mut o = p.clone();
    let pending = p.get("status").and_then(|v| v.as_str()) == Some("pending");
    let verdict = if pending {
        vet(p, ws_root)
    } else {
        p.get("vet").cloned().unwrap_or_else(|| vet(p, ws_root))
    };
    if let Some(m) = o.as_object_mut() {
        m.insert("vet".into(), verdict);
    }
    o
}

pub fn dir(root: &Path) -> PathBuf {
    root.join("proposals")
}

fn path_of(dir: &Path, id: &str) -> Option<PathBuf> {
    // id は英数・-・_ のみ（パス注入防止）
    if id.is_empty()
        || id.len() > 128
        || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    Some(dir.join(format!("{}.json", id)))
}

/// proposals/*.json を id 昇順で読む（id 先頭は時刻なので時系列順）
pub fn list(dir: &Path) -> Vec<Value> {
    let mut out: Vec<Value> = vec![];
    if let Ok(rd) = std::fs::read_dir(dir) {
        let mut names: Vec<String> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_str().map(String::from))
            .filter(|n| n.ends_with(".json"))
            .collect();
        names.sort();
        for n in names {
            if let Ok(s) = std::fs::read_to_string(dir.join(&n)) {
                if let Ok(v) = serde_json::from_str::<Value>(&s) {
                    out.push(v);
                }
            }
        }
    }
    out
}

pub fn load(dir: &Path, id: &str) -> Option<Value> {
    let p = path_of(dir, id)?;
    let s = std::fs::read_to_string(p).ok()?;
    serde_json::from_str(&s).ok()
}

pub fn save(dir: &Path, p: &Value) -> bool {
    let Some(id) = p.get("id").and_then(|v| v.as_str()) else { return false };
    let Some(path) = path_of(dir, id) else { return false };
    let _ = std::fs::create_dir_all(dir);
    std::fs::write(path, serde_json::to_string_pretty(p).unwrap_or_default()).is_ok()
}

/// 新規 proposal を作成して保存（status=pending）
pub fn create(dir: &Path, a: &Value) -> Value {
    let changes = a.get("changes").cloned().unwrap_or(json!([]));
    let p = json!({
        "id": format!("p-{}-{}", now_millis(), rand_hex(3)),
        "title": a.get("title").and_then(|v| v.as_str()).unwrap_or(""),
        "kind": a.get("kind").and_then(|v| v.as_str()).unwrap_or("other"),
        "changes": changes,
        "rationale": a.get("rationale").and_then(|v| v.as_str()).unwrap_or(""),
        "issueKey": a.get("issueKey").cloned().unwrap_or(Value::Null),
        "status": "pending",
        "createdAt": now_iso(),
    });
    save(dir, &p);
    p
}

/// 採否の決定。accept は vet.ok のときのみ。戻り値 (http_status, body)
pub fn decide(dir: &Path, id: &str, accept: bool, ws_root: &str) -> (u16, Value) {
    let Some(mut p) = load(dir, id) else {
        return (404, json!({"ok": false, "error": "not found"}));
    };
    if p.get("status").and_then(|v| v.as_str()) != Some("pending") {
        return (409, json!({"ok": false, "error": "already decided", "proposal": with_vet(&p, ws_root)}));
    }
    let verdict = vet(&p, ws_root);
    if accept && verdict.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        // 機械検査不合格は accept 不能（人ゲートの前提検査）
        return (409, json!({"ok": false, "error": "vet failed", "vet": verdict}));
    }
    let o = p.as_object_mut().unwrap();
    o.insert("status".into(), json!(if accept { "accepted" } else { "declined" }));
    o.insert("decidedAt".into(), json!(now_iso()));
    o.insert("vet".into(), verdict);
    save(dir, &p);
    (200, json!({"ok": true, "proposal": p}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prop(path: &str, content: &str) -> Value {
        json!({
            "title": "t", "kind": "prompt",
            "changes": [{"path": path, "content": content}],
        })
    }
    fn vet_ok(p: &Value) -> bool {
        vet(p, "/ws").get("ok").and_then(|v| v.as_bool()).unwrap_or(false)
    }

    #[test]
    fn allows_prompts_change() {
        assert!(vet_ok(&prop("prompts/assess.md", "new prompt")));
    }
    #[test]
    fn rejects_traversal_and_absolute() {
        assert!(!vet_ok(&prop("../x.md", "x")));
        assert!(!vet_ok(&prop("prompts/../../x.md", "x")));
        assert!(!vet_ok(&prop("/etc/x", "x")));
        assert!(!vet_ok(&prop("C:/tmp/x", "x")));
    }
    #[test]
    fn rejects_disallowed_paths() {
        assert!(!vet_ok(&prop("server/src/main.rs", "x")));
        assert!(!vet_ok(&prop("docs/index.html", "x")));
        assert!(!vet_ok(&prop("secrets.json", "x")));
        assert!(!vet_ok(&prop("state.json", "x")));
    }
    #[test]
    fn rejects_secret_in_content() {
        assert!(!vet_ok(&prop(
            "prompts/x.md",
            "key = sk-abcdefghijklmnopqrstuvwxyz123456"
        )));
        assert!(!vet_ok(&prop(
            "prompts/x.md",
            "-----BEGIN RSA PRIVATE KEY-----\nMII"
        )));
    }
    #[test]
    fn rejects_bad_json_and_missing_frontmatter() {
        assert!(!vet_ok(&prop("config.json", "{oops")));
        assert!(vet_ok(&prop("config.json", "{\"a\":1}")));
        assert!(!vet_ok(&prop("diagrams/x.mmd", "flowchart LR\nA-->B")));
        assert!(vet_ok(&prop(
            "diagrams/x.mmd",
            "---\ntitle: x\n---\nflowchart LR\nA-->B"
        )));
        assert!(vet_ok(&prop(
            "diagrams/docs/9-9-x.md",
            "---\ntitle: x\n---\nbody"
        )));
    }
    #[test]
    fn rejects_bad_shape() {
        assert!(!vet_ok(&json!({"title":"","kind":"prompt","changes":[]})));
        assert!(!vet_ok(&json!({"title":"t","kind":"bogus","changes":[{"path":"prompts/a","content":"b"}]})));
    }
}
