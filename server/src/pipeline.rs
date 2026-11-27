// pipeline.rs — 課題ライフサイクルの state machine
// （正本: diagrams/issue-lifecycle.mmd・detect-pipeline.mmd、README §3 §5）
//
// 状態遷移はこのモジュールだけが行う。detectors・runner・API は
// intake / request_transition / can_dispatch 等の遷移要求を呼ぶだけで、
// issues.json の status を直接書き換えてはならない。
//
// 課題レコード（issues.json の要素）:
//   key, source, repo, slug?, number?, url?, title, body, labels[],
//   symptomHash, status, verifyLevel?, acceptance?, autonomous, priority,
//   retries, verifyBacks, detectedAt, lastSeen, updatedAt, runs[]
use serde_json::{json, Value};
use std::path::Path;

use crate::cases;
use crate::config::{cfg_arr, cfg_f64, cfg_u64, load_config};
use crate::store;
use crate::util::now_iso;

// ---- 状態（issue-lifecycle.mmd） ----
pub const ST_ASSESSED: &str = "assessed";
pub const ST_GATED: &str = "gated";
pub const ST_QUEUED: &str = "queued";
pub const ST_PLANNING: &str = "planning";
pub const ST_PLAN_REVIEW: &str = "plan_review";
pub const ST_RUNNING: &str = "running";
pub const ST_VERIFYING: &str = "verifying";
pub const ST_RETRY: &str = "retry";
pub const ST_ROLLBACK: &str = "rollback";
pub const ST_DONE: &str = "done";
pub const ST_FAILED: &str = "failed";
pub const ST_REJECTED: &str = "rejected";
pub const ST_DROPPED: &str = "dropped";

pub const STATUSES: &[&str] = &[
    ST_ASSESSED,
    ST_GATED,
    ST_QUEUED,
    ST_PLANNING,
    ST_PLAN_REVIEW,
    ST_RUNNING,
    ST_VERIFYING,
    ST_RETRY,
    ST_ROLLBACK,
    ST_DONE,
    ST_FAILED,
    ST_REJECTED,
    ST_DROPPED,
];

/// planning/plan_review は計画承認ゲート（manual-883d17ee）: planGate 課題は
/// dispatch で実装前の計画生成 → critic 批評 → 人 or 自動承認後に running へ。

/// 許可される遷移（detected/deduped は intake 内部で畳み込む）
fn allowed(from: &str, to: &str) -> bool {
    matches!(
        (from, to),
        (ST_ASSESSED, ST_GATED)
            | (ST_ASSESSED, ST_QUEUED)
            | (ST_GATED, ST_QUEUED)
            | (ST_GATED, ST_REJECTED)
            | (ST_QUEUED, ST_REJECTED)
            | (ST_QUEUED, ST_RUNNING)
            | (ST_QUEUED, ST_PLANNING)
            | (ST_PLANNING, ST_PLAN_REVIEW)
            | (ST_PLANNING, ST_RUNNING)
            | (ST_PLANNING, ST_QUEUED)
            | (ST_PLANNING, ST_FAILED)
            | (ST_PLAN_REVIEW, ST_RUNNING)
            | (ST_PLAN_REVIEW, ST_QUEUED)
            | (ST_PLAN_REVIEW, ST_REJECTED)
            | (ST_PLAN_REVIEW, ST_FAILED)
            | (ST_RUNNING, ST_VERIFYING)
            | (ST_RUNNING, ST_FAILED)
            | (ST_VERIFYING, ST_DONE)
            | (ST_VERIFYING, ST_RETRY)
            | (ST_RETRY, ST_QUEUED)
            | (ST_RETRY, ST_ROLLBACK)
            | (ST_ROLLBACK, ST_FAILED)
            | (ST_RETRY, ST_FAILED)
            | (ST_FAILED, ST_QUEUED)
            | (ST_FAILED, ST_REJECTED)
            | (_, ST_DROPPED)
    )
}

/// 遷移要求の適用。許可されない遷移は false。成功時 updatedAt を更新し
/// work-queue.json との整合（queued 出入り）もここで取る。
/// 保存は呼び出し側（load→transition→save の一連をロック下で行うこと）。
pub fn transition(issue: &mut Value, to: &str) -> bool {
    let from = issue
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !allowed(from, to) {
        return false;
    }
    let o = issue.as_object_mut().unwrap();
    o.insert("status".into(), json!(to));
    o.insert("updatedAt".into(), json!(now_iso()));
    if to == ST_RETRY {
        let n = o.get("verifyBacks").and_then(|v| v.as_u64()).unwrap_or(0);
        o.insert("verifyBacks".into(), json!(n + 1));
    }
    true
}

/// issues.json を読み、key の課題へ遷移を適用して保存。
/// 戻り値 Ok(issue) / Err(理由)。queue の整合もここで同期する。
pub fn request_transition(root: &Path, key: &str, to: &str) -> Result<Value, String> {
    let mut issues = store::load_issues(root);
    let Some(i) = issues.iter_mut().find(|x| {
        x.get("key").and_then(|v| v.as_str()) == Some(key)
    }) else {
        return Err("not found".into());
    };
    if !transition(i, to) {
        let from = i.get("status").and_then(|v| v.as_str()).unwrap_or("?");
        return Err(format!("transition {} -> {} not allowed", from, to));
    }
    let done = i.clone();
    store::save_issues(root, &issues);
    sync_queue(root, &issues);
    Ok(done)
}

/// 遷移を伴わない表示用フィールドの更新（pushApproved・reviewNote 等）。
/// status はこの関数では変えられない（遷移は request_transition のみ）。
pub fn set_field(root: &Path, key: &str, field: &str, v: Value) -> Result<Value, String> {
    if field == "status" || field == "key" {
        return Err("use request_transition for status".into());
    }
    let mut issues = store::load_issues(root);
    let Some(i) = issues.iter_mut().find(|x| {
        x.get("key").and_then(|v| v.as_str()) == Some(key)
    }) else {
        return Err("not found".into());
    };
    let o = i.as_object_mut().unwrap();
    o.insert(field.into(), v);
    o.insert("updatedAt".into(), json!(now_iso()));
    let out = i.clone();
    store::save_issues(root, &issues);
    Ok(out)
}

/// 起票後の追記（followUp。intake-snapshot.mmd・manual-15667606）:
/// 起票時に freeze した intakeSnapshot には混入せず followUps[] へ記録し、
/// run 中の課題へは見せない（次回 run の prompt で「フォローアップ入力」
/// として注入されるまで保持）。ref 指定時は同一 ref の重複追記を抑止する
/// （detector が同じ事象を再 emit しても followUps が増殖しない）。
/// 戻り値 Ok(true)=追記 / Ok(false)=重複抑止 / Err=課題なし
pub fn add_followup(
    root: &Path,
    key: &str,
    kind: &str,
    text: &str,
    ref_: Option<&str>,
) -> Result<bool, String> {
    let mut issues = store::load_issues(root);
    let Some(i) = issues
        .iter_mut()
        .find(|x| x.get("key").and_then(|v| v.as_str()) == Some(key))
    else {
        return Err("not found".into());
    };
    let o = i.as_object_mut().unwrap();
    let mut fus: Vec<Value> = o
        .get("followUps")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if let Some(r) = ref_ {
        if !r.is_empty()
            && fus
                .iter()
                .any(|f| f.get("ref").and_then(|v| v.as_str()) == Some(r))
        {
            return Ok(false);
        }
    }
    let mut fu = json!({
        "at": now_iso(),
        "kind": kind,
        "text": text,
        "consumedAt": Value::Null,
    });
    if let Some(r) = ref_ {
        if !r.is_empty() {
            fu["ref"] = json!(r);
        }
    }
    fus.push(fu);
    o.insert("followUps".into(), json!(fus));
    o.insert("updatedAt".into(), json!(now_iso()));
    store::save_issues(root, &issues);
    Ok(true)
}

/// 未消費の followUps を全て consumedAt 記録（run の spawn 成功時に呼ぶ。
/// run 中の課題には後続追記を見せない凍結の対称処理）
pub fn consume_followups(root: &Path, key: &str) {
    let mut issues = store::load_issues(root);
    let Some(i) = issues
        .iter_mut()
        .find(|x| x.get("key").and_then(|v| v.as_str()) == Some(key))
    else {
        return;
    };
    let now = now_iso();
    let o = i.as_object_mut().unwrap();
    let mut fus: Vec<Value> = o
        .get("followUps")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut touched = false;
    for f in fus.iter_mut() {
        let consumed = f
            .get("consumedAt")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty());
        if !consumed {
            f.as_object_mut()
                .unwrap()
                .insert("consumedAt".into(), json!(now));
            touched = true;
        }
    }
    if touched {
        o.insert("followUps".into(), json!(fus));
        store::save_issues(root, &issues);
    }
}

