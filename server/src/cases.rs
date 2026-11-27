// cases.rs — 類似事例検索（CBR）と assess/run への知見参照
// （正本: diagrams/case-search.mmd、README §5.7）
//
// (a) 類似度スコア（決定的・非LLM。埋め込み基盤なしで開始）:
//     repo一致40 + kind一致20 + symptomHash一致20 + title bigram Jaccard*20。
//     score>=SIMILAR_MIN を「類似」として top N を選ぶ
// (b) assess 時の知見参照（manual-a8c93d14）: 新規課題へ related[]
//     （類似事例: 前回の fix / reject / run 成否）を付記し、dispatch
//     prompt へ「類似過去事例」として注入する。参照した知見は
//     runs[].knowledgeRefs に記録して追跡可能にする
// (c) case 蓄積（manual-654b11c2）: 終端課題を knowledge/cases.json へ
//     case（kind/repo/symptomHash/採用アプローチ/結果/verifyLevel）として
//     蓄積する。高一致の成功 case は verifyLevel 採用と few-shot に転用
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::knowledge::key_slug;
use crate::store;
use crate::util::now_iso;

/// 「類似」とみなすスコア閾値（case-search.mmd）
pub const SIMILAR_MIN: i64 = 50;
/// verifyLevel 採用に必要な高一致スコア（成功 case のみ採用）
pub const ADOPT_MIN: i64 = 70;
/// related / 注入に使う類似事例の件数
pub const TOP_N: usize = 3;

pub fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("")
}

// ---- 類似度スコア ----

/// title の正規化文字 bigram 集合（単語分割に依存しないため
/// 日本語・英語混在でも決定的に機能する。1文字語は unigram で補完）
fn title_grams(t: &str) -> BTreeSet<String> {
    let mut g = BTreeSet::new();
    for w in t.to_lowercase().split(|c: char| !c.is_alphanumeric()) {
        if w.is_empty() {
            continue;
        }
        let cs: Vec<char> = w.chars().collect();
        if cs.len() == 1 {
            g.insert(w.to_string());
        } else {
            for pair in cs.windows(2) {
                g.insert(pair.iter().collect());
            }
        }
    }
    g
}

fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    a.intersection(b).count() as f64 / a.union(b).count() as f64
}

/// 課題（または課題形メタを持つ Value）2件の類似度。0..100
pub fn score(a: &Value, b: &Value) -> i64 {
    let mut sc = 0i64;
    let (ra, rb) = (str_of(a, "repo"), str_of(b, "repo"));
    if !ra.is_empty() && ra == rb {
        sc += 40;
    }
    let (ka, kb) = (str_of(a, "kind"), str_of(b, "kind"));
    if !ka.is_empty() && ka == kb {
        sc += 20;
    }
    let (sa, sb) = (str_of(a, "symptomHash"), str_of(b, "symptomHash"));
    if !sa.is_empty() && sa == sb {
        sc += 20;
    }
    sc += (jaccard(&title_grams(str_of(a, "title")), &title_grams(str_of(b, "title"))) * 20.0)
        .round() as i64;
    sc
}

/// issue に類似する他課題（score>=SIMILAR_MIN・降順 top_n）。
/// 各要素 {key,title,status,verifyLevel,kind,repo,score}
pub fn similar(issues: &[Value], issue: &Value, top_n: usize) -> Vec<Value> {
    let key = str_of(issue, "key");
    let mut v: Vec<(i64, &Value)> = issues
        .iter()
        .filter(|i| str_of(i, "key") != key && !str_of(i, "key").is_empty())
        .map(|i| (score(issue, i), i))
        .filter(|(sc, _)| *sc >= SIMILAR_MIN)
        .collect();
    v.sort_by(|a, b| b.0.cmp(&a.0));
    v.truncate(top_n);
    v.iter()
        .map(|(sc, i)| {
            json!({
                "key": str_of(i, "key"),
                "title": str_of(i, "title"),
                "status": str_of(i, "status"),
                "verifyLevel": i.get("verifyLevel").cloned().unwrap_or(Value::Null),
                "kind": str_of(i, "kind"),
                "repo": str_of(i, "repo"),
                "score": sc,
            })
        })
        .collect()
}

// ---- assess 時の知見参照 ----

/// assess で呼ぶ: issue.related に類似事例 top N を付記（前回の
/// fix / reject / run 成否の参照先）。戻り値は付記した related。
pub fn attach_related(issues: &[Value], issue: &mut Value) -> Vec<Value> {
    let rel = similar(issues, issue, TOP_N);
    if !rel.is_empty() {
        issue
            .as_object_mut()
            .unwrap()
            .insert("related".into(), json!(rel.clone()));
    }
    rel
}

