// consolidate.rs — アイドル時の知見圧縮（sleep-time consolidation）
// （正本: diagrams/consolidation.mmd、README §5.7）
//
// 実行系がアイドル（queued/running/verifying 系の課題なし・最終活動から
// consolidateIdleMinutes 経過）のときだけ動く。knowledge/ に溜まった
// 課題単位の生知見を source+kind でクラスタ化し、consolidateMinCluster
// 以上のクラスタから上位レッスン（対処手順書）を決定的に合成する。
// 出力は常に proposals 経由（prompts/runbook/<source>-<kind>.md への
// 変更提案）で、採否は人のゲート。採用されると既存の runbook 注入
// （knowledge::load_runbook → runner.build_prompt）へ反映される。
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;

use crate::cases;
use crate::knowledge::key_slug;
use crate::lessons;
use crate::proposals;
use crate::store;

/// 活動中とみなす課題状態（これらが1件でもあれば統合しない）
const ACTIVE_STATES: &[&str] = &[
    "queued",
    "assessed",
    "running",
    "verifying",
    "retry",
    "planning",
    "plan_review",
];

fn take(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// 課題群の最終活動時刻（updatedAt / runs[].endedAt の最大）。ISO 文字列
fn last_activity(issues: &[Value]) -> String {
    let mut t = String::new();
    for i in issues {
        for k in ["updatedAt", "createdAt"] {
            if let Some(s) = i.get(k).and_then(|v| v.as_str()) {
                if s > t.as_str() {
                    t = s.to_string();
                }
            }
        }
        if let Some(runs) = i.get("runs").and_then(|v| v.as_array()) {
            for r in runs {
                for k in ["endedAt", "startedAt"] {
                    if let Some(s) = r.get(k).and_then(|v| v.as_str()) {
                        if s > t.as_str() {
                            t = s.to_string();
                        }
                    }
                }
            }
        }
    }
    t
}

/// アイドル判定: 活動中の課題がなく、最終活動から idle_minutes 以上経過
pub fn is_idle(issues: &[Value], cfg: &Value) -> bool {
    if issues.iter().any(|i| {
        ACTIVE_STATES.contains(&cases::str_of(i, "status"))
    }) {
        return false;
    }
    let idle_min = crate::config::cfg_u64(cfg, "consolidateIdleMinutes", 30);
    let last = last_activity(issues);
    if last.is_empty() {
        return false;
    }
    let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&last) else {
        return false;
    };
    let elapsed = chrono::Utc::now()
        .signed_duration_since(ts.with_timezone(&chrono::Utc));
    elapsed.num_minutes() >= idle_min as i64
}

/// クラスタ（source-kind）から上位レッスン（runbook 本文）を決定的に合成
fn synthesize(root: &Path, name: &str, members: &[&Value]) -> String {
    let mut md = String::new();
    md.push_str(&format!(
        "# runbook: {}\n\nsleep-time consolidation により knowledge/ の\
         課題知見から合成された上位レッスン（対処手順書）。\n\n## 傾向\n\n",
        name
    ));
    let mut dist: BTreeMap<String, u64> = BTreeMap::new();
    for i in members {
        let st = cases::str_of(i, "status").to_string();
        *dist.entry(st).or_default() += 1;
    }
    for (st, n) in &dist {
        md.push_str(&format!("- {}: {} 件\n", st, n));
    }
    // 失敗の頻出パターン（lessons.json の当該クラスタ分を net helpful 順に）
    let keys: Vec<String> = members
        .iter()
        .filter_map(|i| i.get("key").and_then(|v| v.as_str()).map(String::from))
        .collect();
    let loaded = lessons::load(root);
    let mut ls: Vec<&Value> = loaded
        .iter()
        .filter(|l| keys.contains(&cases::str_of(l, "issueKey").to_string()))
        .collect();
    ls.sort_by(|a, b| {
        let net = |l: &Value| {
            l.get("helpful").and_then(|v| v.as_i64()).unwrap_or(0)
                - l.get("harmful").and_then(|v| v.as_i64()).unwrap_or(0)
        };
        let cf = |l: &Value| l.get("confidence").and_then(|v| v.as_f64()).unwrap_or(0.0);
        net(b)
            .cmp(&net(a))
            .then(cf(b).partial_cmp(&cf(a)).unwrap_or(std::cmp::Ordering::Equal))
    });
    ls.truncate(5);
    if !ls.is_empty() {
        md.push_str("\n## 既知の失敗パターンと対策\n\n");
        for l in ls {
            md.push_str(&format!(
                "- {}（根拠: {}）\n",
                take(cases::str_of(l, "text"), 300),
                take(cases::str_of(l, "basis"), 120)
            ));
        }
    }
    // 成功例の採用アプローチ（cases.json の done ケース）
    let approaches: Vec<String> = cases::load_cases(root)
        .iter()
        .filter(|c| {
            cases::str_of(c, "result") == "done"
                && keys.contains(&cases::str_of(c, "key").to_string())
        })
        .filter_map(|c| {
            let a = cases::str_of(c, "approach");
            if a.is_empty() {
                None
            } else {
                Some(take(a, 300))
            }
        })
        .collect();
    if !approaches.is_empty() {
        md.push_str("\n## 成功した対処アプローチ\n\n");
        for a in approaches {
            md.push_str(&format!("- {}\n", a));
        }
    }
    md.push_str("\n## 対象課題\n\n");
    for i in members {
        md.push_str(&format!(
            "- `{}`（{}）{}\n",
            cases::str_of(i, "key"),
            cases::str_of(i, "status"),
            take(cases::str_of(i, "title"), 120)
        ));
    }
    md
}

