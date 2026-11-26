// knowledge.rs — learn（知見の還流。正本: diagrams/learn-flow.mmd、README §5.6）
//
// (a) 課題 key 単位の知見蓄積: runs[] または evidence/<key>/ が存在し
//     knowledge/<slug>.md が未作成の課題を拾い、run 結果・証跡・課題本文を
//     要約した生の知見を無条件で生成する（採否不要）。
// (b) SKILL draft 起票: 知見から再利用可能な手順・注意を
//     .devin/skills/<slug>/SKILL.md（frontmatter: name/description/
//     status: draft、本文に mermaid 図必須）として組立、proposals::create
//     （kind="skill"・issueKey=<key>）で起票する。採否は人ゲートで、vet が
//     前提検査を行う。
//     slug = source+repo+kind の安定名で mermaid 図の title と同名。
//     key（run id 等の揮発 id）は含まないため、同じ種別の課題は同じ
//     skill 名に収束する。同 path の proposal（pending/decided 問わず）
//     または同一内容があれば再作成しない。
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

/// prompts/runbook/<name>.md を `<source>-<kind>` → `<kind>` → `<source>` の
/// 順に探して最初の一致を返す（runbook.mmd・README §5.4）。
/// source/kind は key_slug でファイル名安全化する（揮発 id は除去される）。
/// 戻り値は (解決した name, 本文)。
pub fn load_runbook(root: &Path, source: &str, kind: &str) -> Option<(String, String)> {
    let dir = root.join("prompts").join("runbook");
    let src = key_slug(source);
    let knd = key_slug(kind);
    let mut names = Vec::new();
    if !src.is_empty() && !knd.is_empty() {
        names.push(format!("{}-{}", src, knd));
    }
    if !knd.is_empty() {
        names.push(knd);
    }
    if !src.is_empty() && names.last().map(|n| n != &src).unwrap_or(true) {
        names.push(src);
    }
    for n in names {
        if let Ok(b) = std::fs::read_to_string(dir.join(format!("{}.md", n))) {
            if !b.trim().is_empty() {
                return Some((n, b));
            }
        }
    }
    None
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

/// SKILL draft の安定 slug。「mermaid 図の題名 = skill 名」:
/// slug = source+repo+kind。課題 key（run id 等の揮発 id を含む）ではなく
/// 種別由来なので、同じ種別の課題は同じ skill 名に収束し重複が潰れる
/// （learn-flow.mmd）。kind 未設定の課題は source+repo に畳む。
fn skill_slug(issue: &Value) -> String {
    let get = |k: &str| issue.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let kind = get("kind");
    let base = if kind.is_empty() {
        format!("{}:{}", get("source"), get("repo"))
    } else {
        format!("{}:{}:{}", get("source"), get("repo"), kind)
    };
    key_slug(&base)
}

/// 知見 → .devin/skills/<slug>/SKILL.md の draft 案（mermaid 図必須・図 title = slug）
fn skill_md(issue: &Value, slug: &str, kslug: &str) -> String {
    let get = |k: &str| issue.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let kind = get("kind");
    let source = get("source");
    let topic = if kind.is_empty() { source } else { kind };
    // 題名は種別（kind）由来。課題題名（commit 題名等の揮発分）は見出しに使わない
    let title = format!("{}: {} の対応（{}）", source, topic, get("repo"));
    let desc = take(&format!("{} の対応手順（{}）", topic, get("repo")), 100);
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

課題 `{key}`（{issue_title}）の対応から得た知見を skill 化した draft 案。生の知見は
`knowledge/{kslug}.md` を参照。採否は proposals の人ゲートで行う
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
---
title: {slug}
---
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
        issue_title = take(get("title"), 120),
        kslug = kslug,
        repo = get("repo"),
        level = level,
    )
}

/// `.devin/skills/<slug>/SKILL.md` から slug を取り出す
fn skill_slug_of(path: &str) -> &str {
    path.strip_prefix(".devin/skills/")
        .and_then(|s| s.strip_suffix("/SKILL.md"))
        .unwrap_or("")
}

/// draft 本文の揮発部分（課題 key / skill slug / 知見ファイル名 / 課題題名）を
/// 正規化し、同一内容判定に使う。
/// CI run id 等を含む key が違うだけの再 run で同じ草案が量産されるのを抑止する。
/// slug は kslug の前置部になり得るため、先に kslug を潰してから slug を畳む。
fn norm_draft(c: &str, key: &str, slug: &str) -> String {
    let mut s = c.to_string();
    if !key.is_empty() {
        s = s.replace(key, "<key>");
        let ks = key_slug(key);
        if !ks.is_empty() && ks != slug {
            s = s.replace(&ks, "<kslug>");
        }
    }
    if !slug.is_empty() {
        s = s.replace(slug, "<slug>");
    }
    // `{key}`（課題題名）— 題名は commit 題名等の揮発分なので括弧ごと畳む
    if let Ok(re) = regex::Regex::new(r"`<key>`（[^）]*）") {
        s = re.replace_all(&s, "`<key>`（<title>）").into_owned();
    }
    s
}

