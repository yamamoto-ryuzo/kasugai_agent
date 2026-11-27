// runner.rs — 課題 run の実行体（issue-lifecycle.mmd / verify-levels.mmd、
// README §5.4-5.5）
//
// dispatch は人判断のみ: queued 課題の起動は一覧の対応ボタン
// （POST /api/issues/:key/dispatch → dispatch_issue）だけが行う。
// control::spawn_watchers の定期 drain は起票 reconcile・中断した
// verifying の再開・孤児 running の failed 化のみを担い、queued を
// 自動起動しない（issue-lifecycle.mmd）。起動した課題ごとの run タスクが
// 次を担う:
//   1. queued → running 遷移・runs[] へ {startedAt, sessionLog} 追記
//   2. 課題・受入条件・knowledge/<key> の知見・MAIN 運用指示を prompt に
//      組立て、chatAgents 実行体を `-p` 非対話モードで spawn
//      （stdout/stderr → conlog + evidence/<slug>/run.log）
//   3. 終了で running → verifying へ遷移し verifyLevel 別の検証を実施
//      （L0: 差分・構文 / L1: build・test 試行 / L2+: locks で実機排他し
//       verifyExe 起動・応答確認。画面駆動は後続課題）
//   4. 合格なら未pushコミットの帰属を確認し、`Issue: <key>` トレーラを
//      持たない混入コミットを git revert で除去（reset 禁止）してから push
//      （repo.requirePushApproval 時は pushApproved まで verifying で待機）
//   5. NG は verifying → retry → queued へ自動差し戻し
//      （verifyBackLimit 超過で failed。再 dispatch は対応ボタン）
// 状態遷移は pipeline::request_transition のみを経由する。
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::audit;
use crate::cases;
use crate::conlog::conlog;
use crate::config::*;
use crate::control::{App, Shared};
use crate::github;
use crate::knowledge;
use crate::lessons;
use crate::locks;
use crate::pipeline;
use crate::repomatch::repo_slug;
use crate::store;
use crate::trace::{self, Trace};
use crate::trajectory::{self, Trajectory};
use crate::util::{hide_console_tokio, now_iso};

/// trajectory の共有ハンドル（出力監視タスクが observation を逐次追記するため）
type SharedTraj = Arc<Mutex<Trajectory>>;
type SharedTrace = Arc<Trace>;

/// run の観測ハンドル（trajectory + trace span を束ねて runner 各段へ渡す。
/// 旧形式 run（ファイル未作成）では両方 None で何もしない）
#[derive(Clone, Default)]
struct Obs {
    traj: Option<SharedTraj>,
    trace: Option<SharedTrace>,
}

/// 既存 run の trajectory を開く（trajectory.jsonl がある run のみ。
/// drain が verify を再開する別タスク経路用。無ければ None で記録なし継続）
fn traj_existing(root: &Path, key: &str) -> Option<Trajectory> {
    let issue = find_issue(root, key)?;
    let idx = trajectory::latest_run_idx(&issue)?;
    let dir = trajectory::run_dir(
        &root.join("evidence").join(knowledge::key_slug(key)),
        idx,
    );
    dir.join("trajectory.jsonl")
        .is_file()
        .then(|| Trajectory::open(root, key, idx))
}

/// trajectory へ1ステップ追記（traj が無い旧形式 run は何もしない）
fn tstep(traj: &Option<SharedTraj>, kind: &str, name: &str, data: Value) {
    if let Some(t) = traj {
        t.lock().unwrap().step(kind, name, data);
    }
}

/// trajectory を終了理由で閉じる
fn tfin(traj: &Option<SharedTraj>, kind: &str, detail: &str) {
    if let Some(t) = traj {
        t.lock().unwrap().finish(trajectory::end_reason(kind, detail));
    }
}

// drain 再入防止（root 単位）と、run/verify 実行中（in-flight）の課題キー。
// 再起動で in-flight は空になるため、残存 running/verifying は drain が復帰処理する
static IN_FLIGHT: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
static DRAINING: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
static DISPATCHING: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

fn in_flight() -> &'static Mutex<HashSet<String>> {
    IN_FLIGHT.get_or_init(|| Mutex::new(HashSet::new()))
}

fn flight_add(key: &str) -> bool {
    in_flight().lock().unwrap().insert(key.to_string())
}
fn flight_has(key: &str) -> bool {
    in_flight().lock().unwrap().contains(key)
}
fn flight_del(key: &str) {
    in_flight().lock().unwrap().remove(key);
}

fn draining() -> &'static Mutex<HashSet<PathBuf>> {
    DRAINING.get_or_init(|| Mutex::new(HashSet::new()))
}

fn dispatching() -> &'static tokio::sync::Mutex<()> {
    DISPATCHING.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn key_of(issue: &Value) -> String {
    issue
        .get("key")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn find_issue(root: &Path, key: &str) -> Option<Value> {
    store::load_issues(root)
        .into_iter()
        .find(|i| key_of(i) == key)
}

// ---------- drain（control から定期呼び出し） ----------

pub async fn drain(app: &Shared) {
    {
        let mut d = draining().lock().unwrap();
        if !d.insert(app.root.clone()) {
            return;
        }
    }
    drain_inner(app).await;
    draining().lock().unwrap().remove(&app.root);
}

async fn drain_inner(app: &Shared) {
    let root = app.root.clone();
    // (0) 起票の reconcile（起票は自動が既定。detect-pipeline.mmd）:
    //     candidate に課題が無ければ intake で補起票する
    let (filed, deferred) = crate::detectors::backfill_issues(&root);
    if filed > 0 {
        app.sse("candidates", &json!({}));
        app.sse("issues", &json!({}));
    } else if deferred > 0 {
        // レート制限の deferred ピル表示のため candidates も即時反映
        app.sse("candidates", &json!({}));
    }
    // (a) 中断・承認待ちの再処理。run/verify は in-flight タスク側で完結するが、
    //     control 再起動や pushApproved 付与で残った分をここで拾う
    let drain_cfg = app.cfg().await;
    for it in store::load_issues(&root) {
        let key = key_of(&it);
        if key.is_empty() || flight_has(&key) {
            continue;
        }
        match it.get("status").and_then(|v| v.as_str()) {
            Some(pipeline::ST_PLANNING) => {
                // 計画生成の追跡を喪失（control 再起動）→ queued へ戻す。
                // 再 dispatch で計画からやり直す（計画フェーズは読み取り
                // 専用なので安全にやり直せる）
                let _ = pipeline::set_field(
                    &root,
                    &key,
                    "planNote",
                    json!("control 再起動で計画生成を中断"),
                );
                if pipeline::request_transition(&root, &key, pipeline::ST_QUEUED).is_ok() {
                    conlog(
                        "runner",
                        "sys",
                        &format!("{}: 中断した計画生成を queued へ差し戻し", key),
                    );
                    app.sse("issues", &json!({}));
                }
            }
            Some(pipeline::ST_PLAN_REVIEW) => {
                // タイムアウト自動承認（planAutoApproveMinutes > 0 のみ。
                // 0=人の承認を無期限に待つ）。planAt から経過を見る
                let mins = cfg_f64(&drain_cfg, "planAutoApproveMinutes", 0.0);
                if mins <= 0.0 {
                    continue;
                }
                let waited = it
                    .get("planAt")
                    .or_else(|| it.get("updatedAt"))
                    .and_then(|v| v.as_str())
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|t| {
                        (chrono::Utc::now() - t.with_timezone(&chrono::Utc))
                            .num_seconds() as f64
                            / 60.0
                    })
                    .unwrap_or(0.0);
                if waited < mins || !flight_add(&key) {
                    continue;
                }
                let app2 = app.clone();
                let key2 = key.clone();
                tokio::spawn(async move {
                    conlog(
                        "runner",
                        "sys",
                        &format!("{}: 計画承認タイムアウト → 自動承認", key2),
                    );
                    if let Err(e) = plan_approve(&app2, &key2).await {
                        conlog(
                            "runner",
                            "err",
                            &format!("{}: 計画自動承認失敗: {}", key2, e),
                        );
                    }
                    flight_del(&key2);
                });
            }
            Some(pipeline::ST_VERIFYING) => {
                if !flight_add(&key) {
                    continue;
                }
                let app2 = app.clone();
                tokio::spawn(async move {
                    resume_verify(&app2, &key, Obs::default()).await;
                    flight_del(&key);
                });
            }
            Some(pipeline::ST_ROLLBACK) => {
                // rollback 中に control 再起動 → 帰属コミットの revert を
                // やり直してから failed へ送る（revert は冪等: 除去済みの
                // コミットは未push履歴から消えているため再 revert されない）
                if !flight_add(&key) {
                    continue;
                }
                let app2 = app.clone();
                let key2 = key.clone();
                tokio::spawn(async move {
                    let it = find_issue(&app2.root, &key2).unwrap_or_default();
                    rollback_issue_commits(&app2, &key2, &it).await;
                    if pipeline::request_transition(
                        &app2.root,
                        &key2,
                        pipeline::ST_FAILED,
                    )
                    .is_ok()
                    {
                        conlog(
                            "runner",
                            "sys",
                            &format!("{}: 中断した rollback を完了し failed へ", key2),
                        );
                        app2.sse("issues", &json!({}));
                    }
                    flight_del(&key2);
                });
            }
            Some(pipeline::ST_RUNNING) => {
                // run 追跡を持たない running（再起動で子プロセスの待機を喪失）
                // は失敗扱いにする。再実行は failed → queued で人が戻す
                let _ = pipeline::set_field(
                    &root,
                    &key,
                    "verifyNote",
                    json!("control 再起動で run の追跡を喪失"),
                );
                // 孤児 run の trajectory に終了理由 aborted を補記する
                trajectory::abort_orphan(&root, &key);
                // 監査にも中断を残し、開いたままの trace root span を閉じる
                audit::record_run(
                    &root,
                    &key,
                    "system",
                    "aborted",
                    Map::from_iter([(
                        "note".into(),
                        json!("control 再起動で run の追跡を喪失"),
                    )]),
                );
                if let Some(idx) = trajectory::latest_run_idx(&it) {
                    let dir = trajectory::run_dir(
                        &root.join("evidence").join(knowledge::key_slug(&key)),
                        idx,
                    );
                    if dir.join("trace.jsonl").is_file() {
                        let tr = Trace::open(dir);
                        tr.event("aborted", Map::new());
                        tr.finish_root(false, "aborted");
                    }
                }
                if pipeline::request_transition(&root, &key, pipeline::ST_FAILED).is_ok() {
                    conlog(
                        "runner",
                        "sys",
                        &format!("{}: 中断 run を failed に遷移", key),
                    );
                    app.sse("issues", &json!({}));
                }
            }
            _ => {}
        }
    }
    // (h) retention 掃除（housekeep.mmd・README §11）: retentionDays を超えた
    //     終端課題の evidence と古い logs を削除する（0=掃除しない）
    let days = cfg_u64(&app.cfg().await, "retentionDays", 0);
    if days > 0 {
        let n = crate::housekeep::sweep(&root, days);
        if n > 0 {
            conlog(
                "housekeep",
                "sys",
                &format!("retention: {} 件削除（{}日超）", n, days),
            );
        }
    }
    // queued の起動はここでは行わない —— dispatch は人判断のみで、
    // 一覧の対応ボタン（dispatch_issue）だけが課題を起動する
    // （issue-lifecycle.mmd: queued → running は自動起動しない）
}

/// queued 課題を起動する唯一の経路（一覧の対応ボタン → dispatch API）。
/// can_dispatch の論理競合規則を使い、同一 repo の running|verifying や
/// 同時の dispatch 要求と競合しないよう lock する。
pub async fn dispatch_issue(app: &Shared, key: &str) -> Result<Value, String> {
    let _dispatch_guard = dispatching().lock().await;
    if flight_has(key) {
        return Err("already running".into());
    }
    let issues = store::load_issues(&app.root);
    pipeline::can_dispatch(&app.cfg().await, &issues, key)?;
    let issue = issues
        .into_iter()
        .find(|i| key_of(i) == key)
        .ok_or_else(|| "issue not found".to_string())?;
    // 計画承認ゲート（manual-883d17ee）: planGate 課題は実装着手前に
    // planning（計画生成 + planning critic）を経由する。承認済み
    // （planApproved）は通常通り running へ
    if issue.get("planGate").and_then(|v| v.as_bool()) == Some(true)
        && issue.get("planApproved").and_then(|v| v.as_bool()) != Some(true)
    {
        start_plan(app, issue).await?;
    } else {
        start_run(app, issue).await?;
    }
    find_issue(&app.root, key).ok_or_else(|| "issue not found".to_string())
}

// ---------- run（実装実行） ----------