/// 分割起票（manual-0577d650・issue-lifecycle.mmd）: 人操作で親課題を
/// 子課題群へ分ける。items 省略時は splitProposal.items を使う。
/// 子は source="split"・splitFrom=親 key で通常 intake 起票される
/// （queued へ。force 起票なので沈黙・rate-limit・storm 等のゲートは
/// 人判断として bypass）。親は splitInto に子 key 群を記録して
/// rejected へ（分割後の親自体は実行しない）。gated|queued のみ分割可
/// （run 開始後は手遅れ）。戻り値は更新後の親課題
pub fn split_issue(
    root: &Path,
    key: &str,
    items: Option<&Vec<Value>>,
) -> Result<Value, String> {
    let issues = store::load_issues(root);
    let Some(parent) = issues.iter().find(|i| {
        i.get("key").and_then(|v| v.as_str()) == Some(key)
    }) else {
        return Err("issue not found".into());
    };
    let cur = parent
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !matches!(cur, ST_GATED | ST_QUEUED) {
        return Err(format!("issue is {} (gated|queued required)", cur));
    }
    let items: Vec<Value> = match items {
        Some(v) if v.len() >= 2 => v.clone(),
        _ => parent
            .get("splitProposal")
            .and_then(|p| p.get("items"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
    };
    if items.len() < 2 {
        return Err("2 項目以上の items が必要".into());
    }
    let parent = parent.clone();
    let repo = parent.get("repo").and_then(|v| v.as_str()).unwrap_or("");
    let mut child_keys: Vec<String> = Vec::new();
    for it in &items {
        let title = it
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if title.is_empty() {
            continue;
        }
        let body = it
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or(title);
        let cand = json!({
            "source": "split",
            "repo": repo,
            "slug": parent.get("slug").cloned().unwrap_or(Value::Null),
            "title": title,
            "body": body,
            "kind": parent.get("kind").cloned().unwrap_or(Value::Null),
            "labels": parent.get("labels").cloned().unwrap_or(json!([])),
            "planGate": parent.get("planGate").cloned().unwrap_or(Value::Null),
            "detectedAt": now_iso(),
        });
        if let IntakeOutcome::Created(ch) = intake_inner(root, &cand, true) {
            let ck = ch
                .get("key")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !ck.is_empty() {
                let _ = set_field(root, &ck, "splitFrom", json!(key));
                child_keys.push(ck);
            }
        }
    }
    if child_keys.is_empty() {
        return Err("no child issues filed".into());
    }
    // 親の更新: splitInto 記録 → rejected へ（子群が実作業を担う）
    let mut issues = store::load_issues(root);
    let Some(i) = issues.iter_mut().find(|x| {
        x.get("key").and_then(|v| v.as_str()) == Some(key)
    }) else {
        return Err("issue not found".into());
    };
    {
        let o = i.as_object_mut().unwrap();
        o.insert("splitInto".into(), json!(child_keys));
        o.insert("splitAt".into(), json!(now_iso()));
    }
    transition(i, ST_REJECTED);
    let out = i.clone();
    store::save_issues(root, &issues);
    sync_queue(root, &issues);
    Ok(out)
}

/// work-queue.json を issues の status=queued と同期（queued は追加・それ以外は除去）。
fn sync_queue(root: &Path, issues: &[Value]) {
    let mut q = store::load_queue(root);
    let queued: Vec<&str> = issues
        .iter()
        .filter(|x| x.get("status").and_then(|v| v.as_str()) == Some(ST_QUEUED))
        .filter_map(|x| x.get("key").and_then(|v| v.as_str()))
        .collect();
    q.retain(|e| {
        e.get("key")
            .and_then(|v| v.as_str())
            .is_some_and(|k| queued.contains(&k))
    });
    for k in queued {
        if !q.iter().any(|e| e.get("key").and_then(|v| v.as_str()) == Some(k)) {
            q.push(json!({"key": k, "enqueuedAt": now_iso()}));
        }
    }
    store::save_queue(root, &q);
}

// ---- assess（README §5.2。ルールベース。LLM 判定は後続課題） ----

fn has_label(issue: &Value, pat: &str) -> bool {
    issue
        .get("labels")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .any(|l| l.to_lowercase().contains(pat))
        })
        .unwrap_or(false)
}

fn text_of(issue: &Value) -> String {
    format!(
        "{}\n{}",
        issue.get("title").and_then(|v| v.as_str()).unwrap_or(""),
        issue.get("body").and_then(|v| v.as_str()).unwrap_or("")
    )
    .to_lowercase()
}

/// verifyLevel（L0〜L4）のルール判定。全課題一律の高深度は要求しない。
fn rule_verify_level(issue: &Value) -> u64 {
    let t = text_of(issue);
    if t.contains("playwright") || t.contains("画面") || t.contains("ui") || has_label(issue, "ui") {
        3
    } else if t.contains("exe") || t.contains("実機") || t.contains("qgis") || has_label(issue, "binary") {
        2
    } else if t.contains("doc") || t.contains("mermaid") || t.contains(".mmd") || has_label(issue, "documentation") {
        0
    } else {
        1
    }
}

/// 爆破半径が大きい対象（自律実行せず gated へ）
fn blast_radius(issue: &Value) -> bool {
    let t = text_of(issue);
    t.contains("secrets.json") || t.contains("force-push") || t.contains("rm -rf")
}

/// 計画承認ゲート要否（manual-883d17ee）のルール判定。verifyLevel と同様に
/// 一律には要求しない: 検証深度・優先度の高い課題（L2+ または P2+）のみ
/// 実装前の計画レビューを経由させる。
pub fn rule_plan_gate(issue: &Value, level: u64, priority: u64) -> bool {
    // 明示指定（起票時の planGate フィールド）があればそれに従う
    if let Some(v) = issue.get("planGate").and_then(|v| v.as_bool()) {
        return v;
    }
    level >= 2 || priority >= 2
}

/// 単語単位の含有判定（"debug" が "bug" に誤爆しないように）
fn has_word(t: &str, words: &[&str]) -> bool {
    t.split(|c: char| !c.is_alphanumeric())
        .any(|w| words.contains(&w))
}

/// priority（0〜3）のルール判定。対応順の目安（README §5.2）。
/// 3=緊急（urgent/critical/security 系）、2=高（ci-failure・local-health 由来・
/// bug 系）、1=通常、0=低（documentation/enhancement/chore 系）
fn rule_priority(issue: &Value) -> u64 {
    let t = text_of(issue);
    if has_label(issue, "urgent")
        || has_label(issue, "critical")
        || has_label(issue, "security")
        || has_word(&t, &["security", "cve", "脆弱性"])
    {
        3
    } else if matches!(
        issue.get("source").and_then(|v| v.as_str()),
        Some("ci-failure") | Some("local-health")
    ) || has_label(issue, "bug")
        || has_word(&t, &["bug", "panic", "crash", "エラー", "失敗"])
    {
        2
    } else if has_label(issue, "documentation")
        || has_label(issue, "enhancement")
        || has_label(issue, "chore")
    {
        0
    } else {
        1
    }
}

/// 大規模課題の検出（manual-0577d650・issue-lifecycle.mmd）: 本文が
/// splitMaxBytes（既定4000・0=無効）超、またはトップレベル項目
/// （`## ` 見出し・行頭箇条書き）が splitMaxItems（既定8・0=無効）超なら
/// 分割提案を返す。Bugbot の PR サイズ上限スキップに倣い、大きすぎる
/// 課題はそのまま自律実行せず人の分割判断を待つ。
/// 戻り値は splitProposal {reason, items:[{title,body}]}（items 空も可）
fn oversized(cfg: &Value, issue: &Value) -> Option<Value> {
    let body = issue
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let max_bytes = cfg_f64(cfg, "splitMaxBytes", 4000.0) as usize;
    let max_items = cfg_f64(cfg, "splitMaxItems", 8.0) as usize;
    let items = split_items(body);
    let over_bytes = max_bytes > 0 && body.len() > max_bytes;
    let over_items = max_items > 0 && items.len() > max_items;
    if !over_bytes && !over_items {
        return None;
    }
    let reason = match (over_bytes, over_items) {
        (true, true) => format!("{}B・{}項目", body.len(), items.len()),
        (true, false) => format!("{}B", body.len()),
        _ => format!("{}項目", items.len()),
    };
    Some(json!({"reason": reason, "items": items}))
}

/// 本文から分割候補項目を抽出する。`## ` 系見出しがあれば見出し単位で
/// 分割（title=見出し・body=節全体）、なければ行頭 `- `/`* `/`- [ ] の
/// 箇条書きを1項目とする
fn split_items(body: &str) -> Vec<Value> {
    let mut items = Vec::new();
    let mut cur_title = String::new();
    let mut cur_body = String::new();
    let flush = |t: &mut String, b: &mut String, out: &mut Vec<Value>| {
        if !t.trim().is_empty() {
            out.push(json!({"title": t.trim(), "body": b.trim()}));
        }
        t.clear();
        b.clear();
    };
    let mut in_headings = false;
    for line in body.lines() {
        let tr = line.trim_start();
        if tr.starts_with("## ") || tr.starts_with("# ") {
            in_headings = true;
            flush(&mut cur_title, &mut cur_body, &mut items);
            cur_title = tr.trim_start_matches('#').trim().to_string();
            cur_body = line.to_string();
        } else if in_headings {
            cur_body.push('\n');
            cur_body.push_str(line);
        }
    }
    flush(&mut cur_title, &mut cur_body, &mut items);
    if !items.is_empty() {
        return items;
    }
    for line in body.lines() {
        let tr = line.trim();
        let item = tr
            .strip_prefix("- [ ] ")
            .or_else(|| tr.strip_prefix("- [x] "))
            .or_else(|| tr.strip_prefix("- "))
            .or_else(|| tr.strip_prefix("* "));
        if let Some(t) = item {
            if !t.trim().is_empty() {
                items.push(json!({"title": t.trim(), "body": line.trim()}));
            }
        }
    }
    items
}

/// assess: verifyLevel・受入条件・自律可否を決め、assessed → gated|queued へ遷移。
/// external は評価にも進めない（候補一覧表示のみ。README §5.3）。
/// 閾値超の大規模課題は splitProposal 付きで gated へ（manual-0577d650）。
fn assess(cfg: &Value, issues: &[Value], issue: &mut Value) {
    // assess 時の知見参照: related[] に類似事例（前回の fix / reject /
    // run 成否）を付記し、dispatch prompt の「類似過去事例」へ繋げる
    // （case-search.mmd・manual-a8c93d14）
    let related = cases::attach_related(issues, issue);
    let priority = rule_priority(issue);
    let autonomous = if has_label(issue, "external") {
        let o = issue.as_object_mut().unwrap();
        o.insert("autonomous".into(), json!(false));
        o.insert("priority".into(), json!(priority));
        transition(issue, ST_GATED);
        return;
    } else if blast_radius(issue) {
        let o = issue.as_object_mut().unwrap();
        o.insert("autonomous".into(), json!(false));
        o.insert("priority".into(), json!(priority));
        transition(issue, ST_GATED);
        return;
    } else {
        true
    };
    let title = issue
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // CBR（manual-654b11c2）: 高一致の成功 case があれば verifyLevel を
    // その値に合わせ、verifyLevelBasis に出典を記録する
    let mut basis = Value::Null;
    let level = match cases::adopted_case(&related) {
        Some(c) => {
            let lv = c
                .get("verifyLevel")
                .and_then(|v| v.as_u64())
                .unwrap_or_else(|| rule_verify_level(issue));
            basis = json!({
                "case": cases::str_of(&c, "key"),
                "score": c.get("score").cloned().unwrap_or(json!(0)),
            });
            lv
        }
        None => rule_verify_level(issue),
    };
    // 計画承認ゲート要否（manual-883d17ee）。planGate=true なら dispatch は
    // 実装の前に planning（計画生成+critic）→ plan_review（承認）を経由する
    let plan_gate = rule_plan_gate(issue, level, priority);
    // 大規模課題の検出（manual-0577d650。o 借用前に判定する）
    let split_prop = oversized(cfg, issue);
    let o = issue.as_object_mut().unwrap();
    o.insert("autonomous".into(), json!(autonomous));
    o.insert("priority".into(), json!(priority));
    o.insert("verifyLevel".into(), json!(level));
    o.insert("planGate".into(), json!(plan_gate));
    if !basis.is_null() {
        o.insert("verifyLevelBasis".into(), basis);
    }
    o.insert(
        "acceptance".into(),
        json!(format!("{} が期待どおり動作することを実機で確認する", title)),
    );
    // 大規模課題の分割提案（manual-0577d650・issue-lifecycle.mmd）:
    // 閾値超なら自律実行せず splitProposal 付きで人ゲートへ。人は
    // 「分割起票」で子課題群へ分けるか、そのまま対応（dispatch）する
    if let Some(prop) = split_prop {
        o.insert("splitProposal".into(), prop);
        transition(issue, ST_GATED);
        return;
    }
    transition(issue, ST_QUEUED);
}

