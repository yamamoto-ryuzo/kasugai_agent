// postpush.rs — push 後フォローアップループ
// （正本: diagrams/postpush-loop.mmd、manual-55878990、README §5.5.1）
//
// CodeRabbit Autofix・Jules CI Fixer に倣い、push した sha/branch に紐付く
// 3 信号を監視して修復課題（source=post-push）を自動起票する:
//   - CI 失敗: `gh run list --commit <sha>` の failure run
//   - レビュー指摘: branch の open PR の CHANGES_REQUESTED レビュー・
//     sha へのコミットコメント
//   - マージコンフリクト: branch の open PR で mergeable=CONFLICTING
//
// 監視状態は課題の postPush サブ状態（status(done) とは別の表示用
// フィールド）で {state, sha, branch, since, checks, repairs, reason}:
//   checking → 検出なしで polling 中（waiting=CI 未完了待ち）
//   fixing   → 修復課題を起票済みで終端待ち
//   done     → 監視窓（postPushWatchMinutes）を無検出で越えた・
//              修復課題が全て done（各修復課題は自身の postPush で再監視）
//   paused   → 修復課題の failed/rejected・repairs 上限・人操作。
//              reason に pause 理由を残し、postpush-resume で再開
//
// 状態更新は pipeline::set_field 経由（postPush は status ではない
// サブ状態のため status 遷移規則の対象外。検出・起票は detector 規約の
// ingest で起票レート制限・dedupe の恩恵を受ける）
use serde_json::{json, Value};
use std::path::Path;

use crate::conlog::conlog;
use crate::config::{cfg_bool_default_true, cfg_f64};
use crate::control::Shared;
use crate::pipeline;
use crate::store;
use crate::util::now_iso;

// ---------- postPush サブ状態 ----------

fn postpush_of(issue: &Value) -> Option<Value> {
    issue.get("postPush").cloned().filter(|p| p.is_object())
}

/// postPush の各フィールドを更新する（state・reason・checks・repairs 等。
/// 呼び出し側が app.sse("issues") を投げる）
fn patch(root: &Path, key: &str, fields: &[(&str, Value)]) -> Result<Value, String> {
    let issues = store::load_issues(root);
    let Some(it) = issues.iter().find(|i| {
        i.get("key").and_then(|v| v.as_str()) == Some(key)
    }) else {
        return Err("issue not found".into());
    };
    let mut pp = postpush_of(it).unwrap_or(json!({}));
    {
        let o = pp.as_object_mut().unwrap();
        for (f, v) in fields {
            if v.is_null() {
                o.remove(*f);
            } else {
                o.insert((*f).into(), v.clone());
            }
        }
        o.insert("lastCheck".into(), json!(now_iso()));
    }
    pipeline::set_field(root, key, "postPush", pp)
}

/// push 成功時に監視を開始する（postpush-loop.mmd: [*] → checking）。
/// sha/branch は監視対象として記録する（branch は PR・コンフリクト検出用）
pub fn arm(root: &Path, key: &str, sha: &str, branch: &str) {
    let _ = pipeline::set_field(
        root,
        key,
        "postPush",
        json!({
            "state": "checking",
            "sha": sha,
            "branch": branch,
            "since": now_iso(),
            "checks": 0,
            "repairs": [],
        }),
    );
}