/// 実行体の解決: chatAgents の active agent の exe。promptArgs（既定 ["-p"]）
/// が非対話モードの引数で、最後に prompt を1引数として連結する。
/// active が exe を持たない場合は exe 持ちの agent を探す。
fn resolve_agent(cfg: &Value) -> Option<(String, Vec<String>)> {
    let (active, agents) = App::chat_agents(cfg);
    let pick = |a: &Value| -> Option<(String, Vec<String>)> {
        let exe = a
            .get("exe")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())?
            .to_string();
        let args = a
            .get("promptArgs")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .filter(|arr: &Vec<String>| !arr.is_empty())
            .unwrap_or_else(|| vec!["-p".to_string()]);
        Some((exe, args))
    };
    active
        .and_then(|id| agents.get(&id))
        .and_then(pick)
        .or_else(|| agents.values().find_map(pick))
}

async fn start_run(app: &Shared, issue: Value) -> Result<(), String> {
    let root = app.root.clone();
    let key = key_of(&issue);
    let title = issue
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let cfg = app.cfg().await;
    let agent = resolve_agent(&cfg);
    let repo = App::git_repo(
        &cfg,
        issue.get("repo").and_then(|v| v.as_str()).unwrap_or(""),
    )
    .cloned();

    if let Err(e) = pipeline::request_transition(&root, &key, pipeline::ST_RUNNING) {
        let msg = format!("{}: running 遷移失敗: {}", key, e);
        conlog("runner", "err", &msg);
        return Err(msg);
    }
    app.sse("issues", &json!({}));
    flight_add(&key);

    // runs[] 追記 {startedAt, sessionLog}
    let slug = knowledge::key_slug(&key);
    let ev_dir = root.join("evidence").join(&slug);
    let _ = std::fs::create_dir_all(&ev_dir);
    let log_rel = format!("evidence/{}/run.log", slug);
    let cur = find_issue(&root, &key).unwrap_or(issue.clone());
    let mut runs = cur
        .get("runs")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    runs.push(json!({"startedAt": now_iso(), "sessionLog": log_rel}));
    let run_idx = runs.len() - 1;
    let _ = pipeline::set_field(&root, &key, "runs", json!(runs));
    conlog("runner", "sys", &format!("run 開始: {} {}", key, title));

    let app2 = app.clone();
    tokio::spawn(async move {
        run_and_verify(&app2, &key, &issue, agent, repo, &ev_dir, run_idx).await;
        flight_del(&key);
    });
    Ok(())
}

// ---------- 計画承認ゲート + Planning Critic（manual-883d17ee） ----------
// dispatch 後・実装着手前に、実行体へ「実装計画のみ」を生成させ
// （queued → planning）、第2呼び出しの planning critic が計画を批評する。
// critic 合格後は autoApprovePlan=on なら即 running、off なら plan_review で
// 人の承認（plan-approve API）or planAutoApproveMinutes の自動承認を待つ。

/// 計画生成 prompt（prompts/plan.md で差替可）
const DEFAULT_PLAN: &str = r#"以下の課題の「実装計画」を作成してください。まだコードは変更しないでください。

- key: `{{key}}`
- repo: `{{repo}}`
- verifyLevel: L{{verifyLevel}}
- 受入条件: {{acceptance}}

## 課題

{{title}}

{{body}}

## 出力ルール

1. 対象リポジトリを読み取り専用で調査し、変更対象ファイル・実装手順・
   確認方法・リスクを箇条書きした実装計画を日本語で出力する
2. ファイルの変更・git commit は一切行わない（計画のみ）
3. 最後に計画全文を ```kasugai-plan ブロックで出力する
4. この実行は非対話（-p）モード。確認を求めず計画を出力して終了する"#;

/// planning critic prompt（prompts/critic.md で差替可）
const DEFAULT_CRITIC: &str = r#"以下は課題の実装計画です。計画を批評してください（実装はしない）。

## 課題

{{title}}

{{body}}

- verifyLevel: L{{verifyLevel}}
- 受入条件: {{acceptance}}

## 実装計画

{{plan}}

## 出力ルール

1. 計画の抜け漏れ・誤った前提・受入条件を満たさない点・爆破半径を指摘する
2. 指摘には severity（severe=このまま実装すべきでない /
   non-severe=改善提案）と confidence（0.0〜1.0）を必ず付ける
3. 問題がなければ findings は空配列にする
4. 最後に必ず次の形式の結果ブロックを1つだけ出力する:

```kasugai-review
{"findings":[{"severity":"severe","confidence":0.9,"note":"指摘"}]}
```

5. この実行は非対話（-p）モード。確認を求めず批評を出力して終了する"#;

const DEFAULT_DIFFREVIEW: &str = r#"以下は課題への対応として生成された未 push の差分です。push 前のレビューとして批評してください（修正・追加のコード生成はしない）。

## 課題

{{title}}

{{body}}

- verifyLevel: L{{verifyLevel}}
- 受入条件: {{acceptance}}

## 差分

```diff
{{diff}}
```

## 出力ルール

1. 課題の受入条件に照らして、誤った変更・抜け漏れ・混入した無関係な変更・
   セキュリティ上の問題（秘密情報の混入等）を指摘する
2. 指摘には severity（severe=このまま push すべきでない /
   non-severe=改善提案）と confidence（0.0〜1.0）を必ず付ける
3. 問題がなければ findings は空配列にする
4. 最後に必ず次の形式の結果ブロックを1つだけ出力する:

```kasugai-review
{"findings":[{"severity":"severe","confidence":0.9,"note":"指摘"}]}
```

5. この実行は非対話（-p）モード。確認を求めず批評を出力して終了する"#;


/// ```kasugai-<tag> ブロックの内容を抽出（最後の1つ）
fn extract_tag_block(text: &str, tag: &str) -> Option<String> {
    let open = format!("```{}", tag);
    let mut out = None;
    let mut rest = text;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        let body = after.trim_start_matches([' ', '\t', '\r', '\n']);
        if let Some(j) = body.find("```") {
            out = Some(body[..j].trim().to_string());
        }
        rest = after;
    }
    out
}

/// 実行体を -p 非対話で1回呼び出し、stdout+stderr を結合して返す
/// （計画生成・critic・diff review の単発批評パス用）
async fn spawn_capture(
    exe: &str,
    args: &[String],
    cwd: &Path,
    prompt: &str,
    timeout: Duration,
) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(args)
        .arg(prompt)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    hide_console_tokio(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("実行体の起動失敗: {}", e))?;
    let buf = Arc::new(Mutex::new(String::new()));
    let mut readers = Vec::new();
    let pipes: Vec<Option<Box<dyn tokio::io::AsyncRead + Unpin + Send>>> = vec![
        child.stdout.take().map(|p| Box::new(p) as _),
        child.stderr.take().map(|p| Box::new(p) as _),
    ];
    for pipe in pipes.into_iter().flatten() {
        let buf = buf.clone();
        readers.push(tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut lines = tokio::io::BufReader::new(pipe).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                let mut b = buf.lock().unwrap();
                b.push_str(&l);
                b.push('\n');
            }
        }));
    }
    let res = tokio::time::timeout(timeout, child.wait()).await;
    let timed_out = res.is_err();
    if timed_out {
        let _ = child.kill().await;
    }
    for r in readers {
        let _ = r.await;
    }
    let out = buf.lock().unwrap().clone();
    if timed_out {
        return Err("実行体がタイムアウト".into());
    }
    match res {
        Ok(Ok(st)) if st.success() => Ok(out),
        Ok(Ok(st)) => Err(format!("実行体が異常終了（code={:?}）", st.code())),
        Ok(Err(e)) => Err(format!("実行失敗: {}", e)),
        Err(_) => Err("実行体がタイムアウト".into()),
    }
}

/// prompt テンプレの {{name}} 置換（課題フィールド + 追加フィールド）
fn subst_fields(tpl: &str, issue: &Value, extra: &[(&str, String)]) -> String {
    let mut out = tpl.to_string();
    for k in [
        "key", "repo", "title", "body", "acceptance", "source", "kind",
    ] {
        let v = issue.get(k).and_then(|v| v.as_str()).unwrap_or("");
        out = out.replace(&format!("{{{{{}}}}}", k), v);
    }
    let lvl = issue.get("verifyLevel").map(|v| v.to_string()).unwrap_or("1".into());
    out = out.replace("{{verifyLevel}}", &lvl);
    let prio = issue.get("priority").map(|v| v.to_string()).unwrap_or("1".into());
    out = out.replace("{{priority}}", &prio);
    for (k, v) in extra {
        out = out.replace(&format!("{{{{{}}}}}", k), v);
    }
    out
}

fn plan_prompt(root: &Path, issue: &Value) -> String {
    let tpl = knowledge::load_prompt(root, "plan").unwrap_or_else(|| DEFAULT_PLAN.into());
    subst_fields(&tpl, issue, &[])
}

fn critic_prompt(root: &Path, issue: &Value, plan: &str) -> String {
    let tpl =
        knowledge::load_prompt(root, "critic").unwrap_or_else(|| DEFAULT_CRITIC.into());
    subst_fields(&tpl, issue, &[("plan", plan.to_string())])
}

fn diff_review_prompt(root: &Path, issue: &Value, diff: &str) -> String {
    let tpl = knowledge::load_prompt(root, "diffreview")
        .unwrap_or_else(|| DEFAULT_DIFFREVIEW.into());
    subst_fields(&tpl, issue, &[("diff", diff.to_string())])
}


/// 計画フェーズ起動: queued → planning で計画生成タスクを spawn
async fn start_plan(app: &Shared, issue: Value) -> Result<(), String> {
    let root = app.root.clone();
    let key = key_of(&issue);
    if let Err(e) = pipeline::request_transition(&root, &key, pipeline::ST_PLANNING) {
        let msg = format!("{}: planning 遷移失敗: {}", key, e);
        conlog("runner", "err", &msg);
        return Err(msg);
    }
    app.sse("issues", &json!({}));
    flight_add(&key);
    let app2 = app.clone();
    tokio::spawn(async move {
        run_plan_phase(&app2, &key, &issue).await;
        flight_del(&key);
    });
    Ok(())
}