/// assess 回帰評価用のプレビュー（eval-set.mmd・manual-a2de7417）:
/// 課題 spec の複製へ assess を適用し、判定結果
/// （verifyLevel/priority/autonomous/planGate）だけを返す。
/// 状態遷移・保存は行わない（複製への評価のみ）。
pub fn assess_preview(issues: &[Value], issue: &Value) -> Value {
    let mut clone = issue.clone();
    // cfg 未指定のプレビューは既定閾値で評価する
    assess(&json!({}), issues, &mut clone);
    json!({
        "verifyLevel": clone.get("verifyLevel").cloned().unwrap_or(Value::Null),
        "priority": clone.get("priority").cloned().unwrap_or(Value::Null),
        "autonomous": clone.get("autonomous").cloned().unwrap_or(Value::Null),
        "planGate": clone.get("planGate").cloned().unwrap_or(Value::Null),
    })
}

// ---- intake（detect → normalize → dedupe。detect-pipeline.mmd） ----

/// intake の結果。Deferred は起票レート制限で課題を作らず
/// candidate に留め置いた状態（candidates.json の deferred に記録するのは
/// 呼び出し側 detectors の責務）。
pub enum IntakeOutcome {
    Created(Value),
    Existing(Value),
    /// 親課題リンク（manual-488ea7f9）: candidate の parentIssue が既存課題に
    /// 帰属解決できた。新規起票はせず元課題へ followUps 追記+再実行要求済み
    Linked(Value),
    Deferred(String),
}

/// candidate の症状持続秒数。lastSeen−detectedAt（入口での再検出期間）と
/// detector emit の durationSeconds（検出時点で既に継続していた時間。
/// medik8s UnhealthyConditions.duration 相当）の大きい方
fn span_seconds(cand: &Value) -> f64 {
    let emitted = cand
        .get("durationSeconds")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let parse = |k: &str| {
        cand.get(k)
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
    };
    let span = match (parse("lastSeen"), parse("detectedAt")) {
        (Some(a), Some(b)) => (a - b).num_seconds() as f64,
        _ => 0.0,
    };
    emitted.max(span).max(0.0)
}

/// 起票クールダウン（detect-pipeline.mmd CD）: バケット単位で
/// 「直近の reject/verify NG から X 分は再 emit を抑止」する
/// （EDA throttle.once_within 相当）。バケットは dedupe hash とは
/// 別次元のパラメータで、config.intakeCooldownBy（既定
/// ["source","repo","kind"]）の属性等値でグルーピングする。
/// バケット内の課題が rejected/failed へ遷移してから
/// config.intakeCooldownMinutes（既定60分、0=無効）未満なら抑止する。
fn bucket_cooling(cfg: &Value, issues: &[Value], cand: &Value) -> bool {
    let minutes = cfg_f64(cfg, "intakeCooldownMinutes", 60.0);
    if minutes <= 0.0 {
        return false;
    }
    let mut by: Vec<String> = cfg_arr(cfg, "intakeCooldownBy")
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    if by.is_empty() {
        by = vec!["source".into(), "repo".into(), "kind".into()];
    }
    let cutoff = chrono::Utc::now() - chrono::Duration::seconds((minutes * 60.0) as i64);
    issues.iter().any(|i| {
        if !matches!(
            i.get("status").and_then(|v| v.as_str()),
            Some(ST_REJECTED) | Some(ST_FAILED)
        ) {
            return false;
        }
        let recent = i
            .get("updatedAt")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t >= cutoff)
            .unwrap_or(false);
        if !recent {
            return false;
        }
        by.iter().all(|attr| {
            cand.get(attr.as_str()).and_then(|v| v.as_str()).unwrap_or("")
                == i.get(attr.as_str()).and_then(|v| v.as_str()).unwrap_or("")
        })
    })
}

/// silence（detect-pipeline.mmd SIL。Alertmanager silence 相当）:
/// silences.json の matcher（source/repo/slug/kind/severity の等値・
/// 未指定はワイルドカード）に合う候補を、startsAt〜endsAt の窓内で
/// 課題化しない（計画作業・定期メンテ中の既知症状のミュート）。
/// endsAt は必須で期限切れは自動解除（save_silences が掃除する）。
/// 戻り値は照合した silence の id（id 無しなら空文字）。
fn silenced_by(root: &Path, cand: &Value) -> Option<String> {
    let now = chrono::Utc::now();
    let ts = |v: &str| {
        chrono::DateTime::parse_from_rfc3339(v)
            .map(|t| t.with_timezone(&chrono::Utc))
            .ok()
    };
    for s in store::load_silences(root) {
        if let Some(t) = s.get("startsAt").and_then(|v| v.as_str()).and_then(ts) {
            if now < t {
                continue; // 開始前の予約窓
            }
        }
        match s.get("endsAt").and_then(|v| v.as_str()).and_then(ts) {
            Some(t) if now < t => {}
            _ => continue, // endsAt 必須・期限切れは照合しない
        }
        let m = s.get("matchers").cloned().unwrap_or(json!({}));
        let hit = ["source", "repo", "slug", "kind", "severity"]
            .iter()
            .all(|attr| match m.get(*attr).and_then(|v| v.as_str()) {
                Some(w) if !w.is_empty() => {
                    cand.get(*attr).and_then(|v| v.as_str()).unwrap_or("") == w
                }
                _ => true,
            });
        if hit {
            return Some(
                s.get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            );
        }
    }
    None
}

/// 課題が open（終端 = done/failed/rejected/dropped 以外）か
fn is_open(issue: &Value) -> bool {
    !matches!(
        issue.get("status").and_then(|v| v.as_str()),
        Some(ST_DONE) | Some(ST_FAILED) | Some(ST_REJECTED) | Some(ST_DROPPED)
    )
}

/// inhibition（detect-pipeline.mmd INH。Alertmanager inhibit_rules 相当）:
/// 原因課題が open の間、その影響で出る下流症状を別課題として起票しない
/// （例: ビルド破壊課題 open 中の派生 CI-failure）。
/// ルールは config.inhibitRules:
///   [{"cause": {source?,repo?,kind?,slug?}, "effect": {...同}, "match": ["repo"]}]
/// cause/effect は属性等値の matcher（未指定属性はワイルドカード）、
/// match は原因課題と下流候補の間で値が一致すべき属性
/// （Alertmanager の equal。既定 ["repo"]）。
/// 候補が inhibitedBy:<issueKey> を明示する場合はその課題が open なら抑制。
/// 戻り値は抑制している原因課題の key（抑制しないなら None）。
fn inhibited_by(cfg: &Value, issues: &[Value], cand: &Value) -> Option<String> {
    // 明示指定: candidate が原因課題 key を名指しする
    if let Some(k) = cand.get("inhibitedBy").and_then(|v| v.as_str()) {
        if !k.is_empty() {
            return issues
                .iter()
                .find(|i| i.get("key").and_then(|v| v.as_str()) == Some(k) && is_open(i))
                .map(|_| k.to_string());
        }
    }
    let matcher = |m: &Value, v: &Value| -> bool {
        ["source", "repo", "slug", "kind", "severity"]
            .iter()
            .all(|attr| match m.get(*attr).and_then(|x| x.as_str()) {
                Some(w) if !w.is_empty() => {
                    v.get(*attr).and_then(|x| x.as_str()).unwrap_or("") == w
                }
                _ => true,
            })
    };
    for rule in cfg_arr(cfg, "inhibitRules") {
        let effect = rule.get("effect").cloned().unwrap_or(json!({}));
        if !matcher(&effect, cand) {
            continue;
        }
        let match_attrs: Vec<String> = rule
            .get("match")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_else(|| vec!["repo".to_string()]);
        let cause = rule.get("cause").cloned().unwrap_or(json!({}));
        let found = issues.iter().find(|i| {
            is_open(i)
                && matcher(&cause, i)
                && match_attrs.iter().all(|attr| {
                    cand.get(attr.as_str()).and_then(|v| v.as_str()).unwrap_or("")
                        == i.get(attr.as_str()).and_then(|v| v.as_str()).unwrap_or("")
                })
        });
        if let Some(i) = found {
            return i.get("key").and_then(|v| v.as_str()).map(String::from);
        }
    }
    None
}