/// 同 path の proposal（pending/decided 問わず）が既にあれば true。
/// 採否済みの skill 名を再提案しない＝同じ種別の重複起票を抑止。
fn has_same_path(existing: &[Value], path: &str) -> bool {
    existing.iter().any(|p| {
        p.get("changes").and_then(|v| v.as_array()).is_some_and(|cs| {
            cs.iter()
                .any(|c| c.get("path").and_then(|v| v.as_str()) == Some(path))
        })
    })
}

/// 同一内容の proposal が pending/declined/accepted のいずれかで既にあれば true
fn has_same_draft(existing: &[Value], content: &str, key: &str, slug: &str) -> bool {
    let want = norm_draft(content, key, slug);
    existing.iter().any(|p| {
        let ekey = p.get("issueKey").and_then(|v| v.as_str()).unwrap_or("");
        p.get("changes").and_then(|v| v.as_array()).is_some_and(|cs| {
            cs.iter().any(|c| {
                let ec = c.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let eslug = skill_slug_of(c.get("path").and_then(|v| v.as_str()).unwrap_or(""));
                !ec.is_empty() && norm_draft(ec, ekey, eslug) == want
            })
        })
    })
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
    let mut existing = proposals::list(&pdir);
    existing.extend(proposals::list(&proposals::decided_dir(&pdir)));
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
        // (b) SKILL draft 起票（同 slug path または同一内容の proposal があれば作らない）
        let sslug = skill_slug(issue);
        let rel = format!(".devin/skills/{}/SKILL.md", sslug);
        let draft = skill_md(issue, &sslug, &slug);
        if has_same_path(&existing, &rel) || has_same_draft(&existing, &draft, key, &sslug) {
            continue;
        }
        // 同一パス内で後続課題が同じ skill 名を見られるよう、
        // 起票した proposal を existing に即時反映する
        let created = proposals::create(
            &pdir,
            &json!({
                "title": format!("skill draft: {}", sslug),
                "kind": "skill",
                "changes": [{"path": rel, "content": draft}],
                "rationale": format!(
                    "課題 {} の対応知見（knowledge/{}.md）から再利用可能な手順を SKILL draft 化",
                    key, slug
                ),
                "issueKey": key,
            }),
        );
        existing.push(created);
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
        // skill 名は課題 key ではなく source+repo（kind なし）由来の安定名
        let sslug = "github-me-repo";
        assert_eq!(path, Some(format!(".devin/skills/{}/SKILL.md", sslug).as_str()));
        let content = props[0]["changes"][0]
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(content.contains(&format!("name: {}", sslug)));
        assert!(content.contains(&format!("title: {}", sslug))); // 図 title = skill 名
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
        proposals::create(
            &proposals::dir(&root),
            &json!({
                "title": "既存", "kind": "skill",
                "changes": [{"path": ".devin/skills/github-me-repo/SKILL.md", "content": "x"}],
            }),
        );
        save_j(
            &root.join("issues.json"),
            &json!([issue(key, json!(["run1"]))]),
        );
        assert_eq!(learn_pass(&root), (1, 0));
        assert!(root
            .join(format!("knowledge/{}.md", key_slug(key)))
            .is_file());
        assert_eq!(proposals::list(&proposals::dir(&root)).len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn suppresses_duplicate_draft_across_issue_keys() {
        let root = temp_root("e");
        let key1 = "github:me/repo:run-aaa";
        save_j(
            &root.join("issues.json"),
            &json!([issue(key1, json!(["run1"]))]),
        );
        assert_eq!(learn_pass(&root), (1, 1));

        // 人が decline → decided/ へ退避（揮発 id 違いの再 run で同じ草案が再登場しないことを確認する）
        let pdir = proposals::dir(&root);
        let id = proposals::list(&pdir)[0]
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_string();
        let (code, _) = proposals::decide(&pdir, &id, false, &root.to_string_lossy());
        assert_eq!(code, 200);
        assert!(proposals::list(&pdir).is_empty());
        assert_eq!(proposals::list(&proposals::decided_dir(&pdir)).len(), 1);

        let key2 = "github:me/repo:run-bbb";
        save_j(
            &root.join("issues.json"),
            &json!([issue(key1, json!(["run1"])), issue(key2, json!(["run2"]))]),
        );
        // key2 の知見は蓄積するが、同じ skill 名（source+repo）は decided 済みなので起票しない
        assert_eq!(learn_pass(&root), (1, 0));
        assert!(root
            .join(format!("knowledge/{}.md", key_slug(key2)))
            .is_file());
        assert!(proposals::list(&pdir).is_empty());
        assert_eq!(proposals::list(&proposals::decided_dir(&pdir)).len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn kind_distinguishes_and_converges_skill_names() {
        let root = temp_root("f");
        // kind（workflow 名等）が違えば別 skill 名、同じなら run id 違いでも同じ名に収束
        let mut i1 = issue("ci-failure:me/repo:ci-1", json!(["run1"]));
        i1["source"] = json!("ci-failure");
        i1["kind"] = json!("build");
        let mut i2 = issue("ci-failure:me/repo:ci-2", json!(["run2"]));
        i2["source"] = json!("ci-failure");
        i2["kind"] = json!("lint");
        // 同じ kind の別 run が同一パスにいても skill draft は1件だけ（パス内即時 dedupe）
        let mut i2b = issue("ci-failure:me/repo:ci-2b", json!(["run2b"]));
        i2b["source"] = json!("ci-failure");
        i2b["kind"] = json!("build");
        save_j(
            &root.join("issues.json"),
            &json!([i1.clone(), i2.clone(), i2b]),
        );
        assert_eq!(learn_pass(&root), (3, 2));
        let props = proposals::list(&proposals::dir(&root));
        let paths: Vec<&str> = props
            .iter()
            .filter_map(|p| p["changes"][0].get("path").and_then(|v| v.as_str()))
            .collect();
        assert!(paths.contains(&".devin/skills/ci-failure-me-repo-build/SKILL.md"));
        assert!(paths.contains(&".devin/skills/ci-failure-me-repo-lint/SKILL.md"));

        // 同じ kind の別 run は知見のみ蓄積し、同 skill 名の起票は抑止される
        let mut i3 = issue("ci-failure:me/repo:ci-3", json!(["run3"]));
        i3["source"] = json!("ci-failure");
        i3["kind"] = json!("build");
        save_j(
            &root.join("issues.json"),
            &json!([i1, i2, i3]),
        );
        assert_eq!(learn_pass(&root), (1, 0));
        assert!(root
            .join(format!("knowledge/{}.md", key_slug("ci-failure:me/repo:ci-3")))
            .is_file());
        assert_eq!(proposals::list(&proposals::dir(&root)).len(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn norm_draft_collapses_volatile_parts() {
        // key・skill slug・知見ファイル名・課題題名が違うだけの草案は同一とみなす
        let key1 = "ci-failure:me/repo:ci-111";
        let key2 = "ci-failure:me/repo:ci-222";
        let slug = "ci-failure-me-repo-pages";
        let mut i1 = issue(key1, json!([]));
        i1["title"] = json!("CI失敗: pages-build-deployment fix A");
        i1["kind"] = json!("pages-build-deployment");
        let mut i2 = issue(key2, json!([]));
        i2["title"] = json!("CI失敗: pages-build-deployment fix B");
        i2["kind"] = json!("pages-build-deployment");
        let d1 = skill_md(&i1, slug, &key_slug(key1));
        let d2 = skill_md(&i2, slug, &key_slug(key2));
        assert_ne!(d1, d2); // 生は異なる
        assert_eq!(norm_draft(&d1, key1, slug), norm_draft(&d2, key2, slug));
        // has_same_draft 経由でも既存提案と一致する
        let existing = vec![json!({
            "issueKey": key1,
            "changes": [{"path": format!(".devin/skills/{}/SKILL.md", slug), "content": d1}],
        })];
        assert!(has_same_draft(&existing, &d2, key2, slug));
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

    #[test]
    fn load_runbook_resolves_source_kind_then_kind_then_source() {
        let root = temp_root("rb");
        let dir = root.join("prompts/runbook");
        std::fs::create_dir_all(&dir).unwrap();
        // kind のみ → <kind>.md が解決される
        std::fs::write(dir.join("gh-auth.md"), "RB KIND").unwrap();
        assert_eq!(
            load_runbook(&root, "local-health", "gh-auth").map(|(n, _)| n),
            Some("gh-auth".into())
        );
        // source-kind があればそちらが優先
        std::fs::write(dir.join("local-health-gh-auth.md"), "RB SRC-KIND").unwrap();
        assert_eq!(
            load_runbook(&root, "local-health", "gh-auth").map(|(n, _)| n),
            Some("local-health-gh-auth".into())
        );
        // kind が無ければ source.md へフォールバック
        std::fs::write(dir.join("ci-failure.md"), "RB SRC").unwrap();
        assert_eq!(
            load_runbook(&root, "ci-failure", "no-such-kind").map(|(n, _)| n),
            Some("ci-failure".into())
        );
        // どれも無ければ None
        assert_eq!(load_runbook(&root, "unknown-src", "unknown-kind"), None);
        // 空文字・空白ファイルはスキップ
        std::fs::write(dir.join("empty-kind.md"), "  \n").unwrap();
        assert_eq!(load_runbook(&root, "nope", "empty-kind"), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