/// planning の実行: 計画生成 → planning critic → 承認判定。
/// critic で Severe（confidence>=planCriticMinConfidence）があれば
/// planning → queued へ差し戻し（次回 dispatch で計画をやり直す）。
/// critic 合格は autoApprovePlan で即 running、off で plan_review。
async fn run_plan_phase(app: &Shared, key: &str, issue: &Value) {
    let root = &app.root;
    let cfg = app.cfg().await;
    let back = |note: &str| {
        let _ = pipeline::set_field(root, key, "planNote", json!(note));
        let _ = pipeline::request_transition(root, key, pipeline::ST_QUEUED);
        conlog("runner", "err", &format!("{}: 計画差し戻し: {}", key, note));
    };
    let Some((exe, args)) = resolve_agent(&cfg) else {
        back("chatAgents に実行可能な exe がありません");
        return;
    };
    let Some(repo_v) = App::git_repo(
        &cfg,
        issue.get("repo").and_then(|v| v.as_str()).unwrap_or(""),
    )
    .cloned() else {
        back("課題の repo が config.repos に未登録");
        return;
    };
    let repo_path = PathBuf::from(
        repo_v.get("path").and_then(|v| v.as_str()).unwrap_or(""),
    );
    if !repo_path.is_dir() {
        back("repo path が存在しない");
        return;
    }

    // 計画フェーズの span は次の run dir（run-<runs.len()>）へ書く。
    // 計画→running へ進んだ場合は同じ run の前半 span になる
    let plan_trace = {
        let next_idx = issue
            .get("runs")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let dir = trajectory::run_dir(
            &root.join("evidence").join(knowledge::key_slug(key)),
            next_idx,
        );
        Trace::open(dir)
    };
    // 1. 計画生成（読み取り専用・実装しない）
    let pspan = plan_trace.start(
        "llm.plan",
        trace::KIND_LLM,
        Map::from_iter([("gen_ai.system".into(), json!(exe))]),
    );
    let ptimeout = Duration::from_secs_f64(cfg_f64(&cfg, "planTimeoutMinutes", 30.0) * 60.0);
    let out = match spawn_capture(&exe, &args, &repo_path, &plan_prompt(root, issue), ptimeout).await {
        Ok(o) => {
            plan_trace.end(&pspan, true, "", Map::new());
            o
        }
        Err(e) => {
            plan_trace.end(&pspan, false, &e.to_string(), Map::new());
            plan_trace.event("plan.back", Map::new());
            back(&format!("計画生成に失敗: {}", e));
            audit::record_run(
                root,
                key,
                "system",
                "plan.back",
                Map::from_iter([("reason".into(), json!(format!("計画生成に失敗: {}", e)))]),
            );
            app.sse("issues", &json!({}));
            return;
        }
    };
    let plan = extract_tag_block(&out, "kasugai-plan").unwrap_or_else(|| {
        // ブロックが無ければ末尾 6000 文字を計画として採用
        out.chars().rev().take(6000).collect::<String>().chars().rev().collect()
    });
    if plan.trim().is_empty() {
        plan_trace.event("plan.back", Map::new());
        back("計画の出力が空");
        app.sse("issues", &json!({}));
        return;
    }
    let _ = pipeline::set_field(root, key, "plan", json!(plan));
    let _ = pipeline::set_field(root, key, "planAt", json!(now_iso()));
    audit::record_run(
        root,
        key,
        "system",
        "plan",
        Map::from_iter([("planChars".into(), json!(plan.len()))]),
    );
    conlog("runner", "sys", &format!("{}: 計画生成完了（{} 文字）", key, plan.len()));

    // 2. Planning Critic（別呼出しの独立コンテキストで計画を批評）
    let ctimeout = Duration::from_secs_f64(cfg_f64(&cfg, "criticTimeoutMinutes", 15.0) * 60.0);
    let min_conf = cfg_f64(&cfg, "planCriticMinConfidence", 0.7);
    let mut critic_ok = false;
    let cspan = plan_trace.start("llm.plan.critic", trace::KIND_LLM, Map::new());
    let critic_res = spawn_capture(&exe, &args, &repo_path, &critic_prompt(root, issue, &plan), ctimeout).await;
    match critic_res {
        Ok(cout) => match crate::verify::extract_review(&cout) {
            Some(review) => {
                let severe = crate::verify::severe_findings(&review, min_conf);
                let _ = pipeline::set_field(
                    root,
                    key,
                    "planCritic",
                    json!({"at": now_iso(), "findings": review.findings}),
                );
                if !severe.is_empty() {
                    let notes: Vec<String> = severe.iter().map(|f| f.note.clone()).collect();
                    plan_trace.end(&cspan, false, &notes.join(" / "), Map::new());
                    plan_trace.event("plan.back", Map::new());
                    back(&format!("計画 critic で Severe 検出: {}", notes.join(" / ")));
                    audit::record_run(
                        root,
                        key,
                        "system",
                        "plan.back",
                        Map::from_iter([
                            ("reason".into(), json!("計画 critic で Severe 検出")),
                            ("findings".into(), json!(notes)),
                        ]),
                    );
                    app.sse("issues", &json!({}));
                    return;
                }
                critic_ok = true;
            }
            None => {
                let _ = pipeline::set_field(
                    root, key, "planNote",
                    json!("計画 critic の結果ブロックを取得できず"),
                );
            }
        },
        Err(e) => {
            let _ = pipeline::set_field(
                root, key, "planNote", json!(format!("計画 critic 実行失敗: {}", e)),
            );
        }
    }
    // critic span を閉じる（severe return 経路は既に end 済みで、
    // ここでの end は open map からの remove なので冪等に無視される）
    plan_trace.end(&cspan, critic_ok, "", Map::new());

    // 3. 承認判定: autoApprovePlan=on は critic 合格（要必須）で即承認、
    //    それ以外（critic 未取得/失敗を含む）は plan_review で人を待つ
    if cfg_bool_default_false(&cfg, "autoApprovePlan") && critic_ok {
        let _ = pipeline::set_field(root, key, "planApproved", json!(true));
        plan_trace.event("plan.approved", Map::from_iter([("actor".into(), json!("system"))]));
        audit::record_run(root, key, "system", "plan.approved", Map::new());
        conlog("runner", "sys", &format!("{}: 計画を自動承認 → running", key));
        if let Some(cur) = find_issue(root, key) {
            let _ = start_run(app, cur).await;
        }
    } else if pipeline::request_transition(root, key, pipeline::ST_PLAN_REVIEW).is_ok() {
        plan_trace.event("plan_review", Map::new());
        conlog(
            "runner",
            "sys",
            &format!("{}: 計画を plan_review で承認待ち", key),
        );
        app.sse("issues", &json!({}));
    }
}

/// plan_review の計画を承認して実装 run を起動する
/// （POST /api/issues/:key/plan-approve。planAutoApproveMinutes の
/// タイムアウト自動承認も同じ経路）
pub async fn plan_approve(app: &Shared, key: &str) -> Result<Value, String> {
    let _dispatch_guard = dispatching().lock().await;
    if flight_has(key) {
        return Err("already running".into());
    }
    let issue = find_issue(&app.root, key).ok_or_else(|| "issue not found".to_string())?;
    if issue.get("status").and_then(|v| v.as_str()) != Some(pipeline::ST_PLAN_REVIEW) {
        return Err("issue is not plan_review".into());
    }
    let _ = pipeline::set_field(&app.root, key, "planApproved", json!(true));
    start_run(app, issue).await?;
    find_issue(&app.root, key).ok_or_else(|| "issue not found".to_string())
}

/// plan_review の計画を差し戻し（plan_review → queued。再 dispatch で計画から
/// やり直す。POST /api/issues/:key/plan-reject）
pub async fn plan_reject(app: &Shared, key: &str) -> Result<Value, String> {
    let issue = find_issue(&app.root, key).ok_or_else(|| "issue not found".to_string())?;
    if issue.get("status").and_then(|v| v.as_str()) != Some(pipeline::ST_PLAN_REVIEW) {
        return Err("issue is not plan_review".into());
    }
    let _ = pipeline::set_field(
        &app.root, key, "planNote", json!("人が計画を差し戻し"),
    );
    pipeline::request_transition(&app.root, key, pipeline::ST_QUEUED)
}

/// run レコード（runs[idx]）へフィールド追記
fn update_run(root: &Path, key: &str, idx: usize, extra: Map<String, Value>) {
    let Some(it) = find_issue(root, key) else {
        return;
    };
    let mut runs = it
        .get("runs")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if let Some(r) = runs.get_mut(idx).and_then(|r| r.as_object_mut()) {
        r.extend(extra);
    }
    let _ = pipeline::set_field(root, key, "runs", json!(runs));
}

/// 実行不能・異常終了時の failed 処理（running → failed）
async fn fail_run(
    app: &Shared,
    key: &str,
    run_idx: usize,
    reason: &str,
    obs: Obs,
) {
    let mut extra = Map::new();
    extra.insert("endedAt".into(), json!(now_iso()));
    extra.insert("error".into(), json!(reason));
    update_run(&app.root, key, run_idx, extra);
    audit::record(
        &app.root,
        key,
        "system",
        "failed",
        Some(run_idx),
        Map::from_iter([("reason".into(), json!(reason))]),
    );
    if let Some(tr) = &obs.trace {
        tr.event("failed", Map::from_iter([("reason".into(), json!(reason))]));
        tr.finish_root(false, reason);
    }
    if let Some(t) = &obs.traj {
        let mut t = t.lock().unwrap();
        t.step("state", "failed", json!({}));
        t.step("thought", "error", json!({"reason": reason}));
        t.finish(trajectory::end_reason("error", reason));
    }
    let _ = pipeline::set_field(&app.root, key, "verifyNote", json!(reason));
    // 失敗レッスン（lesson-loop.mmd・manual-29799d3e）: 何が・なぜ失敗したか
    // を lessons.json へ保存し、次回 run の prompt へ注入する
    if let Some(it) = find_issue(&app.root, key) {
        lessons::record_failure(&app.root, &it, run_idx, reason);
        // ACE カウンタ（manual-bcf981dd）: この run に注入したレッスンは
        // 成果に結びつかなかったので harmful++
        lessons::record_outcome(&app.root, &it, false, &app.cfg().await);
    }
    if pipeline::request_transition(&app.root, key, pipeline::ST_FAILED).is_ok() {
        conlog("runner", "err", &format!("{}: run 失敗: {}", key, reason));
        if let Some(it) = find_issue(&app.root, key) {
            report_issue(app, &it, &format!("[kasugai-agent] 自動処理失敗: {}", reason))
                .await;
        }
    }
    app.sse("issues", &json!({}));
}

async fn run_and_verify(
    app: &Shared,
    key: &str,
    issue: &Value,
    agent: Option<(String, Vec<String>)>,
    repo: Option<Value>,
    ev_dir: &Path,
    run_idx: usize,
) {
    let root = &app.root;
    // trajectory 記録開始（observability.mmd）: meta を確定して
    // state=running から逐次ステップを追記する
    let traj: SharedTraj = {
        let mut meta = Map::new();
        meta.insert("issue".into(), json!(key));
        meta.insert("run".into(), json!(run_idx));
        meta.insert("startedAt".into(), json!(now_iso()));
        meta.insert(
            "verifyLevel".into(),
            issue.get("verifyLevel").cloned().unwrap_or(json!(1)),
        );
        if let Some((e, _)) = &agent {
            meta.insert("agent".into(), json!(e));
        }
        if let Some(r) = &repo {
            meta.insert(
                "repo".into(),
                json!(r.get("path").and_then(|v| v.as_str()).unwrap_or("")),
            );
        }
        let t = Arc::new(Mutex::new(Trajectory::begin(
            trajectory::run_dir(ev_dir, run_idx),
            meta,
        )));
        t.lock().unwrap().step("state", "running", json!({}));
        t
    };
    let traj = Some(traj);
    let obs = Obs {
        traj: traj.clone(),
        trace: Some(Trace::begin(trajectory::run_dir(ev_dir, run_idx))),
    };
    let Some((exe, prompt_args)) = agent else {
        fail_run(app, key, run_idx, "chatAgents に実行可能な exe がありません", obs).await;
        return;
    };
    let Some(repo_v) = repo else {
        fail_run(app, key, run_idx, "課題の repo が config.repos に未登録", obs).await;
        return;
    };
    let repo_path = PathBuf::from(
        repo_v
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
    );
    if !repo_path.is_dir() {
        fail_run(app, key, run_idx, "repo path が存在しない", obs).await;
        return;
    }

    let (prompt, krefs) = build_prompt_full(root, issue);
    // 参照した知見を run の記録へ紐付ける（manual-a8c93d14・case-search.mmd）
    {
        let mut extra = Map::new();
        extra.insert("knowledgeRefs".into(), krefs);
        update_run(root, key, run_idx, extra);
    }
    tstep(
        &traj,
        "action",
        "dispatch",
        json!({"exe": exe, "args": prompt_args, "promptChars": prompt.len()}),
    );
    audit::record(
        root,
        key,
        "system",
        "run.start",
        Some(run_idx),
        Map::from_iter([
            ("exe".into(), json!(exe)),
            ("args".into(), json!(prompt_args)),
            ("promptChars".into(), json!(prompt.len())),
        ]),
    );
    let log_path = ev_dir.join("run.log");
    let mut cmd = tokio::process::Command::new(&exe);
    cmd.args(&prompt_args)
        .arg(&prompt)
        .current_dir(&repo_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    hide_console_tokio(&mut cmd);
    conlog(
        "runner",
        "sys",
        &format!("{}: spawn {} {:?}", key, exe, prompt_args),
    );
    // OTel GenAI 風 span（trace-waterfall.mmd）: 実行体呼出を LLM span で括る
    let exec_span = obs.trace.as_ref().map(|t| {
        t.start(
            "llm.exec",
            trace::KIND_LLM,
            Map::from_iter([
                ("gen_ai.system".into(), json!(exe)),
                ("gen_ai.request.prompt_chars".into(), json!(prompt.len())),
            ]),
        )
    });
    let exec_t0 = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            if let (Some(t), Some(s)) = (&obs.trace, &exec_span) {
                t.end(s, false, &e.to_string(), Map::new());
            }
            fail_run(
                app,
                key,
                run_idx,
                &format!("実行体の起動失敗: {}", e),
                obs,
            )
            .await;
            return;
        }
    };
    // prompt に注入した followUps を consumed 化（spawn 成功後のみ。
    // 起動失敗で消費すると追記を誰にも見せずに失う）
    pipeline::consume_followups(root, key);

    // stdout/stderr → conlog + run.log バッファ
    let src = format!("run {}", key.rsplit(':').next().unwrap_or(key));
    let buf = Arc::new(Mutex::new(String::new()));
    let mut readers = Vec::new();
    let pipes: Vec<(&str, Option<Box<dyn tokio::io::AsyncRead + Unpin + Send>>)> = vec![
        ("out", child.stdout.take().map(|p| Box::new(p) as _)),
        ("err", child.stderr.take().map(|p| Box::new(p) as _)),
    ];
    for (stream, pipe) in pipes {
        let Some(pipe) = pipe else { continue };
        let src = src.clone();
        let buf = buf.clone();
        let traj = traj.clone();
        readers.push(tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut lines = tokio::io::BufReader::new(pipe).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                conlog(&src, stream, &l);
                if let Some(t) = &traj {
                    // observation: 実行体出力を1行1ステップで逐次追記
                    // （中断しても過程が残る。長行は打ち切る）
                    t.lock().unwrap().step(
                        "observation",
                        stream,
                        json!({"line": l.chars().take(4000).collect::<String>()}),
                    );
                }
                let mut b = buf.lock().unwrap();
                b.push_str(&l);
                b.push('\n');
            }
        }));
    }
    let cfg = app.cfg().await;
    let timeout =
        Duration::from_secs_f64(cfg_f64(&cfg, "runTimeoutMinutes", 120.0) * 60.0);
    let res = tokio::time::timeout(timeout, child.wait()).await;
    let (code, timed_out, wait_err) = match res {
        Ok(Ok(st)) => (st.code(), false, None),
        Ok(Err(e)) => (None, false, Some(e.to_string())),
        Err(_) => {
            let _ = child.kill().await;
            (None, true, None)
        }
    };
    for r in readers {
        let _ = r.await;
    }
    let out_text = buf.lock().unwrap().clone();
    let _ = std::fs::write(&log_path, out_text.as_str());
    // run コスト（run-cost.mmd）: 経過時間・入出力は常時記録、
    // トークン/費用は出力末尾の usage JSON を検出できた実行体のみ
    let usage = {
        let mut u = Map::new();
        u.insert("elapsedMs".into(), json!(exec_t0.elapsed().as_millis() as u64));
        u.insert("promptChars".into(), json!(prompt.chars().count()));
        u.insert("outputChars".into(), json!(out_text.chars().count()));
        u.insert("outputLines".into(), json!(out_text.lines().count()));
        for (k, v) in extract_usage(&out_text) {
            u.insert(k, v);
        }
        u
    };
    {
        let mut extra = Map::new();
        extra.insert("usage".into(), Value::Object(usage.clone()));
        extra.insert("endedAt".into(), json!(now_iso()));
        if let Some(c) = code {
            extra.insert("exitCode".into(), json!(c));
        }
        if timed_out {
            extra.insert("timedOut".into(), json!(true));
        }
        if let Some(e) = &wait_err {
            extra.insert("error".into(), json!(e));
        }
        update_run(root, key, run_idx, extra);
    }
    if let (Some(t), Some(s)) = (&obs.trace, &exec_span) {
        let mut attrs = Map::new();
        attrs.insert("gen_ai.usage.elapsed_ms".into(), json!(usage["elapsedMs"]));
        if let Some(v) = usage.get("tokens") {
            attrs.insert("gen_ai.usage.total_tokens".into(), v.clone());
        }
        if let Some(v) = usage.get("costUsd") {
            attrs.insert("gen_ai.usage.cost_usd".into(), v.clone());
        }
        attrs.insert("exit_code".into(), code.map(|c| json!(c)).unwrap_or(Value::Null));
        t.end(s, code == Some(0) && !timed_out, &format!("{:?}", code), attrs);
    }
    audit::record(
        root,
        key,
        "system",
        "exec",
        Some(run_idx),
        Map::from_iter([
            ("exitCode".into(), code.map(|c| json!(c)).unwrap_or(Value::Null)),
            ("timedOut".into(), json!(timed_out)),
            ("elapsedMs".into(), usage["elapsedMs"].clone()),
        ]),
    );
    tstep(
        &traj,
        "action",
        "exec.end",
        json!({"exitCode": code, "timedOut": timed_out, "error": wait_err}),
    );
    conlog(
        "runner",
        "sys",
        &format!(
            "{}: run 終了（code={:?}{}{}）",
            key,
            code,
            if timed_out { " timeout" } else { "" },
            wait_err.map(|e| format!(" err={}", e)).unwrap_or_default()
        ),
    );
    // 終了コードに関わらず検証へ進める（コミット済み成果の評価は verify の責務）
    if let Err(e) = pipeline::request_transition(root, key, pipeline::ST_VERIFYING) {
        conlog(
            "runner",
            "err",
            &format!("{}: verifying 遷移失敗: {}", key, e),
        );
        tstep(
            &traj,
            "thought",
            "transition.error",
            json!({"note": format!("verifying 遷移失敗: {}", e)}),
        );
        return;
    }
    app.sse("issues", &json!({}));
    tstep(&traj, "state", "verifying", json!({}));
    if let Some(t) = &obs.trace {
        t.event("state.verifying", Map::new());
    }
    resume_verify(app, key, obs).await;
}