/// 課題ストームの一括事案化（detect-pipeline.mmd STORM。manual-a7e8d412）:
/// 同一 source+repo で stormWindowMinutes（既定10分）内に stormBatchMin
/// （既定5・0=無効）件以上の起票が集中したら、それ以上は個別起票せず
/// 1件の「一括事案」課題（source="storm"・status=gated・人レビュー要求）
/// へ集約する。open の storm 課題がある間は後続候補もその事案の
/// stormMembers に追加する（上流障害サインを1件で見せる）。
/// 戻り値は集約先 storm 課題の key。
fn storm_aggregate(cfg: &Value, issues: &mut Vec<Value>, cand: &Value) -> Option<String> {
    let min = cfg_f64(cfg, "stormBatchMin", 5.0) as i64;
    if min <= 0 {
        return None;
    }
    let win_min = cfg_f64(cfg, "stormWindowMinutes", 10.0) as i64;
    let src = cand.get("source").and_then(|v| v.as_str()).unwrap_or("");
    let repo = cand.get("repo").and_then(|v| v.as_str()).unwrap_or("");
    let since = chrono::Utc::now() - chrono::Duration::minutes(win_min);
    let detected = |i: &Value| {
        i.get("detectedAt")
            .and_then(|v| v.as_str())
            .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
            .map(|t| t.with_timezone(&chrono::Utc))
    };
    // 集中判定: 窓内に起票された同一 source+repo の非 storm 課題数
    let n = issues
        .iter()
        .filter(|i| {
            i.get("storm").and_then(|v| v.as_bool()) != Some(true)
                && i.get("source").and_then(|v| v.as_str()) == Some(src)
                && i.get("repo").and_then(|v| v.as_str()) == Some(repo)
                && detected(i).map(|t| t > since).unwrap_or(false)
        })
        .count();
    // 集約先: open の storm 課題（同じ上流 source+repo）。人が終端へ落とす
    // まで後続候補は全てこの事案へ入る
    let open_storm = issues.iter().position(|i| {
        i.get("storm").and_then(|v| v.as_bool()) == Some(true)
            && is_open(i)
            && i.get("repo").and_then(|v| v.as_str()) == Some(repo)
            && i.get("stormSource").and_then(|v| v.as_str()) == Some(src)
    });
    if open_storm.is_none() && n < min as usize {
        return None;
    }
    let csh = cand
        .get("symptomHash")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| {
            store::symptom_hash(
                cand.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                cand.get("body").and_then(|v| v.as_str()).unwrap_or(""),
            )
        });
    let ck = store::issue_key(src, repo, &csh);
    let now = now_iso();
    let pos = open_storm.unwrap_or_else(|| {
        let skey = store::issue_key("storm", repo, &format!("{}", crate::util::rand_hex(4)));
        issues.push(json!({
            "key": skey,
            "source": "storm",
            "stormSource": src,
            "repo": repo,
            "slug": cand.get("slug").cloned().unwrap_or(json!(repo)),
            "title": format!(
                "課題ストーム: {} @ {} （{}分で{}件+の集中起票）",
                src, repo, win_min, min),
            "body": format!(
                "同一 source+repo の課題が {} 分以内に {} 件以上起票されました。\n\
                 上流障害または detector 暴走のサインです。\n\
                 stormMembers の内容を確認して対応を判断してください。\n\
                 open の間、後続の同桶候補はこの事案へ集約されます。",
                win_min, min),
            "symptomHash": format!("storm-{}", crate::util::rand_hex(4)),
            "detectedAt": now, "lastSeen": now, "updatedAt": now,
            "status": ST_GATED,
            "autonomous": false,
            "storm": true,
            "stormMembers": [],
            "labels": ["storm"],
            "priority": 1,
            "verifyLevel": 0,
            "runs": [],
        }));
        issues.len() - 1
    });
    let skey = issues[pos]
        .get("key")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let o = issues[pos].as_object_mut().unwrap();
    let mut mem: Vec<Value> = o
        .get("stormMembers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if !mem.iter().any(|m| m.get("key").and_then(|v| v.as_str()) == Some(ck.as_str())) {
        mem.push(json!({
            "key": ck,
            "title": cand.get("title").and_then(|v| v.as_str()).unwrap_or(""),
            "detectedAt": cand.get("detectedAt").cloned().unwrap_or(json!(now)),
        }));
        o.insert("stormMembers".into(), json!(mem));
        o.insert("lastSeen".into(), json!(now));
        o.insert("updatedAt".into(), json!(now));
    }
    Some(skey)
}

/// observing（保留観測。detect-pipeline.mmd OBS）の保留判定。
/// 一過性 flap の起票を構造的に排除するため、detector 由来の candidate は
/// 「N 分継続 or M 回再検出」を満たすまで課題化せず保留する:
///   - config.intakeObserveMinutes（既定 10）… lastSeen−detectedAt または
///     emit の durationSeconds がこの窓を超えると昇格
///   - config.intakeObserveMinHits（既定 2）… 再検出がこの回数に達すれば昇格
///   - config.observingExempt（既定 ["manual","derived"]）… 人・派生の
///     明示入力は観測せず即起票する
/// 単発検出で窓内に消えた（再検出されない）症状は hits=1・span=0 のまま
/// 昇格せず、課題を作らない。
fn observing_hold(cfg: &Value, cand: &Value) -> bool {
    let minutes = cfg_f64(cfg, "intakeObserveMinutes", 10.0);
    let min_hits = cfg_u64(cfg, "intakeObserveMinHits", 2);
    if minutes <= 0.0 && min_hits <= 1 {
        return false; // 観測窓なし = 即起票（従来動作）
    }
    let source = cand.get("source").and_then(|v| v.as_str()).unwrap_or("manual");
    let mut exempt: Vec<String> = cfg_arr(cfg, "observingExempt")
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    if exempt.is_empty() {
        exempt = vec!["manual".to_string(), "derived".to_string()];
    }
    if exempt.iter().any(|s| s == source) {
        return false;
    }
    // 昇格条件: 再検出回数 or 持続時間が閾値に達したら起票する
    let hits = cand
        .get("hits")
        .or_else(|| cand.get("count"))
        .and_then(|v| v.as_u64())
        .unwrap_or(1);
    if hits >= min_hits {
        return false;
    }
    if minutes > 0.0 && span_seconds(cand) >= minutes * 60.0 {
        return false;
    }
    true
}

/// 起票ストーム抑制（README §5.1）: config.intakeMaxPerHour（既定 0=無制限）
/// を超える source+repo 単位の新規起票を止める。直近1時間の起票数を
/// detectedAt で数える。
fn rate_limited(cfg: &Value, issues: &[Value], source: &str, repo: &str) -> bool {
    let max = cfg_u64(cfg, "intakeMaxPerHour", 0);
    if max == 0 {
        return false;
    }
    let cutoff = chrono::Utc::now() - chrono::Duration::hours(1);
    let n = issues
        .iter()
        .filter(|i| {
            i.get("source").and_then(|v| v.as_str()) == Some(source)
                && i.get("repo").and_then(|v| v.as_str()) == Some(repo)
                && i
                    .get("detectedAt")
                    .or_else(|| i.get("updatedAt"))
                    .and_then(|v| v.as_str())
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|t| t >= cutoff)
                    .unwrap_or(false)
        })
        .count() as u64;
    n >= max
}

/// candidate {source, repo, title, body, symptomHash?, slug?, number?, url?, labels?, kind?}
/// を入口で正規化・重複判定する。既出は lastSeen のみ更新、新規は課題レコード作成
/// → assess → queued|gated へ。
/// dedupe はこの1箇所のみ（detector 個別には持たせない。AGENTS.md）
pub fn intake(root: &Path, cand: &Value) -> IntakeOutcome {
    intake_inner(root, cand, false)
}

/// 人操作（candidates 一覧の対応/却下ボタン）による即時起票は
/// 起票レート制限を bypass する（人判断はレート制限より優先。
/// detect-pipeline.mmd DEFER → INBOX）
pub fn intake_forced(root: &Path, cand: &Value) -> IntakeOutcome {
    intake_inner(root, cand, true)
}