/// 人操作: postpush-pause（checking|waiting|fixing → paused・reason=manual）/
/// postpush-resume（paused → checking・since 再アンカー）
pub fn manual(root: &Path, key: &str, action: &str) -> Result<Value, String> {
    let issues = store::load_issues(root);
    let it = issues
        .iter()
        .find(|i| i.get("key").and_then(|v| v.as_str()) == Some(key))
        .ok_or_else(|| "issue not found".to_string())?;
    let pp = postpush_of(it).ok_or_else(|| "no postPush watch".to_string())?;
    let st = pp.get("state").and_then(|v| v.as_str()).unwrap_or("");
    match action {
        "postpush-pause" => {
            if !matches!(st, "checking" | "waiting" | "fixing") {
                return Err(format!("cannot pause from {}", st));
            }
            patch(root, key, &[("state", json!("paused")), ("reason", json!("manual"))])
        }
        "postpush-resume" => {
            if st != "paused" {
                return Err(format!("cannot resume from {}", st));
            }
            patch(
                root,
                key,
                &[
                    ("state", json!("checking")),
                    ("reason", Value::Null),
                    ("since", json!(now_iso())),
                    ("checks", json!(0)),
                ],
            )
        }
        _ => Err("unknown action".into()),
    }
}

// ---------- 信号収集 ----------

/// 検出1件。ref_id は symptomHash（pp-<kind>-<ref>）で同一事象の再検出を
/// dedupe するための安定 id（run id / PR 番号 / comment id / review id）
struct Finding {
    kind: &'static str, // ci | review | conflict
    ref_id: String,
    title: String,
    url: String,
    detail: String,
}

struct Signals {
    /// 監視中の CI run がまだ完了していない（waiting 状態へ）
    pending: bool,
    findings: Vec<Finding>,
}

fn iso_parse(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&chrono::Utc))
        .ok()
}

