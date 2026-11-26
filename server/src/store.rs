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
pub fn silences_path(root: &Path) -> PathBuf {
    root.join("silences.json")
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
pub fn load_silences(root: &Path) -> Vec<Value> {
    load_j(&silences_path(root), json!([]))
        .as_array()
        .cloned()
        .unwrap_or_default()
}
/// 保存時に endsAt 経過済みの silence を除く（期限切れ自動解除。
/// endsAt 欠落・不正なレコードも残さない）
pub fn save_silences(root: &Path, sils: &[Value]) {
    let now = chrono::Utc::now();
    let kept: Vec<Value> = sils
        .iter()
        .filter(|s| {
            s.get("endsAt")
                .and_then(|v| v.as_str())
                .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
                .map(|t| t.with_timezone(&chrono::Utc) > now)
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    save_j(&silences_path(root), &json!(kept));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "kasugai-store-test-{}-{}",
            tag,
            crate::util::rand_hex(4)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn symptom_hash_is_stable_and_normalized() {
        // 同一入力で安定
        assert_eq!(symptom_hash("Title", "Body"), symptom_hash("Title", "Body"));
        // 小文字化・空白正規化が効く
        assert_eq!(symptom_hash("Title", "Body"), symptom_hash("title", "body"));
        assert_eq!(symptom_hash("a   b", "c"), symptom_hash("a b", "c"));
        // 異なる入力は別ハッシュ（16桁 hex）
        let h = symptom_hash("a", "b");
        assert_eq!(h.len(), 16);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(h, symptom_hash("a", "c"));
    }

    #[test]
    fn issue_key_format_is_source_repo_hash() {
        assert_eq!(
            issue_key("github", "me/repo", "0123456789abcdef"),
            "github:me/repo:0123456789abcdef"
        );
        assert_eq!(issue_key("local", "C:/x/y", "deadbeef"), "local:C:/x/y:deadbeef");
    }

    #[test]
    fn issues_candidates_queue_roundtrip() {
        let root = temp_root("io");
        // 未存在は空配列
        assert!(load_issues(&root).is_empty());
        assert!(load_candidates(&root).is_empty());
        assert!(load_queue(&root).is_empty());

        save_issues(&root, &[json!({"key": "k1"})]);
        save_candidates(&root, &[json!({"title": "c1"})]);
        save_queue(&root, &[json!({"key": "k1"})]);
        assert_eq!(load_issues(&root)[0]["key"], json!("k1"));
        assert_eq!(load_candidates(&root)[0]["title"], json!("c1"));
        assert_eq!(load_queue(&root)[0]["key"], json!("k1"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