/// 実行体出力の末尾からトークン/費用を抽出する（run-cost.mmd）。
/// claude -p 等が JSON 応答で返す usage/total_cost_usd を拾う。
/// 対応しない実行体では空 Map（経過時間・文字数は呼出側で常時記録）
fn extract_usage(out: &str) -> Map<String, Value> {
    let mut found = Map::new();
    for l in out.lines().rev().take(200) {
        let l = l.trim();
        if !l.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(l) else {
            continue;
        };
        for k in ["total_cost_usd", "cost_usd"] {
            if let Some(c) = v.get(k).and_then(|x| x.as_f64()) {
                found.insert("costUsd".into(), json!(c));
            }
        }
        if let Some(u) = v.get("usage") {
            let tot = u
                .get("input_tokens")
                .and_then(|x| x.as_u64())
                .unwrap_or(0)
                + u
                    .get("output_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0);
            let tot = if tot > 0 {
                tot
            } else {
                u.get("total_tokens").and_then(|x| x.as_u64()).unwrap_or(0)
            };
            if tot > 0 {
                found.insert("tokens".into(), json!(tot));
            }
        }
        if !found.is_empty() {
            break;
        }
    }
    found
}

// ---------- prompt 組立（dispatch。非対話 -p 実行） ----------

const DEFAULT_DISPATCH: &str = r#"以下の課題に対応してください。

- key: `{{key}}`
- repo: `{{repo}}`
- verifyLevel: L{{verifyLevel}}
- priority: P{{priority}}
- 受入条件: {{acceptance}}

## 課題

{{title}}

{{body}}

## 運用ルール

1. 対象リポジトリ内で実装を行う。課題の状態遷移・ラベル操作は
   パイプライン側が行うので触れない
2. 変更は `Issue: {{key}}` トレーラ付きで git commit する
   （未pushコミットの帰属判定に使用。`git reset` は禁止）
3. verifyLevel L{{verifyLevel}} に従って実機で確認し、証跡
   （スクショ・応答・ログ）を `evidence/` に残す
4. 完了したら実施内容・結果を簡潔に報告する
5. 対応できない場合は、ブロック理由を明確に報告する
6. この実行は非対話（-p）モード。確認を求めず、最後に
   完了またはブロックの報告を出力して終了すること"#;

fn build_prompt_full(root: &Path, issue: &Value) -> (String, Value) {
    let get = |k: &str| {
        issue
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    // intake スナップショット（intake-snapshot.mmd・manual-15667606）:
    // 課題本文は起票時に freeze した intakeSnapshot を使う。起票後の追記は
    // followUps へ入り run 中の課題には混入しない
    let snap = issue.get("intakeSnapshot").cloned().unwrap_or(json!({}));
    let snap_get = |k: &str| {
        snap.get(k)
            .and_then(|v| v.as_str())
            .map(String::from)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| get(k))
    };
    let key = get("key");
    let level = issue
        .get("verifyLevel")
        .map(|v| v.to_string())
        .unwrap_or_else(|| "1".into());
    let prio = issue
        .get("priority")
        .map(|v| v.to_string())
        .unwrap_or_else(|| "1".into());
    let tpl = knowledge::load_prompt(root, "dispatch").unwrap_or_else(|| DEFAULT_DISPATCH.into());
    let fields = [
        ("key", key.clone()),
        ("repo", get("repo")),
        ("verifyLevel", level),
        ("priority", prio),
        ("acceptance", get("acceptance")),
        ("title", snap_get("title")),
        ("body", snap_get("body")),
    ];
    let subst = |s: String| -> String {
        let mut out = s;
        for (k, v) in &fields {
            out = out.replace(&format!("{{{{{}}}}}", k), v);
        }
        out
    };
    let mut p = subst(tpl);
    // runbook（kind 別対処手順。runbook.mmd・README §5.4）:
    // prompts/runbook/ を <source>-<kind> → <kind> → <source> の順に探す
    let mut injected_runbook: Option<String> = None;
    if let Some((name, rb)) = knowledge::load_runbook(root, &get("source"), &get("kind")) {
        let rb: String = subst(rb.chars().take(4000).collect());
        p.push_str(&format!("\n\n## 対処手順（runbook: {}）\n\n{}", name, rb));
        injected_runbook = Some(name);
    }
    // knowledge/<slug>.md の知見を注入（課題 key 単位の蓄積）
    let kslug = knowledge::key_slug(&key);
    let mut injected_knowledge = false;
    let kfile = root.join("knowledge").join(format!("{}.md", kslug));
    if let Ok(kn) = std::fs::read_to_string(&kfile) {
        let kn: String = kn.chars().take(4000).collect();
        if !kn.trim().is_empty() {
            p.push_str("\n\n## 既知の知見（knowledge/）\n\n");
            p.push_str(&kn);
            injected_knowledge = true;
        }
    }
    // 類似過去事例（case-search.mmd・manual-a8c93d14）: assess で付記した
    // related と同じ決定的スコアで、前回の fix / reject / run 成否を注入する
    let (sec, case_keys) = cases::similar_section(root, issue, cases::TOP_N);
    if !sec.is_empty() {
        p.push_str("\n\n## 類似過去事例（knowledge/ 参照）\n\n");
        p.push_str(&sec);
    }
    // 失敗レッスン（lesson-loop.mmd・manual-29799d3e）: 同一課題の直近
    // レッスンと類似課題のレッスンを注入し、同種の失敗を繰り返さない
    let (lsec, lesson_ids) = lessons::inject_section(root, issue, &load_config(root));
    if !lsec.is_empty() {
        p.push_str("\n\n## 失敗レッスン（同種の失敗を繰り返さない）\n\n");
        p.push_str(&lsec);
    }
    // 起票後の追記（followUps）: 課題スナップショットには混入せず、
    // 未消費分のみ「フォローアップ入力」として別節で注入する
    // （run 中の追記は次回 run まで見せない。intake-snapshot.mmd）
    let fus: Vec<&Value> = issue
        .get("followUps")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter(|f| {
                    f.get("consumedAt")
                        .and_then(|v| v.as_str())
                        .map(|s| s.is_empty())
                        .unwrap_or(true)
                })
                .collect()
        })
        .unwrap_or_default();
    if !fus.is_empty() {
        p.push_str("\n\n## フォローアップ入力（起票後の追記）\n\n");
        for f in fus {
            let kind = f.get("kind").and_then(|v| v.as_str()).unwrap_or("note");
            let text = f.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let at = f.get("at").and_then(|v| v.as_str()).unwrap_or("");
            p.push_str(&format!("- [{}] {} （{}）\n", kind, text, at));
        }
    }
    // 事案（incidents.mmd・manual-343b5d37）: 所属事案のメンバー課題を
    // コンテキストとして注入し、関連症状を一体の事案として扱わせる
    if let Some(inc_id) = issue.get("incident").and_then(|v| v.as_str()) {
        let me = issue.get("key").and_then(|v| v.as_str()).unwrap_or("");
        let members: Vec<Value> = crate::incidents::load_incidents(root)
            .into_iter()
            .find(|i| i.get("id").and_then(|v| v.as_str()) == Some(inc_id))
            .and_then(|i| i.get("members").and_then(|m| m.as_array()).cloned())
            .unwrap_or_default();
        if !members.is_empty() {
            let issues = store::load_issues(root);
            p.push_str("\n\n## 同一事案の関連課題（detector 横断の症状群）\n\n");
            for mk in members.iter().filter_map(|k| k.as_str()) {
                if mk == me {
                    continue;
                }
                if let Some(m) = issues.iter().find(|i| {
                    i.get("key").and_then(|v| v.as_str()) == Some(mk)
                }) {
                    let t = m.get("title").and_then(|v| v.as_str()).unwrap_or("");
                    let s = m.get("status").and_then(|v| v.as_str()).unwrap_or("");
                    let src = m.get("source").and_then(|v| v.as_str()).unwrap_or("");
                    p.push_str(&format!("- {} [{} / {}] {}\n", mk, src, s, t));
                }
            }
        }
    }
    // 計画承認ゲート（manual-883d17ee）: 承認済み計画を持つ課題は
    // 計画を prompt に注入し、計画どおりの実装を指示する
    if let Some(plan) = issue
        .get("plan")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
    {
        p.push_str("\n\n## 承認済み実装計画（この計画に従って実装する）\n\n");
        p.push_str(plan);
    }
    // 再現ファースト検証（manual-612ec7e1・verify-levels.mmd）: L2+ は
    // 実装着手前に症状を再現する最小手順を evidence/<slug>/repro.md へ
    // 記録するよう prompt に指示する（verify が証跡を必須チェックする）
    let lvl_n = issue
        .get("verifyLevel")
        .map(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                .unwrap_or(1)
        })
        .unwrap_or(1);
    if lvl_n >= 2 && cfg_bool_default_true(&load_config(root), "reproFirst") {
        p.push_str(&format!(
            "\n\n## 再現ファースト検証（L2+ 必須）\n\n\
             実装に着手する前に、症状を再現する最小手順とベースライン結果\
             （未修正状態での再現結果）を {} へ記録する。\
             修正後は同じ手順で非再現となった結果を追記する。\
             verify はこの証跡を必須チェックする",
            root.join("evidence").join(&kslug).join("repro.md").display()
        ));
    }
    // 参照した知見を run の記録に残す（どの知見を使ったか追跡可能に）
    let refs = json!({
        "knowledge": if injected_knowledge { json!(kslug) } else { Value::Null },
        "runbook": injected_runbook.map(Value::String).unwrap_or(Value::Null),
        "cases": case_keys,
        "lessons": lesson_ids,
    });
    (p, refs)
}