fn intake_inner(root: &Path, cand: &Value, force: bool) -> IntakeOutcome {
    let source = cand.get("source").and_then(|v| v.as_str()).unwrap_or("manual");
    let repo = cand.get("repo").and_then(|v| v.as_str()).unwrap_or("");
    let title = cand.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let body = cand.get("body").and_then(|v| v.as_str()).unwrap_or("");
    let sh = cand
        .get("symptomHash")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| store::symptom_hash(title, body));
    let key = store::issue_key(source, repo, &sh);

    let mut issues = store::load_issues(root);
    if let Some(ex) = issues
        .iter_mut()
        .find(|x| x.get("key").and_then(|v| v.as_str()) == Some(key.as_str()))
    {
        // 既出: lastSeen 更新のみ（detect-pipeline.mmd）
        let o = ex.as_object_mut().unwrap();
        o.insert("lastSeen".into(), json!(now_iso()));
        let out = ex.clone();
        store::save_issues(root, &issues);
        return IntakeOutcome::Existing(out);
    }

    // 親課題リンク（manual-488ea7f9）: candidate が parentIssue（例: ci-failure
    // detector が失敗コミットの Issue: トレーラから逆引きした帰属 key）を
    // emit したら既存課題へ帰属させる。新規課題は作らず、元課題の followUps
    // へ追記（ref=候補 key で重複抑止）し、failed の元課題は queued へ
    // 差し戻して再実行を要求する。帰属できない・parentIssue 無しは従来どおり
    // 新規起票へ進む。force（人の対応ボタン）はこの帰属を bypass する
    if !force {
        if let Some(pk) = cand.get("parentIssue").and_then(|v| v.as_str()) {
            if !pk.is_empty() {
                if let Some(pos) = issues
                    .iter()
                    .position(|i| i.get("key").and_then(|v| v.as_str()) == Some(pk))
                {
                    let fu_ref = key.clone();
                    let fu_text = format!(
                        "{} — {}",
                        title,
                        cand.get("url").and_then(|v| v.as_str()).unwrap_or("")
                    );
                    let o = issues[pos].as_object_mut().unwrap();
                    let mut fus: Vec<Value> = o
                        .get("followUps")
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default();
                    if !fus
                        .iter()
                        .any(|f| f.get("ref").and_then(|v| v.as_str()) == Some(fu_ref.as_str()))
                    {
                        fus.push(json!({
                            "at": now_iso(),
                            "kind": source,
                            "text": fu_text,
                            "ref": fu_ref,
                            "consumedAt": Value::Null,
                        }));
                        o.insert("followUps".into(), json!(fus));
                    }
                    o.insert("updatedAt".into(), json!(now_iso()));
                    // 再実行要求: failed の元課題は queued へ差し戻す
                    if issues[pos]
                        .get("status")
                        .and_then(|v| v.as_str())
                        == Some(ST_FAILED)
                    {
                        transition(&mut issues[pos], ST_QUEUED);
                    }
                    let parent = issues[pos].clone();
                    store::save_issues(root, &issues);
                    sync_queue(root, &issues);
                    return IntakeOutcome::Linked(parent);
                }
            }
        }
    }

    let cfg = load_config(root);
    // silence（detect-pipeline.mmd SIL）: メンテ窓・既知症状のミュート。
    // matcher+期限に合う候補は課題を作らない（期限切れは自動解除）
    if !force {
        if let Some(sid) = silenced_by(root, cand) {
            return IntakeOutcome::Deferred(format!("silenced:{}", sid));
        }
    }

    // 起票クールダウン（detect-pipeline.mmd CD）: 同一バケットの課題が
    // reject/verify NG で終端へ落ちた直後の再 emit は課題を作らない
    if !force && bucket_cooling(&cfg, &issues, cand) {
        return IntakeOutcome::Deferred("cooldown".into());
    }

    // inhibition（detect-pipeline.mmd INH）: 原因課題が open の間は下流症状を
    // 別課題にしない。抑制中は原因課題の inhibits に候補を追記し、
    // candidate は inhibitedBy=<原因課題key> として一覧に表示する
    if !force {
        if let Some(cause_key) = inhibited_by(&cfg, &issues, cand) {
            if let Some(ci) = issues.iter_mut().find(|i| {
                i.get("key").and_then(|v| v.as_str()) == Some(cause_key.as_str())
            }) {
                let o = ci.as_object_mut().unwrap();
                let mut inh: Vec<Value> = o
                    .get("inhibits")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                if !inh.iter().any(|v| v.as_str() == Some(key.as_str())) {
                    inh.push(json!(key));
                    o.insert("inhibits".into(), json!(inh));
                    store::save_issues(root, &issues);
                }
            }
            return IntakeOutcome::Deferred(format!("inhibited:{}", cause_key));
        }
    }

    // observing（detect-pipeline.mmd OBS）: 一過性症状を即起票しない。
    // 昇格条件（再検出回数・持続時間）未満は課題を作らず candidate に留め置く
    if !force && observing_hold(&cfg, cand) {
        return IntakeOutcome::Deferred("observing".into());
    }

    // 起票レート制限（detect-pipeline.mmd RATE）。超過時は課題を作らず
    // candidate に留め置き、drain の reconcile が窓の空いた後に再試行する
    if !force && rate_limited(&cfg, &issues, source, repo) {
        return IntakeOutcome::Deferred("rate-limit".into());
    }

    // 課題ストームの一括事案化（detect-pipeline.mmd STORM）: 同一
    // source+repo の集中起票は個別課題でなく1件の gated 事案へ集約する
    if !force {
        if let Some(skey) = storm_aggregate(&cfg, &mut issues, cand) {
            store::save_issues(root, &issues);
            return IntakeOutcome::Deferred(format!("storm:{}", skey));
        }
    }

    let now = now_iso();
    let mut issue = json!({
        "key": key,
        "source": source,
        "repo": repo,
        "slug": cand.get("slug").cloned().unwrap_or(Value::Null),
        "number": cand.get("number").cloned().unwrap_or(Value::Null),
        "url": cand.get("url").cloned().unwrap_or(Value::Null),
        "title": title,
        "body": body,
        "kind": cand.get("kind").cloned().unwrap_or(Value::Null),
        "labels": cand.get("labels").cloned().unwrap_or(json!([])),
        "ghState": cand.get("ghState").cloned().unwrap_or(Value::Null),
        "ghStateReason": cand.get("ghStateReason").cloned().unwrap_or(Value::Null),
        "planGate": cand.get("planGate").cloned().unwrap_or(Value::Null),
        "symptomHash": sh,
        "status": ST_ASSESSED,
        // intake スナップショット（intake-snapshot.mmd・manual-15667606）:
        // 起票時点の課題コンテキストを freeze。dispatch prompt はこの
        // スナップショットを使い、起票後の追記は followUps として
        // 別途記録し run 中には見せない
        "intakeSnapshot": {
            "title": title,
            "body": body,
            "at": now,
        },
        "followUps": [],
        "autonomous": false,
        "retries": 0,
        "verifyBacks": 0,
        "detectedAt": cand.get("detectedAt").and_then(|v| v.as_str()).unwrap_or(&now),
        "lastSeen": now,
        "updatedAt": now,
        "runs": [],
    });
    // GitHub closed Issue の取込（--import-github）: GitHub 側で既に完了済みの
    // 履歴として終端状態で起票する（COMPLETED→done / NOT_PLANNED→rejected）。
    // 新規レコードの初期状態であり allowed() の遷移ではない。assess は
    // 実行計画（verifyLevel・受入条件）を立てないためスキップする
    let gh_closed = cand
        .get("ghState")
        .and_then(|v| v.as_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("closed"));
    if gh_closed {
        let st = match cand.get("ghStateReason").and_then(|v| v.as_str()) {
            Some(r) if r.eq_ignore_ascii_case("not_planned") => ST_REJECTED,
            _ => ST_DONE,
        };
        let prio = rule_priority(&issue);
        let o = issue.as_object_mut().unwrap();
        o.insert("status".into(), json!(st));
        o.insert("priority".into(), json!(prio));
        o.insert("verifyLevel".into(), json!(0));
    } else {
        assess(&cfg, &issues, &mut issue);
        // config.planGate=false は計画ゲートの全体無効化
        // （candidate/課題の明示 planGate:true は残す）
        if !crate::config::cfg_bool_default_true(&cfg, "planGate")
            && cand.get("planGate").and_then(|v| v.as_bool()) != Some(true)
        {
            issue
                .as_object_mut()
                .unwrap()
                .insert("planGate".into(), json!(false));
        }
    }
    issues.push(issue.clone());
    store::save_issues(root, &issues);
    sync_queue(root, &issues);
    // 事案集約（incidents.mmd・manual-343b5d37）: 関連する open 課題が
    // あれば同一事案へ所属化し issue.incident を記録する。
    // GitHub closed 取込は過去履歴のため集約対象外
    if !gh_closed
        && crate::incidents::auto_assign(root, &issue).is_some()
    {
        let k = issue
            .get("key")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        issue = store::load_issues(root)
            .into_iter()
            .find(|i| i.get("key").and_then(|v| v.as_str()) == Some(k.as_str()))
            .unwrap_or(issue);
    }
    IntakeOutcome::Created(issue)
}

// ---- dispatch 判定（論理競合ベース。README §5.4） ----
// dispatch は人判断のみ: queued の起動は一覧の対応ボタン
// （POST /api/issues/:key/dispatch → runner::dispatch_issue）だけが行い、
// drain は queued を自動起動しない（issue-lifecycle.mmd）。

