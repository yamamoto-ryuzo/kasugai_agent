// store.rs — 課題系 JSON 永続化（issues/candidates/work-queue。README §6）
// 課題レコードの状態遷移は pipeline.rs だけが行う。ここは保存・読込・
// キー/ハッシュの純粋な補助のみ。
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::config::{load_j, save_j};

pub fn issues_path(root: &Path) -> PathBuf {
    root.join("issues.json")
}
pub fn candidates_path(root: &Path) -> PathBuf {
    root.join("candidates.json")
}
pub fn queue_path(root: &Path) -> PathBuf {
    root.join("work-queue.json")
}

pub fn load_issues(root: &Path) -> Vec<Value> {
    load_j(&issues_path(root), json!([]))
        .as_array()
        .cloned()
        .unwrap_or_default()
}
pub fn save_issues(root: &Path, issues: &[Value]) {
    save_j(&issues_path(root), &json!(issues));
}
pub fn load_candidates(root: &Path) -> Vec<Value> {
    load_j(&candidates_path(root), json!([]))
        .as_array()
        .cloned()
        .unwrap_or_default()
}
pub fn save_candidates(root: &Path, cands: &[Value]) {
    save_j(&candidates_path(root), &json!(cands));
}
pub fn load_queue(root: &Path) -> Vec<Value> {
    load_j(&queue_path(root), json!([]))
        .as_array()
        .cloned()
        .unwrap_or_default()
}
pub fn save_queue(root: &Path, q: &[Value]) {
    save_j(&queue_path(root), &json!(q));
}

// ---- issue key / symptomHash（detect-pipeline.mmd: key = source+repo+symptomHash） ----

/// 安定した issue key。repo は slug または正規化 path。
pub fn issue_key(source: &str, repo: &str, symptom_hash: &str) -> String {
    format!("{}:{}:{}", source, repo, symptom_hash)
}

/// 検出署名。detector が symptomHash を emit しない場合のフォールバック
/// （title+body の正規化テキストを FNV-1a でハッシュ）
pub fn symptom_hash(title: &str, body: &str) -> String {
    let norm = |s: &str| s.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
    let text = format!("{}\n{}", norm(title), norm(body));
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", h)
}