#[cfg(test)]
fn build_prompt(root: &Path, issue: &Value) -> String {
    build_prompt_full(root, issue).0
}

// ---------- verify（L0〜L4） + push ----------

/// verifying の再開: verifyOk 未記録なら検証を（再）実行、記録済みなら
/// push 段階（帰属整理 → 承認判定 → push）へ進む。
/// traj は run 側で開いた trajectory（drain 再開経路は None → 既存分を開く）
async fn resume_verify(app: &Shared, key: &str, obs: Obs) {
    let root = &app.root;
    let Some(issue) = find_issue(root, key) else {
        return;
    };
    if issue.get("status").and_then(|v| v.as_str()) != Some(pipeline::ST_VERIFYING)
    {
        return;
    }
    // drain 再開経路（obs 空）は既存 run の観測ファイルを開いて追記を続ける
    let run_idx = trajectory::latest_run_idx(&issue);
    let obs = Obs {
        traj: obs.traj.or_else(|| {
            traj_existing(root, key).map(|t| Arc::new(Mutex::new(t)))
        }),
        trace: obs.trace.or_else(|| {
            run_idx.map(|i| {
                Trace::open(trajectory::run_dir(
                    &root.join("evidence").join(knowledge::key_slug(key)),
                    i,
                ))
            })
        }),
    };
    let traj = &obs.traj;
    let cfg = app.cfg().await;
    let repo = App::git_repo(
        &cfg,
        issue.get("repo").and_then(|v| v.as_str()).unwrap_or(""),
    )
    .cloned();
    let Some(repo_v) = repo else {
        send_back(app, key, "repo が config.repos に未登録", obs).await;
        return;
    };
    let ev_dir = root.join("evidence").join(knowledge::key_slug(key));
    let verified = issue
        .get("verifyOk")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !verified {
        let level = issue
            .get("verifyLevel")
            .and_then(|v| v.as_u64())
            .unwrap_or(1);
        tstep(traj, "action", "verify.start", json!({"level": level}));
        // EVALUATOR span（verify L0-L4 全体を1spanで括る）
        let vspan = obs.trace.as_ref().map(|t| {
            t.start(
                "eval.verify",
                trace::KIND_EVALUATOR,
                Map::from_iter([("gen_ai.eval.level".into(), json!(level))]),
            )
        });
        let (ok, note) = run_checks(app, &issue, &repo_v, &ev_dir).await;
        if let (Some(t), Some(s)) = (&obs.trace, &vspan) {
            t.end(
                s,
                ok,
                &note,
                Map::from_iter([("gen_ai.eval.ok".into(), json!(ok))]),
            );
        }
        audit::record(
            root,
            key,
            "system",
            "verify",
            run_idx,
            Map::from_iter([
                ("level".into(), json!(level)),
                ("ok".into(), json!(ok)),
                ("note".into(), json!(note)),
            ]),
        );
        tstep(
            traj,
            "action",
            "verify.end",
            json!({"level": level, "ok": ok, "note": note}),
        );
        if !ok {
            tstep(traj, "thought", "verify.ng", json!({"note": note}));
            send_back(app, key, "verify 不合格（evidence の verify.log を参照）", obs)
                .await;
            return;
        }
        let _ = pipeline::set_field(root, key, "verifyOk", json!(true));
    }
    let issue = find_issue(root, key).unwrap_or(issue);
    finalize(app, key, &issue, &repo_v, obs).await;
}