/// push した sha/branch に紐付く 3 信号を gh で収集する。
/// slug 無し・gh 不在・API 失敗はその信号を諦める（エラーは conlog）
async fn collect(
    slug: &str,
    cwd: Option<&Path>,
    pp: &Value,
) -> Signals {
    let sha = pp.get("sha").and_then(|v| v.as_str()).unwrap_or("");
    let branch = pp.get("branch").and_then(|v| v.as_str()).unwrap_or("");
    let since = pp
        .get("since")
        .and_then(|v| v.as_str())
        .and_then(iso_parse);
    let mut out = Signals {
        pending: false,
        findings: Vec::new(),
    };
    // CI: push したコミットの workflow run（完了待ち / 失敗）
    if !sha.is_empty() {
        match crate::github::gh(
            &[
                "run", "list", "--repo", slug, "--commit", sha, "--limit", "20",
                "--json", "databaseId,workflowName,status,conclusion,url",
            ],
            cwd,
        )
        .await
        {
            Ok(s) => match serde_json::from_str::<Vec<Value>>(&s) {
                Ok(runs) => {
                    for r in runs {
                        let status = r
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if status != "completed" {
                            out.pending = true;
                            continue;
                        }
                        if r.get("conclusion").and_then(|v| v.as_str())
                            == Some("failure")
                        {
                            let id = r
                                .get("databaseId")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            let wf = r
                                .get("workflowName")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let url = r
                                .get("url")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            out.findings.push(Finding {
                                kind: "ci",
                                ref_id: format!("{}", id),
                                title: format!("CI失敗: {}", wf),
                                url: url.to_string(),
                                detail: format!("run: {}\nworkflow: {}\nsha: {}", url, wf, sha),
                            });
                        }
                    }
                }
                Err(e) => conlog(
                    "postpush",
                    "err",
                    &format!("{}: run list json: {}", slug, e),
                ),
            },
            Err(e) => conlog(
                "postpush",
                "err",
                &format!("{}: CI 監視の gh 呼出に失敗: {}", slug, e),
            ),
        }
    }
    if branch.is_empty() {
        return out;
    }
    // branch の open PR: mergeable=CONFLICTING / CHANGES_REQUESTED レビュー
    let prs = crate::github::gh(
        &[
            "pr", "list", "--repo", slug, "--head", branch, "--state", "open",
            "--limit", "10", "--json", "number,mergeable,url,title",
        ],
        cwd,
    )
    .await
    .and_then(|s| {
        serde_json::from_str::<Vec<Value>>(&s)
            .map_err(|e| format!("pr list json: {}", e))
    });
    let prs = match prs {
        Ok(v) => v,
        Err(e) => {
            conlog(
                "postpush",
                "err",
                &format!("{}: PR 監視の gh 呼出に失敗: {}", slug, e),
            );
            Vec::new()
        }
    };
    for pr in &prs {
        let num = pr.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
        let url = pr.get("url").and_then(|v| v.as_str()).unwrap_or("");
        if pr.get("mergeable").and_then(|v| v.as_str()) == Some("CONFLICTING") {
            out.findings.push(Finding {
                kind: "conflict",
                ref_id: format!("pr{}", num),
                title: format!("マージコンフリクト: PR #{}", num),
                url: url.to_string(),
                detail: format!("pr: {}\nbranch: {}", url, branch),
            });
        }
        // CHANGES_REQUESTED レビュー（since 以降のもののみ新規指摘とする）
        let n = num.to_string();
        if let Ok(s) = crate::github::gh(
            &["pr", "view", &n, "--repo", slug, "--json", "reviews"],
            cwd,
        )
        .await
        {
            if let Ok(v) = serde_json::from_str::<Value>(&s) {
                for r in v.get("reviews").and_then(|a| a.as_array()).cloned().unwrap_or_default() {
                    if r.get("state").and_then(|x| x.as_str())
                        != Some("CHANGES_REQUESTED")
                    {
                        continue;
                    }
                    let submitted = r
                        .get("submittedAt")
                        .and_then(|x| x.as_str())
                        .unwrap_or("");
                    if let (Some(s_), Some(t)) = (since, iso_parse(submitted)) {
                        if t <= s_ {
                            continue;
                        }
                    }
                    let rid = r
                        .get("id")
                        .and_then(|x| x.as_str())
                        .map(String::from)
                        .unwrap_or_else(|| format!("pr{}-{}", num, submitted));
                    let author = r
                        .get("author")
                        .and_then(|a| a.get("login"))
                        .and_then(|x| x.as_str())
                        .unwrap_or("");
                    out.findings.push(Finding {
                        kind: "review",
                        ref_id: format!("{}", rid),
                        title: format!("レビュー指摘: PR #{} ({})", num, author),
                        url: url.to_string(),
                        detail: format!(
                            "pr: {}\nreview by {} at {}\n{}",
                            url,
                            author,
                            submitted,
                            r.get("body").and_then(|x| x.as_str()).unwrap_or("")
                        ),
                    });
                }
            }
        }
    }
    // コミットコメント（sha へのレビュー相当の指摘。since 以降のみ）
    if !sha.is_empty() {
        let endpoint = format!("repos/{}/commits/{}/comments", slug, sha);
        if let Ok(s) = crate::github::gh(&["api", &endpoint], cwd).await {
            if let Ok(comments) = serde_json::from_str::<Vec<Value>>(&s) {
                for c in comments {
                    let created = c
                        .get("created_at")
                        .and_then(|x| x.as_str())
                        .unwrap_or("");
                    if let (Some(s_), Some(t)) = (since, iso_parse(created)) {
                        if t <= s_ {
                            continue;
                        }
                    }
                    let id = c.get("id").and_then(|x| x.as_u64()).unwrap_or(0);
                    let author = c
                        .get("user")
                        .and_then(|u| u.get("login"))
                        .and_then(|x| x.as_str())
                        .unwrap_or("");
                    out.findings.push(Finding {
                        kind: "review",
                        ref_id: format!("c{}", id),
                        title: format!("コミットコメント: {} ({})", &sha[..sha.len().min(8)], author),
                        url: c
                            .get("html_url")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string(),
                        detail: format!(
                            "comment by {} at {}\n{}",
                            author,
                            created,
                            c.get("body").and_then(|x| x.as_str()).unwrap_or("")
                        ),
                    });
                }
            }
        }
    }
    out
}

// ---------- poll（control の watcher から定期呼出） ----------