/// consolidation の1回分。アイドル時のみ動き、各クラスタの runbook を
/// proposal 起票する。戻り値は起票した proposal 数。
/// control.rs の learn watcher から定期呼び出しされる。
pub fn consolidate_pass(root: &Path, cfg: &Value) -> usize {
    let issues = store::load_issues(root);
    if issues.is_empty() || !is_idle(&issues, cfg) {
        return 0;
    }
    let min_cluster = crate::config::cfg_u64(cfg, "consolidateMinCluster", 3) as usize;
    // source+kind でクラスタ化（runbook 名解決が source-kind なので揃える）
    let mut clusters: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    let kdir = root.join("knowledge");
    for i in &issues {
        let slug = i
            .get("key")
            .and_then(|v| v.as_str())
            .map(key_slug)
            .unwrap_or_default();
        if slug.is_empty() || !kdir.join(format!("{}.md", slug)).is_file() {
            continue; // 生知見のある課題だけを統合対象にする
        }
        let name = format!(
            "{}-{}",
            cases::str_of(i, "source"),
            cases::str_of(i, "kind")
        );
        if name.len() <= 1 {
            continue;
        }
        clusters.entry(name).or_default().push(i);
    }
    let mut existing = proposals::list(&proposals::dir(root));
    existing.extend(proposals::list(&proposals::decided_dir(
        &proposals::dir(root),
    )));
    let mut n = 0;
    for (name, members) in clusters {
        if members.len() < min_cluster {
            continue;
        }
        let rel = format!("prompts/runbook/{}.md", name);
        let body = synthesize(root, &name, &members);
        // 既存 runbook と同内容なら起票しない
        let cur = std::fs::read_to_string(root.join(&rel)).unwrap_or_default();
        if cur == body {
            continue;
        }
        // 同一 path の pending proposal があれば再起票しない
        if existing.iter().any(|p| {
            p.get("changes").and_then(|c| c.as_array()).is_some_and(|cs| {
                cs.iter().any(|c| {
                    c.get("path").and_then(|v| v.as_str()) == Some(rel.as_str())
                })
            })
        }) {
            continue;
        }
        proposals::create(
            &proposals::dir(root),
            &json!({
                "title": format!("runbook 合成: {}", name),
                "kind": "other",
                "changes": [{"path": rel, "content": body}],
                "rationale": format!(
                    "sleep-time consolidation: knowledge/ の {} 件の課題知見を\
                     {} クラスタへ統合し上位レッスン（対処手順書）を合成",
                    members.len(), name
                ),
            }),
        );
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "kasugai-consol-test-{}-{}",
            tag,
            crate::util::rand_hex(4)
        ));
        std::fs::create_dir_all(p.join("knowledge")).unwrap();
        p
    }

    fn done_issue(key: &str, source: &str, kind: &str) -> Value {
        json!({"key": key, "source": source, "repo": "me/repo", "kind": kind,
               "symptomHash": key, "title": "t", "status": "done",
               "updatedAt": "2020-01-01T00:00:00+00:00"})
    }

    #[test]
    fn idle_requires_no_active_and_elapsed() {
        let cfg = json!({"consolidateIdleMinutes": 30});
        let mut active = done_issue("a", "s", "k");
        active["status"] = json!("running");
        assert!(!is_idle(&[active], &cfg));
        let old = done_issue("a", "s", "k");
        assert!(is_idle(&[old], &cfg));
        // 活動が新しければ busy
        let mut fresh = done_issue("a", "s", "k");
        fresh["updatedAt"] = json!(crate::util::now_iso());
        assert!(!is_idle(&[fresh], &cfg));
    }

    #[test]
    fn consolidates_cluster_into_runbook_proposal() {
        let root = temp_root("c");
        let mut issues = vec![];
        for i in 0..3 {
            let k = format!("ci-failure:me/repo:h{}", i);
            let slug = key_slug(&k);
            std::fs::write(
                root.join("knowledge").join(format!("{}.md", slug)),
                "# 知見\n",
            )
            .unwrap();
            issues.push(done_issue(&k, "ci-failure", "build"));
        }
        store::save_issues(&root, &issues);
        let cfg = json!({"consolidateIdleMinutes": 0, "consolidateMinCluster": 3});
        assert_eq!(consolidate_pass(&root, &cfg), 1);
        let props = proposals::list(&proposals::dir(&root));
        assert_eq!(
            props[0]["changes"][0]["path"].as_str().unwrap(),
            "prompts/runbook/ci-failure-build.md"
        );
        // 再起票しない（pending 残）
        assert_eq!(consolidate_pass(&root, &cfg), 0);
        let _ = std::fs::remove_dir_all(&root);
    }
}
