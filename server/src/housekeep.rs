// housekeep.rs — drain 定期 tick の retention 掃除（housekeep.mmd・README §11）
//
// config.retentionDays（既定 0=掃除しない）を超えて古いものを削除する:
//   - evidence/<key-slug>/ … 終端課題（done/failed/rejected/dropped）に
//     対応する dir のみ。課題が残る・進行中の証跡は障害調査の材料のため保持
//   - logs/*.log … 更新時刻が期限切れのもの
use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::knowledge;
use crate::pipeline;
use crate::store;

fn older_than(p: &Path, cutoff: SystemTime) -> bool {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .map(|t| t < cutoff)
        .unwrap_or(false)
}

/// retention 掃除を1回実行。戻り値は削除したパス数。
pub fn sweep(root: &Path, retention_days: u64) -> usize {
    if retention_days == 0 {
        return 0;
    }
    let cutoff = SystemTime::now() - Duration::from_secs(retention_days * 86400);
    let mut removed = 0;

    // evidence/<key-slug>: 終端課題のみ対象（housekeep.mmd）
    let terminal: HashSet<String> = store::load_issues(root)
        .iter()
        .filter(|i| {
            matches!(
                i.get("status").and_then(|v| v.as_str()),
                Some(pipeline::ST_DONE)
                    | Some(pipeline::ST_FAILED)
                    | Some(pipeline::ST_REJECTED)
                    | Some(pipeline::ST_DROPPED)
            )
        })
        .filter_map(|i| {
            i.get("key")
                .and_then(|v| v.as_str())
                .map(knowledge::key_slug)
        })
        .collect();
    let ev = root.join("evidence");
    if ev.is_dir() {
        for e in std::fs::read_dir(&ev).into_iter().flatten().flatten() {
            let p = e.path();
            if !p.is_dir() || !older_than(&p, cutoff) {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if terminal.contains(&name) && std::fs::remove_dir_all(&p).is_ok() {
                removed += 1;
            }
        }
    }

    // logs/*.log: 期限切れのみ
    let logs = root.join("logs");
    if logs.is_dir() {
        for e in std::fs::read_dir(&logs).into_iter().flatten().flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("log") || !older_than(&p, cutoff) {
                continue;
            }
            if std::fs::remove_file(&p).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "kasugai-housekeep-test-{}-{}",
            tag,
            crate::util::rand_hex(4)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn sweep_disabled_and_fresh_dirs_remove_nothing() {
        let root = temp_root("sweep");
        // retentionDays=0 は掃除しない
        assert_eq!(sweep(&root, 0), 0);
        // 終端課題の evidence でも新しい dir は残す（期限切れのみ対象）
        let key = "test:me/repo:h1";
        store::save_issues(
            &root,
            &[json!({"key": key, "status": pipeline::ST_DONE})],
        );
        let ev = root
            .join("evidence")
            .join(knowledge::key_slug(key));
        std::fs::create_dir_all(&ev).unwrap();
        std::fs::write(ev.join("shot.png"), b"x").unwrap();
        std::fs::create_dir_all(root.join("logs")).unwrap();
        std::fs::write(root.join("logs/a.log"), b"x").unwrap();
        assert_eq!(sweep(&root, 30), 0);
        assert!(ev.is_dir());
        assert!(root.join("logs/a.log").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }
}
