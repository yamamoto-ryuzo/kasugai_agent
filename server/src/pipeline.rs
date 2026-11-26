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

use crate::config::{cfg_arr, cfg_f64, cfg_u64, load_config};
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
            | (ST_QUEUED, ST_REJECTED)
            | (ST_QUEUED, ST_RUNNING)
            | (ST_RUNNING, ST_VERIFYING)
            | (ST_RUNNING, ST_FAILED)
            | (ST_VERIFYING, ST_DONE)
            | (ST_VERIFYING, ST_RETRY)
            | (ST_RETRY, ST_QUEUED)
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

/// assess: verifyLevel・受入条件・自律可否を決め、assessed → gated|queued へ遷移。
/// external は評価にも進めない（候補一覧表示のみ。README §5.3）。
fn assess(issue: &mut Value) {
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
    let level = rule_verify_level(issue);
    let o = issue.as_object_mut().unwrap();
    o.insert("autonomous".into(), json!(autonomous));
    o.insert("priority".into(), json!(priority));
    o.insert("verifyLevel".into(), json!(level));
    o.insert(
        "acceptance".into(),
        json!(format!("{} が期待どおり動作することを実機で確認する", title)),
    );
    transition(issue, ST_QUEUED);
}

// ---- intake（detect → normalize → dedupe。detect-pipeline.mmd） ----

/// intake の結果。Deferred は起票レート制限で課題を作らず
/// candidate に留め置いた状態（candidates.json の deferred に記録するのは
/// 呼び出し側 detectors の責務）。
pub enum IntakeOutcome {
    Created(Value),
    Existing(Value),
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

    let cfg = load_config(root);
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
        assess(&mut issue);
    }
    issues.push(issue.clone());
    store::save_issues(root, &issues);
    sync_queue(root, &issues);
    IntakeOutcome::Created(issue)
}

// ---- dispatch 判定（論理競合ベース。README §5.4） ----
// dispatch は人判断のみ: queued の起動は一覧の対応ボタン
// （POST /api/issues/:key/dispatch → runner::dispatch_issue）だけが行い、
// drain は queued を自動起動しない（issue-lifecycle.mmd）。

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
    fn can_dispatch_blocks_same_repo_only() {
        let issues = vec![
            json!({"key": "a", "repo": "me/r1", "status": ST_RUNNING}),
            json!({"key": "b", "repo": "me/r1", "status": ST_QUEUED}),
            json!({"key": "c", "repo": "me/r2", "status": ST_QUEUED}),
        ];
        // 同一 repo に running 課題 → Err(blocked by <key>)
        let e = can_dispatch(&issues, "b").unwrap_err();
        assert!(e.starts_with("blocked by"), "got: {}", e);
        // 他 repo の queued は影響なし
        assert!(can_dispatch(&issues, "c").is_ok());
        // queued 以外・未存在は Err
        assert!(can_dispatch(&issues, "a").is_err());
        assert!(can_dispatch(&issues, "missing").is_err());
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
}