/// 検出を source=post-push の修復課題として起票する。同一事象は
/// symptomHash（pp-<kind>-<ref>）で dedupe される。戻り値は修復課題 key
fn file_repair(root: &Path, slug: &str, parent: &Value, f: &Finding) -> String {
    let sh = format!("pp-{}-{}", f.kind, f.ref_id);
    let parent_key = parent
        .get("key")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let parent_title = parent
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let cand = json!({
        "source": "post-push",
        "repo": slug,
        "slug": slug,
        "url": f.url,
        "title": format!("{} — push 後フォローアップ（{}）", f.title, parent_title),
        "body": format!(
            "push 後フォローアップループの検出（manual-55878990）。\n\
             修復元課題: {}\n\n{}",
            parent_key, f.detail
        ),
        "kind": format!("postpush-{}", f.kind),
        "labels": ["post-push"],
        "symptomHash": sh,
        "repairFor": parent_key,
        "detectedAt": now_iso(),
    });
    let (issue, created, _deferred) = crate::detectors::ingest(root, &cand);
    if created {
        conlog(
            "postpush",
            "sys",
            &format!(
                "{}: {} → 修復課題を起票（{}）",
                parent_key, f.title, f.ref_id
            ),
        );
    }
    // ingest の結果に依らず修復課題 key は dedupe hash から決定的に導ける
    issue
        .get("key")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| store::issue_key("post-push", slug, &sh))
}

