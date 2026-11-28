// trajectory.rs — run 全過程の trajectory 記録（observability.mmd・README §5.8）
//
// SWE-agent の .traj を参考に、evidence（結果の証拠）とは別に run の全過程
// （thought/action/observation/state）を JSON で保存する。steps は run 単位の
// artifact ディレクトリ evidence/<slug>/run-<idx>/trajectory.jsonl へ1行
// 1ステップで逐次追記し（中断しても過程が残る）、終了時に trajectory.json
// へ meta + steps + endReason をまとめて書き出す。
// 終了理由（success / verify_ng / limit / stuck / aborted / error）は
// 構造化データとして持ち、SKILL draft 生成・失敗分析の原料にする。
use serde_json::{json, Map, Value};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::util::now_iso;

/// run 単位の artifact ディレクトリ（evidence/<slug>/run-<idx>/）
pub fn run_dir(ev_dir: &Path, idx: usize) -> PathBuf {
    ev_dir.join(format!("run-{}", idx))
}

/// 課題の最新 run index（runs[] 末尾。無ければ None）
pub fn latest_run_idx(issue: &Value) -> Option<usize> {
    issue
        .get("runs")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .filter(|n| *n > 0)
        .map(|n| n - 1)
}

/// endReason の構造化レコードを組み立てる
pub fn end_reason(kind: &str, detail: &str) -> Value {
    json!({"kind": kind, "detail": detail})
}

/// trajectory 追記器。begin で run のメタを確定し、step で逐次追記、
/// finish で終了理由を確定して trajectory.json を書き出す
pub struct Trajectory {
    dir: PathBuf,
    meta: Map<String, Value>,
    steps_path: PathBuf,
    n: usize,
}

impl Trajectory {
    /// dir = evidence/<slug>/run-<idx>/。meta には issue/run/agent/repo/
    /// verifyLevel 等の run 特定情報を入れる（揮発 id は含めてよい —
    /// trajectory は課題履歴であり安定名を要求されない）。
    /// meta は即座に meta.json へ永続化し、別タスクの open/finish で復元する
    pub fn begin(dir: PathBuf, meta: Map<String, Value>) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        if !meta.is_empty() {
            let _ = std::fs::write(
                dir.join("meta.json"),
                serde_json::to_string_pretty(&Value::Object(meta.clone()))
                    .unwrap_or_default(),
            );
        }
        let steps_path = dir.join("trajectory.jsonl");
        Self {
            dir,
            meta,
            steps_path,
            n: 0,
        }
    }

    /// 課題 key + run idx から trajectory を開く（verify 再開等の別タスクで
    /// 同じ run へ追記を続ける用途。steps は追記モードで開く）
    pub fn open(root: &Path, key: &str, idx: usize) -> Self {
        let dir = run_dir(
            &root
                .join("evidence")
                .join(crate::knowledge::key_slug(key)),
            idx,
        );
        Self::begin(dir, Map::new())
    }

    /// 1ステップ追記: kind=thought|action|observation|state
    pub fn step(&mut self, kind: &str, name: &str, data: Value) {
        let line = json!({
            "ts": now_iso(),
            "kind": kind,
            "name": name,
            "data": data,
        });
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.steps_path)
        {
            // secret redaction（manual-a3a60c51）: step data は出力由来の
            // 文字列を含み得るため書き込み点でもマスクする
            let _ = writeln!(f, "{}", crate::redact::scrub(&line.to_string()));
        }
        self.n += 1;
    }

    /// 終了: trajectory.json に meta + steps + endReason をまとめて書く。
    /// meta が空（open で復帰した側）なら meta.json の meta を使う
    pub fn finish(&self, end_reason: Value) {
        let mut steps = Vec::new();
        if let Ok(text) = std::fs::read_to_string(&self.steps_path) {
            for l in text.lines() {
                if let Ok(v) = serde_json::from_str::<Value>(l) {
                    steps.push(v);
                }
            }
        }
        let out = self.dir.join("trajectory.json");
        let mut doc = self.meta.clone();
        if doc.is_empty() {
            if let Ok(prev) = std::fs::read_to_string(self.dir.join("meta.json")) {
                if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(&prev) {
                    doc = m;
                }
            }
        }
        doc.insert("endedAt".into(), json!(now_iso()));
        doc.insert("endReason".into(), end_reason);
        doc.insert("stepCount".into(), json!(steps.len()));
        doc.insert("steps".into(), json!(steps));
        // secret redaction（manual-a3a60c51）: 出力由来の steps/meta を
        // 束ねた最終形も書き込み点でマスクする
        let _ = std::fs::write(
            &out,
            crate::redact::scrub(
                &serde_json::to_string_pretty(&Value::Object(doc)).unwrap_or_default(),
            ),
        );
    }
}

/// 孤児 run（control 再起動で追跡を喪失）への終了補記。
/// 直近 run の trajectory に state ステップを追記し endReason=aborted で閉じる
/// （既に trajectory.json がある run は上書きしない）
pub fn abort_orphan(root: &Path, key: &str) {
    let Some(issue) = crate::store::load_issues(root)
        .into_iter()
        .find(|i| i.get("key").and_then(|v| v.as_str()) == Some(key))
    else {
        return;
    };
    let Some(idx) = latest_run_idx(&issue) else {
        return;
    };
    let dir = run_dir(
        &root.join("evidence").join(crate::knowledge::key_slug(key)),
        idx,
    );
    if dir.join("trajectory.json").is_file() {
        return; // 終了済み
    }
    if !dir.join("trajectory.jsonl").is_file() {
        return; // trajectory 未開始（旧形式の run）
    }
    let mut t = Trajectory::open(root, key, idx);
    t.step(
        "state",
        "aborted",
        json!({"note": "control 再起動で run の追跡を喪失"}),
    );
    t.finish(end_reason("aborted", "control 再起動で run の追跡を喪失"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "kasugai-traj-test-{}-{}",
            tag,
            crate::util::rand_hex(4)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn trajectory_steps_and_finish() {
        let root = temp_root("io");
        let dir = root.join("evidence").join("k").join("run-0");
        let mut t = Trajectory::begin(
            dir.clone(),
            Map::from_iter([
                ("issue".into(), json!("k")),
                ("run".into(), json!(0)),
            ]),
        );
        t.step("action", "spawn", json!({"exe": "devin"}));
        t.step("observation", "out", json!({"line": "hello"}));
        t.step("state", "verifying", json!({}));
        assert!(dir.join("trajectory.jsonl").is_file());
        t.finish(end_reason("success", "push 済み"));
        let doc: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("trajectory.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(doc["issue"], json!("k"));
        assert_eq!(doc["endReason"]["kind"], json!("success"));
        assert_eq!(doc["steps"].as_array().unwrap().len(), 3);
        assert_eq!(doc["steps"][0]["kind"], json!("action"));
        assert_eq!(doc["stepCount"], json!(3));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn trajectory_open_appends_to_same_run() {
        let root = temp_root("reopen");
        let slug = crate::knowledge::key_slug("src:repo:h");
        let dir = root.join("evidence").join(&slug).join("run-2");
        {
            let mut t = Trajectory::begin(dir.clone(), Map::new());
            t.step("action", "spawn", json!({}));
            t.step("observation", "out", json!({"line": "a"}));
        }
        // 別タスクが open で追記を続ける
        let mut t = Trajectory::open(&root, "src:repo:h", 2);
        t.step("state", "done", json!({}));
        t.finish(end_reason("success", ""));
        let doc: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("trajectory.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(doc["steps"].as_array().unwrap().len(), 3);
        let _ = std::fs::remove_dir_all(&root);
    }
}
