// lessons.rs — 失敗レッスン（Reflexion 型リトライ）+ ACE 型カウンタ管理
// （正本: diagrams/lesson-loop.mmd、README §5.7）
//
// (a) 失敗レッスンの生成（manual-29799d3e）: verify NG / run failed 時に
//     「何が・なぜ失敗したか」の短いレッスン文を決定的に生成し、出典
//     （run idx・verifyNote/error・証跡 dir）と確度メタを付けて
//     knowledge/lessons.json へ lesson 種として保存する
// (b) プロンプト注入: dispatch prompt へ (1) 同一課題の直近レッスン、
//     (2) symptomHash/kind/repo の近い他課題のレッスン上位 N 件を注入し、
//     注入分は runs[].knowledgeRefs.lessons に記録する（注入選定は
//     決定的ロジックのみ。context collapse・プロンプト肥大を防ぐ）
// (c) ACE 型カウンタ（manual-bcf981dd）: レッスンは id + helpful/harmful
//     カウンタを持ち、注入後 run の成否で更新する。harmful が
//     lessonHarmLimit に達したレッスンは注入対象から自動除外
//     （誤教訓による汚染対策）し、削除を proposal 起票する
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::cases;
use crate::knowledge::key_slug;
use crate::proposals;
use crate::util::now_iso;

/// 同一課題から注入する直近レッスン数（Reflexion 型リトライ）
const SAME_ISSUE_N: usize = 3;
/// 注入するレッスンの総数上限（prompt 肥大防止）
const INJECT_MAX: usize = 6;

pub fn path(root: &Path) -> PathBuf {
    root.join("knowledge").join("lessons.json")
}

