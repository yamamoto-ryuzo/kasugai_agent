// pipeline.rs — 課題ライフサイクルの state machine
// （正本: diagrams/issue-lifecycle.mmd・detect-pipeline.mmd、README §3 §5）
//
// 状態遷移はこのモジュールだけが行う。detectors・runner・API は
// intake / request_transition / can_dispatch 等の遷移要求を呼ぶだけで、
// issues.json の status を直接書き換えてはならない。
//
// 課題レコード（issues.json の要素）:
//   key, source, repo, slug?, number?, url?, title, body, labels[],
//   symptomHash, status, verifyLevel?, acceptance?, autonomous,
//   retries, verifyBacks, detectedAt, lastSeen, updatedAt, runs[]
use serde_json::{json, Value};
use std::path::Path;

use crate::store;
use crate::util::now_iso;

// ---- 状態（issue-lifecycle.mmd） ----
pub const ST_ASSESSED: &str = "assessed";
pub const ST_GATED: &str = "gated";
pub const ST_QUEUED: &str = "queued";
pub const ST_RUNNING: &str = "running";
pub const ST_VERIFYING: &str = "verifying";
pub const ST_RETRY: &str = "retry";
pub const ST_DONE: &str = "done";
pub const ST_FAILED: &str = "failed";
pub const ST_REJECTED: &str = "rejected";
pub const ST_DROPPED: &str = "dropped";

pub const STATUSES: &[&str] = &[
    ST_ASSESSED,
    ST_GATED,
    ST_QUEUED,
    ST_RUNNING,
    ST_VERIFYING,
    ST_RETRY,
    ST_DONE,
    ST_FAILED,
    ST_REJECTED,
    ST_DROPPED,
];

/// 許可される遷移（detected/deduped は intake 内部で畳み込む）
fn allowed(from: &str, to: &str) -> bool {
    matches!(
        (from, to),
        (ST_ASSESSED, ST_GATED)
            | (ST_ASSESSED, ST_QUEUED)
            | (ST_GATED, ST_QUEUED)
            | (ST_GATED, ST_REJECTED)
            | (ST_QUEUED, ST_RUNNING)
            | (ST_RUNNING, ST_VERIFYING)
            | (ST_RUNNING, ST_FAILED)
            | (ST_VERIFYING, ST_DONE)
            | (ST_VERIFYING, ST_RETRY)
            | (ST_RETRY, ST_QUEUED)
            | (ST_RETRY, ST_FAILED)
            | (ST_FAILED, ST_QUEUED)
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

/// assess: verifyLevel・受入条件・自律可否を決め、assessed → gated|queued へ遷移。
/// external は評価にも進めない（候補一覧表示のみ。README §5.3）。
fn assess(issue: &mut Value) {
    let autonomous = if has_label(issue, "external") {
        let o = issue.as_object_mut().unwrap();
        o.insert("autonomous".into(), json!(false));
        transition(issue, ST_GATED);
        return;
    } else if blast_radius(issue) {
        let o = issue.as_object_mut().unwrap();
        o.insert("autonomous".into(), json!(false));
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
    let level = rule_verify_level(issue);
    let o = issue.as_object_mut().unwrap();
    o.insert("autonomous".into(), json!(autonomous));
    o.insert("verifyLevel".into(), json!(level));
    o.insert(
        "acceptance".into(),
        json!(format!("{} が期待どおり動作することを実機で確認する", title)),
    );
    transition(issue, ST_QUEUED);
}

// ---- intake（detect → normalize → dedupe。detect-pipeline.mmd） ----

/// candidate {source, repo, title, body, symptomHash?, slug?, number?, url?, labels?}
/// を入口で正規化・重複判定する。既出は lastSeen のみ更新、新規は課題レコード作成
/// → assess → queued|gated へ。戻り値 (issue, created?)。
/// dedupe はこの1箇所のみ（detector 個別には持たせない。AGENTS.md）
pub fn intake(root: &Path, cand: &Value) -> (Value, bool) {
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
        return (out, false);
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
        "labels": cand.get("labels").cloned().unwrap_or(json!([])),
        "symptomHash": sh,
        "status": ST_ASSESSED,
        "autonomous": false,
        "retries": 0,
        "verifyBacks": 0,
        "detectedAt": cand.get("detectedAt").and_then(|v| v.as_str()).unwrap_or(&now),
        "lastSeen": now,
        "updatedAt": now,
        "runs": [],
    });
    assess(&mut issue);
    issues.push(issue.clone());
    store::save_issues(root, &issues);
    sync_queue(root, &issues);
    (issue, true)
}

// ---- dispatch 判定（論理競合ベース。README §5.4） ----

/// queued 課題を起動してよいか。同一 repo に running|verifying の課題があれば
/// Err(ブロック中の key)。実機リソースの排他は locks.rs の責務。
pub fn can_dispatch(issues: &[Value], key: &str) -> Result<(), String> {
    let Some(issue) = issues
        .iter()
        .find(|x| x.get("key").and_then(|v| v.as_str()) == Some(key))
    else {
        return Err("not found".into());
    };
    if issue.get("status").and_then(|v| v.as_str()) != Some(ST_QUEUED) {
        return Err("not queued".into());
    }
    let repo = issue.get("repo").and_then(|v| v.as_str()).unwrap_or("");
    if let Some(b) = issues.iter().find(|x| {
        x.get("key").and_then(|v| v.as_str()) != Some(key)
            && x.get("repo").and_then(|v| v.as_str()) == Some(repo)
            && matches!(
                x.get("status").and_then(|v| v.as_str()),
                Some(ST_RUNNING) | Some(ST_VERIFYING)
            )
    }) {
        return Err(format!(
            "blocked by {}",
            b.get("key").and_then(|v| v.as_str()).unwrap_or("")
        ));
    }
    Ok(())
}

/// work-queue.json の先頭から起動可能な課題を探す
pub fn next_runnable(root: &Path) -> Option<Value> {
    let q = store::load_queue(root);
    let issues = store::load_issues(root);
    for e in &q {
        let Some(k) = e.get("key").and_then(|v| v.as_str()) else {
            continue;
        };
        if can_dispatch(&issues, k).is_ok() {
            return issues
                .iter()
                .find(|x| x.get("key").and_then(|v| v.as_str()) == Some(k))
                .cloned();
        }
    }
    None
}