/// NG 差し戻し: verifying → retry（verifyBacks++）→ queued or failed
/// （verifyBackLimit 超過で failed）
async fn send_back(app: &Shared, key: &str, reason: &str, obs: Obs) {
    let traj = &obs.traj;
    let root = &app.root;
    let _ = pipeline::set_field(root, key, "verifyNote", json!(reason));
    if pipeline::request_transition(root, key, pipeline::ST_RETRY).is_err() {
        return;
    }
    let it = find_issue(root, key).unwrap_or_default();
    // 失敗レッスン（lesson-loop.mmd・manual-29799d3e）: verify NG の
    // レッスンを lessons.json へ保存し、差し戻し後の再 run へ注入する
    {
        let run_idx = it
            .get("runs")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0)
            .saturating_sub(1);
        lessons::record_failure(root, &it, run_idx, reason);
        // ACE カウンタ（manual-bcf981dd）: verify NG の run に注入された
        // レッスンは harmful++
        lessons::record_outcome(root, &it, false, &app.cfg().await);
    }
    let backs = it
        .get("verifyBacks")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let limit = cfg_u64(&app.cfg().await, "verifyBackLimit", 3);
    audit::record(
        root,
        key,
        "system",
        "send_back",
        trajectory::latest_run_idx(&it),
        Map::from_iter([
            ("reason".into(), json!(reason)),
            ("verifyBacks".into(), json!(backs)),
            ("limit".into(), json!(limit)),
        ]),
    );
    if let Some(t) = &obs.trace {
        t.event(
            "send_back",
            Map::from_iter([
                ("reason".into(), json!(reason)),
                ("verifyBacks".into(), json!(backs)),
            ]),
        );
    }
    // 段階的修復（manual-139ec5ce・EscalatingRemediations 型）: config
    // `remediations` を対処手段の順序リスト（[{name, backs, timeoutMinutes?}]）
    // として順に試す。各手段は verify NG backs 回または timeoutMinutes で
    // 次の重い手段へ昇格する（既定: retry → human）。全手段失敗で人ゲート
    let remedies: Vec<Value> = {
        let cfg = app.cfg().await;
        cfg.get("remediations")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_else(|| {
                vec![json!({"name": "retry", "backs": limit}), json!({"name": "human"})]
            })
    };
    let stage_idx = it
        .get("remediationStage")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    let stage = remedies.get(stage_idx).or_else(|| remedies.last());
    let stage_name = stage
        .and_then(|s| s.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("retry");
    // 現在手段の backs を差し戻し上限として扱う（無指定は verifyBackLimit）
    let limit = stage
        .and_then(|s| s.get("backs"))
        .and_then(|v| v.as_u64())
        .unwrap_or(limit);
    if it.get("remediationStage").is_none() {
        let _ = pipeline::set_field(root, key, "remediationStage", json!(stage_idx));
        let _ = pipeline::set_field(root, key, "remediation", json!(stage_name));
        let _ = pipeline::set_field(
            root,
            key,
            "remediationSince",
            json!(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        );
    }
    let timed_out = stage
        .and_then(|s| s.get("timeoutMinutes"))
        .and_then(|v| v.as_f64())
        .is_some_and(|mins| {
            it.get("remediationSince")
                .and_then(|v| v.as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .is_some_and(|since| {
                    (chrono::Utc::now() - since.with_timezone(&chrono::Utc)).num_seconds()
                        as f64
                        > mins * 60.0
                })
        });
    if timed_out || backs > limit {
        // 昇格: 次の対処手段へ。リストを尽きれば human ゲート（failed）
        let next = remedies.get(stage_idx + 1);
        let next_name = next
            .and_then(|s| s.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("human");
        let why = if timed_out { "timeout" } else { "backs 上限" };
        let _ = pipeline::set_field(root, key, "remediationStage", json!(stage_idx + 1));
        let _ = pipeline::set_field(root, key, "verifyBacks", json!(0));
        let _ = pipeline::set_field(root, key, "remediation", json!(next_name));
        let _ = pipeline::set_field(
            root,
            key,
            "remediationSince",
            json!(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        );
        conlog(
            "runner",
            "warn",
            &format!(
                "{}: 対処手段昇格 {} → {}（{}）",
                key, stage_name, next_name, why
            ),
        );
        if next_name == "replan" {
            // 計画からやり直し（より重い手段）: 計画・批評の成果を捨て、
            // planGate 経由で再 dispatch → planning → 承認ゲートを再通過する
            for f in [
                "plan",
                "planApproved",
                "planNote",
                "criticOk",
                "criticNote",
                "specNote",
            ] {
                let _ = pipeline::set_field(root, key, f, Value::Null);
            }
            let _ = pipeline::set_field(root, key, "planGate", json!(true));
        }
        if next_name == "human" || next.is_none() {
            // verify NG 確定の自動クリーン復帰（manual-c11945d2）: rollback
            // 遷移で帰属コミットのみ git revert してから failed へ
            let _ = pipeline::request_transition(root, key, pipeline::ST_ROLLBACK);
            rollback_issue_commits(app, key, &it).await;
            let _ = pipeline::request_transition(root, key, pipeline::ST_FAILED);
            conlog(
                "runner",
                "err",
                &format!("{}: 全対処手段失敗 → rollback 済み failed（人ゲートへ差し戻し）", key),
            );
            report_issue(
                app,
                &it,
                &format!(
                    "[kasugai-agent] 自動処理が失敗しました（対処手段 {} が尽きました）: {}",
                    stage_name, reason
                ),
            )
            .await;
        } else {
            let _ = pipeline::request_transition(root, key, pipeline::ST_QUEUED);
            report_issue(
                app,
                &it,
                &format!(
                    "[kasugai-agent] 対処手段を昇格しました（{}→{}）: {}",
                    stage_name, next_name, reason
                ),
            )
            .await;
        }
        app.sse("issues", &json!({}));
        return;
    }
    if backs > limit {
        let _ = pipeline::request_transition(root, key, pipeline::ST_FAILED);
        tstep(traj, "state", "failed", json!({"verifyBacks": backs}));
        tfin(
            traj,
            "limit",
            &format!("差し戻し上限超過（{}回）: {}", backs, reason),
        );
        if let Some(t) = &obs.trace {
            t.finish_root(false, "verify 差し戻し上限超過");
        }
        conlog(
            "runner",
            "err",
            &format!("{}: 差し戻し上限（{}）超過 → failed", key, limit),
        );
        report_issue(
            app,
            &it,
            &format!(
                "[kasugai-agent] 自動処理が失敗しました（差し戻し {} 回超過）: {}",
                backs, reason
            ),
        )
        .await;
    } else {
        let _ = pipeline::request_transition(root, key, pipeline::ST_QUEUED);
        tstep(
            traj,
            "state",
            "retry",
            json!({"verifyBacks": backs, "note": reason}),
        );
        tfin(traj, "verify_ng", reason);
        if let Some(t) = &obs.trace {
            t.finish_root(false, reason);
        }
        // 指数バックオフ（manual-6c3ed946）: verify NG の再試行は
        // verifyBackoffMinutes × 2^(backs-1)（上限 verifyBackoffMaxMinutes）
        // だけ待機させる。retryAfter 経過前の再 dispatch は can_dispatch が止める
        let base = cfg_f64(&app.cfg().await, "verifyBackoffMinutes", 5.0);
        let cap = cfg_f64(&app.cfg().await, "verifyBackoffMaxMinutes", 60.0);
        let wait = (base * 2f64.powi(backs as i32 - 1)).min(cap);
        if wait > 0.0 {
            let until = chrono::Utc::now() + chrono::Duration::seconds((wait * 60.0) as i64);
            let _ = pipeline::set_field(
                root,
                key,
                "retryAfter",
                json!(until.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
            );
        }
        conlog(
            "runner",
            "sys",
            &format!(
                "{}: verify NG → 差し戻し ({}/{}・再試行は {:.0}分後まで待機): {}",
                key, backs, limit, wait, reason
            ),
        );
    }
    app.sse("issues", &json!({}));
}

/// verify 合格後の仕上げ: 帰属整理（非本課題コミットを revert で除去）
/// → requirePushApproval/pushApproved 判定 → git push → done
async fn finalize(
    app: &Shared,
    key: &str,
    issue: &Value,
    repo: &Value,
    obs: Obs,
) {
    let traj = &obs.traj;
    let root = &app.root;
    // ACE カウンタ（manual-bcf981dd）: verify 合格まで到達した run に
    // 注入されたレッスンは helpful++（finalize は verify 合格後のみ呼ばれる）
    lessons::record_outcome(root, issue, true, &app.cfg().await);
    let repo_path = PathBuf::from(
        repo.get("path").and_then(|v| v.as_str()).unwrap_or(""),
    );
    let cspan = obs
        .trace
        .as_ref()
        .map(|t| t.start("tool.git.clean", trace::KIND_TOOL, Map::new()));
    let clean_res = clean_unpushed(&repo_path, key).await;
    match &clean_res {
        Ok(removed) => {
            for s in removed {
                conlog(
                    "runner",
                    "sys",
                    &format!("{}: 混入コミットを revert 除去: {}", key, s),
                );
            }
            if let (Some(t), Some(s)) = (&obs.trace, &cspan) {
                t.end(s, true, "", Map::new());
            }
            tstep(
                traj,
                "action",
                "git.clean",
                json!({"reverted": removed.len(), "commits": removed}),
            );
        }
        Err(e) => {
            if let (Some(t), Some(s)) = (&obs.trace, &cspan) {
                t.end(s, false, e, Map::new());
            }
            send_back(app, key, &format!("帰属整理に失敗: {}", e), obs).await;
            return;
        }
    }
    // 独立 diff レビュー（manual-475831d2・verify-levels.mmd）: push 前に
    // 生成系と独立したコンテキストで未push差分を批評する。Severe 検出は
    // verify NG と同じ差し戻し経路へ
    if cfg_bool_default_true(&app.cfg().await, "diffReview")
        && issue.get("diffReviewOk").and_then(|v| v.as_bool()) != Some(true)
    {
        if let Err(note) = diff_review(app, key, issue, repo).await {
            send_back(app, key, &note, obs).await;
            return;
        }
    }
    let need_approval = repo
        .get("requirePushApproval")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let approved = issue
        .get("pushApproved")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if need_approval && !approved {
        conlog(
            "runner",
            "sys",
            &format!("{}: verify 合格。push は承認待ち（requirePushApproval）", key),
        );
        tstep(traj, "state", "push_pending", json!({}));
        if let Some(t) = &obs.trace {
            t.event("push_pending", Map::new());
        }
        audit::record(
            root,
            key,
            "system",
            "push_pending",
            trajectory::latest_run_idx(issue),
            Map::from_iter([("reason".into(), json!("requirePushApproval"))]),
        );
        return; // drain が pushApproved を拾って再開する
    }
    let pspan = obs
        .trace
        .as_ref()
        .map(|t| t.start("tool.push", trace::KIND_TOOL, Map::new()));
    match git_push(&repo_path).await {
        Ok(_) => {
            if let (Some(t), Some(s)) = (&obs.trace, &pspan) {
                t.end(s, true, "", Map::new());
            }
            tstep(traj, "action", "push", json!({"ok": true}));
            audit::record(
                root,
                key,
                "system",
                "push",
                trajectory::latest_run_idx(issue),
                Map::from_iter([("ok".into(), json!(true))]),
            );
            let _ = pipeline::set_field(root, key, "pushedAt", json!(now_iso()));
            if pipeline::request_transition(root, key, pipeline::ST_DONE).is_ok() {
                tstep(traj, "state", "done", json!({}));
                tfin(traj, "success", "verify 合格・push 済み");
                if let Some(t) = &obs.trace {
                    t.event("done", Map::new());
                    t.finish_root(true, "verify 合格・push 済み");
                }
                audit::record(
                    root,
                    key,
                    "system",
                    "done",
                    trajectory::latest_run_idx(issue),
                    Map::new(),
                );
                conlog(
                    "runner",
                    "sys",
                    &format!("{}: verify 合格・push 済み → done", key),
                );
                report_issue(
                    app,
                    issue,
                    "[kasugai-agent] 対応完了: 検証合格・コミットを push しました",
                )
                .await;
            }
        }
        Err(e) => {
            conlog("runner", "err", &format!("{}: push 失敗: {}", key, e));
            if let (Some(t), Some(s)) = (&obs.trace, &pspan) {
                t.end(s, false, &e, Map::new());
            }
            tstep(
                traj,
                "action",
                "push",
                json!({"ok": false, "error": e}),
            );
            audit::record(
                root,
                key,
                "system",
                "push",
                trajectory::latest_run_idx(issue),
                Map::from_iter([
                    ("ok".into(), json!(false)),
                    ("error".into(), json!(e)),
                ]),
            );
            let _ = pipeline::set_field(
                root,
                key,
                "verifyNote",
                json!(format!("push 失敗: {}", e)),
            );
            // verifying のまま残し、drain が再試行する
        }
    }
    app.sse("issues", &json!({}));
}

/// gh issue への結果報告（slug+number がある課題のみ。失敗は conlog のみ）
async fn report_issue(app: &Shared, issue: &Value, body: &str) {
    let cfg = app.cfg().await;
    let Some(repo) = App::git_repo(
        &cfg,
        issue.get("repo").and_then(|v| v.as_str()).unwrap_or(""),
    ) else {
        return;
    };
    let slug = issue
        .get("slug")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| repo_slug(Some(repo)))
        .unwrap_or_default();
    let Some(number) = issue.get("number").and_then(|v| v.as_u64()) else {
        return;
    };
    if slug.is_empty() {
        return;
    }
    let dir = Path::new(repo.get("path").and_then(|v| v.as_str()).unwrap_or(""));
    if let Err(e) = github::issue_comment(dir, &slug, number, body).await {
        conlog(
            "runner",
            "err",
            &format!("issue #{} への報告失敗: {}", number, e),
        );
    }
}

// ---- git 補助 ----

/// git 実行（120s timeout）。戻り値 (code, stdout+stderr)
async fn git(dir: &Path, args: &[&str]) -> (i32, String) {
    let mut c = tokio::process::Command::new("git");
    c.args(args)
        .current_dir(dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    hide_console_tokio(&mut c);
    let out = tokio::time::timeout(Duration::from_secs(120), c.output()).await;
    match out {
        Ok(Ok(o)) => (
            o.status.code().unwrap_or(-1),
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            )
            .trim()
            .to_string(),
        ),
        Ok(Err(e)) => (-1, e.to_string()),
        Err(_) => (-1, "timeout".to_string()),
    }
}

/// 任意コマンド実行（build/test 試行用。600s timeout）
async fn run_cmd(dir: &Path, argv: &[String], timeout_secs: u64) -> (i32, String) {
    let Some((prog, args)) = argv.split_first() else {
        return (-1, "empty command".into());
    };
    let mut c = tokio::process::Command::new(prog);
    c.args(args)
        .current_dir(dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    hide_console_tokio(&mut c);
    let out = tokio::time::timeout(Duration::from_secs(timeout_secs), c.output()).await;
    match out {
        Ok(Ok(o)) => (
            o.status.code().unwrap_or(-1),
            format!(
                "$ {}\n{}{}",
                argv.join(" "),
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            )
            .trim()
            .to_string(),
        ),
        Ok(Err(e)) => (-1, format!("$ {} → {}", argv.join(" "), e)),
        Err(_) => (-1, format!("$ {} → timeout", argv.join(" "))),
    }
}

struct Unpushed {
    sha: String,
    subject: String,
    /// コミット本文の `Issue: <key>` トレーラ値（帰属先課題）
    issue: Option<String>,
}

fn parse_issue_trailer(body: &str) -> Option<String> {
    crate::util::issue_trailer(body)
}

/// 独立 diff レビュー（diffReview=on・manual-475831d2）: 未push範囲の差分を
/// 生成系と独立したコンテキストの批評パスでレビューし、Severe
/// （confidence>=diffReviewMinConfidence）の finding があれば Err を返す
/// （呼び出し側は send_back で差し戻す）。レビュー結果は課題の
/// diffReview フィールドへ残す。合格時は diffReviewOk を立て、
/// push リトライ経路での再批評を抑止する
async fn diff_review(app: &Shared, key: &str, issue: &Value, repo: &Value) -> Result<(), String> {
    let root = &app.root;
    let cfg = app.cfg().await;
    let repo_path = PathBuf::from(repo.get("path").and_then(|v| v.as_str()).unwrap_or(""));
    let Some(range) = unpushed_range(&repo_path).await else {
        // clean_unpushed で upstream 不明は既に拒否済み（到達しない想定）
        return Ok(());
    };
    let (c, diff) = git(&repo_path, &["diff", &range]).await;
    if c != 0 || diff.trim().is_empty() {
        return Ok(());
    }
    let Some((exe, args)) = resolve_agent(&cfg) else {
        conlog("runner", "sys", &format!("{}: diff レビュー実行体無し（skip）", key));
        return Ok(());
    };
    // 巨大差分は頭打ち（prompt 投入上限。末尾側の情報が失われても
    // 差し戻し判定の精度には影響しにくい）
    let diff: String = diff.chars().take(30000).collect();
    let prompt = diff_review_prompt(root, issue, &diff);
    let timeout = Duration::from_secs_f64(cfg_f64(&cfg, "diffReviewTimeoutMinutes", 15.0) * 60.0);
    let review = match spawn_capture(&exe, &args, &repo_path, &prompt, timeout).await {
        Ok(out) => crate::verify::extract_review(&out),
        Err(e) => {
            conlog("runner", "err", &format!("{}: diff レビュー実行失敗: {}", key, e));
            None
        }
    };
    let Some(review) = review else {
        // 批評結果が取れない場合はブロックしない（verify 合格・人ゲートへ委ねる）
        let _ = pipeline::set_field(
            root,
            key,
            "diffReview",
            json!({"at": now_iso(), "error": "レビュー結果を取得できず"}),
        );
        return Ok(());
    };
    let min_conf = cfg_f64(&cfg, "diffReviewMinConfidence", 0.7);
    let severe = crate::verify::severe_findings(&review, min_conf);
    let n = review.findings.len();
    let _ = pipeline::set_field(
        root,
        key,
        "diffReview",
        json!({"at": now_iso(), "findings": review.findings}),
    );
    if severe.is_empty() {
        let _ = pipeline::set_field(root, key, "diffReviewOk", json!(true));
        conlog(
            "runner",
            "sys",
            &format!("{}: diff レビュー合格（findings {} 件）", key, n),
        );
        return Ok(());
    }
    let notes: Vec<String> = severe.iter().map(|f| f.note.clone()).collect();
    Err(format!(
        "diff レビューで Severe 検出（{}/{} 件）: {}",
        severe.len(),
        n,
        notes.join(" / ")
    ))
}

/// 未push範囲（`@{u}..HEAD`。upstream 未設定は `origin/HEAD..HEAD`、
/// どちらも無ければ None＝未pushが判定不能。この場合全履歴を対象にすると
/// 帰属整理で過去コミットまで revert してしまうため呼び出し側で拒否する）
async fn unpushed_range(repo: &Path) -> Option<String> {
    let (c, up) = git(
        repo,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{u}",
        ],
    )
    .await;
    if c == 0 && !up.trim().is_empty() {
        return Some(format!("{}..HEAD", up.trim()));
    }
    let (c2, d) = git(
        repo,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .await;
    if c2 == 0 && !d.trim().is_empty() {
        Some(format!("{}..HEAD", d.trim()))
    } else {
        None
    }
}

/// 未pushコミット（新しい順、直近50件で頭打ち）
async fn unpushed_commits(repo: &Path) -> Vec<Unpushed> {
    let Some(range) = unpushed_range(repo).await else {
        return vec![];
    };
    let (c, out) = git(
        repo,
        &["log", "--max-count=50", "--format=%H%x00%s%x00%b%x1e", &range],
    )
    .await;
    if c != 0 {
        return vec![];
    }
    out.split('\x1e')
        .filter_map(|rec| {
            let mut it = rec.trim_start_matches('\n').split('\x00');
            let sha = it.next()?.trim().to_string();
            if sha.is_empty() {
                return None;
            }
            let subject = it.next().unwrap_or("").to_string();
            let body = it.next().unwrap_or("");
            // トレーラは本文が正規だが、件名へ書かれた場合も拾う
            Some(Unpushed {
                sha,
                issue: parse_issue_trailer(body)
                    .or_else(|| parse_issue_trailer(&subject)),
                subject,
            })
        })
        .collect()
}

/// verify NG 確定時の自動クリーン復帰（manual-c11945d2・issue-lifecycle.mmd）:
/// 打ち切り時にその課題に帰属する（`Issue: <key>` トレーラ付き）未push
/// コミットだけを git revert で除去し、base を健全状態へ戻す。
/// git reset は使わない（規約。他課題コミットを破壊する）。
/// push 前の帰属整理と同じく新しい順に revert し、除去コミットにも
/// 本課題のトレーラを付ける（後続の帰属整理で revert 自体が落とされない）。
async fn rollback_issue_commits(app: &Shared, key: &str, it: &Value) {
    let root = &app.root;
    let repo = App::git_repo(
        &app.cfg().await,
        it.get("repo").and_then(|v| v.as_str()).unwrap_or(""),
    )
    .cloned();
    let Some(repo_v) = repo else {
        conlog(
            "runner",
            "warn",
            &format!("{}: rollback スキップ（repo が config.repos に未登録）", key),
        );
        return;
    };
    let repo_path = PathBuf::from(
        repo_v
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
    );
    if !repo_path.is_dir() || unpushed_range(&repo_path).await.is_none() {
        conlog(
            "runner",
            "warn",
            &format!("{}: rollback スキップ（repo/未push範囲を特定できない）", key),
        );
        return;
    }
    let mut removed = 0usize;
    // 帰属コミットのみを新しい順に revert。他課題・帰属不明は触らない
    for c in unpushed_commits(&repo_path).await {
        if c.issue.as_deref() != Some(key) {
            continue;
        }
        let (rc, rout) = git(&repo_path, &["revert", "--no-commit", &c.sha]).await;
        if rc != 0 {
            let _ = git(&repo_path, &["revert", "--abort"]).await;
            conlog(
                "runner",
                "err",
                &format!(
                    "{}: rollback revert {} 失敗: {}",
                    key,
                    &c.sha[..8.min(c.sha.len())],
                    rout
                ),
            );
            break;
        }
        let msg = format!(
            "Revert \"{}\"\n\nThis reverts commit {}.\n\nIssue: {}",
            c.subject, c.sha, key
        );
        let (cc, cout) = git(&repo_path, &["commit", "-m", &msg]).await;
        if cc != 0 {
            let _ = git(&repo_path, &["revert", "--abort"]).await;
            conlog(
                "runner",
                "err",
                &format!("{}: rollback revert コミット失敗: {}", key, cout),
            );
            break;
        }
        removed += 1;
        conlog(
            "runner",
            "sys",
            &format!(
                "{}: rollback で {}（{}）を revert",
                key,
                &c.sha[..8.min(c.sha.len())],
                c.subject
            ),
        );
    }
    // revert 後も tracked 変更が残るなら run の残滓として破棄する
    // （課題起因の変更を残さない。untracked は証跡等他資産を潰さないため触らない）
    let (_, st) = git(&repo_path, &["status", "--porcelain"]).await;
    if st
        .lines()
        .any(|l| matches!(l.chars().nth(1), Some('M') | Some('D')))
    {
        let _ = git(&repo_path, &["checkout", "--", "."]).await;
    }
    let _ = pipeline::set_field(root, key, "rollbackAt", json!(now_iso()));
    let _ = pipeline::set_field(root, key, "rollbackCount", json!(removed));
    conlog(
        "runner",
        "sys",
        &format!("{}: rollback 完了（{} 件の帰属コミットを除去）", key, removed),
    );
}
/// 帰属整理: `Issue: <key>` トレーラを持たない未pushコミットを
/// git revert で除去する（reset 禁止。新しい順に revert し、
/// 除去コミット自体にも本課題のトレーラを付ける）。
/// 他課題・closed・devin:rejected・帰属不明の混入もここで落とす
/// （検証済みでないコミットは push しない規則）。
async fn clean_unpushed(repo: &Path, key: &str) -> Result<Vec<String>, String> {
    // upstream 基準が無いと未push範囲が全履歴になり revert が履歴を壊すため拒否
    if unpushed_range(repo).await.is_none() {
        return Err("upstream/origin 未設定のため未push範囲を特定できない".into());
    }
    let mut removed = Vec::new();
    for c in unpushed_commits(repo).await {
        if c.issue.as_deref() == Some(key) {
            continue;
        }
        let (rc, rout) = git(repo, &["revert", "--no-commit", &c.sha]).await;
        if rc != 0 {
            let _ = git(repo, &["revert", "--abort"]).await;
            return Err(format!("revert {} 失敗: {}", &c.sha[..8.min(c.sha.len())], rout));
        }
        let msg = format!(
            "Revert \"{}\"\n\nThis reverts commit {}.\n\nIssue: {}",
            c.subject, c.sha, key
        );
        let (cc, cout) = git(repo, &["commit", "-m", &msg]).await;
        if cc != 0 {
            let _ = git(repo, &["revert", "--abort"]).await;
            return Err(format!("revert コミット失敗: {}", cout));
        }
        removed.push(format!("{} {}", &c.sha[..8.min(c.sha.len())], c.subject));
    }
    Ok(removed)
}

/// git push（upstream 未設定なら -u origin <branch> で再試行）
async fn git_push(repo: &Path) -> Result<String, String> {
    let (c, out) = git(repo, &["push"]).await;
    if c == 0 {
        return Ok(out);
    }
    let (uc, _) = git(
        repo,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{u}",
        ],
    )
    .await;
    if uc == 0 {
        return Err(out);
    }
    let (_, br) = git(repo, &["branch", "--show-current"]).await;
    let br = br.trim().to_string();
    if br.is_empty() {
        return Err(out);
    }
    let (_, rem) = git(repo, &["remote"]).await;
    let remote = rem
        .lines()
        .map(|l| l.trim())
        .find(|l| *l == "origin")
        .or_else(|| rem.lines().map(|l| l.trim()).find(|l| !l.is_empty()))
        .map(String::from);
    let Some(remote) = remote else {
        return Err(out);
    };
    let (c2, out2) = git(repo, &["push", "-u", &remote, &br]).await;
    if c2 == 0 {
        Ok(out2)
    } else {
        Err(out2)
    }
}

/// build/test 試行コマンド列: repo.verifyCmds があればそれ、
/// 無ければ repo 構成から自動検出（sync ツール・cargo build）
fn detect_checks(repo: &Value, repo_path: &Path) -> Vec<Vec<String>> {
    if let Some(arr) = repo.get("verifyCmds").and_then(|v| v.as_array()) {
        let cmds: Vec<Vec<String>> = arr
            .iter()
            .filter_map(|c| match c {
                Value::String(s) => {
                    #[cfg(windows)]
                    {
                        Some(vec!["cmd".into(), "/C".into(), s.clone()])
                    }
                    #[cfg(not(windows))]
                    {
                        Some(vec!["sh".into(), "-c".into(), s.clone()])
                    }
                }
                Value::Array(a) => {
                    let v: Vec<String> =
                        a.iter().filter_map(|x| x.as_str().map(String::from)).collect();
                    (!v.is_empty()).then_some(v)
                }
                _ => None,
            })
            .collect();
        if !cmds.is_empty() {
            return cmds;
        }
    }
    let mut v: Vec<Vec<String>> = vec![];
    let s = |p: &str| repo_path.join(p).is_file();
    if s("tools/sync-diagrams.mjs") {
        v.push(vec![
            "node".into(),
            "tools/sync-diagrams.mjs".into(),
            "--check".into(),
        ]);
    }
    if s("tools/sync-docs.mjs") {
        v.push(vec![
            "node".into(),
            "tools/sync-docs.mjs".into(),
            "--check".into(),
        ]);
    }
    if s("server/Cargo.toml") {
        v.push(vec![
            "cargo".into(),
            "build".into(),
            "--manifest-path".into(),
            "server/Cargo.toml".into(),
        ]);
    } else if s("Cargo.toml") {
        v.push(vec!["cargo".into(), "build".into()]);
    }
    v
}

/// verifyLevel 別の検証を実行し証跡を evidence/<slug>/ へ残す。
/// 戻り値 (合格?, 不合格理由)
async fn run_checks(app: &Shared, issue: &Value, repo: &Value, ev_dir: &Path) -> (bool, String) {
    let root = &app.root;
    let key = key_of(issue);
    let repo_path = PathBuf::from(
        repo.get("path").and_then(|v| v.as_str()).unwrap_or(""),
    );
    let level = issue
        .get("verifyLevel")
        .and_then(|v| v.as_u64())
        .unwrap_or(1);
    let _ = std::fs::create_dir_all(ev_dir);
    let mut log = String::new();
    let mut ok = true;
    let mut note = String::new();

    // ---- L0: 差分（Issue:<key> トレーラ付き実装コミットの存在） ----
    let unpushed = unpushed_commits(&repo_path).await;
    log.push_str(&format!("== unpushed ({} commits)\n", unpushed.len()));
    for c in &unpushed {
        let tag = match c.issue.as_deref() {
            Some(k) if k == key => "[本課題]",
            Some(_) => "[他課題]",
            None => "[帰属不明]",
        };
        log.push_str(&format!(
            "  {} {} {}\n",
            &c.sha[..8.min(c.sha.len())],
            c.subject,
            tag
        ));
    }
    let (_, status_out) = git(&repo_path, &["status", "--porcelain"]).await;
    let _ = std::fs::write(
        ev_dir.join("status.txt"),
        format!("{}\n", status_out),
    );
    if !unpushed.iter().any(|c| c.issue.as_deref() == Some(key.as_str())) {
        ok = false;
        note = "Issue トレーラ付きの実装コミットが見つからない".into();
        log.push_str(&format!("NG: {}\n", note));
    }

    // ---- L1: build/test 試行 ----
    let mut ran_checks: Vec<Vec<String>> = vec![];
    if ok && level >= 1 {
        let checks = detect_checks(repo, &repo_path);
        if checks.is_empty() {
            log.push_str("== L1: 自動検出チェックなし\n");
        }
        for argv in checks {
            ran_checks.push(argv.clone());
            let (code, out) = run_cmd(&repo_path, &argv, 600).await;
            log.push_str(&format!("\n== L1 {}\n{}\n", argv.join(" "), out));
            if code != 0 {
                ok = false;
                note = format!("チェック失敗: {}", argv.join(" "));
                break;
            }
        }
    }

    // ---- L2+: 実機起動（実機リソースを locks で排他取得） ----
    if ok && level >= 2 {
        // 再現ファースト証跡（manual-612ec7e1・verify-levels.mmd）: L2+ は
        // 実装前の最小再現手順・ベースラインを記した repro.md が必須
        let has_repro = std::fs::metadata(ev_dir.join("repro.md"))
            .map(|m| m.len() > 0)
            .unwrap_or(false);
        if !has_repro && cfg_bool_default_true(&app.cfg().await, "reproFirst") {
            ok = false;
            note = "L2+ は repro.md（最小再現手順＋修正前ベースライン）の証跡が必須".into();
            log.push_str(&format!("NG: {}\n", note));
        }
        let mut got = locks::acquire(root, "machine");
        let mut waited = 0u64;
        while !got && waited < 120 {
            tokio::time::sleep(Duration::from_secs(2)).await;
            got = locks::acquire(root, "machine");
            waited += 2;
        }
        if !got {
            log.push_str("== L2: machine ロック取得断念（他 run が保持中）→ 排他なしで続行\n");
        }
        let r = launch_probe(repo, &repo_path, &mut log).await;
        if got {
            locks::release(root, "machine");
        }
        if !r {
            ok = false;
            note = "実機起動・応答確認に失敗".into();
        }
    }

    let _ = std::fs::write(ev_dir.join("verify.log"), &log);
    let _ = std::fs::write(
        ev_dir.join("verdict.json"),
        serde_json::to_string_pretty(&json!({
            "level": level, "ok": ok, "note": note, "at": now_iso(),
            // verify リプレイ（eval-set.mmd・manual-a2de7417）用に
            // L1 で実行したチェックコマンドを残す
            "checks": ran_checks,
        }))
        .unwrap_or_default(),
    );
    conlog(
        "runner",
        if ok { "sys" } else { "err" },
        &format!(
            "{}: verify L{} {} {}",
            key,
            level,
            if ok { "合格" } else { "NG" },
            note
        ),
    );
    (ok, note)
}

/// L2+ の実機起動確認（最小実装）: repo.verifyExe を起動し、
/// verifyPort 指定なら TCP 応答を、無指定なら生存を確認して kill する。
/// verifyExe 未設定なら L1 チェック結果で代用（画面駆動は後続課題）。
async fn launch_probe(repo: &Value, repo_path: &Path, log: &mut String) -> bool {
    let exe_s = repo.get("verifyExe").and_then(|v| v.as_str()).unwrap_or("");
    if exe_s.is_empty() {
        log.push_str("== L2: verifyExe 未設定 — 実機起動はスキップ（L1 で代用）\n");
        return true;
    }
    let exe = crate::util::resolve(&repo_path.to_string_lossy(), exe_s);
    if !Path::new(&exe).is_file() {
        log.push_str(&format!("== L2 NG: verifyExe が存在しない: {}\n", exe));
        return false;
    }
    let args: Vec<String> = repo
        .get("verifyArgs")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let mut c = tokio::process::Command::new(&exe);
    c.args(&args)
        .current_dir(repo_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    hide_console_tokio(&mut c);
    let mut child = match c.spawn() {
        Ok(c) => c,
        Err(e) => {
            log.push_str(&format!("== L2 NG: 起動失敗 {}: {}\n", exe, e));
            return false;
        }
    };
    log.push_str(&format!("== L2: {} を起動\n", exe));
    let port = repo
        .get("verifyPort")
        .and_then(|v| v.as_u64())
        .map(|v| v as u16);
    let wait_secs = repo
        .get("verifyWaitSec")
        .and_then(|v| v.as_u64())
        .unwrap_or(10)
        .max(1);
    let alive = match port {
        Some(p) => {
            // ポート応答確認（最大 wait_secs+20 秒）
            let mut responded = false;
            for _ in 0..(wait_secs + 20) {
                if crate::launch::port_alive(p).await {
                    responded = true;
                    break;
                }
                if let Ok(Some(_)) = child.try_wait() {
                    break; // 早期終了
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            log.push_str(&format!(
                "== L2: :{} 応答{}\n",
                p,
                if responded { "あり" } else { "なし" }
            ));
            responded
        }
        None => {
            tokio::time::sleep(Duration::from_secs(wait_secs)).await;
            let a = matches!(child.try_wait(), Ok(None));
            log.push_str(&format!(
                "== L2: {}秒後の生存確認{}\n",
                wait_secs,
                if a { "OK" } else { "（早期終了）" }
            ));
            a
        }
    };
    let _ = child.kill().await;
    alive
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicI64};
    use std::time::Instant;
    use tokio::sync::{broadcast, mpsc};

    fn temp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "kasugai-runner-test-{}-{}",
            tag,
            crate::util::rand_hex(4)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn std_git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git spawn");
        assert!(out.status.success(), "git {:?} → {:?}", args, out);
    }

    /// temp_root 配下に git repo（初期 commit 済み。remote=true なら bare
    /// remote を origin として push -u 済み）を作る
    fn make_repo(root: &Path, name: &str, remote: bool) -> PathBuf {
        let repo = root.join(name);
        std::fs::create_dir_all(&repo).unwrap();
        std_git(&repo, &["init"]);
        std_git(&repo, &["config", "user.email", "t@t"]);
        std_git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("README.md"), "x\n").unwrap();
        std_git(&repo, &["add", "-A"]);
        std_git(&repo, &["commit", "-m", "init"]);
        if remote {
            let bare = root.join(format!("{}.git", name));
            std_git(&repo, &["init", "--bare", &bare.to_string_lossy()]);
            std_git(&repo, &["remote", "add", "origin", &bare.to_string_lossy()]);
            std_git(&repo, &["push", "-u", "origin", "HEAD"]);
        }
        repo
    }

    fn test_app(root: &Path, cfg: Value) -> Shared {
        let (sse, _) = broadcast::channel(8);
        let (tx, _rx) = mpsc::unbounded_channel();
        Arc::new(App {
            root: root.to_path_buf(),
            panel: root.join("panel"),
            uploads: root.join("chat-uploads"),
            snaps: root.join("chat-snapshots"),
            ws_root: root.to_string_lossy().replace('\\', "/"),
            cfg: tokio::sync::RwLock::new(cfg),
            port: 0,
            chats: tokio::sync::Mutex::new(std::collections::BTreeMap::new()),
            chat_seq: AtomicI64::new(0),
            sse,
            evt_tx: tx,
            started: Instant::now(),
            restart_pending: AtomicBool::new(false),
            restarting: AtomicBool::new(false),
            busy_prev: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            assets: tokio::sync::Mutex::new(crate::assets::Mgr::new()),
            http: reqwest::Client::new(),
        })
    }

    fn queued_issue(key: &str, repo: &str, level: u64) -> Value {
        let now = now_iso();
        json!({
            "key": key, "source": "manual", "repo": repo,
            "title": "テスト課題", "body": "", "labels": [],
            "symptomHash": "t", "status": pipeline::ST_QUEUED,
            "autonomous": true, "verifyLevel": level,
            "acceptance": "動くこと", "retries": 0, "verifyBacks": 0,
            "detectedAt": now, "lastSeen": now, "updatedAt": now,
            "runs": [],
        })
    }

    fn seed(root: &Path, issue: &Value) {
        save_j(
            &store::issues_path(root),
            &json!([issue]),
        );
        save_j(
            &store::queue_path(root),
            &json!([{ "key": issue["key"], "enqueuedAt": now_iso() }]),
        );
    }

    fn issue_status(root: &Path, key: &str) -> String {
        find_issue(root, key)
            .and_then(|i| i.get("status").and_then(|v| v.as_str()).map(String::from))
            .unwrap_or_default()
    }

    async fn wait_status(root: &Path, key: &str, want: &str, secs: u64) -> String {
        for _ in 0..(secs * 4) {
            let s = issue_status(root, key);
            if s == want {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        issue_status(root, key)
    }

    fn cfg_for(repo: &Path, exe: &str, prompt_args: Value, approval: bool) -> Value {
        json!({
            "chatAgent": "t",
            "chatAgents": {
                "t": {"label": "t", "exe": exe, "promptArgs": prompt_args}
            },
            "verifyBackLimit": 3,
            "repos": [{
                "slug": "t/r", "path": repo.to_string_lossy().replace('\\', "/"),
                "requirePushApproval": approval,
            }],
        })
    }

    /// 実装コミットを行う偽実行体（prompt 引数は無視）。cwd は repo。
    /// Windows cmd バッチ: git add+commit（Issue: トレーラ付き）
    fn fake_agent(root: &Path, key: &str) -> PathBuf {
        let bat = root.join("fake-agent.cmd");
        std::fs::write(
            &bat,
            format!(
                "@echo off\r\necho RUN-OK\r\necho x >> impl.txt\r\ngit add -A\r\ngit commit -m \"impl\" -m \"Issue: {}\"\r\nexit /b 0\r\n",
                key
            ),
        )
        .unwrap();
        bat
    }

    #[tokio::test]
    async fn dispatch_issue_starts_queued_issue() {
        let root = temp_root("dispatch");
        let repo = make_repo(&root, "repo", true);
        let key = "manual:t/r:dispatch1";
        let agent = fake_agent(&root, key);
        let app = test_app(
            &root,
            cfg_for(&repo, "cmd", json!(["/C", agent.to_string_lossy()]), false),
        );
        let issue = queued_issue(key, "t/r", 0);
        seed(&root, &issue);

        let started = dispatch_issue(&app, key).await.unwrap();
        assert_eq!(started["status"], json!(pipeline::ST_RUNNING));
        assert!(dispatch_issue(&app, key).await.is_err());
        let s = wait_status(&root, key, pipeline::ST_DONE, 60).await;
        assert_eq!(s, pipeline::ST_DONE);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn drain_does_not_autostart_queued() {
        // dispatch は人判断のみ: drain は queued を起動しない（issue-lifecycle.mmd）
        let root = temp_root("noauto");
        let repo = make_repo(&root, "repo", true);
        let key = "manual:t/r:noauto1";
        let agent = fake_agent(&root, key);
        let app = test_app(
            &root,
            cfg_for(&repo, "cmd", json!(["/C", agent.to_string_lossy()]), false),
        );
        let issue = queued_issue(key, "t/r", 0);
        seed(&root, &issue);

        drain(&app).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(issue_status(&root, key), pipeline::ST_QUEUED);
        assert!(find_issue(&root, key).unwrap()["runs"]
            .as_array()
            .unwrap()
            .is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn dispatch_run_to_done_with_push() {
        let root = temp_root("done");
        let repo = make_repo(&root, "repo", true);
        let key = "manual:t/r:done1";
        let agent = fake_agent(&root, key);
        let app = test_app(
            &root,
            cfg_for(&repo, "cmd", json!(["/C", agent.to_string_lossy()]), false),
        );
        let issue = queued_issue(key, "t/r", 0);
        seed(&root, &issue);

        dispatch_issue(&app, key).await.unwrap();
        let s = wait_status(&root, key, pipeline::ST_DONE, 60).await;
        assert_eq!(s, pipeline::ST_DONE);
        let it = find_issue(&root, key).unwrap();
        assert_eq!(it["verifyOk"], json!(true));
        assert!(it["pushedAt"].is_string());
        // run 記録・証跡
        assert_eq!(it["runs"].as_array().unwrap().len(), 1);
        assert!(root
            .join("evidence")
            .join(knowledge::key_slug(key))
            .join("run.log")
            .is_file());
        // リモートへ実際に push されている
        let (c, _) = git(&repo, &["rev-list", "--count", "@{u}..HEAD"]).await;
        assert_eq!(c, 0);
        let (_, n) = git(&repo, &["rev-list", "--count", "@{u}..HEAD"]).await;
        assert_eq!(n.trim(), "0");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn dispatch_waits_push_approval() {
        let root = temp_root("gate");
        let repo = make_repo(&root, "repo", true);
        let key = "manual:t/r:gate1";
        let agent = fake_agent(&root, key);
        let app = test_app(
            &root,
            cfg_for(&repo, "cmd", json!(["/C", agent.to_string_lossy()]), true),
        );
        let issue = queued_issue(key, "t/r", 0);
        seed(&root, &issue);

        dispatch_issue(&app, key).await.unwrap();
        // requirePushApproval → verify 合格後も verifying で待機
        let s = wait_status(&root, key, pipeline::ST_VERIFYING, 60).await;
        assert_eq!(s, pipeline::ST_VERIFYING);
        for _ in 0..240 {
            let it = find_issue(&root, key).unwrap();
            if it["verifyOk"] == json!(true) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        assert_eq!(find_issue(&root, key).unwrap()["verifyOk"], json!(true));
        // push 承認 → 定期 drain（本番の15s tick相当）が push して done。
        // 承認時点で run タスクが in-flight なら次回 drain が拾うため繰り返す
        let _ = pipeline::set_field(&root, key, "pushApproved", json!(true));
        let mut s = String::new();
        for _ in 0..40 {
            drain(&app).await;
            s = issue_status(&root, key);
            if s == pipeline::ST_DONE {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        assert_eq!(s, pipeline::ST_DONE);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn dispatch_verify_ng_goes_back_to_queue() {
        let root = temp_root("ng");
        let repo = make_repo(&root, "repo", false);
        let key = "manual:t/r:ng1";
        // 実装しない偽実行体（echo のみ）
        let app = test_app(&root, cfg_for(&repo, "cmd", json!(["/C", "echo", "RUN-OK"]), false));
        let issue = queued_issue(key, "t/r", 0);
        seed(&root, &issue);

        dispatch_issue(&app, key).await.unwrap();
        // Issue: トレーラ付きコミット無し → NG → retry → queued
        let s = wait_status(&root, key, pipeline::ST_QUEUED, 60).await;
        assert_eq!(s, pipeline::ST_QUEUED);
        let it = find_issue(&root, key).unwrap();
        assert_eq!(it["verifyBacks"], json!(1));
        assert_eq!(it["runs"].as_array().unwrap().len(), 1);
        assert!(it["verifyNote"].is_string());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn build_prompt_uses_intake_snapshot_and_followups() {
        // intake-snapshot.mmd: prompt の課題本文は freeze した intakeSnapshot、
        // 起票後の追記は未消費分のみ「フォローアップ入力」として別節注入
        let root = temp_root("prompt");
        let mut issue = queued_issue("manual:t/r:snap1", "t/r", 1);
        issue["title"] = json!("live title");
        issue["body"] = json!("live body");
        issue["intakeSnapshot"] = json!({
            "title": "frozen title", "body": "frozen body", "at": now_iso(),
        });
        issue["followUps"] = json!([
            {"at": now_iso(), "kind": "ci-failure", "text": "ci broke",
             "ref": "ci-1", "consumedAt": Value::Null},
            {"at": now_iso(), "kind": "manual", "text": "old note",
             "consumedAt": now_iso()},
        ]);
        let p = build_prompt_full(&root, &issue).0;
        assert!(p.contains("frozen title"), "prompt: {}", p);
        assert!(p.contains("frozen body"));
        assert!(!p.contains("live title"));
        assert!(p.contains("フォローアップ入力"));
        assert!(p.contains("ci broke"));
        assert!(!p.contains("old note"), "consumed は注入しない");
        // スナップショット無しの課題は live 値を使う（後方互換）
        let issue2 = queued_issue("manual:t/r:snap2", "t/r", 1);
        let p2 = build_prompt_full(&root, &issue2).0;
        assert!(p2.contains("テスト課題"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn send_back_sets_exponential_retry_after() {
        // verify NG 差し戻しは指数バックオフの retryAfter を課題へ記録し、
        // 経過前の再 dispatch は can_dispatch が止める（manual-6c3ed946）
        let root = temp_root("backoff");
        let repo = make_repo(&root, "repo", false);
        let key = "manual:t/r:backoff1";
        let app = test_app(&root, cfg_for(&repo, "cmd", json!(["/C", "echo", "x"]), false));
        let mut issue = queued_issue(key, "t/r", 0);
        issue["status"] = json!(pipeline::ST_VERIFYING);
        seed(&root, &issue);

        send_back(&app, key, "検証失敗", Obs::default()).await;
        let it = find_issue(&root, key).unwrap();
        assert_eq!(it["status"], json!(pipeline::ST_QUEUED));
        assert_eq!(it["verifyBacks"], json!(1));
        let t = chrono::DateTime::parse_from_rfc3339(it["retryAfter"].as_str().unwrap())
            .unwrap();
        // 1回目の NG = base（既定5分）× 2^0 ≈ 5分後
        let ahead = (t.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_seconds();
        assert!(ahead > 60 && ahead <= 5 * 60 + 30, "ahead={}", ahead);
        let issues = store::load_issues(&root);
        let e = pipeline::can_dispatch(&json!({}), &issues, key).unwrap_err();
        assert!(e.contains("backoff"), "got: {}", e);
        let _ = std::fs::remove_dir_all(&root);
    }
}