/// queued 課題を起動してよいか。同一 repo に running|verifying の課題があれば
/// Err(ブロック中の key)。実機リソースの排他は locks.rs の責務。
/// verify NG 差し戻しの指数バックオフ（retryAfter）中も Err を返す
/// （verify NG ループを構造的に空転させない。manual-6c3ed946）。
/// config.maxConcurrentRuns（既定4・0=無制限）超の同時 running|verifying
/// でも Err（ブラスト半径上限。manual-a7e8d412）。
pub fn can_dispatch(cfg: &Value, issues: &[Value], key: &str) -> Result<(), String> {
    let Some(issue) = issues
        .iter()
        .find(|x| x.get("key").and_then(|v| v.as_str()) == Some(key))
    else {
        return Err("not found".into());
    };
    if issue.get("status").and_then(|v| v.as_str()) != Some(ST_QUEUED) {
        return Err("not queued".into());
    }
    if let Some(ra) = issue.get("retryAfter").and_then(|v| v.as_str()) {
        if chrono::DateTime::parse_from_rfc3339(ra)
            .map(|t| t > chrono::Utc::now())
            .unwrap_or(false)
        {
            return Err(format!("retry backoff until {}", ra));
        }
    }
    // 同時実行上限（medik8s maxUnhealthy 相当のクォーラム）。
    // planning（計画生成中）は dispatch 枠を占有する（plan_review は
    // 実行リソースを使わない待機のため数えない）
    let max_runs = cfg_f64(cfg, "maxConcurrentRuns", 4.0) as i64;
    if max_runs > 0 {
        let active = issues
            .iter()
            .filter(|x| {
                matches!(
                    x.get("status").and_then(|v| v.as_str()),
                    Some(ST_RUNNING) | Some(ST_VERIFYING) | Some(ST_PLANNING) | Some(ST_ROLLBACK)
                )
            })
            .count();
        if active >= max_runs as usize {
            return Err(format!("maxConcurrentRuns {} reached", max_runs));
        }
    }
    let repo = issue.get("repo").and_then(|v| v.as_str()).unwrap_or("");
    if let Some(b) = issues.iter().find(|x| {
        x.get("key").and_then(|v| v.as_str()) != Some(key)
            && x.get("repo").and_then(|v| v.as_str()) == Some(repo)
            && matches!(
                x.get("status").and_then(|v| v.as_str()),
                Some(ST_RUNNING) | Some(ST_VERIFYING) | Some(ST_PLANNING) | Some(ST_ROLLBACK)
            )
    }) {
        return Err(format!(
            "blocked by {}",
            b.get("key").and_then(|v| v.as_str()).unwrap_or("")
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "kasugai-pipeline-test-{}-{}",
            tag,
            crate::util::rand_hex(4)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn cand(title: &str, body: &str) -> Value {
        // hits=99 で observing 昇格済み扱い（従来テストは即起票の挙動を見る）
        json!({"source": "test", "repo": "me/repo", "title": title, "body": body,
               "hits": 99})
    }

    /// detector 由来（観測対象。hits 未設定=初検出）の candidate
    fn det_cand(title: &str, body: &str) -> Value {
        json!({"source": "ci-failure", "repo": "me/repo", "title": title,
               "body": body})
    }

    fn issue_at(status: &str) -> Value {
        json!({"key": "k", "status": status})
    }

    fn queued_keys(root: &Path) -> Vec<String> {
        store::load_queue(root)
            .iter()
            .filter_map(|e| e.get("key").and_then(|v| v.as_str()).map(String::from))
            .collect()
    }

    fn intake_v(root: &Path, c: &Value) -> (Value, bool) {
        match intake(root, c) {
            IntakeOutcome::Created(i) => (i, true),
            IntakeOutcome::Existing(i) => (i, false),
            IntakeOutcome::Linked(i) => (i, false),
            IntakeOutcome::Deferred(_) => (Value::Null, false),
        }
    }

    #[test]
    fn intake_creates_queued_issue_then_dedupes() {
        let root = temp_root("intake");
        let (issue, created) = intake_v(&root, &cand("panic in parser", "crashes on startup"));
        assert!(created);
        assert_eq!(issue["status"], json!(ST_QUEUED));
        assert_eq!(issue["verifyLevel"], json!(1));
        assert_eq!(issue["autonomous"], json!(true));
        let key = issue["key"].as_str().unwrap().to_string();

        // 同一 candidate 再投入: created=false・lastSeen 更新のみ
        let mut issues = store::load_issues(&root);
        issues[0]
            .as_object_mut()
            .unwrap()
            .insert("lastSeen".into(), json!("2000-01-01T00:00:00.000Z"));
        issues[0]
            .as_object_mut()
            .unwrap()
            .insert("title".into(), json!("edited title stays"));
        store::save_issues(&root, &issues);

        let (again, created) = intake_v(&root, &cand("panic in parser", "crashes on startup"));
        assert!(!created);
        assert_eq!(again["key"], json!(key));
        assert_ne!(again["lastSeen"], json!("2000-01-01T00:00:00.000Z"));
        assert_eq!(again["title"], json!("edited title stays"));
        assert_eq!(store::load_issues(&root).len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn intake_external_label_gates_issue() {
        let root = temp_root("ext");
        let mut c = cand("panic in parser", "crashes on startup");
        c["labels"] = json!(["external"]);
        let (issue, created) = intake_v(&root, &c);
        assert!(created);
        assert_eq!(issue["status"], json!(ST_GATED));
        assert_eq!(issue["autonomous"], json!(false));
        // gated は work-queue に入らない
        assert!(queued_keys(&root).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn transition_allowed_table() {
        assert!(transition(&mut issue_at(ST_GATED), ST_QUEUED));
        assert!(!transition(&mut issue_at(ST_QUEUED), ST_DONE));
        assert!(transition(&mut issue_at(ST_VERIFYING), ST_RETRY));
        assert!(transition(&mut issue_at(ST_RETRY), ST_FAILED));
        assert!(transition(&mut issue_at(ST_FAILED), ST_QUEUED));
        assert!(transition(&mut issue_at(ST_FAILED), ST_REJECTED));
        // 不許可遷移は状態を変えない
        let mut i = issue_at(ST_QUEUED);
        assert!(!transition(&mut i, ST_DONE));
        assert_eq!(i["status"], json!(ST_QUEUED));
        // retry 遷移は verifyBacks をインクリメント
        let mut i = issue_at(ST_VERIFYING);
        assert!(transition(&mut i, ST_RETRY));
        assert_eq!(i["verifyBacks"], json!(1));
    }

    #[test]
    fn request_transition_syncs_work_queue() {
        let root = temp_root("queue");
        let (issue, _) = intake_v(&root, &cand("panic in parser", "crashes on startup"));
        let key = issue["key"].as_str().unwrap().to_string();
        assert_eq!(queued_keys(&root), vec![key.clone()]);

        // queued → running で queue から除去
        assert!(request_transition(&root, &key, ST_RUNNING).is_ok());
        assert!(queued_keys(&root).is_empty());

        // running → verifying → retry → queued で queue に復帰
        assert!(request_transition(&root, &key, ST_VERIFYING).is_ok());
        assert!(request_transition(&root, &key, ST_RETRY).is_ok());
        assert!(request_transition(&root, &key, ST_QUEUED).is_ok());
        assert_eq!(queued_keys(&root), vec![key.clone()]);

        // 不許可遷移は Err・状態も queue も変わらない
        assert!(request_transition(&root, &key, ST_DONE).is_err());
        assert_eq!(queued_keys(&root), vec![key.clone()]);
        assert!(request_transition(&root, "no-such-key", ST_QUEUED).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bucket_cooldown_defers_after_terminal_issue() {
        let root = temp_root("cooldown");
        // 同一バケット（source+repo+kind）に rejected 課題が直近にある
        store::save_issues(
            &root,
            &[json!({
                "key": "ci-failure:me/repo:old",
                "source": "ci-failure", "repo": "me/repo", "kind": "build",
                "symptomHash": "old", "status": ST_REJECTED,
                "updatedAt": now_iso(),
            })],
        );
        // 別症状・同バケットの再 emit は cooldown で留め置く
        let mut c = det_cand("another build failure", "b");
        c["kind"] = json!("build");
        c["hits"] = json!(99);
        match intake(&root, &c) {
            IntakeOutcome::Deferred(r) => assert_eq!(r, "cooldown"),
            _ => panic!("expected cooldown deferral"),
        }
        assert!(store::load_issues(&root).len() == 1);
        // 別 kind は別バケット → 起票できる
        let mut c2 = det_cand("test failure", "b");
        c2["kind"] = json!("test");
        c2["hits"] = json!(99);
        match intake(&root, &c2) {
            IntakeOutcome::Created(_) => {}
            _ => panic!("different bucket should file"),
        }
        // クールダウン窓を過ぎた終端課題は抑止しない（同バケットでも起票できる）
        let old = (chrono::Utc::now() - chrono::Duration::hours(2))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        store::save_issues(
            &root,
            &[json!({
                "key": "ci-failure:me/repo:old",
                "source": "ci-failure", "repo": "me/repo", "kind": "build",
                "symptomHash": "old", "status": ST_FAILED,
                "updatedAt": old,
            })],
        );
        let mut c3 = det_cand("later build failure", "b");
        c3["kind"] = json!("build");
        c3["hits"] = json!(99);
        match intake(&root, &c3) {
            IntakeOutcome::Created(_) => {}
            IntakeOutcome::Deferred(r) => panic!("cooldown expired, got {}", r),
            _ => panic!("expected Created after cooldown window"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- silence（matcher+期限の起票ミュート） ----

    #[test]
    fn silence_defers_matched_and_expires() {
        let root = temp_root("silence");
        let fut = (chrono::Utc::now() + chrono::Duration::minutes(30))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        store::save_silences(
            &root,
            &[json!({
                "id": "s1", "endsAt": fut,
                "matchers": {"source": "ci-failure", "kind": "disk"},
            })],
        );
        let mut c = det_cand("disk almost full", "b");
        c["kind"] = json!("disk");
        c["hits"] = json!(99);
        match intake(&root, &c) {
            IntakeOutcome::Deferred(r) => assert_eq!(r, "silenced:s1"),
            _ => panic!("expected silenced deferral"),
        }
        // matcher 不一致はミュートされない
        let mut c2 = det_cand("port conflict", "b");
        c2["kind"] = json!("port");
        c2["hits"] = json!(99);
        match intake(&root, &c2) {
            IntakeOutcome::Created(_) => {}
            IntakeOutcome::Deferred(r) => panic!("expected Created, got {}", r),
            _ => panic!("expected Created"),
        }
        // 期限切れは照合しない（起票できる）。save_silences が期限切れを
        // 除去するので台帳にも残らない
        let past = (chrono::Utc::now() - chrono::Duration::minutes(1))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        crate::config::save_j(
            &store::silences_path(&root),
            &json!([{"id": "s2", "endsAt": past,
                "matchers": {"source": "ci-failure"}}]),
        );
        match intake(&root, &c) {
            IntakeOutcome::Created(_) => {}
            IntakeOutcome::Deferred(r) => panic!("expired silence, got {}", r),
            _ => panic!("expected Created"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn silence_scheduled_window_and_force_bypass() {
        let root = temp_root("silence2");
        // 開始前の予約窓は照合しない
        let st = (chrono::Utc::now() + chrono::Duration::minutes(10))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let en = (chrono::Utc::now() + chrono::Duration::minutes(40))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        store::save_silences(
            &root,
            &[json!({"id": "w1", "startsAt": st, "endsAt": en,
                "matchers": {"kind": "disk"}})],
        );
        let mut c = det_cand("disk almost full", "b");
        c["kind"] = json!("disk");
        c["hits"] = json!(99);
        match intake(&root, &c) {
            IntakeOutcome::Created(_) => {}
            IntakeOutcome::Deferred(r) => panic!("not yet started, got {}", r),
            _ => panic!("expected Created"),
        }
        // アクティブ窓では force 起票（人判断）がミュートを bypass する。
        // 別症状で試す（同じ候補は既起票で dedupe に引っ掛かる）
        let now_s = (chrono::Utc::now() - chrono::Duration::minutes(1))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        store::save_silences(
            &root,
            &[json!({"id": "w2", "startsAt": now_s, "endsAt": en,
                "matchers": {"kind": "disk"}})],
        );
        let mut c2 = det_cand("disk io stalls", "b");
        c2["kind"] = json!("disk");
        c2["hits"] = json!(99);
        match intake(&root, &c2) {
            IntakeOutcome::Deferred(r) => assert_eq!(r, "silenced:w2"),
            _ => panic!("expected silenced"),
        }
        match intake_forced(&root, &c2) {
            IntakeOutcome::Created(_) | IntakeOutcome::Existing(_) => {}
            IntakeOutcome::Linked(_) => {}
            IntakeOutcome::Deferred(r) => panic!("forced, got {}", r),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- inhibition（原因課題 open 中の下流抑制） ----

    #[test]
    fn inhibition_defers_downstream_while_cause_open() {
        let root = temp_root("inhibit");
        std::fs::write(
            root.join("config.json"),
            r#"{"inhibitRules": [{"cause": {"kind": "build"},
                "effect": {"kind": "test"}, "match": ["repo"]}]}"#,
        )
        .unwrap();
        // 原因課題（kind=build・queued）が open
        store::save_issues(
            &root,
            &[json!({
                "key": "ci-failure:me/repo:cause",
                "source": "ci-failure", "repo": "me/repo", "kind": "build",
                "symptomHash": "cause", "status": ST_QUEUED,
                "updatedAt": now_iso(),
            })],
        );
        let mut c = det_cand("test job failed", "b");
        c["kind"] = json!("test");
        c["hits"] = json!(99);
        match intake(&root, &c) {
            IntakeOutcome::Deferred(r) => {
                assert_eq!(r, "inhibited:ci-failure:me/repo:cause")
            }
            _ => panic!("expected inhibited deferral"),
        }
        // 原因課題の inhibits に候補が追記される
        let cause = &store::load_issues(&root)[0];
        let want = store::issue_key(
            "ci-failure",
            "me/repo",
            &store::symptom_hash("test job failed", "b"),
        );
        assert_eq!(cause["inhibits"], json!([want]));
        // 原因課題が終端なら抑制解除 → 起票できる
        store::save_issues(
            &root,
            &[json!({
                "key": "ci-failure:me/repo:cause",
                "source": "ci-failure", "repo": "me/repo", "kind": "build",
                "symptomHash": "cause", "status": ST_DONE,
                "updatedAt": now_iso(),
            })],
        );
        match intake(&root, &c) {
            IntakeOutcome::Created(_) => {}
            IntakeOutcome::Deferred(r) => panic!("expected Created, got {}", r),
            _ => panic!("expected Created"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn inhibition_explicit_inhibited_by_and_repo_mismatch() {
        let root = temp_root("inhibit2");
        // 明示 inhibitedBy: open 課題を名指し
        store::save_issues(
            &root,
            &[json!({
                "key": "manual:me/repo:parent",
                "source": "manual", "repo": "me/repo",
                "symptomHash": "parent", "status": ST_RUNNING,
                "updatedAt": now_iso(),
            })],
        );
        let mut c = det_cand("child symptom", "b");
        c["inhibitedBy"] = json!("manual:me/repo:parent");
        match intake(&root, &c) {
            IntakeOutcome::Deferred(r) => assert_eq!(r, "inhibited:manual:me/repo:parent"),
            _ => panic!("expected inhibited"),
        }
        // 存在しない・終端の key は抑制しない
        let mut c2 = det_cand("other symptom", "b");
        c2["inhibitedBy"] = json!("manual:me/repo:gone");
        c2["hits"] = json!(99);
        match intake(&root, &c2) {
            IntakeOutcome::Created(_) => {}
            IntakeOutcome::Deferred(r) => panic!("expected Created, got {}", r),
            _ => panic!("expected Created"),
        }
        // match 属性（repo）不一致は抑制しない
        std::fs::write(
            root.join("config.json"),
            r#"{"inhibitRules": [{"cause": {"source": "ci-failure"},
                "effect": {"source": "ci-failure"}, "match": ["repo"]}]}"#,
        )
        .unwrap();
        let mut c3 = det_cand("other repo failure", "b");
        c3["repo"] = json!("other/repo");
        c3["hits"] = json!(99);
        match intake(&root, &c3) {
            IntakeOutcome::Created(_) => {}
            IntakeOutcome::Deferred(r) => panic!("repo mismatch, got {}", r),
            _ => panic!("expected Created"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn can_dispatch_honors_retry_after() {
        let future = (chrono::Utc::now() + chrono::Duration::minutes(5))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let past = (chrono::Utc::now() - chrono::Duration::minutes(5))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let issues = vec![
            json!({"key": "a", "repo": "me/r", "status": ST_QUEUED, "retryAfter": future}),
            json!({"key": "b", "repo": "me/r", "status": ST_QUEUED, "retryAfter": past}),
            json!({"key": "c", "repo": "me/r", "status": ST_QUEUED}),
        ];
        let e = can_dispatch(&json!({}), &issues, "a").unwrap_err();
        assert!(e.contains("backoff"), "got: {}", e);
        assert!(can_dispatch(&json!({}), &issues, "b").is_ok());
        assert!(can_dispatch(&json!({}), &issues, "c").is_ok());
    }

    // ---- 課題ストームの一括事案化（ブラスト半径） ----

    #[test]
    fn storm_aggregates_burst_into_gated_case() {
        let root = temp_root("storm");
        std::fs::write(
            root.join("config.json"),
            r#"{"stormBatchMin": 3, "stormWindowMinutes": 10}"#,
        )
        .unwrap();
        // 3件目まで個別起票（窓内集中で stormBatchMin=3 に到達）
        for i in 1..=3 {
            let mut c = det_cand(&format!("failure {}", i), "b");
            c["hits"] = json!(99);
            match intake(&root, &c) {
                IntakeOutcome::Created(_) => {}
                IntakeOutcome::Deferred(r) => panic!("issue {} deferred: {}", i, r),
                _ => panic!("expected Created"),
            }
        }
        // 4件目以降は一括事案へ集約（個別課題は作らない）
        let mut c = det_cand("failure 4", "b");
        c["hits"] = json!(99);
        let skey = match intake(&root, &c) {
            IntakeOutcome::Deferred(r) => {
                assert!(r.starts_with("storm:"), "got {}", r);
                r.strip_prefix("storm:").unwrap().to_string()
            }
            _ => panic!("expected storm deferral"),
        };
        let storm = store::load_issues(&root)
            .into_iter()
            .find(|i| i["key"] == json!(skey))
            .expect("storm issue");
        // 一括事案は gated（人レビュー要求）で自律対象外
        assert_eq!(storm["status"], json!(ST_GATED));
        assert_eq!(storm["storm"], json!(true));
        assert_eq!(storm["autonomous"], json!(false));
        assert_eq!(storm["stormMembers"].as_array().unwrap().len(), 1);
        // 後続候補も open の間は同じ事案へ集約される
        let mut c2 = det_cand("failure 5", "b");
        c2["hits"] = json!(99);
        match intake(&root, &c2) {
            IntakeOutcome::Deferred(r) => assert_eq!(r, format!("storm:{}", skey)),
            _ => panic!("expected aggregation to same storm"),
        }
        let storm = store::load_issues(&root)
            .into_iter()
            .find(|i| i["key"] == json!(skey))
            .unwrap();
        assert_eq!(storm["stormMembers"].as_array().unwrap().len(), 2);
        // storm 事案が終端（rejected）なら集約終了 → 個別起票に戻る
        // （stormBatchMin=3 の集中は継続するので新 storm が立つこともある。
        //  ここでは窓外として検証するため detectedAt を過去にずらす）
        let mut iss = store::load_issues(&root);
        for i in iss.iter_mut() {
            if i["key"] == json!(skey) {
                i["status"] = json!(ST_REJECTED);
            } else {
                i["detectedAt"] = json!("2020-01-01T00:00:00.000Z");
            }
        }
        store::save_issues(&root, &iss);
        let mut c3 = det_cand("failure 6", "b");
        c3["hits"] = json!(99);
        match intake(&root, &c3) {
            IntakeOutcome::Created(_) => {}
            IntakeOutcome::Deferred(r) => panic!("expected Created, got {}", r),
            _ => panic!("expected Created"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn storm_disabled_when_min_zero() {
        let root = temp_root("storm0");
        std::fs::write(
            root.join("config.json"),
            r#"{"stormBatchMin": 0}"#,
        )
        .unwrap();
        for i in 1..=6 {
            let mut c = det_cand(&format!("failure {}", i), "b");
            c["hits"] = json!(99);
            match intake(&root, &c) {
                IntakeOutcome::Created(_) => {}
                IntakeOutcome::Deferred(r) => panic!("issue {} deferred: {}", i, r),
                _ => panic!("expected Created"),
            }
        }
        assert!(store::load_issues(&root).iter().all(|i| {
            i.get("storm").and_then(|v| v.as_bool()) != Some(true)
        }));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn can_dispatch_enforces_max_concurrent_runs() {
        let cfg = json!({"maxConcurrentRuns": 2});
        let mk = |key: &str, status: &str, repo: &str| {
            json!({"key": key, "status": status, "repo": repo})
        };
        let issues = vec![
            mk("r1", ST_RUNNING, "a/x"),
            mk("r2", ST_VERIFYING, "b/y"),
            mk("q", ST_QUEUED, "c/z"),
        ];
        let e = can_dispatch(&cfg, &issues, "q").unwrap_err();
        assert!(e.contains("maxConcurrentRuns"), "got: {}", e);
        // 0 = 無制限
        let cfg0 = json!({"maxConcurrentRuns": 0});
        assert!(can_dispatch(&cfg0, &issues, "q").is_ok());
    }

    #[test]
    fn can_dispatch_blocks_same_repo_only() {
        let issues = vec![
            json!({"key": "a", "repo": "me/r1", "status": ST_RUNNING}),
            json!({"key": "b", "repo": "me/r1", "status": ST_QUEUED}),
            json!({"key": "c", "repo": "me/r2", "status": ST_QUEUED}),
        ];
        // 同一 repo に running 課題 → Err(blocked by <key>)
        let e = can_dispatch(&json!({}), &issues, "b").unwrap_err();
        assert!(e.starts_with("blocked by"), "got: {}", e);
        // 他 repo の queued は影響なし
        assert!(can_dispatch(&json!({}), &issues, "c").is_ok());
        // queued 以外・未存在は Err
        assert!(can_dispatch(&json!({}), &issues, "a").is_err());
        assert!(can_dispatch(&json!({}), &issues, "missing").is_err());
    }

    #[test]
    fn set_field_rejects_status_and_key() {
        let root = temp_root("setf");
        let (issue, _) = intake_v(&root, &cand("panic in parser", "crashes on startup"));
        let key = issue["key"].as_str().unwrap().to_string();

        assert!(set_field(&root, &key, "status", json!(ST_DONE)).is_err());
        assert!(set_field(&root, &key, "key", json!("other")).is_err());
        // 他フィールドは更新できる（status は変わらない）
        let updated = set_field(&root, &key, "reviewNote", json!("looks ok")).unwrap();
        assert_eq!(updated["reviewNote"], json!("looks ok"));
        assert_eq!(updated["status"], json!(ST_QUEUED));
        assert!(set_field(&root, "no-such-key", "reviewNote", json!("x")).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn intake_rate_limit_defers_over_limit() {
        let root = temp_root("ratelimit");
        // intakeMaxPerHour=1: 同 source+repo の新規起票は1件/時間まで
        std::fs::write(
            root.join("config.json"),
            r#"{"intakeMaxPerHour": 1}"#,
        )
        .unwrap();
        let (i1, created) = intake_v(&root, &cand("first issue", "body a"));
        assert!(created);
        assert_eq!(i1["status"], json!(ST_QUEUED));
        // 2件目は deferred（課題を作らない）
        match intake(&root, &cand("second issue", "body b")) {
            IntakeOutcome::Deferred(reason) => assert_eq!(reason, "rate-limit"),
            other => panic!("expected Deferred, got created?={:?}", matches!(other, IntakeOutcome::Created(_))),
        }
        assert_eq!(store::load_issues(&root).len(), 1);
        // intake_forced は人判断を優先して rate-limit を bypass する
        match intake_forced(&root, &cand("second issue forced", "body b2")) {
            IntakeOutcome::Created(i) => assert_eq!(i["status"], json!(ST_QUEUED)),
            _ => panic!("expected Created via intake_forced"),
        }
        // 別 source は別カウント（制限なしで起票できる）
        let mut c = cand("third issue", "body c");
        c["source"] = json!("other");
        let (i3, created) = intake_v(&root, &c);
        assert!(created);
        assert_eq!(i3["status"], json!(ST_QUEUED));
        // 制限 0（既定）なら無制限
        std::fs::remove_file(root.join("config.json")).unwrap();
        let (_i, created) = intake_v(&root, &cand("fourth", "d"));
        assert!(created);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn assess_sets_priority() {
        let root = temp_root("prio");
        // セキュリティ系 = P3
        let mut c = cand("xss vulnerability in form", "b");
        c["labels"] = json!(["security"]);
        let (i, _) = intake_v(&root, &c);
        assert_eq!(i["priority"], json!(3));
        // ci-failure 由来 = P2
        let mut c = cand("CI failure", "b");
        c["source"] = json!("ci-failure");
        let (i, _) = intake_v(&root, &c);
        assert_eq!(i["priority"], json!(2));
        // documentation = P0
        let mut c = cand("readme typo", "b");
        c["labels"] = json!(["documentation"]);
        let (i, _) = intake_v(&root, &c);
        assert_eq!(i["priority"], json!(0));
        // 既定 = P1
        let (i, _) = intake_v(&root, &cand("add feature x", "b"));
        assert_eq!(i["priority"], json!(1));
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- observing（保留観測） ----

    #[test]
    fn observing_holds_single_shot_and_promotes_on_reemit() {
        let root = temp_root("observe");
        // 単発検出は課題を作らない（deferred=observing）
        match intake(&root, &det_cand("flaky port", "bind failed once")) {
            IntakeOutcome::Deferred(reason) => assert_eq!(reason, "observing"),
            _ => panic!("expected observing deferral"),
        }
        assert!(store::load_issues(&root).is_empty());
        // 再検出（hits>=intakeObserveMinHits 既定2）で昇格して queued 起票
        let mut c2 = det_cand("flaky port", "bind failed once");
        c2["hits"] = json!(2);
        match intake(&root, &c2) {
            IntakeOutcome::Created(i) => assert_eq!(i["status"], json!(ST_QUEUED)),
            _ => panic!("expected promotion to issue"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn observing_promotes_on_duration_and_emit_count() {
        let root = temp_root("observe-dur");
        // durationSeconds（detector が載せる持続情報）が窓を超えれば初回でも昇格
        let mut c = det_cand("long outage", "b");
        c["durationSeconds"] = json!(3600.0); // 60分 >= 既定窓10分
        match intake(&root, &c) {
            IntakeOutcome::Created(i) => assert_eq!(i["status"], json!(ST_QUEUED)),
            _ => panic!("expected durationSeconds promotion"),
        }
        // count（emit 時の検出回数）でも昇格
        let root = temp_root("observe-count");
        let mut c = det_cand("burst", "b");
        c["count"] = json!(5);
        match intake(&root, &c) {
            IntakeOutcome::Created(_) => {}
            _ => panic!("expected count promotion"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- intake スナップショット + followUps ----

    #[test]
    fn intake_freezes_snapshot_and_followups_flow() {
        let root = temp_root("snap");
        let (issue, created) = intake_v(&root, &cand("original title", "original body"));
        assert!(created);
        let key = issue["key"].as_str().unwrap().to_string();
        // 起票時に intakeSnapshot が freeze される
        assert_eq!(issue["intakeSnapshot"]["title"], json!("original title"));
        assert_eq!(issue["intakeSnapshot"]["body"], json!("original body"));
        assert_eq!(issue["followUps"], json!([]));

        // 起票後の追記は followUps へ（本文には混入しない）
        assert!(add_followup(&root, &key, "manual", "first note", None).unwrap());
        assert!(add_followup(&root, &key, "ci-failure", "ci note", Some("ci-1")).unwrap());
        // 同一 ref は重複追記しない
        assert!(!add_followup(&root, &key, "ci-failure", "ci note dup", Some("ci-1")).unwrap());
        assert!(add_followup(&root, "no-such", "x", "y", None).is_err());

        let it = store::load_issues(&root)
            .into_iter()
            .find(|i| i["key"] == json!(key))
            .unwrap();
        let fus = it["followUps"].as_array().unwrap();
        assert_eq!(fus.len(), 2);
        assert_eq!(fus[1]["ref"], json!("ci-1"));
        assert!(fus.iter().all(|f| f["consumedAt"].is_null()));

        // consume で未消費分が consumedAt 記録される
        consume_followups(&root, &key);
        let it = store::load_issues(&root)
            .into_iter()
            .find(|i| i["key"] == json!(key))
            .unwrap();
        assert!(it["followUps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["consumedAt"].as_str().is_some()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn observing_exempt_and_disabled_paths() {
        // manual/derived は観測対象外で即起票
        let root = temp_root("observe-exempt");
        for src in ["manual", "derived"] {
            let mut c = det_cand(&format!("{} cand", src), "b");
            c["source"] = json!(src);
            match intake(&root, &c) {
                IntakeOutcome::Created(_) => {}
                _ => panic!("{} should be exempt from observing", src),
            }
        }
        // config で無効化（窓0・回数1）は従来どおり即起票
        let root = temp_root("observe-off");
        std::fs::write(
            root.join("config.json"),
            r#"{"intakeObserveMinutes": 0, "intakeObserveMinHits": 1}"#,
        )
        .unwrap();
        match intake(&root, &det_cand("one shot", "b")) {
            IntakeOutcome::Created(_) => {}
            _ => panic!("expected immediate filing when observing disabled"),
        }
        // 人判断（intake_forced）は観測窓を bypass する
        let root = temp_root("observe-force");
        match intake_forced(&root, &det_cand("one shot", "b")) {
            IntakeOutcome::Created(_) => {}
            _ => panic!("expected forced filing to bypass observing"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- 親課題リンク（manual-488ea7f9） ----

    #[test]
    fn parent_issue_links_followup_and_requeues_failed() {
        let root = temp_root("plink");
        // 親課題を failed にしておく（ci-failure の帰属先）
        let (parent, created) = intake_v(&root, &cand("original bug", "b"));
        assert!(created);
        let pk = parent["key"].as_str().unwrap().to_string();
        let mut issues = store::load_issues(&root);
        let p = issues
            .iter_mut()
            .find(|i| i["key"].as_str() == Some(pk.as_str()))
            .unwrap();
        p["status"] = json!(ST_FAILED);
        store::save_issues(&root, &issues);

        // parentIssue 付き候補は新規起票せず親へ帰属（Linked）
        let mut c = cand("ci failure on commit", "build broke");
        c["source"] = json!("ci-failure");
        c["parentIssue"] = json!(pk);
        match intake(&root, &c) {
            IntakeOutcome::Linked(i) => {
                assert_eq!(i["key"].as_str().unwrap(), pk);
                // failed の親は queued へ差し戻し（再実行要求）
                assert_eq!(i["status"], json!(ST_QUEUED));
            }
            _ => panic!("expected Linked"),
        }
        // followUps に ref=候補 key で追記される
        let parent = store::load_issues(&root)
            .into_iter()
            .find(|i| i["key"].as_str() == Some(pk.as_str()))
            .unwrap();
        let fus = parent["followUps"].as_array().unwrap();
        assert_eq!(fus.len(), 1);
        assert_eq!(fus[0]["kind"], json!("ci-failure"));
        assert!(fus[0]["ref"].as_str().unwrap().contains("ci-failure"));

        // 同一候補の再 emit は ref dedupe で followUps が増殖しない
        match intake(&root, &c) {
            IntakeOutcome::Linked(i) => {
                assert_eq!(i["followUps"].as_array().unwrap().len(), 1);
            }
            _ => panic!("expected Linked on re-emit"),
        }
        // 新規課題は増えない（候補は起票されない）
        assert_eq!(store::load_issues(&root).len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn parent_issue_link_bypassed_by_force_and_unknown_parent() {
        let root = temp_root("plink-force");
        // 存在しない親 key は帰属せず通常起票
        let mut c = cand("ci failure", "b");
        c["parentIssue"] = json!("no:such:parent");
        let (i, created) = intake_v(&root, &c);
        assert!(created);
        assert_eq!(i["status"], json!(ST_QUEUED));
        // force（人の対応ボタン）は帰属を bypass して新規起票する
        let (parent, _) = intake_v(&root, &cand("parent issue", "b"));
        let pk = parent["key"].as_str().unwrap().to_string();
        let mut c = cand("ci failure forced", "b");
        c["source"] = json!("ci-failure");
        c["parentIssue"] = json!(pk);
        match intake_forced(&root, &c) {
            IntakeOutcome::Created(i) => assert_eq!(i["status"], json!(ST_QUEUED)),
            _ => panic!("forced intake should file a new issue"),
        }
        // 親の followUps には触れない
        let parent = store::load_issues(&root)
            .into_iter()
            .find(|i| i["key"].as_str() == Some(pk.as_str()))
            .unwrap();
        assert_eq!(parent["followUps"].as_array().unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- 大規模課題の分割起票（manual-0577d650） ----

    #[test]
    fn oversized_issue_gates_with_split_proposal() {
        let root = temp_root("split-gate");
        // splitMaxItems 超の箇条書き = 分割提案付き gated
        let body = (1..=10)
            .map(|n| format!("- task {}", n))
            .collect::<Vec<_>>()
            .join("\n");
        let (i, created) = intake_v(&root, &cand("big multi-part work", &body));
        assert!(created);
        assert_eq!(i["status"], json!(ST_GATED));
        let prop = &i["splitProposal"];
        assert!(prop.is_object());
        assert_eq!(prop["items"].as_array().unwrap().len(), 10);
        // 小さな課題は従来どおり queued
        let (i, _) = intake_v(&root, &cand("small work", "tiny"));
        assert_eq!(i["status"], json!(ST_QUEUED));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn split_issue_files_children_and_rejects_parent() {
        let root = temp_root("split-exec");
        let body = "## part A\ndo a\n## part B\ndo b\n## part C\ndo c";
        let (i, _) = intake_v(&root, &cand("huge", body));
        // 3 見出し < splitMaxItems(8) ・本文 < splitMaxBytes(4000) なので
        // 提案は無いが人判断で分割できる（queued も対象）
        assert_eq!(i["status"], json!(ST_QUEUED));
        let pk = i["key"].as_str().unwrap().to_string();
        let items = vec![
            json!({"title": "part A", "body": "do a"}),
            json!({"title": "part B", "body": "do b"}),
            json!({"title": "part C", "body": "do c"}),
        ];
        let parent = split_issue(&root, &pk, Some(&items)).unwrap();
        assert_eq!(parent["status"], json!(ST_REJECTED));
        let into = parent["splitInto"].as_array().unwrap();
        assert_eq!(into.len(), 3);
        let issues = store::load_issues(&root);
        for ck in into.iter().filter_map(|v| v.as_str()) {
            let ch = issues
                .iter()
                .find(|x| x["key"].as_str() == Some(ck))
                .unwrap();
            assert_eq!(ch["source"], json!("split"));
            assert_eq!(ch["splitFrom"], json!(pk));
            assert_eq!(ch["status"], json!(ST_QUEUED));
        }
        // rejected の親は再分割できない
        assert!(split_issue(&root, &pk, Some(&items)).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn split_items_extracts_headings_and_bullets() {
        let items = split_items("## a\nbody a\n## b\nbody b");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["title"], json!("a"));
        let items = split_items("- one\n- two\nplain line\n- three");
        assert_eq!(items.len(), 3);
        assert_eq!(items[2]["title"], json!("three"));
        assert!(split_items("no structure").is_empty());
    }
}
