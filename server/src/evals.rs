// evals.rs — 過去課題の回帰評価セット（closed issue → eval case）
// （正本: diagrams/eval-set.mmd、README §5.7）
//
// (a) eval case 蓄積: 終端課題（done/failed/rejected）を
//     knowledge/evals.json へ eval case 化する。入力=課題 spec、
//     期待=採用された assess 判定（verifyLevel/priority/autonomous/
//     planGate）+ verify verdict（evidence/verdict.json の写し。
//     L1 実行コマンド含む）
// (b) assess 回帰テスト: 過去課題を pipeline::assess_preview で再評価し、
//     verifyLevel/kind 判定の退化を差分レポートする（実機不要で安い）
// (c) verify リプレイ: 記録済み L0-L2 の L1 コマンドを再実行し、
//     verdict の ok と比較して pipeline 変更影響を検出する
//     （アイドル時のみ・少量に限定）
// (d) smoke eval: proposal vet から呼ぶ軽量版 (b)。prompts/config/
//     skills 変更の採否前に機械的退行を検出する
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::cases;
use crate::config::{load_j, save_j};
use crate::knowledge::key_slug;
use crate::pipeline;
use crate::store;
use crate::util::now_iso;

/// smoke eval で回帰検査する直近 eval case 数
const SMOKE_N: usize = 10;
/// verify リプレイの1回あたり対象 case 数（アイドル時のみ）
const REPLAY_N: usize = 1;
/// リプレイ対象の verifyLevel 上限（L0-L2 の証跡検証のみ）
const REPLAY_MAX_LEVEL: u64 = 2;
/// リプレイ1コマンドのタイムアウト秒
const REPLAY_TIMEOUT_SECS: u64 = 300;

pub fn path(root: &Path) -> PathBuf {
    root.join("knowledge").join("evals.json")
}

