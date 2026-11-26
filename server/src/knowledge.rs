// knowledge.rs — learn（知見の還流。正本: diagrams/learn-flow.mmd、README §5.6）
//
// (a) 課題 key 単位の知見蓄積: runs[] または evidence/<key>/ が存在し
//     knowledge/<slug>.md が未作成の課題を拾い、run 結果・証跡・課題本文を
//     要約した生の知見を無条件で生成する（採否不要）。
// (b) SKILL draft 起票: 知見から再利用可能な手順・注意を
//     .devin/skills/<slug>/SKILL.md（frontmatter: name/description/
//     status: draft、本文に mermaid 図必須）として組立、proposals::create
//     （kind="skill"・issueKey=<key>）で起票する。採否は人ゲートで、vet が
//     前提検査を行う。同じ path の pending proposal がある場合は再作成しない。
//
// issue key は `source:repo:hash` 形式で `:` `/` を含みファイル名に使えない
// ため、ファイル/ディレクトリ名には key_slug を用いる。
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::proposals;
use crate::store;
use crate::util::now_iso;

/// issue key → ファイルシステム安全な slug（非英数の連続を `-` に畳み小文字化）
pub fn key_slug(key: &str) -> String {
    let mut s = String::with_capacity(key.len());
    let mut dash = true; // 先頭の `-` を抑止
    for c in key.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            s.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash {
            s.push('-');
            dash = true;
        }
    }
    while s.ends_with('-') {
        s.pop();
    }
    if s.len() > 120 {
        s.truncate(120);
        while s.ends_with('-') {
            s.pop();
        }
    }
    if s.is_empty() {
        "issue".to_string()
    } else {
        s
    }
}

pub fn dir(root: &Path) -> PathBuf {
    root.join("knowledge")
}

/// prompts/<name>.md を読むプロンプト差し替え口（runner/detector/verify 等用）。
/// name は英数・-・_ のみ（パストラバーサル防止）。無い/空なら None を返し、
/// 呼び出し側は組込既定へフォールバックする。
pub fn load_prompt(root: &Path, name: &str) -> Option<String> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    let s = std::fs::read_to_string(root.join("prompts").join(format!("{}.md", name))).ok()?;
    if s.trim().is_empty() {
        None
    } else {
        Some(s)
    }
}