/// related のうち高一致の「成功 case」（status=done・score>=ADOPT_MIN）を返す
pub fn adopted_case(related: &[Value]) -> Option<Value> {
    related
        .iter()
        .find(|r| {
            str_of(r, "status") == "done"
                && r.get("score").and_then(|v| v.as_i64()).unwrap_or(0) >= ADOPT_MIN
        })
        .cloned()
}

// ---- dispatch prompt への注入 ----

fn take(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// 参照先課題の知見抜粋: knowledge/<key-slug>.md の「## run 結果」を
/// 優先し、無ければ先頭部へフォールバック（最大 max 字）
fn excerpt(root: &Path, key: &str, max: usize) -> String {
    let slug = key_slug(key);
    let Ok(md) = std::fs::read_to_string(root.join("knowledge").join(format!("{}.md", slug)))
    else {
        return String::new();
    };
    if let Some(sec) = md.split("## run 結果").nth(1) {
        let t = take(sec.trim(), max);
        if !t.trim().is_empty() {
            return t.trim().to_string();
        }
    }
    take(md.trim(), max).trim().to_string()
}

/// dispatch prompt 注入用の「類似過去事例」節。参照した case の key を
/// 併せて返す（runs[].knowledgeRefs へ記録するため）。
/// 戻り値 (節テキスト, 参照 case keys)
pub fn similar_section(root: &Path, issue: &Value, top_n: usize) -> (String, Vec<String>) {
    let issues = store::load_issues(root);
    let rel = similar(&issues, issue, top_n);
    if rel.is_empty() {
        return (String::new(), vec![]);
    }
    let mut out = String::new();
    let mut keys = vec![];
    for r in &rel {
        let key = str_of(r, "key").to_string();
        keys.push(key.clone());
        let lv = r
            .get("verifyLevel")
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".into());
        out.push_str(&format!(
            "- `{}`（status={}・verifyLevel L{}・score={}）{}\n",
            key,
            str_of(r, "status"),
            lv,
            r.get("score")
                .map(|v| v.to_string())
                .unwrap_or_else(|| "?".into()),
            take(str_of(r, "title"), 120)
        ));
        // 成功 case の採用アプローチは few-shot として末尾行へ付記
        if str_of(r, "status") == "done" {
            if let Some(c) = load_cases(root).iter().find(|c| str_of(c, "key") == key) {
                let ap = str_of(c, "approach");
                if !ap.is_empty() {
                    out.push_str(&format!("  採用アプローチ: {}\n", take(ap, 300)));
                }
            }
        }
        let ex = excerpt(root, &key, 600);
        if !ex.is_empty() {
            out.push_str(&format!("  前回の記録: {}\n", take(&ex.replace('\n', " / "), 600)));
        }
    }
    (out, keys)
}

// ---- case 蓄積（終端課題 → knowledge/cases.json） ----

pub fn cases_path(root: &Path) -> PathBuf {
    root.join("knowledge").join("cases.json")
}

pub fn load_cases(root: &Path) -> Vec<Value> {
    crate::config::load_j(&cases_path(root), json!([]))
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn save_cases(root: &Path, cases: &[Value]) {
    let _ = std::fs::create_dir_all(root.join("knowledge"));
    crate::config::save_j(&cases_path(root), &json!(cases));
}

/// 課題の「採用アプローチ」: 最終 run の報告サマリ（runs[] 末尾の
/// result/summary/note）を抜粋。無ければ空文字
fn approach_of(issue: &Value) -> String {
    let runs = issue
        .get("runs")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for r in runs.iter().rev() {
        for k in ["summary", "result", "note"] {
            if let Some(t) = r.get(k).and_then(|v| v.as_str()).filter(|t| !t.is_empty()) {
                return take(t, 400);
            }
        }
    }
    String::new()
}

/// 終端課題（done/failed/rejected）を knowledge/cases.json へ case 化する。
/// 既存 case は結果・verifyLevel・アプローチを最新へ更新する。
/// learn_pass から定期呼び出し。戻り値は新規・更新した case 数。
pub fn sync_cases(root: &Path) -> usize {
    let issues = store::load_issues(root);
    let mut cases = load_cases(root);
    let mut touched = 0;
    for i in &issues {
        let key = str_of(i, "key");
        if key.is_empty() {
            continue;
        }
        let status = str_of(i, "status");
        if !matches!(status, "done" | "failed" | "rejected") {
            continue;
        }
        let mut c = json!({
            "key": key,
            "source": str_of(i, "source"),
            "repo": str_of(i, "repo"),
            "kind": str_of(i, "kind"),
            "symptomHash": str_of(i, "symptomHash"),
            "title": str_of(i, "title"),
            "verifyLevel": i.get("verifyLevel").cloned().unwrap_or(Value::Null),
            "result": status,
            "approach": approach_of(i),
            "updatedAt": now_iso(),
        });
        match cases.iter_mut().find(|c| str_of(c, "key") == key) {
            Some(e) => {
                if e.get("result") != c.get("result") || e.get("approach") != c.get("approach") {
                    *e = c;
                    touched += 1;
                }
            }
            None => {
                c.as_object_mut()
                    .unwrap()
                    .insert("createdAt".into(), json!(now_iso()));
                cases.push(c);
                touched += 1;
            }
        }
    }
    if touched > 0 {
        save_cases(root, &cases);
    }
    touched
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(key: &str, repo: &str, kind: &str, title: &str) -> Value {
        json!({"key": key, "source": "ci-failure", "repo": repo, "kind": kind,
               "symptomHash": key, "title": title, "status": "done",
               "verifyLevel": 2, "runs": [{"summary": "pages を再デプロイして解消"}]})
    }

    #[test]
    fn score_weights_repo_kind_hash_and_title() {
        let a = issue("a", "me/repo", "build", "pages build failed");
        let same = issue("a", "me/repo", "build", "pages build failed");
        assert_eq!(score(&a, &same), 100); // repo+kind+hash+title
        let other_repo = issue("a", "me/other", "build", "pages build failed");
        assert_eq!(score(&a, &other_repo), 60);
        let diff_kind = issue("a", "me/repo", "lint", "pages build failed");
        assert_eq!(score(&a, &diff_kind), 80);
        let unrelated = issue("e", "you/x", "other", "completely different topic");
        assert!(score(&a, &unrelated) < SIMILAR_MIN);
    }

    #[test]
    fn similar_returns_top_scored_over_threshold() {
        let cur = issue("cur", "me/repo", "build", "pages build failed");
        let issues = vec![
            issue("cur", "me/repo", "build", "pages build failed"),
            issue("past1", "me/repo", "build", "pages build failed"), // 100
            issue("past2", "me/repo", "lint", "pages build failed"),  // 80
            issue("past3", "me/repo", "other", "totally unrelated"),  // 40 → 閾値未満
        ];
        let rel = similar(&issues, &cur, 3);
        assert_eq!(rel.len(), 2);
        assert_eq!(str_of(&rel[0], "key"), "past1");
    }

    #[test]
    fn attach_related_writes_issue_field() {
        let root = std::env::temp_dir().join(format!(
            "kasugai-cases-test-{}",
            crate::util::rand_hex(4)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut cur = issue("cur", "me/repo", "build", "pages build failed");
        cur["status"] = json!("queued");
        let issues = vec![issue("past", "me/repo", "build", "pages build failed")];
        let rel = attach_related(&issues, &mut cur);
        assert_eq!(rel.len(), 1);
        assert_eq!(str_of(&cur["related"][0], "key"), "past");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn adopted_case_requires_done_and_high_score() {
        let rel = vec![
            json!({"key": "k1", "status": "rejected", "score": 90}),
            json!({"key": "k2", "status": "done", "score": 60}),
            json!({"key": "k3", "status": "done", "score": 80}),
        ];
        let a = adopted_case(&rel).unwrap();
        assert_eq!(str_of(&a, "key"), "k3");
        assert!(adopted_case(&rel[..1]).is_none());
    }

    #[test]
    fn sync_cases_records_terminal_issues() {
        let root = std::env::temp_dir().join(format!(
            "kasugai-cases-test-{}",
            crate::util::rand_hex(4)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut open = issue("open", "me/repo", "build", "t");
        open["status"] = json!("running");
        let closed = issue("closed", "me/repo", "build", "t2");
        store::save_issues(&root, &[open, closed]);
        assert_eq!(sync_cases(&root), 1);
        let cs = load_cases(&root);
        assert_eq!(cs.len(), 1);
        assert_eq!(str_of(&cs[0], "key"), "closed");
        assert_eq!(str_of(&cs[0], "approach"), "pages を再デプロイして解消");
        assert_eq!(sync_cases(&root), 0); // 冪等
        // 結果変化で更新
        let mut upd = issue("closed", "me/repo", "build", "t2");
        upd["status"] = json!("failed");
        store::save_issues(&root, &[upd]);
        assert_eq!(sync_cases(&root), 1);
        assert_eq!(str_of(&load_cases(&root)[0], "result"), "failed");
        let _ = std::fs::remove_dir_all(&root);
    }
}