/// 課題1件分の 1 tick。postPush.state が遷移・更新されたら true
async fn tick(app: &Shared, cfg: &Value, issue: &Value) -> bool {
    let root = &app.root;
    let key = issue
        .get("key")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let Some(pp) = postpush_of(issue) else {
        return false;
    };
    let st = pp.get("state").and_then(|v| v.as_str()).unwrap_or("");
    match st {
        "checking" | "waiting" => {}
        "fixing" => return tick_fixing(root, issue, &pp),
        _ => return false,
    }
    // repo 解決（slug が無い repo は gh 監視できない → 監視終了）
    let Some(repo) = crate::control::App::git_repo(
        cfg,
        issue.get("repo").and_then(|v| v.as_str()).unwrap_or(""),
    ) else {
        return patch(
            root,
            &key,
            &[("state", json!("done")), ("reason", json!("no-repo"))],
        )
        .is_ok();
    };
    let slug = repo
        .get("slug")
        .or_else(|| repo.get("github"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if slug.is_empty() {
        return patch(
            root,
            &key,
            &[("state", json!("done")), ("reason", json!("no-slug"))],
        )
        .is_ok();
    }
    let path = repo.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let cwd = if path.is_empty() {
        None
    } else {
        Some(Path::new(path))
    };
    let sig = collect(slug, cwd, &pp).await;
    // 修復課題の起票（repairs 上限超過は paused）
    let mut repairs: Vec<String> = pp
        .get("repairs")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let max_repairs = cfg_f64(cfg, "postPushMaxRepairs", 3.0) as usize;
    let known: std::collections::HashSet<String> = repairs.iter().cloned().collect();
    let new_findings: Vec<&Finding> = sig
        .findings
        .iter()
        .filter(|f| {
            !known.contains(&store::issue_key(
                "post-push",
                slug,
                &format!("pp-{}-{}", f.kind, f.ref_id),
            ))
        })
        .collect();
    if !new_findings.is_empty() {
        if max_repairs > 0 && repairs.len() >= max_repairs {
            conlog(
                "postpush",
                "err",
                &format!("{}: 修復課題が上限（{}）に到達 → paused", key, max_repairs),
            );
            return patch(
                root,
                &key,
                &[("state", json!("paused")), ("reason", json!("max-repairs"))],
            )
            .is_ok();
        }
        for f in new_findings {
            let rk = file_repair(root, slug, issue, f);
            if !rk.is_empty() && !repairs.contains(&rk) {
                repairs.push(rk);
            }
        }
        return patch(
            root,
            &key,
            &[("state", json!("fixing")), ("repairs", json!(repairs))],
        )
        .is_ok();
    }
    // 修復課題が未終端のまま検出が解消しても fixing を維持
    // （repairs が空でない限り起票済み修復の終端を待つ）
    if !repairs.is_empty() {
        return tick_fixing(root, issue, &pp);
    }
    if sig.pending {
        return patch(root, &key, &[("state", json!("waiting"))]).is_ok();
    }
    // 無検出: 監視窓経過で done、未満なら checking のまま checks++
    let window_min = cfg_f64(cfg, "postPushWatchMinutes", 120.0);
    let elapsed_min = pp
        .get("since")
        .and_then(|v| v.as_str())
        .and_then(iso_parse)
        .map(|s| (chrono::Utc::now() - s).num_seconds() as f64 / 60.0)
        .unwrap_or(0.0);
    if window_min > 0.0 && elapsed_min >= window_min {
        conlog(
            "postpush",
            "sys",
            &format!("{}: 監視窓 {:.0}分を無検出で経過 → 監視終了", key, window_min),
        );
        return patch(root, &key, &[("state", json!("done"))]).is_ok();
    }
    let checks = pp.get("checks").and_then(|v| v.as_u64()).unwrap_or(0) + 1;
    patch(
        root,
        &key,
        &[("state", json!("checking")), ("checks", json!(checks))],
    )
    .is_ok()
}

/// fixing の終端判定: 修復課題が全て done → done（各修復課題は自身の
/// postPush で再監視される）。failed/rejected 混在は paused
/// （reason=repair-failed:<key>）。未終端が残る限り fixing を維持
fn tick_fixing(root: &Path, issue: &Value, pp: &Value) -> bool {
    let key = issue
        .get("key")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let repairs: Vec<String> = pp
        .get("repairs")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    if repairs.is_empty() {
        return patch(root, &key, &[("state", json!("checking"))]).is_ok();
    }
    let issues = store::load_issues(root);
    let mut n_done = 0usize;
    for rk in &repairs {
        let st = issues
            .iter()
            .find(|i| i.get("key").and_then(|v| v.as_str()) == Some(rk.as_str()))
            .and_then(|i| i.get("status"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        match st {
            "done" => n_done += 1,
            "failed" | "rejected" | "dropped" => {
                conlog(
                    "postpush",
                    "err",
                    &format!("{}: 修復課題 {} が {} → paused", key, rk, st),
                );
                return patch(
                    root,
                    &key,
                    &[
                        ("state", json!("paused")),
                        ("reason", json!(format!("repair-{}:{}", st, rk))),
                    ],
                )
                .is_ok();
            }
            _ => return false, // 未終端の修復課題あり → fixing 維持
        }
    }
    if n_done == repairs.len() {
        conlog(
            "postpush",
            "sys",
            &format!("{}: 修復課題 {} 件が全て done → 監視終了", key, n_done),
        );
        return patch(root, &key, &[("state", json!("done"))]).is_ok();
    }
    false
}

/// 1 周期分の監視。postPushWatch=false で無効化。
/// 戻り値は postPush が更新された課題数（呼出側の SSE push 判定用）
pub async fn poll(app: &Shared) -> usize {
    let cfg = app.cfg().await;
    if !cfg_bool_default_true(&cfg, "postPushWatch") {
        return 0;
    }
    let issues = store::load_issues(&app.root);
    let mut touched = 0usize;
    for it in &issues {
        let active = it
            .get("postPush")
            .and_then(|p| p.get("state"))
            .and_then(|v| v.as_str())
            .map(|s| matches!(s, "checking" | "waiting" | "fixing"))
            .unwrap_or(false);
        if !active {
            continue;
        }
        if tick(app, &cfg, it).await {
            touched += 1;
        }
    }
    touched
}