/// evidence/<key>/ が存在すれば (dir, ファイル名一覧)。生キー（`:` 入りは
//  Windows では不可だが他 OS 互換）と slug 形の両方を見る。
fn evidence_dir(root: &Path, key: &str, slug: &str) -> Option<Vec<String>> {
    for cand in [
        root.join("evidence").join(key),
        root.join("evidence").join(slug),
    ] {
        if cand.is_dir() {
            let mut files: Vec<String> = std::fs::read_dir(&cand)
                .map(|rd| {
                    rd.flatten()
                        .filter_map(|e| e.file_name().to_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            files.sort();
            return Some(files);
        }
    }
    None
}

fn take(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn one_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

/// 課題1件の生の知見（knowledge/<slug>.md の中身）
fn knowledge_md(issue: &Value, evidence_files: &[String]) -> String {
    let get = |k: &str| issue.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let mut out = String::new();
    out.push_str(&format!("# 知見: {}\n\n", take(get("title"), 120)));
    out.push_str(&format!("- key: `{}`\n", get("key")));
    out.push_str(&format!("- source: `{}`\n- repo: `{}`\n", get("source"), get("repo")));
    out.push_str(&format!(
        "- status: `{}` / verifyLevel: {}\n",
        get("status"),
        issue
            .get("verifyLevel")
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".into())
    ));
    out.push_str(&format!("- 生成: {}\n\n", now_iso()));

    let body = get("body").trim();
    if !body.is_empty() {
        out.push_str("## 課題\n\n");
        out.push_str(&take(body, 2000));
        out.push_str("\n\n");
    }

    let runs = issue
        .get("runs")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if !runs.is_empty() {
        out.push_str("## run 結果\n\n");
        for (i, r) in runs.iter().enumerate() {
            let line = if let Some(s) = r.as_str() {
                take(s.trim(), 400)
            } else {
                let mut parts: Vec<String> = vec![];
                for k in [
                    "chat", "startedAt", "endedAt", "result", "summary", "verdict", "note",
                    "error",
                ] {
                    if let Some(v) = r.get(k).and_then(|x| x.as_str()).filter(|s| !s.is_empty())
                    {
                        parts.push(format!("{}: {}", k, take(&one_line(v), 200)));
                    }
                }
                if parts.is_empty() {
                    take(&serde_json::to_string(r).unwrap_or_default(), 300)
                } else {
                    parts.join(" / ")
                }
            };
            out.push_str(&format!("- run {}: {}\n", i + 1, line));
        }
        out.push('\n');
    }

    if !evidence_files.is_empty() {
        out.push_str("## 証跡\n\n");
        for f in evidence_files {
            out.push_str(&format!("- `{}`\n", f));
        }
        out.push('\n');
    }
    out
}

/// 知見 → .devin/skills/<slug>/SKILL.md の draft 案（mermaid 図必須）
fn skill_md(issue: &Value, slug: &str) -> String {
    let get = |k: &str| issue.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let title = take(get("title"), 120);
    let desc = take(
        &one_line(get("title"))
            .replace(['"', '\''], "")
            .trim()
            .to_string(),
        100,
    );
    let level = issue
        .get("verifyLevel")
        .map(|v| v.to_string())
        .unwrap_or_else(|| "1".into());
    format!(
        r#"---
name: {slug}
description: {desc}
status: draft
---

# {title}

課題 `{key}` の対応から得た知見を skill 化した draft 案。生の知見は
`knowledge/{slug}.md` を参照。採否は proposals の人ゲートで行う
（vet 不合格は accept 不能）。

## 手順

1. 課題の再現条件を確認する（repo: `{repo}`）
2. 実装し、`Issue: {key}` トレーラ付きで git commit する
3. verifyLevel L{level} に従って実機検証し、証跡を `evidence/` に残す
4. 実施内容・結果を簡潔に報告する

## 注意

- 状態遷移は pipeline のみが行う。issues.json を直接編集しない
- 知見の反映は proposals 経由（`.devin/skills` を直接編集しない）

```mermaid
flowchart LR
    REPRO[課題の確認] --> IMPL[実装<br/>Issue: key トレーラ]
    IMPL --> VERIFY[実機検証<br/>evidence/ へ証跡]
    VERIFY --> REPORT[結果報告]
```
"#,
        slug = slug,
        desc = if desc.is_empty() { slug } else { &desc },
        title = title,
        key = get("key"),
        repo = get("repo"),
        level = level,
    )
}

/// learn の1回分。戻り値 (知見ファイル生成数, proposal 起票数)。
/// control.rs の watcher から定期呼び出しされる。
pub fn learn_pass(root: &Path) -> (usize, usize) {
    let issues = store::load_issues(root);
    if issues.is_empty() {
        return (0, 0);
    }
    let kdir = dir(root);
    let pdir = proposals::dir(root);
    let pending = proposals::list(&pdir);
    let has_pending = |path: &str| {
        pending.iter().any(|p| {
            p.get("status").and_then(|v| v.as_str()) == Some("pending")
                && p.get("changes").and_then(|v| v.as_array()).is_some_and(|cs| {
                    cs.iter()
                        .any(|c| c.get("path").and_then(|v| v.as_str()) == Some(path))
                })
        })
    };
    let mut kn = 0;
    let mut pn = 0;
    for issue in &issues {
        let Some(key) = issue.get("key").and_then(|v| v.as_str()) else {
            continue;
        };
        let slug = key_slug(key);
        let kpath = kdir.join(format!("{}.md", slug));
        if kpath.is_file() {
            continue;
        }
        let has_runs = issue
            .get("runs")
            .and_then(|v| v.as_array())
            .is_some_and(|a| !a.is_empty());
        let ev = evidence_dir(root, key, &slug);
        if !has_runs && ev.is_none() {
            continue;
        }
        // (a) 生の知見を無条件蓄積
        let _ = std::fs::create_dir_all(&kdir);
        if std::fs::write(&kpath, knowledge_md(issue, &ev.unwrap_or_default())).is_err() {
            continue;
        }
        kn += 1;
        // (b) SKILL draft 起票（同 path の pending proposal があれば作らない）
        let rel = format!(".devin/skills/{}/SKILL.md", slug);
        if has_pending(&rel) {
            continue;
        }
        proposals::create(
            &pdir,
            &json!({
                "title": format!("skill draft: {}", slug),
                "kind": "skill",
                "changes": [{"path": rel, "content": skill_md(issue, &slug)}],
                "rationale": format!(
                    "課題 {} の対応知見（knowledge/{}.md）から再利用可能な手順を SKILL draft 化",
                    key, slug
                ),
                "issueKey": key,
            }),
        );
        pn += 1;
    }
    (kn, pn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::save_j;

    fn temp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "kasugai-knowledge-test-{}-{}",
            tag,
            crate::util::rand_hex(4)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn issue(key: &str, runs: Value) -> Value {
        json!({
            "key": key, "source": "github", "repo": "me/repo",
            "title": "テスト課題", "body": "再現手順など",
            "status": "done", "verifyLevel": 2, "runs": runs,
        })
    }

    #[test]
    fn creates_knowledge_and_skill_proposal() {
        let root = temp_root("a");
        let key = "github:me/repo:abc123";
        save_j(
            &root.join("issues.json"),
            &json!([issue(key, json!([{"result": "ok", "summary": "修正済み"}]))]),
        );
        let (kn, pn) = learn_pass(&root);
        assert_eq!((kn, pn), (1, 1));
        let slug = key_slug(key);
        assert!(root.join(format!("knowledge/{}.md", slug)).is_file());
        let props = proposals::list(&proposals::dir(&root));
        assert_eq!(props.len(), 1);
        assert_eq!(props[0].get("kind").and_then(|v| v.as_str()), Some("skill"));
        assert_eq!(
            props[0].get("issueKey").and_then(|v| v.as_str()),
            Some(key)
        );
        let path = props[0]["changes"][0].get("path").and_then(|v| v.as_str());
        assert_eq!(path, Some(format!(".devin/skills/{}/SKILL.md", slug).as_str()));
        // vet が通る内容であること
        assert!(proposals::vet(&props[0], &root.to_string_lossy())
            .get("ok")
            .and_then(|v| v.as_bool())
            .unwrap_or(false));
        // 2回目は何もしない（冪等）
        assert_eq!(learn_pass(&root), (0, 0));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn picks_up_evidence_only_issue() {
        let root = temp_root("b");
        let key = "local:/x/y:deadbeef";
        save_j(
            &root.join("issues.json"),
            &json!([issue(key, json!([]))]),
        );
        std::fs::create_dir_all(root.join("evidence").join(key_slug(key))).unwrap();
        let (kn, pn) = learn_pass(&root);
        assert_eq!((kn, pn), (1, 1));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skips_empty_issue_and_dedupes_pending() {
        let root = temp_root("c");
        let key = "github:me/repo:nope";
        // runs も evidence も無い → 対象外
        save_j(&root.join("issues.json"), &json!([issue(key, json!([]))]));
        assert_eq!(learn_pass(&root), (0, 0));
        // 同 path の pending proposal 済み → proposal は作らない
        let slug = key_slug(key);
        proposals::create(
            &proposals::dir(&root),
            &json!({
                "title": "既存", "kind": "skill",
                "changes": [{"path": format!(".devin/skills/{}/SKILL.md", slug), "content": "x"}],
            }),
        );
        save_j(
            &root.join("issues.json"),
            &json!([issue(key, json!(["run1"]))]),
        );
        assert_eq!(learn_pass(&root), (1, 0));
        assert!(root.join(format!("knowledge/{}.md", slug)).is_file());
        assert_eq!(proposals::list(&proposals::dir(&root)).len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn load_prompt_reads_prompts_dir() {
        let root = temp_root("d");
        std::fs::create_dir_all(root.join("prompts")).unwrap();
        std::fs::write(root.join("prompts/verify.md"), "VERIFY TEMPLATE").unwrap();
        assert_eq!(load_prompt(&root, "verify").as_deref(), Some("VERIFY TEMPLATE"));
        assert_eq!(load_prompt(&root, "missing"), None);
        assert_eq!(load_prompt(&root, "../etc/passwd"), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