pub fn load(root: &Path) -> Vec<Value> {
    crate::config::load_j(&path(root), json!([]))
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn save(root: &Path, lessons: &[Value]) {
    let _ = std::fs::create_dir_all(root.join("knowledge"));
    crate::config::save_j(&path(root), &json!(lessons));
}

fn take(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// ---- (a) 失敗レッスンの生成 ----

/// 失敗理由から決定的な助言を引く（Reflexion の「次試行への助言」に相当。
/// 根拠のある失敗ほど確度を高くする）
fn lesson_advice(reason: &str) -> (&'static str, f64) {
    let r = reason.to_lowercase();
    if r.contains("issue トレーラ") || r.contains("実装コミットが見つからない") {
        ("実装コミットには `Issue: <key>` トレーラを必ず付ける（帰属判定に使われる）", 0.7)
    } else if r.contains("チェック失敗") || r.contains("build") || r.contains("cargo") {
        ("コミット前にローカルで build/test を通してから報告する", 0.6)
    } else if r.contains("実機") || r.contains("起動") || r.contains("応答") {
        ("verifyExe/ポート/起動引数を確認し、起動後の応答確認まで行う", 0.6)
    } else if r.contains("起動失敗") || r.contains("spawn") || r.contains("exe") {
        ("実行体・パスの環境前提を確認する（環境起因の失敗は再現しても解消しない）", 0.4)
    } else if r.contains("upstream") || r.contains("push") {
        ("upstream/remote 設定を確認する（帰属整理・push の前提）", 0.5)
    } else {
        ("失敗理由を verify.log/run.log で確認し、同じ手順の再実行は避ける", 0.4)
    }
}

/// verify NG / run failed のレッスンを lessons.json へ保存する。
/// 同一 (issueKey, runIdx) のレッスンは重複生成しない。戻り値は生成したか
pub fn record_failure(root: &Path, issue: &Value, run_idx: usize, reason: &str) -> bool {
    let key = cases::str_of(issue, "key");
    if key.is_empty() {
        return false;
    }
    let id = format!("les-{}-r{}", key_slug(key), run_idx);
    let mut lessons = load(root);
    if lessons.iter().any(|l| l.get("id").and_then(|v| v.as_str()) == Some(&*id)) {
        return false;
    }
    let (advice, confidence) = lesson_advice(reason);
    let title = take(cases::str_of(issue, "title"), 120);
    let reason1 = take(reason.lines().next().unwrap_or(""), 300);
    let text = format!(
        "「{}」の対応で失敗（{}）。再試行時は: {}",
        title, reason1, advice
    );
    lessons.push(json!({
        "id": id,
        "issueKey": key,
        "source": cases::str_of(issue, "source"),
        "repo": cases::str_of(issue, "repo"),
        "kind": cases::str_of(issue, "kind"),
        "symptomHash": cases::str_of(issue, "symptomHash"),
        "title": title,
        "text": text,
        // 根拠メタ（誤教訓の検証材料。Reflexion の既知弱点対策）
        "basis": reason1,
        "evidence": format!("evidence/{}", key_slug(key)),
        "confidence": confidence,
        "runIdx": run_idx,
        "helpful": 0,
        "harmful": 0,
        "injected": 0,
        "excluded": false,
        "createdAt": now_iso(),
    }));
    save(root, &lessons);
    true
}

// ---- (b) 注入選定 ----

/// 注入対象にできるレッスンか（harmful 蓄積で自動除外されたものは出さない）
fn injectable(l: &Value, harm_limit: u64) -> bool {
    !l.get("excluded").and_then(|v| v.as_bool()).unwrap_or(false)
        && l.get("harmful").and_then(|v| v.as_u64()).unwrap_or(0) < harm_limit
}

/// dispatch prompt 注入用の「失敗レッスン」節。選定は決定的:
///   1. 同一課題の直近 SAME_ISSUE_N 件（Reflexion 型リトライ）
///   2. cases::score で近い他課題のレッスン上位（INJECT_MAX - same）件
/// 戻り値 (節テキスト, 注入したレッスン id)。
/// 注入分は injected カウンタへ記録する
pub fn inject_section(root: &Path, issue: &Value, cfg: &Value) -> (String, Vec<String>) {
    let harm_limit = crate::config::cfg_u64(cfg, "lessonHarmLimit", 2);
    let key = cases::str_of(issue, "key");
    let mut lessons = load(root);
    if lessons.is_empty() {
        return (String::new(), vec![]);
    }
    // 同一課題の直近（createdAt 降順）
    let mut same: Vec<&Value> = lessons
        .iter()
        .filter(|l| l.get("issueKey").and_then(|v| v.as_str()) == Some(key))
        .filter(|l| injectable(l, harm_limit))
        .collect();
    same.sort_by(|a, b| {
        cases::str_of(b, "createdAt").cmp(cases::str_of(a, "createdAt"))
    });
    same.truncate(SAME_ISSUE_N);
    let same_ids: Vec<String> = same
        .iter()
        .filter_map(|l| l.get("id").and_then(|v| v.as_str()).map(String::from))
        .collect();
    // 類似課題のレッスン（score>=SIMILAR_MIN、スコア→helpful-harmful 順）
    let mut cross: Vec<(i64, i64, &Value)> = lessons
        .iter()
        .filter(|l| l.get("issueKey").and_then(|v| v.as_str()) != Some(key))
        .filter(|l| injectable(l, harm_limit))
        .map(|l| {
            let sc = cases::score(issue, l);
            let net = l.get("helpful").and_then(|v| v.as_i64()).unwrap_or(0)
                - l.get("harmful").and_then(|v| v.as_i64()).unwrap_or(0);
            (sc, net, l)
        })
        .filter(|(sc, _, _)| *sc >= cases::SIMILAR_MIN)
        .collect();
    cross.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    let cross_take = INJECT_MAX.saturating_sub(same.len());
    cross.truncate(cross_take);

    if same.is_empty() && cross.is_empty() {
        return (String::new(), vec![]);
    }
    let mut out = String::new();
    let mut ids: Vec<String> = same_ids;
    let fmt = |l: &Value, prefix: &str| -> String {
        format!(
            "- {}[{}] helpful={} harmful={} :: {}\n",
            prefix,
            l.get("id").and_then(|v| v.as_str()).unwrap_or(""),
            l.get("helpful").and_then(|v| v.as_u64()).unwrap_or(0),
            l.get("harmful").and_then(|v| v.as_u64()).unwrap_or(0),
            take(l.get("text").and_then(|v| v.as_str()).unwrap_or(""), 400),
        )
    };
    for l in &same {
        out.push_str(&fmt(l, ""));
    }
    for (_, _, l) in &cross {
        let k = l.get("issueKey").and_then(|v| v.as_str()).unwrap_or("");
        ids.push(l.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string());
        out.push_str(&format!("（類似課題 `{}`）", k));
        out.push_str(&fmt(l, "  "));
    }
    // 注入記録（injected++）
    for l in lessons.iter_mut() {
        if ids.iter().any(|id| {
            Some(&**id) == l.get("id").and_then(|v| v.as_str())
        }) {
            let n = l.get("injected").and_then(|v| v.as_u64()).unwrap_or(0);
            l.as_object_mut().unwrap().insert("injected".into(), json!(n + 1));
        }
    }
    save(root, &lessons);
    (out, ids)
}

// ---- (c) ACE カウンタ更新・harmful 除外・削除 proposal ----

/// 注入後 run の成否で helpful/harmful を更新する。
/// issue.runs[last].knowledgeRefs.lessons の id 群へ ok なら helpful++、
/// NG なら harmful++。harmful が lessonHarmLimit へ達したレッスンは
/// excluded=true にし、削除 proposal を起票する。戻り値は変更した数
pub fn record_outcome(root: &Path, issue: &Value, ok: bool, cfg: &Value) -> usize {
    let runs = issue
        .get("runs")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let ids: Vec<String> = runs
        .last()
        .and_then(|r| r.get("knowledgeRefs"))
        .and_then(|k| k.get("lessons"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() {
        return 0;
    }
    let harm_limit = crate::config::cfg_u64(cfg, "lessonHarmLimit", 2);
    let mut lessons = load(root);
    let mut changed = 0;
    let mut excluded: Vec<String> = vec![];
    for l in lessons.iter_mut() {
        let Some(id) = l.get("id").and_then(|v| v.as_str()).map(String::from) else {
            continue;
        };
        if !ids.contains(&id) {
            continue;
        }
        let field = if ok { "helpful" } else { "harmful" };
        let n = l.get(field).and_then(|v| v.as_u64()).unwrap_or(0);
        l.as_object_mut()
            .unwrap()
            .insert(field.into(), json!(n + 1));
        changed += 1;
        if !ok && n + 1 >= harm_limit
            && !l.get("excluded").and_then(|v| v.as_bool()).unwrap_or(false)
        {
            l.as_object_mut()
                .unwrap()
                .insert("excluded".into(), json!(true));
            excluded.push(id);
        }
    }
    save(root, &lessons);
    for id in &excluded {
        propose_removal(root, &lessons, id);
    }
    changed
}

/// harmful 蓄積レッスンの削除 proposal（人の採否ゲート）。
/// lessons.json から当該レッスンを除いた版を changes に持つ。
/// 同じレッスン id の pending proposal があれば再起票しない
fn propose_removal(root: &Path, lessons: &[Value], id: &str) {
    let pdir = proposals::dir(root);
    let title = format!("harmful レッスンの削除: {}", id);
    let mut pending = proposals::list(&pdir);
    pending.extend(proposals::list(&proposals::decided_dir(&pdir)));
    if pending.iter().any(|p| {
        p.get("title").and_then(|v| v.as_str()) == Some(title.as_str())
    }) {
        return;
    }
    let kept: Vec<Value> = lessons
        .iter()
        .filter(|l| l.get("id").and_then(|v| v.as_str()) != Some(id))
        .cloned()
        .collect();
    let content = serde_json::to_string_pretty(&json!(kept)).unwrap_or_default();
    proposals::create(
        &pdir,
        &json!({
            "title": title,
            "kind": "other",
            "changes": [{"path": "knowledge/lessons.json", "content": content}],
            "rationale": format!(
                "レッスン {} は注入後 run の失敗で harmful が上限に達し注入除外済み。\
                 playbook の汚染を止めるため台帳から削除する提案（ACE 型カウンタ管理）",
                id
            ),
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::save_j;
    use crate::store;

    fn temp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "kasugai-lessons-test-{}-{}",
            tag,
            crate::util::rand_hex(4)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn issue(key: &str, repo: &str, kind: &str) -> Value {
        json!({"key": key, "source": "ci-failure", "repo": repo, "kind": kind,
               "symptomHash": key, "title": "pages build failed", "status": "queued"})
    }

    #[test]
    fn records_failure_lesson_once_per_run() {
        let root = temp_root("rec");
        let i = issue("ci-failure:me/repo:h1", "me/repo", "build");
        assert!(record_failure(&root, &i, 0, "Issue トレーラ付きの実装コミットが見つからない"));
        assert!(!record_failure(&root, &i, 0, "same")); // 同 run では再生成しない
        assert!(record_failure(&root, &i, 1, "チェック失敗: cargo build"));
        let ls = load(&root);
        assert_eq!(ls.len(), 2);
        assert!(cases::str_of(&ls[0], "basis").contains("トレーラ"));
        assert_eq!(ls[0]["helpful"], json!(0));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn injects_same_issue_and_cross_issue_lessons() {
        let root = temp_root("inj");
        let cur = issue("ci-failure:me/repo:cur", "me/repo", "build");
        let past = issue("ci-failure:me/repo:past", "me/repo", "build");
        let other = issue("ci-failure:you/x:o", "you/x", "lint");
        record_failure(&root, &past, 0, "チェック失敗: cargo build");
        record_failure(&root, &other, 0, "チェック失敗: cargo build");
        record_failure(&root, &cur, 0, "Issue トレーラ付きの実装コミットが見つからない");
        let cfg = json!({});
        let (sec, ids) = inject_section(&root, &cur, &cfg);
        // 同一課題 1 + 類似課題（同 repo+kind の past）1 = 2 件
        assert_eq!(ids.len(), 2);
        assert!(sec.contains("les-ci-failure-me-repo-cur-r0"));
        assert!(sec.contains("類似課題 `ci-failure:me/repo:past`"));
        // injected カウンタが進む
        let ls = load(&root);
        assert_eq!(ls[0]["injected"], json!(1));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn outcome_updates_counters_and_excludes_harmful() {
        let root = temp_root("out");
        let cur = issue("ci-failure:me/repo:cur", "me/repo", "build");
        record_failure(&root, &cur, 0, "チェック失敗");
        let cfg = json!({"lessonHarmLimit": 2});
        let (_sec, ids) = inject_section(&root, &cur, &cfg);
        assert_eq!(ids.len(), 1);
        // 注入分を持つ run 記録を持つ課題を用意
        let mut run_issue = cur.clone();
        run_issue["runs"] = json!([{"knowledgeRefs": {"lessons": ids}}]);
        store::save_issues(&root, &[run_issue.clone()]);
        // 1回目 NG → harmful=1（まだ注入可）
        assert_eq!(record_outcome(&root, &run_issue, false, &cfg), 1);
        let (sec2, _) = inject_section(&root, &cur, &cfg);
        assert!(sec2.contains("harmful=1"));
        // 2回目 NG → harmful=2 = limit → excluded + 削除 proposal
        assert_eq!(record_outcome(&root, &run_issue, false, &cfg), 1);
        let ls = load(&root);
        assert!(ls[0]["excluded"].as_bool().unwrap());
        let (sec3, ids3) = inject_section(&root, &cur, &cfg);
        assert!(sec3.is_empty() && ids3.is_empty());
        let props = proposals::list(&proposals::dir(&root));
        assert_eq!(props.len(), 1);
        assert_eq!(
            props[0]["changes"][0]["path"].as_str().unwrap(),
            "knowledge/lessons.json"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
