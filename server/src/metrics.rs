// metrics.rs — KPI ダッシュボード集計（metrics.mmd・README §5.12）
//
// AgentOps / Weave leaderboard・SRE 式 SLI を参考に、課題/run データから
// 成功率・MTTR・verify NG 率等を決定的に算出して /api/metrics で返す。
// 成功の定義は「verify 通過・push 済み」（pushedAt 記録）で厳密化し、
// エージェント自己申告には依らない。集計は issues.json の読み取りのみで、
// 状態は一切書き換えない。
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;

fn ts(v: &Value) -> Option<i64> {
    v.as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp_millis())
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn u(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(|x| x.as_u64()).unwrap_or(0)
}

/// dispatched 判定: run が1回以上起動した課題（実行系に載ったもの。
/// queued/gated/rejected で一度も走らなかった課題は分母に含めない）
fn dispatched(i: &Value) -> bool {
    i.get("runs")
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false)
}

/// success 判定: verify 通過・push 済み（pushedAt が記録されるのは
/// finalize の push 成功時のみ）
fn succeeded(i: &Value) -> bool {
    i.get("pushedAt").and_then(|v| v.as_str()).is_some()
}

/// issues.json 全件から KPI を算出する（GET /api/metrics の応答本体）
pub fn compute(root: &Path) -> Value {
    let issues = crate::store::load_issues(root);
    let total = issues.len() as u64;
    let mut dispatched_n = 0u64;
    let mut succeeded_n = 0u64;
    let mut failed_n = 0u64;
    let mut rejected_n = 0u64;
    let mut external_n = 0u64;
    let mut ng_n = 0u64;
    let mut mttr_ms: Vec<i64> = Vec::new();
    let mut retry_dist: BTreeMap<String, u64> = BTreeMap::new();
    let mut per_level: BTreeMap<String, (u64, u64)> = BTreeMap::new(); // level → (dispatched, ng)
    let mut by_kind: BTreeMap<String, (u64, u64, u64)> = BTreeMap::new(); // kind → (dispatched, done, failed)
    let mut by_repo: BTreeMap<String, (u64, u64, u64)> = BTreeMap::new();
    let mut hash_count: BTreeMap<String, u64> = BTreeMap::new();
    let mut elapsed_ms = 0u64;
    let mut tokens = 0u64;
    let mut cost_usd = 0f64;
    let mut runs_total = 0u64;

    for i in &issues {
        let st = s(i, "status");
        if st == "rejected" {
            rejected_n += 1;
        }
        let labels = i.get("labels").and_then(|v| v.as_array());
        if labels
            .map(|a| {
                a.iter()
                    .filter_map(|l| l.as_str())
                    .any(|l| l.to_lowercase().contains("external"))
            })
            .unwrap_or(false)
        {
            external_n += 1;
        }
        let hash = s(i, "symptomHash");
        if !hash.is_empty() {
            *hash_count.entry(hash).or_default() += 1;
        }
        if !dispatched(i) {
            continue;
        }
        dispatched_n += 1;
        let ok = succeeded(i);
        if ok {
            succeeded_n += 1;
            if let (Some(a), Some(b)) = (
                i.get("detectedAt").and_then(|v| ts(&json!(v))),
                i.get("pushedAt").and_then(|v| ts(&json!(v))),
            ) {
                if b >= a {
                    mttr_ms.push(b - a);
                }
            }
        }
        if st == "failed" {
            failed_n += 1;
        }
        let backs = u(i, "verifyBacks");
        if backs > 0 {
            ng_n += 1;
        }
        let bucket = if backs >= 3 { "3+".to_string() } else { backs.to_string() };
        *retry_dist.entry(bucket).or_default() += 1;
        let level = u(i, "verifyLevel").to_string();
        let e = per_level.entry(level).or_default();
        e.0 += 1;
        if backs > 0 {
            e.1 += 1;
        }
        for (m, v) in [(&mut by_kind, s(i, "kind")), (&mut by_repo, s(i, "repo"))] {
            if v.is_empty() {
                continue;
            }
            let e = m.entry(v).or_default();
            e.0 += 1;
            if ok {
                e.1 += 1;
            }
            if st == "failed" {
                e.2 += 1;
            }
        }
        for r in i.get("runs").and_then(|v| v.as_array()).into_iter().flatten() {
            runs_total += 1;
            let us = r.get("usage").cloned().unwrap_or(json!({}));
            elapsed_ms += u(&us, "elapsedMs");
            tokens += u(&us, "tokens");
            cost_usd += us
                .get("costUsd")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
        }
    }

    mttr_ms.sort_unstable();
    let mttr_median = mttr_ms.get(mttr_ms.len() / 2).cloned().unwrap_or(0);
    let mttr_mean = if mttr_ms.is_empty() {
        0
    } else {
        mttr_ms.iter().sum::<i64>() / mttr_ms.len() as i64
    };
    let recur_issues: u64 = hash_count.values().filter(|n| **n > 1).sum();
    let recur_hashes = hash_count.values().filter(|n| **n > 1).count() as u64;
    let rate = |a: u64, b: u64| {
        if b == 0 {
            0.0
        } else {
            (a as f64 / b as f64 * 1000.0).round() / 1000.0
        }
    };
    let breakdown = |m: BTreeMap<String, (u64, u64, u64)>| -> Vec<Value> {
        m.into_iter()
            .map(|(k, (d, ok, f))| {
                json!({"name": k, "dispatched": d, "done": ok, "failed": f,
                       "successRate": rate(ok, d)})
            })
            .collect()
    };
    json!({
        "issuesTotal": total,
        "dispatched": dispatched_n,
        "succeeded": succeeded_n,
        "failed": failed_n,
        "rejected": rejected_n,
        "external": external_n,
        "successRate": rate(succeeded_n, dispatched_n),
        "rejectRate": rate(rejected_n, total),
        "externalRate": rate(external_n, total),
        "mttrMedianMs": mttr_median,
        "mttrMeanMs": mttr_mean,
        "verifyNg": {
            "issues": ng_n,
            "rate": rate(ng_n, dispatched_n),
            "byLevel": per_level.into_iter().map(|(lv, (d, n))| {
                json!({"level": lv, "dispatched": d, "ng": n, "rate": rate(n, d)})
            }).collect::<Vec<_>>(),
        },
        "retryDist": retry_dist.into_iter()
            .map(|(k, n)| json!({"backs": k, "count": n}))
            .collect::<Vec<_>>(),
        "cost": {
            "runs": runs_total,
            "elapsedMs": elapsed_ms,
            "tokens": tokens,
            "costUsd": (cost_usd * 1000.0).round() / 1000.0,
            "perIssueElapsedMs": if dispatched_n == 0 { 0 } else { elapsed_ms / dispatched_n },
        },
        "byKind": breakdown(by_kind),
        "byRepo": breakdown(by_repo),
        "recurrence": {
            "dupHashes": recur_hashes,
            "issues": recur_issues,
            "rate": rate(recur_issues, total),
        },
        "at": crate::util::now_iso(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(key: &str, status: &str, backs: u64, pushed: bool) -> Value {
        json!({
            "key": key, "status": status, "repo": "me/r", "kind": "build",
            "verifyLevel": 2, "verifyBacks": backs,
            "symptomHash": "h", "detectedAt": "2024-01-01T00:00:00Z",
            "pushedAt": if pushed { json!("2024-01-01T01:00:00Z") } else { Value::Null },
            "runs": [{"startedAt": "x", "usage": {"elapsedMs": 60000, "tokens": 100}}],
        })
    }

    #[test]
    fn computes_rates_and_breakdown() {
        let root = std::env::temp_dir().join(format!(
            "kasugai-metrics-test-{}",
            crate::util::rand_hex(4)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut rejected = issue("a:r:3", "rejected", 0, false);
        rejected["runs"] = json!([]); // 一度も走らず却下 → dispatched 分母外
        crate::store::save_issues(
            &root,
            &vec![
                issue("a:r:1", "done", 0, true),
                issue("a:r:2", "queued", 1, false),
                rejected,
            ],
        );
        let m = compute(&root);
        // dispatched は runs>0 のみ → rejected は分母外
        assert_eq!(m["dispatched"], json!(2));
        assert_eq!(m["succeeded"], json!(1));
        assert_eq!(m["successRate"], json!(0.5));
        assert_eq!(m["mttrMedianMs"], json!(3600000));
        assert_eq!(m["verifyNg"]["issues"], json!(1));
        assert_eq!(m["cost"]["elapsedMs"], json!(120000));
        assert_eq!(m["cost"]["tokens"], json!(200));
        // 同一 symptomHash="h" が3課題 → 再発率集計へ載る
        assert_eq!(m["recurrence"]["issues"], json!(3));
        let _ = std::fs::remove_dir_all(&root);
    }
}