pub fn load(root: &Path) -> Vec<Value> {
    load_j(&path(root), json!([]))
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn save(root: &Path, evals: &[Value]) {
    let _ = std::fs::create_dir_all(root.join("knowledge"));
    save_j(&path(root), &json!(evals));
}

// ---- (a) eval case 蓄積 ----

fn verdict_of(root: &Path, key: &str) -> Value {
    let p = root
        .join("evidence")
        .join(key_slug(key))
        .join("verdict.json");
    load_j(&p, Value::Null)
}

/// 終端課題を eval case 化して knowledge/evals.json へ upsert する。
/// learn watcher から定期呼び出し。戻り値は新規・更新数。
pub fn sync_evals(root: &Path) -> usize {
    let issues = store::load_issues(root);
    let mut evals = load(root);
    let mut touched = 0;
    for i in &issues {
        let key = cases::str_of(i, "key");
        if key.is_empty() {
            continue;
        }
        let status = cases::str_of(i, "status");
        if !matches!(status, "done" | "failed" | "rejected") {
            continue;
        }
        let mut c = json!({
            "key": key,
            "input": {
                "title": cases::str_of(i, "title"),
                "source": cases::str_of(i, "source"),
                "repo": cases::str_of(i, "repo"),
                "kind": cases::str_of(i, "kind"),
                "symptomHash": cases::str_of(i, "symptomHash"),
                "labels": i.get("labels").cloned().unwrap_or(json!([])),
            },
            "expected": {
                "verifyLevel": i.get("verifyLevel").cloned().unwrap_or(Value::Null),
                "priority": i.get("priority").cloned().unwrap_or(Value::Null),
                "autonomous": i.get("autonomous").cloned().unwrap_or(Value::Null),
                "planGate": i.get("planGate").cloned().unwrap_or(Value::Null),
            },
            "result": status,
            "verify": verdict_of(root, key),
            "updatedAt": now_iso(),
        });
        match evals.iter_mut().find(|e| cases::str_of(e, "key") == key) {
            Some(e) => {
                if e.get("expected") != c.get("expected")
                    || e.get("result") != c.get("result")
                    || e.get("verify") != c.get("verify")
                {
                    *e = c;
                    touched += 1;
                }
            }
            None => {
                c.as_object_mut()
                    .unwrap()
                    .insert("recordedAt".into(), json!(now_iso()));
                evals.push(c);
                touched += 1;
            }
        }
    }
    if touched > 0 {
        save(root, &evals);
    }
    touched
}

// ---- (b) assess 回帰テスト ----

/// eval case 群を現在の assess ルールで再評価し、期待との差分を
/// 返す。max 件まで（0=全件。新しい順）。実機不要・決定的。
pub fn assess_regression(root: &Path, max: usize) -> Value {
    let evals = load(root);
    let issues = store::load_issues(root);
    let mut diffs = vec![];
    let mut n = 0;
    for e in evals.iter().rev() {
        if max > 0 && n >= max {
            break;
        }
        n += 1;
        let input = e.get("input").cloned().unwrap_or(json!({}));
        let mut spec = input.clone();
        // assess の入力として課題形を再構成（status は assessed 相当）
        let o = spec.as_object_mut().unwrap();
        o.insert("key".into(), e.get("key").cloned().unwrap_or(Value::Null));
        o.insert("status".into(), json!("assessed"));
        let actual = pipeline::assess_preview(&issues, &spec);
        let expected = e.get("expected").cloned().unwrap_or(json!({}));
        let mut case_diffs = vec![];
        for f in ["verifyLevel", "priority", "autonomous", "planGate"] {
            let ex = expected.get(f).cloned().unwrap_or(Value::Null);
            let ac = actual.get(f).cloned().unwrap_or(Value::Null);
            if ex != ac {
                case_diffs.push(json!({
                    "field": f, "expected": ex, "actual": ac,
                }));
            }
        }
        if !case_diffs.is_empty() {
            diffs.push(json!({
                "key": e.get("key"),
                "diffs": case_diffs,
            }));
        }
    }
    let report = json!({
        "kind": "assess-regression",
        "total": n,
        "diffCount": diffs.len(),
        "diffs": diffs,
        "at": now_iso(),
    });
    report
}

// ---- (c) verify リプレイ ----

/// 記録済み L1 コマンドを再実行し verdict と比較する。
/// アイドル時に learn watcher から呼ぶ（重いため REPLAY_N 件のみ。
/// verifyLevel<=REPLAY_MAX_LEVEL の case のみ）。
/// 戻り値はレポート（diffs=verdict 不一致）。
pub async fn verify_replay(root: &Path) -> Value {
    let evals = load(root);
    let issues = store::load_issues(root);
    let mut diffs = vec![];
    let mut n = 0;
    for e in evals.iter().rev() {
        if n >= REPLAY_N {
            break;
        }
        let v = e.get("verify").cloned().unwrap_or(Value::Null);
        let level = v.get("level").and_then(|x| x.as_u64()).unwrap_or(99);
        let checks: Vec<Vec<String>> = v
            .get("checks")
            .and_then(|c| c.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|argv| {
                        argv.as_array().map(|x| {
                            x.iter()
                                .filter_map(|s| s.as_str().map(String::from))
                                .collect()
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        if level > REPLAY_MAX_LEVEL || checks.is_empty() {
            continue;
        }
        let Some(issue) = issues
            .iter()
            .find(|i| cases::str_of(i, "key") == cases::str_of(e, "key"))
        else {
            continue;
        };
        let repo_path = repo_path_of(root, cases::str_of(issue, "repo"));
        if repo_path.is_empty() {
            continue;
        }
        n += 1;
        // L1 コマンド群を再実行し全体 ok を比較（L0 差分/L2 実機は対象外）
        let mut ok = true;
        for argv in &checks {
            if argv.is_empty() {
                continue;
            }
            let r = tokio::time::timeout(
                std::time::Duration::from_secs(REPLAY_TIMEOUT_SECS),
                tokio::process::Command::new(&argv[0])
                    .args(&argv[1..])
                    .current_dir(&repo_path)
                    .output(),
            )
            .await;
            let code = match r {
                Ok(Ok(o)) => o.status.code().unwrap_or(-1),
                _ => -1,
            };
            if code != 0 {
                ok = false;
                break;
            }
        }
        let expected_ok = v.get("ok").and_then(|x| x.as_bool()).unwrap_or(true);
        if ok != expected_ok {
            diffs.push(json!({
                "key": e.get("key"),
                "expectedOk": expected_ok,
                "actualOk": ok,
            }));
        }
    }
    json!({
        "kind": "verify-replay",
        "total": n,
        "diffCount": diffs.len(),
        "diffs": diffs,
        "at": now_iso(),
    })
}

/// 課題の repo 名を config.repos のワークツリーパスへ解決する
fn repo_path_of(root: &Path, repo: &str) -> String {
    let cfg = load_j(&root.join("config.json"), json!({}));
    cfg.get("repos")
        .and_then(|r| r.as_array())
        .and_then(|a| {
            a.iter().find_map(|r| {
                if cases::str_of(r, "slug") == repo {
                    Some(cases::str_of(r, "path").to_string())
                } else {
                    None
                }
            })
        })
        .unwrap_or_default()
}

// ---- (d) smoke eval（proposal vet 用） ----

/// prompts/config/skills 変更の採否前に回帰退行を機械検査する。
/// 直近 SMOKE_N 件の assess 回帰のみ（verify リプレイは vet では
/// 実行しない＝コマンド実行を伴い重いため）。戻り値 (ok, detail)。
pub fn smoke(root: &Path) -> (bool, String) {
    let rep = assess_regression(root, SMOKE_N);
    let diff = rep.get("diffCount").and_then(|v| v.as_u64()).unwrap_or(0);
    let total = rep.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
    if total == 0 {
        return (true, "no eval cases yet".into());
    }
    (
        diff == 0,
        format!("{} eval cases, {} diffs", total, diff),
    )
}

/// learn watcher 用のレポート保存。evals.json とは別に
/// knowledge/eval-report.json へ最新結果を残す（監視・通知用）
pub fn save_report(root: &Path, report: &Value) {
    save_j(
        &root.join("knowledge").join("eval-report.json"),
        report,
    );
}

/// smoke の評価対象となる変更パスか（prompts/config/skills/.devin/skills）
pub fn affects_behavior(path: &str) -> bool {
    let p = path.to_lowercase();
    p == "config.json"
        || p.starts_with("prompts/")
        || p.starts_with("skills/")
        || p.starts_with(".devin/skills/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "kasugai-evals-test-{}-{}",
            tag,
            crate::util::rand_hex(4)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn closed(key: &str, status: &str) -> Value {
        json!({"key": key, "source": "ci-failure", "repo": "me/repo",
               "kind": "build", "symptomHash": key, "title": "t",
               "status": status, "verifyLevel": 1, "priority": 3,
               "autonomous": true})
    }

    #[test]
    fn sync_evals_records_terminal_issues_idempotently() {
        let root = temp_root("sync");
        store::save_issues(&root, &[closed("k1", "done"), closed("k2", "running")]);
        assert_eq!(sync_evals(&root), 1);
        let es = load(&root);
        assert_eq!(es.len(), 1);
        assert_eq!(cases::str_of(&es[0], "key"), "k1");
        assert_eq!(es[0]["expected"]["verifyLevel"], json!(1));
        assert_eq!(sync_evals(&root), 0); // 冪等
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn assess_regression_diffs_changed_expectations() {
        let root = temp_root("reg");
        store::save_issues(&root, &[closed("k1", "done")]);
        sync_evals(&root);
        // 期待をわざと変えて退化検出を確認
        let mut es = load(&root);
        es[0]["expected"]["verifyLevel"] = json!(99);
        save(&root, &es);
        let rep = assess_regression(&root, 0);
        assert_eq!(rep["total"], json!(1));
        assert_eq!(rep["diffCount"], json!(1));
        let d = &rep["diffs"][0]["diffs"][0];
        assert_eq!(d["field"], json!("verifyLevel"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn smoke_passes_when_no_diffs() {
        let root = temp_root("smk");
        store::save_issues(&root, &[closed("k1", "done")]);
        // eval 未蓄積 → pass
        assert!(smoke(&root).0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn affects_behavior_scopes_behavioral_paths() {
        assert!(affects_behavior("prompts/dispatch.md"));
        assert!(affects_behavior("config.json"));
        assert!(!affects_behavior("docs/x.html"));
        assert!(!affects_behavior("README.md"));
    }
}
